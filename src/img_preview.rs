//! Inline terminal image preview renderer using `image` (PNG/JPEG/GIF/WebP) ->
//! Truecolor Unicode Half-Blocks (`▀` U+2580) for `ratatui` + Kitty / iTerm2 / Sixel / ANSI Half-Block rendering for TUI & CLI.

const ENV_PROTOCOL_KEYS: &[&str] = &["ONENOTE_IMAGE_PROTOCOL", "TEAMS_IMAGE_PROTOCOL"];
const ENV_CELL_PX_KEYS: &[&str] = &["ONENOTE_CELL_PIXEL_SIZE", "TEAMS_CELL_PIXEL_SIZE"];

use std::collections::HashMap;
use std::fmt::Write as _;
#[cfg(unix)]
use std::io::Write as _;
use std::io::{Cursor, IsTerminal};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use image::{imageops::FilterType, DynamicImage, ImageFormat, Rgba};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
};

use crate::config::ImageProtocolPreference;

/// Upper memory budget (8 MiB) for decoded terminal inline image caches.
pub const MAX_IMAGE_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Fixed Kitty graphics image ID used by the TUI preview overlay so prior placements can be cleanly deleted.
pub const KITTY_PREVIEW_IMAGE_ID: u32 = 4242;

/// Memory safety caps for pixel rendering (supports 4K/HiDPI and 200%-300% zoom).
pub const MAX_RENDER_PX_W: u32 = 4096;
pub const MAX_RENDER_PX_H: u32 = 8192;

/// Proportional downscale pixel budget for Sixel raster buffers.
pub const MAX_SIXEL_PIXELS: u64 = 4_000_000;

/// Maximum entries allowed in the Sixel quantization RGB lookup cache.
pub const MAX_QUANT_LOOKUP_CACHE: usize = 16_384;

const MAX_ITERM2_RAW_BYTES: usize = 2 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

/// Explicit viewport sizing and zoom contract for terminal image rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FitBox {
    pub view_cols: u16,
    pub view_rows: u16,
    pub zoom_pct: u16,
    pub scrollable: bool,
}

impl FitBox {
    /// Non-scrollable contained fit within `(view_cols, view_rows)` at 100% zoom.
    pub fn contained(view_cols: u16, view_rows: u16) -> Self {
        Self {
            view_cols: view_cols.max(1),
            view_rows: view_rows.max(1),
            zoom_pct: 100,
            scrollable: false,
        }
    }

    /// Scrollable viewport fit with explicit zoom percentage (`25..=300`).
    pub fn scrollable(view_cols: u16, view_rows: u16, zoom_pct: u16) -> Self {
        Self {
            view_cols: view_cols.max(1),
            view_rows: view_rows.max(1),
            zoom_pct: zoom_pct.clamp(25, 300),
            scrollable: true,
        }
    }
}

/// Constraining axis emitted for Kitty Graphics Protocol placements (never both `c` and `r`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KittyFitAxis {
    Cols(u16),
    Rows(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalImageProtocol {
    Kitty,
    ITerm2,
    Sixel,
    Halfblock,
}

impl TerminalImageProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::ITerm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::Halfblock => "halfblock",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Kitty => "Kitty",
            Self::ITerm2 => "iTerm2",
            Self::Sixel => "Sixel",
            Self::Halfblock => "Halfblock",
        }
    }

    pub fn is_pixel(self) -> bool {
        !matches!(self, Self::Halfblock)
    }

    pub fn next_manual(self) -> Self {
        match self {
            Self::Kitty => Self::ITerm2,
            Self::ITerm2 => Self::Sixel,
            Self::Sixel => Self::Halfblock,
            Self::Halfblock => Self::Kitty,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalGraphicsCaps {
    pub protocol: TerminalImageProtocol,
    pub cell_px: (u16, u16),
    pub cell_px_exact: bool,
}

impl Default for TerminalGraphicsCaps {
    fn default() -> Self {
        Self {
            protocol: TerminalImageProtocol::Halfblock,
            cell_px: default_fallback_cell_px(),
            cell_px_exact: false,
        }
    }
}

fn default_fallback_cell_px() -> (u16, u16) {
    #[cfg(windows)]
    {
        (10, 20)
    }
    #[cfg(not(windows))]
    {
        (8, 16)
    }
}

/// Converts a `ratatui::style::Color` into concrete `(r, g, b)` for alpha compositing,
/// returning `fallback` when the color is `Color::Reset`.
pub fn color_to_rgb(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (204, 204, 204),
        Color::DarkGray => (118, 118, 118),
        Color::LightRed => (241, 76, 76),
        Color::LightGreen => (35, 209, 139),
        Color::LightYellow => (245, 245, 67),
        Color::LightBlue => (59, 142, 234),
        Color::LightMagenta => (214, 112, 214),
        Color::LightCyan => (41, 184, 219),
        Color::White => (255, 255, 255),
        Color::Indexed(idx) => xterm256_to_rgb(idx),
        Color::Reset => fallback,
    }
}

fn xterm256_to_rgb(idx: u8) -> (u8, u8, u8) {
    const ANSI16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    if idx < 16 {
        return ANSI16[idx as usize];
    }
    if idx >= 232 {
        let level = 8 + (idx - 232) * 10;
        return (level, level, level);
    }
    let cube = idx - 16;
    let r = cube / 36;
    let g = (cube % 36) / 6;
    let b = cube % 6;
    let to_ch = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
    (to_ch(r), to_ch(g), to_ch(b))
}

#[derive(Debug, Clone)]
enum PixelScrollBacking {
    SixelIndexed {
        width: u32,
        height: u32,
        cell_px: (u16, u16),
        palette: Vec<(u8, u8, u8)>,
        indexed: Arc<Vec<u8>>,
    },
    Source {
        image: Arc<DynamicImage>,
        target_px_w: u32,
        target_px_h: u32,
        cell_px: (u16, u16),
    },
}

impl PixelScrollBacking {
    fn approx_heap_bytes(&self) -> usize {
        match self {
            Self::SixelIndexed {
                palette, indexed, ..
            } => palette.capacity() * std::mem::size_of::<(u8, u8, u8)>() + indexed.capacity(),
            Self::Source { image, .. } => image.as_bytes().len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RenderedTerminalImage {
    pub width_px: u32,
    pub height_px: u32,
    pub cols: u16,
    pub rows: u16,
    pub format_label: String,
    pub protocol: TerminalImageProtocol,
    pub lines: Vec<Line<'static>>,
    /// Pre-encoded escape sequence payload when `protocol.is_pixel()` is true (positioned by caller in TUI).
    pub pixel_payload: Option<String>,
    /// Optional scrollable pixel backing used to generate viewport-cropped slices on scroll.
    scroll_backing: Option<PixelScrollBacking>,
}

impl RenderedTerminalImage {
    /// Approximate heap footprint of this rendered terminal image in bytes.
    pub fn approx_heap_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<Self>()
            + self.format_label.capacity()
            + self.lines.capacity() * std::mem::size_of::<Line<'static>>()
            + self.pixel_payload.as_ref().map_or(0, |s| s.capacity())
            + self
                .scroll_backing
                .as_ref()
                .map_or(0, PixelScrollBacking::approx_heap_bytes);
        for line in &self.lines {
            total += line.spans.capacity() * std::mem::size_of::<Span<'static>>();
            for span in &line.spans {
                total += span.content.len();
            }
        }
        total
    }

    /// Returns a viewport-cropped pixel protocol payload `(payload, draw_cols, draw_rows)` guaranteed
    /// to fit within `(view_cols, view_rows)` cells starting at cell offset `(scroll_x, scroll_y)`.
    pub fn viewport_pixel_payload(
        &self,
        scroll_x: u16,
        scroll_y: u16,
        view_cols: u16,
        view_rows: u16,
    ) -> Option<(String, u16, u16)> {
        if !self.protocol.is_pixel() || view_cols == 0 || view_rows == 0 {
            return None;
        }
        if scroll_x == 0 && scroll_y == 0 && self.cols <= view_cols && self.rows <= view_rows {
            return Some((self.pixel_payload.clone()?, self.cols, self.rows));
        }

        let backing = self.scroll_backing.as_ref()?;
        match backing {
            PixelScrollBacking::SixelIndexed {
                width,
                height,
                cell_px,
                palette,
                indexed,
            } => {
                let cell_w = cell_px.0.max(1) as u32;
                let cell_h = cell_px.1.max(1) as u32;
                let x0 = (scroll_x as u32).saturating_mul(cell_w);
                let y0 = (scroll_y as u32).saturating_mul(cell_h);
                if x0 >= *width || y0 >= *height {
                    return None;
                }
                let max_w = (view_cols as u32).saturating_mul(cell_w);
                let max_h = (view_rows as u32).saturating_mul(cell_h);
                let safe_h = compute_sixel_safe_pixel_height(view_rows, cell_px.1);
                let effective_max_h = if safe_h >= 6 { safe_h } else { max_h };
                let crop_w = (*width - x0).min(max_w);
                let crop_h = (*height - y0).min(effective_max_h);
                if crop_w == 0 || crop_h == 0 {
                    return None;
                }
                let draw_cols = crop_w.div_ceil(cell_w).clamp(1, view_cols as u32) as u16;
                let band_px_h = crop_h.div_ceil(6) * 6;
                let draw_rows = band_px_h.div_ceil(cell_h).clamp(1, view_rows as u32) as u16;
                let payload = encode_sixel_indexed_slice(
                    palette,
                    indexed,
                    *width as usize,
                    x0 as usize,
                    y0 as usize,
                    crop_w as usize,
                    crop_h as usize,
                );
                Some((payload, draw_cols, draw_rows))
            }
            PixelScrollBacking::Source {
                image,
                target_px_w,
                target_px_h,
                cell_px,
            } => {
                let cell_w = cell_px.0.max(1) as u32;
                let cell_h = cell_px.1.max(1) as u32;
                let target_w = (*target_px_w).max(1);
                let target_h = (*target_px_h).max(1);
                let x0 = (scroll_x as u32).saturating_mul(cell_w);
                let y0 = (scroll_y as u32).saturating_mul(cell_h);
                if x0 >= target_w || y0 >= target_h {
                    return None;
                }
                let max_w = (view_cols as u32).saturating_mul(cell_w);
                let max_h = (view_rows as u32).saturating_mul(cell_h);
                let crop_w = (target_w - x0).min(max_w);
                let crop_h = (target_h - y0).min(max_h);
                if crop_w == 0 || crop_h == 0 {
                    return None;
                }
                let draw_cols = crop_w.div_ceil(cell_w).clamp(1, view_cols as u32) as u16;
                let draw_rows = crop_h.div_ceil(cell_h).clamp(1, view_rows as u32) as u16;

                // Reverse-map virtual viewport coordinates onto native source pixels (no double scaling).
                let src_w = image.width().max(1);
                let src_h = image.height().max(1);
                let src_x0 = (((x0 as u64) * (src_w as u64)) / (target_w as u64))
                    .min((src_w - 1) as u64) as u32;
                let src_y0 = (((y0 as u64) * (src_h as u64)) / (target_h as u64))
                    .min((src_h - 1) as u64) as u32;
                let src_crop_w = ((crop_w as u64) * (src_w as u64))
                    .div_ceil(target_w as u64)
                    .clamp(1, (src_w - src_x0) as u64) as u32;
                let src_crop_h = ((crop_h as u64) * (src_h as u64))
                    .div_ceil(target_h as u64)
                    .clamp(1, (src_h - src_y0) as u64) as u32;

                let sub_img = image.crop_imm(src_x0, src_y0, src_crop_w, src_crop_h);
                let png_bytes = encode_png(&sub_img).ok()?;
                let payload = match self.protocol {
                    TerminalImageProtocol::Kitty => {
                        let fit_axis = choose_kitty_fit_axis(
                            sub_img.width(),
                            sub_img.height(),
                            draw_cols,
                            draw_rows,
                            *cell_px,
                        );
                        encode_kitty_tui_payload(&png_bytes, fit_axis)
                    }
                    TerminalImageProtocol::ITerm2 => {
                        let b64 = B64_STANDARD.encode(&png_bytes);
                        format!(
                            "\x1b]1337;File=inline=1;width={draw_cols};height={draw_rows};preserveAspectRatio=1;doNotMoveCursor=1:{b64}\x07"
                        )
                    }
                    TerminalImageProtocol::Sixel | TerminalImageProtocol::Halfblock => {
                        return None;
                    }
                };
                Some((payload, draw_cols, draw_rows))
            }
        }
    }
}

/// Tracks an active terminal pixel graphic placement (Kitty / iTerm2 / Sixel) painted onto the screen
/// so frame transitions can deterministically erase stale pixels before Ratatui redraws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivePixelGraphic {
    pub cache_key: String,
    pub rect: Rect,
    pub protocol: TerminalImageProtocol,
    pub cols: u16,
    pub rows: u16,
    pub payload_len: usize,
}

/// Returns the Kitty graphics delete sequence for the fixed TUI preview placement (`q=2` suppresses terminal responses).
pub fn kitty_delete_preview_sequence() -> &'static str {
    "\x1b_Ga=d,d=i,i=4242,q=2\x1b\\"
}

/// Deletes all visible Kitty graphics placements and frees unused image data (`d=A`, `q=2` quiet).
pub fn kitty_delete_all_sequence() -> &'static str {
    "\x1b_Ga=d,d=A,q=2\x1b\\"
}

/// Builds an ANSI sequence (`\x1b[0m` + row cursor moves + space runs) that overwrites `rect`
/// (expanded by +2 cols / +2 rows and clamped to `term_cols x term_rows`) so Sixel and iTerm2
/// cell-bound pixel tiles are deterministically erased before Ratatui redraws.
pub fn erase_pixel_rect_sequence(
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    term_cols: u16,
    term_rows: u16,
) -> String {
    if width == 0 || height == 0 || x >= term_cols || y >= term_rows {
        return String::new();
    }
    let x_end = x.saturating_add(width).saturating_add(2).min(term_cols);
    let y_end = y.saturating_add(height).saturating_add(2).min(term_rows);
    if x >= x_end || y >= y_end {
        return String::new();
    }
    let span_w = (x_end - x) as usize;
    let rows = (y_end - y) as usize;
    let spaces = " ".repeat(span_w);
    let mut out = String::with_capacity(4 + rows * (12 + span_w));
    out.push_str("\x1b[0m");
    for row in y..y_end {
        let _ = write!(out, "\x1b[{};{}H{spaces}", row + 1, x + 1);
    }
    out
}

/// Floors the maximum vertical pixel budget for Sixel encoding to a multiple of 6
/// (`<= rows * cell_px_h`) so the final 6-pixel band never overhangs into the row below `rect`.
pub fn compute_sixel_safe_pixel_height(rows: u16, cell_px_h: u16) -> u32 {
    let budget = (rows as u32).saturating_mul(cell_px_h as u32);
    (budget / 6) * 6
}

/// Computes rounded cell pixel dimensions `(cell_w, cell_h)` from window pixel dimensions and cell grid size.
pub fn cell_px_from_window_px(win_w: u32, win_h: u32, cols: u16, rows: u16) -> Option<(u16, u16)> {
    if win_w == 0 || win_h == 0 || cols == 0 || rows == 0 {
        return None;
    }
    let cols_u32 = cols as u32;
    let rows_u32 = rows as u32;
    let cw = ((win_w + cols_u32 / 2) / cols_u32).clamp(4, 64) as u16;
    let ch = ((win_h + rows_u32 / 2) / rows_u32).clamp(8, 128) as u16;
    Some((cw, ch))
}

/// Resolves terminal graphics protocol and cell pixel dimensions without writing queries to stdin/stdout.
pub fn detect_graphics_caps(pref: ImageProtocolPreference) -> TerminalGraphicsCaps {
    let protocol = resolve_protocol_with(pref, |k| std::env::var(k).ok());
    let (cell_px, cell_px_exact) = detect_cell_pixel_size();
    TerminalGraphicsCaps {
        protocol,
        cell_px,
        cell_px_exact,
    }
}

/// Timeout-bounded (`150ms`) active terminal graphics & cell-pixel probing.
///
/// Must be called once on TUI startup while raw mode is active (before `EventStream` starts reading stdin).
/// Queries:
/// - `CSI 16 t` (`\x1b[6;<h>;<w>t`) for exact cell pixel dimensions
/// - `CSI 14 t` (`\x1b[4;<h>;<w>t`) window pixel dimensions fallback
/// - Kitty Graphics query (`a=q`) and DA1 (`\x1b[c`) for Sixel (`;4`) when `pref == Auto` and env detection
///   did not already identify a pixel protocol.
pub fn probe_terminal_graphics_caps(pref: ImageProtocolPreference) -> TerminalGraphicsCaps {
    let mut caps = detect_graphics_caps(pref);

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return caps;
    }

    let get_env = |k: &str| std::env::var(k).ok();
    let is_mux = non_empty_env(&get_env, "TMUX").is_some() || non_empty_env(&get_env, "STY").is_some();
    let has_env_override = ENV_PROTOCOL_KEYS
        .iter()
        .find_map(|k| non_empty_env(&get_env, k))
        .and_then(|raw| ImageProtocolPreference::parse_str(&raw))
        .is_some_and(|p| p != ImageProtocolPreference::Auto);

    let need_cell_px = !caps.cell_px_exact;
    let need_proto = pref == ImageProtocolPreference::Auto
        && !has_env_override
        && !is_mux
        && caps.protocol == TerminalImageProtocol::Halfblock;

    if !need_cell_px && !need_proto {
        return caps;
    }

    let mut query = String::with_capacity(96);
    if need_cell_px {
        query.push_str("\x1b[16t\x1b[14t");
    }
    if need_proto {
        query.push_str("\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    }
    // Always end with DA1 sentinel so responsive terminals let us exit the probe loop immediately.
    query.push_str("\x1b[c");

    let reply = write_and_read_terminal_probe(&query, PROBE_TIMEOUT);
    if reply.is_empty() {
        return caps;
    }

    let term_size = crossterm::terminal::size().ok();
    apply_probe_reply_to_caps(&mut caps, reply.as_bytes(), need_cell_px, need_proto, term_size);
    caps
}

/// Refreshes `caps.cell_px` (e.g. on terminal resize / HiDPI monitor move / font size change).
///
/// On Unix, reads `TIOCGWINSZ` synchronously via `crossterm::terminal::window_size()` (no `EventStream` contention).
/// On Windows, caller must drop any active `EventStream` before calling this so the `CSI 16t/14t + DA1` probe
/// has exclusive access to `CONIN$`, then re-enable mouse capture and recreate `EventStream`.
/// Returns `true` if `caps.cell_px` or `caps.cell_px_exact` changed.
pub fn refresh_cell_pixel_size(caps: &mut TerminalGraphicsCaps) -> bool {
    let prev = (caps.cell_px, caps.cell_px_exact);

    if let Some(parsed) = resolve_cell_pixel_size_with(|k| std::env::var(k).ok()) {
        caps.cell_px = parsed;
        caps.cell_px_exact = true;
        return prev != (caps.cell_px, caps.cell_px_exact);
    }

    #[cfg(unix)]
    {
        if let Ok(ws) = crossterm::terminal::window_size() {
            if let Some(px) =
                cell_px_from_window_px(ws.width as u32, ws.height as u32, ws.columns, ws.rows)
            {
                caps.cell_px = px;
                caps.cell_px_exact = true;
                return prev != (caps.cell_px, caps.cell_px_exact);
            }
        }
    }

    #[cfg(windows)]
    {
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            let reply = write_and_read_terminal_probe("\x1b[16t\x1b[14t\x1b[c", PROBE_TIMEOUT);
            if !reply.is_empty() {
                let term_size = crossterm::terminal::size().ok();
                apply_probe_reply_to_caps(caps, reply.as_bytes(), true, false, term_size);
                return prev != (caps.cell_px, caps.cell_px_exact);
            }
        }
        if let Ok(ws) = crossterm::terminal::window_size() {
            if let Some(px) =
                cell_px_from_window_px(ws.width as u32, ws.height as u32, ws.columns, ws.rows)
            {
                caps.cell_px = px;
                caps.cell_px_exact = true;
            }
        }
    }

    prev != (caps.cell_px, caps.cell_px_exact)
}

fn apply_probe_reply_to_caps(
    caps: &mut TerminalGraphicsCaps,
    reply: &[u8],
    need_cell_px: bool,
    need_proto: bool,
    term_size: Option<(u16, u16)>,
) {
    if need_cell_px {
        if let Some(px) = parse_csi_cell_size_reply(reply) {
            caps.cell_px = px;
            caps.cell_px_exact = true;
        } else if let Some((cols, rows)) = term_size {
            if let Some(px) = parse_csi_win_size_reply(reply, cols, rows) {
                caps.cell_px = px;
                caps.cell_px_exact = true;
            }
        }
    }

    if need_proto {
        if parse_kitty_query_ok(reply) {
            caps.protocol = TerminalImageProtocol::Kitty;
        } else if parse_da1_has_sixel(reply) {
            caps.protocol = TerminalImageProtocol::Sixel;
        }
    }
}

/// Parses a `CSI 16 t` terminal reply (`\x1b[6;<height>;<width>t`) into `(cell_width_px, cell_height_px)`.
pub fn parse_csi_cell_size_reply(buf: &[u8]) -> Option<(u16, u16)> {
    let mut i = 0;
    while i + 4 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'6' && buf[i + 3] == b';' {
            let start = i + 4;
            if let Some(rel_end) = buf[start..].iter().position(|&b| b == b't') {
                if let Ok(body) = std::str::from_utf8(&buf[start..start + rel_end]) {
                    if let Some((h_str, w_str)) = body.split_once(';') {
                        if let (Ok(h), Ok(w)) =
                            (h_str.trim().parse::<u32>(), w_str.trim().parse::<u32>())
                        {
                            if w > 0 && h > 0 {
                                return Some((w.clamp(4, 64) as u16, h.clamp(8, 128) as u16));
                            }
                        }
                    }
                }
                i = start + rel_end + 1;
                continue;
            }
        }
        i += 1;
    }
    None
}

/// Parses a `CSI 14 t` terminal reply (`\x1b[4;<win_h>;<win_w>t`) and divides (rounded) by `(cols, rows)`
/// into `(cell_width_px, cell_height_px)`.
pub fn parse_csi_win_size_reply(buf: &[u8], cols: u16, rows: u16) -> Option<(u16, u16)> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let mut i = 0;
    while i + 4 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'4' && buf[i + 3] == b';' {
            let start = i + 4;
            if let Some(rel_end) = buf[start..].iter().position(|&b| b == b't') {
                if let Ok(body) = std::str::from_utf8(&buf[start..start + rel_end]) {
                    if let Some((h_str, w_str)) = body.split_once(';') {
                        if let (Ok(win_h), Ok(win_w)) =
                            (h_str.trim().parse::<u32>(), w_str.trim().parse::<u32>())
                        {
                            if let Some(px) = cell_px_from_window_px(win_w, win_h, cols, rows) {
                                return Some(px);
                            }
                        }
                    }
                }
                i = start + rel_end + 1;
                continue;
            }
        }
        i += 1;
    }
    None
}

/// Returns `true` if `buf` contains a positive Kitty Graphics query response (`\x1b_Gi=31...;OK\x1b\\`).
pub fn parse_kitty_query_ok(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'_' && buf[i + 2] == b'G' {
            let start = i + 3;
            let mut end = start;
            let mut found_term = false;
            while end < buf.len() {
                if buf[end] == 0x07 {
                    found_term = true;
                    break;
                }
                if buf[end] == 0x1b && end + 1 < buf.len() && buf[end + 1] == b'\\' {
                    found_term = true;
                    break;
                }
                end += 1;
            }
            if found_term {
                if let Ok(body) = std::str::from_utf8(&buf[start..end]) {
                    if let Some((ctrl, payload)) = body.split_once(';') {
                        let has_id = ctrl.split(',').any(|tok| tok.trim() == "i=31");
                        if has_id && payload.trim() == "OK" {
                            return true;
                        }
                    }
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    false
}

/// Returns `true` if `buf` contains a Primary Device Attributes (DA1) reply advertising Sixel (`;4`).
pub fn parse_da1_has_sixel(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?' {
            let start = i + 3;
            let mut end = start;
            while end < buf.len() && (buf[end].is_ascii_digit() || buf[end] == b';') {
                end += 1;
            }
            if end < buf.len() && buf[end] == b'c' {
                if let Ok(params) = std::str::from_utf8(&buf[start..end]) {
                    if params.split(';').skip(1).any(|p| p == "4") {
                        return true;
                    }
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    false
}

fn da1_reply_complete(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?' {
            let mut end = i + 3;
            while end < buf.len() && (buf[end].is_ascii_digit() || buf[end] == b';') {
                end += 1;
            }
            if end < buf.len() && buf[end] == b'c' {
                return true;
            }
        }
        i += 1;
    }
    false
}

#[cfg(any(windows, test))]
const WIN_ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
#[cfg(any(windows, test))]
const WIN_ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
#[cfg(any(windows, test))]
const WIN_ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

/// Restores Windows console input mode while always setting `ENABLE_EXTENDED_FLAGS` (`0x0080`)
/// and clearing `ENABLE_QUICK_EDIT_MODE` (`0x0040`) so mouse capture is never clobbered by QuickEdit.
#[cfg(any(windows, test))]
pub(crate) fn restore_windows_console_mode_flags(prev_mode: u32) -> u32 {
    (prev_mode | WIN_ENABLE_EXTENDED_FLAGS) & !WIN_ENABLE_QUICK_EDIT_MODE
}

/// Enables `ENABLE_VIRTUAL_TERMINAL_INPUT` via RAII `ModeRestore` guard **before** writing `query`
/// and flushing, then invokes `read_reply`, restoring console mode (with QuickEdit cleared) on drop.
#[cfg(any(windows, test))]
fn write_and_read_windows_probe_with<W, FSet, FRead>(
    prev_mode: u32,
    query: &str,
    writer: &mut W,
    mut set_mode: FSet,
    read_reply: FRead,
) -> String
where
    W: std::io::Write,
    FSet: FnMut(u32),
    FRead: FnOnce() -> Vec<u8>,
{
    struct ModeRestore<'a, F: FnMut(u32)> {
        active: bool,
        restore_mode: u32,
        set_mode: &'a mut F,
    }
    impl<F: FnMut(u32)> Drop for ModeRestore<'_, F> {
        fn drop(&mut self) {
            if self.active {
                (self.set_mode)(self.restore_mode);
            }
        }
    }

    let restore_mode = restore_windows_console_mode_flags(prev_mode);
    let probe_mode = restore_mode | WIN_ENABLE_VIRTUAL_TERMINAL_INPUT;
    let active = prev_mode != probe_mode;
    if active {
        set_mode(probe_mode);
    }
    let _restore = ModeRestore {
        active,
        restore_mode,
        set_mode: &mut set_mode,
    };

    if writer.write_all(query.as_bytes()).is_err() || writer.flush().is_err() {
        return String::new();
    }
    let bytes = read_reply();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(unix)]
fn write_and_read_terminal_probe(query: &str, timeout: Duration) -> String {
    use std::os::fd::AsRawFd;

    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    const POLLIN: i16 = 0x0001;

    extern "C" {
        fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
        fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    }

    let mut stdout = std::io::stdout();
    if stdout.write_all(query.as_bytes()).is_err() || stdout.flush().is_err() {
        return String::new();
    }

    let fd = std::io::stdin().as_raw_fd();
    let deadline = Instant::now() + timeout;
    let mut out = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];

    while out.len() < 4096 {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let wait_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        if wait_ms <= 0 {
            break;
        }
        let mut pfd = PollFd {
            fd,
            events: POLLIN,
            revents: 0,
        };
        let rc = unsafe { poll(&mut pfd, 1, wait_ms) };
        if rc <= 0 || (pfd.revents & POLLIN) == 0 {
            break;
        }
        let n = unsafe { read(fd, chunk.as_mut_ptr(), chunk.len()) };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n as usize]);
        if da1_reply_complete(&out) {
            break;
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(windows)]
fn write_and_read_terminal_probe(query: &str, timeout: Duration) -> String {
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6; // -10i32 as u32
    const WAIT_OBJECT_0: u32 = 0x0000_0000;
    const KEY_EVENT: u16 = 0x0001;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyEventRecord {
        b_key_down: i32,
        w_repeat_count: u16,
        w_virtual_key_code: u16,
        w_virtual_scan_code: u16,
        u_char: u16,
        dw_control_key_state: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    union InputEventUnion {
        key_event: KeyEventRecord,
        _pad: [u32; 4],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InputRecord {
        event_type: u16,
        _padding: u16,
        event: InputEventUnion,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut std::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: isize,
        ) -> isize;
        fn CloseHandle(h_object: isize) -> i32;
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn GetConsoleMode(h_console_handle: isize, lp_mode: *mut u32) -> i32;
        fn SetConsoleMode(h_console_handle: isize, dw_mode: u32) -> i32;
        fn WaitForSingleObject(h_handle: isize, dw_milliseconds: u32) -> u32;
        fn GetNumberOfConsoleInputEvents(
            h_console_input: isize,
            lpc_number_of_events: *mut u32,
        ) -> i32;
        fn PeekConsoleInputW(
            h_console_input: isize,
            lp_buffer: *mut InputRecord,
            n_length: u32,
            lp_number_of_events_read: *mut u32,
        ) -> i32;
        fn ReadConsoleInputW(
            h_console_input: isize,
            lp_buffer: *mut InputRecord,
            n_length: u32,
            lp_number_of_events_read: *mut u32,
        ) -> i32;
    }

    struct OwnedHandle(Option<isize>);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if let Some(h) = self.0 {
                unsafe {
                    CloseHandle(h);
                }
            }
        }
    }

    const CONIN_WIDE: [u16; 7] = [
        b'C' as u16,
        b'O' as u16,
        b'N' as u16,
        b'I' as u16,
        b'N' as u16,
        b'$' as u16,
        0,
    ];
    let conin = unsafe {
        CreateFileW(
            CONIN_WIDE.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            0,
        )
    };
    let (handle, _owned_handle) = if conin != 0 && conin != INVALID_HANDLE_VALUE {
        (conin, OwnedHandle(Some(conin)))
    } else {
        let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        (std_in, OwnedHandle(None))
    };
    if handle == 0 || handle == INVALID_HANDLE_VALUE {
        return String::new();
    }

    let mut prev_mode = 0u32;
    if unsafe { GetConsoleMode(handle, &mut prev_mode) } == 0 {
        return String::new();
    }

    let mut stdout = std::io::stdout();
    // `_owned_handle` outlives `write_and_read_windows_probe_with`, so `ModeRestore::drop`
    // always runs BEFORE `CloseHandle`.
    write_and_read_windows_probe_with(
        prev_mode,
        query,
        &mut stdout,
        |mode| {
            let _ = unsafe { SetConsoleMode(handle, mode) };
        },
        || {
            let deadline = Instant::now() + timeout;
            let mut out = Vec::with_capacity(256);
            let mut records = [InputRecord {
                event_type: 0,
                _padding: 0,
                event: InputEventUnion { _pad: [0; 4] },
            }; 64];

            while out.len() < 4096 {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                let wait_ms = remaining.as_millis().min(u32::MAX as u128) as u32;
                if wait_ms == 0 {
                    break;
                }
                let wait_res = unsafe { WaitForSingleObject(handle, wait_ms) };
                if wait_res != WAIT_OBJECT_0 {
                    break;
                }
                let mut avail = 0u32;
                if unsafe { GetNumberOfConsoleInputEvents(handle, &mut avail) } == 0 || avail == 0 {
                    break;
                }
                let to_read = (avail as usize).min(records.len()) as u32;
                let mut read_count = 0u32;
                if unsafe {
                    ReadConsoleInputW(handle, records.as_mut_ptr(), to_read, &mut read_count)
                } == 0
                    || read_count == 0
                {
                    break;
                }
                for rec in &records[..read_count as usize] {
                    if rec.event_type == KEY_EVENT {
                        let ke = unsafe { rec.event.key_event };
                        if ke.b_key_down != 0 && ke.u_char != 0 && ke.u_char <= 0x7f {
                            out.push(ke.u_char as u8);
                        }
                    }
                }
                if da1_reply_complete(&out) {
                    break;
                }
            }

            // Non-blocking drain of any trailing KEY_EVENT bytes from the probe burst so nothing leaks into EventStream.
            for _ in 0..8 {
                if unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0 {
                    break;
                }
                let mut avail = 0u32;
                if unsafe { GetNumberOfConsoleInputEvents(handle, &mut avail) } == 0 || avail == 0 {
                    break;
                }
                let to_peek = (avail as usize).min(records.len()) as u32;
                let mut peeked = 0u32;
                if unsafe {
                    PeekConsoleInputW(handle, records.as_mut_ptr(), to_peek, &mut peeked)
                } == 0
                    || peeked == 0
                {
                    break;
                }
                let key_prefix = records[..peeked as usize]
                    .iter()
                    .take_while(|rec| rec.event_type == KEY_EVENT)
                    .count() as u32;
                if key_prefix == 0 {
                    break;
                }
                let mut read_count = 0u32;
                if unsafe {
                    ReadConsoleInputW(handle, records.as_mut_ptr(), key_prefix, &mut read_count)
                } == 0
                    || read_count == 0
                {
                    break;
                }
                for rec in &records[..read_count as usize] {
                    if rec.event_type == KEY_EVENT {
                        let ke = unsafe { rec.event.key_event };
                        if ke.b_key_down != 0 && ke.u_char != 0 && ke.u_char <= 0x7f && out.len() < 4096 {
                            out.push(ke.u_char as u8);
                        }
                    }
                }
            }

            out
        },
    )
}

#[cfg(not(any(unix, windows)))]
fn write_and_read_terminal_probe(_query: &str, _timeout: Duration) -> String {
    String::new()
}

/// Returns `true` when running inside a Stackbite-managed PTY environment, where inherited parent
/// terminal env vars (`WT_SESSION`, etc.) must not be trusted without live DA1/Kitty probing.
pub fn is_stackbite_env<F>(get_env: &F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    non_empty_env(get_env, "STACKBITE_ROOM_ID").is_some()
        || non_empty_env(get_env, "STACKBITE_HOME").is_some()
        || non_empty_env(get_env, "STACKBITE_PTY_BACKEND").is_some()
}

/// Pure protocol resolver taking an environment variable map (for deterministic testing and diagnostics).
pub fn resolve_protocol_from_env_map(
    pref: ImageProtocolPreference,
    env: &HashMap<&str, &str>,
) -> TerminalImageProtocol {
    resolve_protocol_with(pref, |k| env.get(k).map(|v| (*v).to_string()))
}

fn resolve_protocol_with<F>(pref: ImageProtocolPreference, get_env: F) -> TerminalImageProtocol
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(raw) = ENV_PROTOCOL_KEYS
        .iter()
        .find_map(|k| non_empty_env(&get_env, k))
    {
        if let Some(parsed) = ImageProtocolPreference::parse_str(&raw) {
            if parsed != ImageProtocolPreference::Auto {
                return protocol_from_pref(parsed);
            }
        }
    }

    if pref != ImageProtocolPreference::Auto {
        return protocol_from_pref(pref);
    }

    // Multiplexer guard: avoid raw pixel escape sequences inside tmux/screen without DCS passthrough.
    if non_empty_env(&get_env, "TMUX").is_some() || non_empty_env(&get_env, "STY").is_some() {
        return TerminalImageProtocol::Halfblock;
    }

    // Stackbite PTY guard: never trust inherited parent terminal env vars statically;
    // TUI live probing (Kitty a=q + DA1 ;4) determines actual PTY capabilities.
    if is_stackbite_env(&get_env) {
        return TerminalImageProtocol::Halfblock;
    }

    let term_program = lower_env(&get_env, "TERM_PROGRAM");
    let term = lower_env(&get_env, "TERM");

    // Kitty Graphics Protocol (Kitty, Ghostty, WezTerm).
    if non_empty_env(&get_env, "KITTY_WINDOW_ID").is_some()
        || non_empty_env(&get_env, "GHOSTTY_RESOURCES_DIR").is_some()
        || non_empty_env(&get_env, "WEZTERM_EXECUTABLE").is_some()
        || non_empty_env(&get_env, "WEZTERM_PANE").is_some()
        || matches!(term_program.as_str(), "kitty" | "ghostty" | "wezterm")
        || term.contains("xterm-kitty")
        || term.contains("ghostty")
    {
        return TerminalImageProtocol::Kitty;
    }

    // iTerm2 Inline Images (`OSC 1337`).
    if matches!(
        term_program.as_str(),
        "iterm.app" | "rio" | "mintty" | "hyper"
    ) {
        return TerminalImageProtocol::ITerm2;
    }

    // Sixel (`DCS q`) for known native Linux Sixel terminals.
    // Note: `WT_SESSION` is intentionally NOT trusted statically so `Auto` always verifies
    // actual Sixel support via DA1 `;4` probing (avoiding blank renders on WT < 1.22 or inherited hosts).
    if matches!(
        term.as_str(),
        "foot" | "foot-extra" | "mlterm" | "yaft-256color"
    ) || matches!(term_program.as_str(), "foot" | "contour")
    {
        return TerminalImageProtocol::Sixel;
    }

    TerminalImageProtocol::Halfblock
}

fn protocol_from_pref(pref: ImageProtocolPreference) -> TerminalImageProtocol {
    match pref {
        ImageProtocolPreference::Auto | ImageProtocolPreference::Halfblock => {
            TerminalImageProtocol::Halfblock
        }
        ImageProtocolPreference::Kitty => TerminalImageProtocol::Kitty,
        ImageProtocolPreference::Iterm2 => TerminalImageProtocol::ITerm2,
        ImageProtocolPreference::Sixel => TerminalImageProtocol::Sixel,
    }
}

fn non_empty_env<F>(get_env: &F, key: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key).and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn lower_env<F>(get_env: &F, key: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key)
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

fn detect_cell_pixel_size() -> ((u16, u16), bool) {
    if let Some(parsed) = resolve_cell_pixel_size_with(|k| std::env::var(k).ok()) {
        return (parsed, true);
    }
    if let Ok(ws) = crossterm::terminal::window_size() {
        if let Some(px) =
            cell_px_from_window_px(ws.width as u32, ws.height as u32, ws.columns, ws.rows)
        {
            return (px, true);
        }
    }
    (default_fallback_cell_px(), false)
}

fn resolve_cell_pixel_size_with<F>(get_env: F) -> Option<(u16, u16)>
where
    F: Fn(&str) -> Option<String>,
{
    for key in ENV_CELL_PX_KEYS {
        if let Some(raw) = non_empty_env(&get_env, key) {
            if let Some(parsed) = parse_cell_pixel_size(&raw) {
                return Some(parsed);
            }
        }
    }
    None
}

fn parse_cell_pixel_size(raw: &str) -> Option<(u16, u16)> {
    let (w_str, h_str) = raw.trim().split_once(['x', 'X'])?;
    let w = w_str.trim().parse::<u16>().ok()?;
    let h = h_str.trim().parse::<u16>().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w.clamp(4, 64), h.clamp(8, 128)))
}

/// Strips optional UTF-8 BOM and leading ASCII whitespace, returning up to `limit` ASCII-lowercased bytes.
fn normalized_markup_prefix(bytes: &[u8], limit: usize) -> String {
    let without_bom = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let trimmed = without_bom
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map(|idx| &without_bom[idx..])
        .unwrap_or(&[]);
    let sample = &trimmed[..trimmed.len().min(limit)];
    sample
        .iter()
        .map(|&b| {
            if b.is_ascii() {
                (b as char).to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect()
}

/// Returns `true` if `bytes` looks like an HTML document (e.g. auth redirect or error page) rather than a raster image.
pub fn looks_like_html_bytes(bytes: &[u8]) -> bool {
    let prefix = normalized_markup_prefix(bytes, 512);
    if prefix.is_empty() {
        return false;
    }
    const HTML_STARTS: &[&str] = &[
        "<!doctype html",
        "<html",
        "<head",
        "<body",
        "<title",
        "<meta",
        "<script",
        "<style",
        "<div",
        "<table",
        "<form",
        "<iframe",
        "<h1",
        "<h2",
        "<h3",
        "<p>",
        "<p ",
    ];
    if HTML_STARTS.iter().any(|p| prefix.starts_with(p)) {
        return true;
    }
    if prefix.starts_with("<!--") || prefix.starts_with("<?xml") {
        return prefix.contains("<!doctype html") || prefix.contains("<html");
    }
    false
}

/// Returns `true` if `bytes` looks like an SVG vector image (`image/svg+xml`).
pub fn looks_like_svg_bytes(bytes: &[u8]) -> bool {
    let prefix = normalized_markup_prefix(bytes, 1024);
    if prefix.is_empty() {
        return false;
    }
    if let Some(rest) = prefix.strip_prefix("<svg") {
        return rest.is_empty()
            || rest.starts_with(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/');
    }
    if prefix.starts_with("<!doctype svg") {
        return true;
    }
    if (prefix.starts_with("<?xml") || prefix.starts_with("<!--"))
        && !prefix.contains("<!doctype html")
        && !prefix.contains("<html")
    {
        if let Some(pos) = prefix.find("<svg") {
            let after = &prefix[pos + 4..];
            return after.is_empty()
                || after.starts_with(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/');
        }
    }
    false
}

fn validate_raster_image_bytes(bytes: &[u8]) -> Result<()> {
    if looks_like_html_bytes(bytes) {
        bail!(
            "Received HTML document instead of raster image bytes (possible auth redirect or error page)"
        );
    }
    Ok(())
}

fn maybe_rasterize_svg(
    bytes: &[u8],
    max_px_w: u32,
    max_px_h: u32,
) -> Result<(std::borrow::Cow<'_, [u8]>, bool)> {
    if looks_like_svg_bytes(bytes) {
        let cap_w = max_px_w.clamp(1, bitefoundry_html::graphics::MAX_RASTER_DIM_PX);
        let cap_h = max_px_h.clamp(1, bitefoundry_html::graphics::MAX_RASTER_DIM_PX);
        let raster = bitefoundry_html::graphics::rasterize_svg_to_png(bytes, cap_w, cap_h, None)
            .context("Failed to rasterize SVG image")?;
        Ok((std::borrow::Cow::Owned(raster.png_bytes.to_vec()), true))
    } else {
        Ok((std::borrow::Cow::Borrowed(bytes), false))
    }
}

/// Returns true if a MIME type or filename looks like a supported image (`png`, `jpeg`, `jpg`, `gif`, `webp`, `svg`).
pub fn is_image_mime_or_name(content_type: &str, file_name: Option<&str>) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if matches!(ct.as_str(), "text/html" | "application/xhtml+xml") {
        return false;
    }
    if matches!(
        ct.as_str(),
        "image/png"
            | "image/jpeg"
            | "image/jpg"
            | "image/gif"
            | "image/webp"
            | "image/svg+xml"
            | "hostedcontent"
    ) {
        return true;
    }
    if let Some(name) = file_name {
        let lower = name.trim().to_ascii_lowercase();
        if lower.ends_with(".html") || lower.ends_with(".htm") {
            return false;
        }
        if lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".gif")
            || lower.ends_with(".webp")
            || lower.ends_with(".svg")
        {
            return true;
        }
    }
    false
}

/// Decodes an image buffer and renders aspect-ratio-preserving Truecolor Unicode Half-Blocks (`▀`)
/// for `ratatui` using a default `1:2` (`8x16`) cell assumption.
pub fn decode_and_render_halfblocks(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    bg_rgb: (u8, u8, u8),
) -> Result<RenderedTerminalImage> {
    decode_and_render_halfblocks_with_cell_px(
        bytes,
        FitBox::contained(max_cols, max_rows),
        bg_rgb,
        (8, 16),
    )
}

/// Decodes an image buffer and renders Truecolor Unicode Half-Blocks (`▀`) using the terminal's
/// actual `cell_px` geometry and explicit `FitBox` sizing contract.
pub fn decode_and_render_halfblocks_with_cell_px(
    bytes: &[u8],
    fit: FitBox,
    bg_rgb: (u8, u8, u8),
    cell_px: (u16, u16),
) -> Result<RenderedTerminalImage> {
    validate_raster_image_bytes(bytes)?;
    let zoom_scale = (fit.zoom_pct.clamp(25, 300) as u32).max(100);
    let max_px_w = ((fit.view_cols.max(1) as u32)
        .saturating_mul(cell_px.0.max(1) as u32)
        .saturating_mul(zoom_scale)
        / 100)
        .clamp(64, bitefoundry_html::graphics::MAX_RASTER_DIM_PX);
    let max_px_h = ((fit.view_rows.max(1) as u32)
        .saturating_mul(cell_px.1.max(1) as u32)
        .saturating_mul(zoom_scale)
        / 100)
        .clamp(64, bitefoundry_html::graphics::MAX_RASTER_DIM_PX);
    let (raster_bytes, is_svg) = maybe_rasterize_svg(bytes, max_px_w, max_px_h)?;
    let guessed = image::guess_format(&raster_bytes).ok();
    let format_label = if is_svg {
        "SVG".to_string()
    } else {
        format_label_for(guessed)
    };
    let dyn_img = image::load_from_memory(&raster_bytes).context("Failed to decode image")?;
    let rgba = dyn_img.into_rgba8();
    let (width_px, height_px) = rgba.dimensions();
    if width_px == 0 || height_px == 0 {
        bail!("Image dimensions are 0x0");
    }

    let (target_w, target_h) = compute_halfblock_dims(width_px, height_px, fit, cell_px);

    let resized = image::imageops::resize(&rgba, target_w, target_h, FilterType::Lanczos3);
    let mut lines = Vec::with_capacity((target_h / 2) as usize);

    for y in (0..target_h).step_by(2) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut current_pair: Option<((u8, u8, u8), (u8, u8, u8))> = None;
        let mut run = String::new();

        for x in 0..target_w {
            let top = blend_over_bg(*resized.get_pixel(x, y), bg_rgb);
            let bot = blend_over_bg(*resized.get_pixel(x, y + 1), bg_rgb);
            let pair = (top, bot);

            if current_pair == Some(pair) {
                run.push('▀');
            } else {
                if let Some((prev_top, prev_bot)) = current_pair {
                    spans.push(Span::styled(
                        std::mem::take(&mut run),
                        Style::default()
                            .fg(Color::Rgb(prev_top.0, prev_top.1, prev_top.2))
                            .bg(Color::Rgb(prev_bot.0, prev_bot.1, prev_bot.2)),
                    ));
                }
                current_pair = Some(pair);
                run.push('▀');
            }
        }

        if let Some((prev_top, prev_bot)) = current_pair {
            spans.push(Span::styled(
                run,
                Style::default()
                    .fg(Color::Rgb(prev_top.0, prev_top.1, prev_top.2))
                    .bg(Color::Rgb(prev_bot.0, prev_bot.1, prev_bot.2)),
            ));
        }

        lines.push(Line::from(spans));
    }

    Ok(RenderedTerminalImage {
        width_px,
        height_px,
        cols: target_w as u16,
        rows: (target_h / 2) as u16,
        format_label,
        protocol: TerminalImageProtocol::Halfblock,
        lines,
        pixel_payload: None,
        scroll_backing: None,
    })
}

/// Decodes and renders an image using the requested terminal graphics protocol (`Kitty`, `ITerm2`, `Sixel`, or `Halfblock`).
pub fn decode_and_render_for_protocol(
    bytes: &[u8],
    fit: FitBox,
    bg_rgb: (u8, u8, u8),
    caps: TerminalGraphicsCaps,
) -> Result<RenderedTerminalImage> {
    if caps.protocol == TerminalImageProtocol::Halfblock {
        return decode_and_render_halfblocks_with_cell_px(bytes, fit, bg_rgb, caps.cell_px);
    }

    validate_raster_image_bytes(bytes)?;
    let (max_px_w, max_px_h) = if fit.scrollable {
        (
            bitefoundry_html::graphics::MAX_RASTER_DIM_PX,
            bitefoundry_html::graphics::MAX_RASTER_DIM_PX,
        )
    } else {
        (
            (fit.view_cols.max(1) as u32)
                .saturating_mul(caps.cell_px.0.max(1) as u32)
                .clamp(64, bitefoundry_html::graphics::MAX_RASTER_DIM_PX),
            (fit.view_rows.max(1) as u32)
                .saturating_mul(caps.cell_px.1.max(1) as u32)
                .clamp(64, bitefoundry_html::graphics::MAX_RASTER_DIM_PX),
        )
    };
    let (raster_bytes, is_svg) = maybe_rasterize_svg(bytes, max_px_w, max_px_h)?;
    let guessed = image::guess_format(&raster_bytes).ok();
    let format_label = if is_svg {
        "SVG".to_string()
    } else {
        format_label_for(guessed)
    };
    let dyn_img = image::load_from_memory(&raster_bytes).context("Failed to decode image")?;
    let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
    if width_px == 0 || height_px == 0 {
        bail!("Image dimensions are 0x0");
    }

    let (target_px_w, target_px_h, occ_cols, occ_rows) =
        compute_pixel_fit_for_protocol(width_px, height_px, fit, caps.cell_px, caps.protocol);

    let (payload, scroll_backing) = match caps.protocol {
        TerminalImageProtocol::Kitty => {
            let bounded_img = if width_px <= MAX_RENDER_PX_W && height_px <= MAX_RENDER_PX_H {
                dyn_img
            } else {
                fit_within_max_bounds(&dyn_img, MAX_RENDER_PX_W, MAX_RENDER_PX_H)
            };
            let png_owned;
            let png_bytes: &[u8] = if matches!(guessed, Some(ImageFormat::Png))
                && width_px <= MAX_RENDER_PX_W
                && height_px <= MAX_RENDER_PX_H
            {
                &raster_bytes
            } else {
                png_owned = encode_png(&bounded_img)?;
                &png_owned
            };
            let fit_axis = choose_kitty_fit_axis(
                bounded_img.width(),
                bounded_img.height(),
                occ_cols,
                occ_rows,
                caps.cell_px,
            );
            let backing = PixelScrollBacking::Source {
                image: Arc::new(bounded_img),
                target_px_w,
                target_px_h,
                cell_px: caps.cell_px,
            };
            (
                encode_kitty_tui_payload(png_bytes, fit_axis),
                Some(backing),
            )
        }
        TerminalImageProtocol::ITerm2 => {
            let bounded_img = if width_px <= MAX_RENDER_PX_W && height_px <= MAX_RENDER_PX_H {
                dyn_img
            } else {
                fit_within_max_bounds(&dyn_img, MAX_RENDER_PX_W, MAX_RENDER_PX_H)
            };
            let encoded_owned;
            let raw_bytes: &[u8] = if matches!(
                guessed,
                Some(
                    ImageFormat::Png
                        | ImageFormat::Jpeg
                        | ImageFormat::Gif
                        | ImageFormat::WebP
                )
            ) && width_px <= MAX_RENDER_PX_W
                && height_px <= MAX_RENDER_PX_H
                && raster_bytes.len() <= MAX_ITERM2_RAW_BYTES
            {
                &raster_bytes
            } else {
                encoded_owned = encode_png(&bounded_img)?;
                &encoded_owned
            };
            let b64 = B64_STANDARD.encode(raw_bytes);
            let payload = format!(
                "\x1b]1337;File=inline=1;width={occ_cols};height={occ_rows};preserveAspectRatio=1;doNotMoveCursor=1:{b64}\x07"
            );
            let backing = PixelScrollBacking::Source {
                image: Arc::new(bounded_img),
                target_px_w,
                target_px_h,
                cell_px: caps.cell_px,
            };
            (payload, Some(backing))
        }
        TerminalImageProtocol::Sixel => {
            let rgba = dyn_img.to_rgba8();
            let resized = if width_px == target_px_w && height_px == target_px_h {
                rgba
            } else {
                image::imageops::resize(&rgba, target_px_w, target_px_h, FilterType::Lanczos3)
            };
            let mut flat_rgba = Vec::with_capacity((target_px_w * target_px_h * 4) as usize);
            for px in resized.pixels() {
                let (r, g, b) = blend_over_bg(*px, bg_rgb);
                flat_rgba.extend_from_slice(&[r, g, b, 255]);
            }
            let (palette, indexed) =
                quantize_rgba_to_256(&flat_rgba, target_px_w as usize, target_px_h as usize);
            let payload = encode_sixel_indexed_slice(
                &palette,
                &indexed,
                target_px_w as usize,
                0,
                0,
                target_px_w as usize,
                target_px_h as usize,
            );
            let backing = PixelScrollBacking::SixelIndexed {
                width: target_px_w,
                height: target_px_h,
                cell_px: caps.cell_px,
                palette,
                indexed: Arc::new(indexed),
            };
            (payload, Some(backing))
        }
        TerminalImageProtocol::Halfblock => unreachable!("handled above"),
    };

    Ok(RenderedTerminalImage {
        width_px,
        height_px,
        cols: occ_cols,
        rows: occ_rows,
        format_label,
        protocol: caps.protocol,
        lines: Vec::new(),
        pixel_payload: Some(payload),
        scroll_backing,
    })
}

fn fit_within_max_bounds(img: &DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w <= max_w && h <= max_h {
        return img.clone();
    }
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    let nw = ((w as f64 * scale).round() as u32).clamp(1, max_w);
    let nh = ((h as f64 * scale).round() as u32).clamp(1, max_h);
    img.resize_exact(nw, nh, FilterType::Lanczos3)
}

fn choose_kitty_fit_axis(
    src_w: u32,
    src_h: u32,
    occ_cols: u16,
    occ_rows: u16,
    cell_px: (u16, u16),
) -> KittyFitAxis {
    let cell_w = cell_px.0.max(1) as u64;
    let cell_h = cell_px.1.max(1) as u64;
    let lhs = (src_w as u64).saturating_mul((occ_rows.max(1) as u64).saturating_mul(cell_h));
    let rhs = (src_h as u64).saturating_mul((occ_cols.max(1) as u64).saturating_mul(cell_w));
    if lhs >= rhs {
        KittyFitAxis::Cols(occ_cols.max(1))
    } else {
        KittyFitAxis::Rows(occ_rows.max(1))
    }
}

fn encode_kitty_tui_payload(png_bytes: &[u8], fit_axis: KittyFitAxis) -> String {
    let b64 = B64_STANDARD.encode(png_bytes);
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).expect("base64 is valid ASCII"))
        .collect();
    let total = chunks.len();
    let mut out = String::with_capacity(b64.len() + total * 48);
    let axis_param = match fit_axis {
        KittyFitAxis::Cols(c) => format!("c={c}"),
        KittyFitAxis::Rows(r) => format!("r={r}"),
    };
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let more = if idx + 1 < total { 1 } else { 0 };
        if idx == 0 {
            let _ = write!(
                out,
                "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},{axis_param},C=1,m={more};{chunk}\x1b\\"
            );
        } else {
            let _ = write!(out, "\x1b_Gq=2,m={more};{chunk}\x1b\\");
        }
    }
    out
}

/// Computes target half-block subpixel dimensions `(target_w, target_h)` (`target_h` always even)
/// honoring actual terminal `cell_px` geometry and `FitBox` zoom/scroll rules.
pub fn compute_halfblock_dims(
    width_px: u32,
    height_px: u32,
    fit: FitBox,
    cell_px: (u16, u16),
) -> (u32, u32) {
    let cell_w = cell_px.0.max(1) as f64;
    let cell_h = cell_px.1.max(1) as f64;
    let view_cols = fit.view_cols.max(1) as u32;
    let view_rows = fit.view_rows.max(1) as u32;
    let zoom_pct = fit.zoom_pct.clamp(25, 300);

    // Each half-block subpixel spans `cell_w` x `(cell_h / 2)` physical pixels.
    let subpx_aspect = cell_h / (2.0 * cell_w);
    let eff_src_h = (height_px as f64) / subpx_aspect;
    let vp_sub_w = view_cols as f64;
    let vp_sub_h = (view_rows as f64) * 2.0;

    let is_tall_image = (height_px as u64) * 4 > (width_px as u64) * 5;
    let is_tall_scroll = fit.scrollable
        && is_tall_image
        && eff_src_h * vp_sub_w > (width_px as f64) * vp_sub_h * 1.5;

    let base_scale = if is_tall_scroll {
        (vp_sub_w / width_px as f64).min(1.0)
    } else {
        (vp_sub_w / width_px as f64)
            .min(vp_sub_h / eff_src_h)
            .min(1.0)
    };
    let scale = base_scale * (zoom_pct as f64 / 100.0);

    let max_cols = if fit.scrollable { 512 } else { view_cols };
    let max_rows = if fit.scrollable { 1024 } else { view_rows };

    let target_w = ((width_px as f64 * scale).round() as u32).clamp(1, max_cols);
    let target_rows = (((eff_src_h * scale) / 2.0).round() as u32).clamp(1, max_rows);
    (target_w, target_rows * 2)
}

/// Computes `(target_px_w, target_px_h, occ_cols, occ_rows)` for pixel protocols using explicit `FitBox`.
pub fn compute_pixel_fit_for_protocol(
    width_px: u32,
    height_px: u32,
    fit: FitBox,
    cell_px: (u16, u16),
    protocol: TerminalImageProtocol,
) -> (u32, u32, u16, u16) {
    let cell_w = cell_px.0.max(1) as u32;
    let cell_h = cell_px.1.max(1) as u32;
    let view_cols = fit.view_cols.max(1);
    let view_rows = fit.view_rows.max(1);
    let zoom_pct = fit.zoom_pct.clamp(25, 300);

    let vp_w = (view_cols as u32).saturating_mul(cell_w).max(1);
    let raw_vp_h = (view_rows as u32).saturating_mul(cell_h).max(1);
    let sixel_safe_vp_h = compute_sixel_safe_pixel_height(view_rows, cell_px.1.max(1));
    let vp_h = if protocol == TerminalImageProtocol::Sixel && !fit.scrollable && sixel_safe_vp_h >= 6 {
        sixel_safe_vp_h
    } else {
        raw_vp_h
    };

    // Only tall portrait screenshots whose fit-to-width height exceeds 1.5x viewport height scroll vertically at 100%.
    // Standard aspect ratios (including 16:9 on wide/low terminals) always fit wholly inside the viewport at 100%.
    let is_tall_image = (height_px as u64) * 4 > (width_px as u64) * 5;
    let is_tall_scroll = fit.scrollable
        && is_tall_image
        && (height_px as u64).saturating_mul(vp_w as u64)
            > (width_px as u64)
                .saturating_mul(vp_h as u64)
                .saturating_mul(3)
                / 2;

    let base_scale = if is_tall_scroll {
        (vp_w as f64 / width_px as f64).min(1.0)
    } else {
        (vp_w as f64 / width_px as f64)
            .min(vp_h as f64 / height_px as f64)
            .min(1.0)
    };

    let mut scale = base_scale * (zoom_pct as f64 / 100.0);

    let cap_scale = (MAX_RENDER_PX_W as f64 / width_px as f64)
        .min(MAX_RENDER_PX_H as f64 / height_px as f64);
    if scale > cap_scale {
        scale = cap_scale;
    }

    if protocol == TerminalImageProtocol::Sixel {
        let est_w = (width_px as f64 * scale).max(1.0);
        let est_h = (height_px as f64 * scale).max(1.0);
        let est_px = est_w * est_h;
        if est_px > MAX_SIXEL_PIXELS as f64 {
            scale *= (MAX_SIXEL_PIXELS as f64 / est_px).sqrt();
        }
    }

    let max_w = if fit.scrollable {
        MAX_RENDER_PX_W
    } else {
        vp_w.min(MAX_RENDER_PX_W)
    };
    let max_h = if fit.scrollable {
        MAX_RENDER_PX_H
    } else {
        vp_h.min(MAX_RENDER_PX_H)
    };

    let mut target_px_w = ((width_px as f64 * scale).round() as u32).clamp(1, max_w);
    let mut target_px_h = ((height_px as f64 * scale).round() as u32).clamp(1, max_h);

    if protocol == TerminalImageProtocol::Sixel {
        if !fit.scrollable && sixel_safe_vp_h >= 6 && target_px_h > sixel_safe_vp_h {
            target_px_h = sixel_safe_vp_h;
        }
        if fit.scrollable && !is_tall_scroll && zoom_pct <= 100 && sixel_safe_vp_h >= 6 && target_px_h > sixel_safe_vp_h {
            let ratio = sixel_safe_vp_h as f64 / target_px_h as f64;
            target_px_h = sixel_safe_vp_h;
            target_px_w = ((target_px_w as f64 * ratio).round() as u32).clamp(1, max_w);
        }
        if (target_px_w as u64) * (target_px_h as u64) > MAX_SIXEL_PIXELS {
            let ratio =
                (MAX_SIXEL_PIXELS as f64 / ((target_px_w as f64) * (target_px_h as f64))).sqrt();
            target_px_w = ((target_px_w as f64 * ratio).floor() as u32).max(1);
            target_px_h = ((target_px_h as f64 * ratio).floor() as u32).max(1);
        }
    }

    let occ_cols = target_px_w.div_ceil(cell_w).clamp(1, u16::MAX as u32) as u16;
    let occ_rows = if protocol == TerminalImageProtocol::Sixel {
        let band_px_h = target_px_h.div_ceil(6) * 6;
        band_px_h.div_ceil(cell_h).clamp(1, u16::MAX as u32) as u16
    } else {
        target_px_h.div_ceil(cell_h).clamp(1, u16::MAX as u32) as u16
    };

    (target_px_w, target_px_h, occ_cols, occ_rows)
}

/// Encodes opaque RGBA pixel data (`4 * width * height` bytes) into a standard DEC VT340 / Windows Terminal Sixel stream.
pub fn encode_sixel_rgba(flat_rgba: &[u8], width: u32, height: u32) -> String {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || flat_rgba.len() < w * h * 4 {
        return String::from("\x1bP0;1;q\"1;1;0;0\x1b\\");
    }

    let (palette, indexed) = quantize_rgba_to_256(flat_rgba, w, h);
    encode_sixel_indexed_slice(&palette, &indexed, w, 0, 0, w, h)
}

fn encode_sixel_indexed_slice(
    palette: &[(u8, u8, u8)],
    indexed: &[u8],
    full_w: usize,
    x0: usize,
    y0: usize,
    crop_w: usize,
    crop_h: usize,
) -> String {
    if crop_w == 0 || crop_h == 0 || full_w == 0 || palette.is_empty() {
        return String::from("\x1bP0;1;q\"1;1;0;0\x1b\\");
    }

    let mut out = String::with_capacity(crop_w * crop_h / 2 + palette.len() * 16 + 32);
    let _ = write!(out, "\x1bP0;1;q\"1;1;{crop_w};{crop_h}");

    for (idx, &(r, g, b)) in palette.iter().enumerate() {
        let r_pct = (r as u32 * 100 + 127) / 255;
        let g_pct = (g as u32 * 100 + 127) / 255;
        let b_pct = (b as u32 * 100 + 127) / 255;
        let _ = write!(out, "#{idx};2;{r_pct};{g_pct};{b_pct}");
    }

    let mut color_seen = [false; 256];
    let mut band_colors: Vec<u8> = Vec::with_capacity(palette.len().min(64));
    let mut band_masks = vec![0u8; palette.len().max(1) * crop_w];

    for band_y in (0..crop_h).step_by(6) {
        band_colors.clear();
        let band_h = (crop_h - band_y).min(6);

        for dy in 0..band_h {
            let bit = 1u8 << dy;
            let row_offset = (y0 + band_y + dy) * full_w + x0;
            for x in 0..crop_w {
                let c = indexed[row_offset + x];
                if !color_seen[c as usize] {
                    color_seen[c as usize] = true;
                    band_colors.push(c);
                }
                band_masks[c as usize * crop_w + x] |= bit;
            }
        }

        for (ci, &color_idx) in band_colors.iter().enumerate() {
            if ci > 0 {
                out.push('$');
            }
            let _ = write!(out, "#{color_idx}");

            let mask_start = color_idx as usize * crop_w;
            let row_slice = &band_masks[mask_start..mask_start + crop_w];
            let end = row_slice
                .iter()
                .rposition(|&bits| bits != 0)
                .map_or(0, |idx| idx + 1);

            write_sixel_rle(&row_slice[..end], &mut out);
        }

        for &c in &band_colors {
            color_seen[c as usize] = false;
            let start = c as usize * crop_w;
            band_masks[start..start + crop_w].fill(0);
        }

        if band_y + 6 < crop_h {
            out.push('-');
        }
    }

    out.push_str("\x1b\\");
    out
}

fn write_sixel_rle(masks: &[u8], out: &mut String) {
    let mut i = 0;
    while i < masks.len() {
        let bits = masks[i];
        let mut run_len = 1usize;
        while i + run_len < masks.len() && masks[i + run_len] == bits {
            run_len += 1;
        }
        let ch = (bits + 63) as char;
        if run_len >= 4 {
            let _ = write!(out, "!{run_len}{ch}");
        } else {
            for _ in 0..run_len {
                out.push(ch);
            }
        }
        i += run_len;
    }
}

/// Integer-only Redmean perceptually weighted squared color distance.
#[inline]
pub fn redmean_dist_sq(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let r_mean = (r1 as i32 + r2 as i32) / 2;
    let dr = r1 as i32 - r2 as i32;
    let dg = g1 as i32 - g2 as i32;
    let db = b1 as i32 - b2 as i32;
    let wr = ((512 + r_mean) * dr * dr) >> 8;
    let wg = 4 * dg * dg;
    let wb = ((767 - r_mean) * db * db) >> 8;
    (wr + wg + wb) as u32
}

#[derive(Clone, Copy)]
struct ColorBucket {
    r_avg: u8,
    g_avg: u8,
    b_avg: u8,
    count: u32,
    sum_r: u64,
    sum_g: u64,
    sum_b: u64,
}

fn quantize_rgba_to_256(
    flat_rgba: &[u8],
    width: usize,
    height: usize,
) -> (Vec<(u8, u8, u8)>, Vec<u8>) {
    let (palette, indexed, _cache_len) = quantize_rgba_to_256_with_cache_len(flat_rgba, width, height);
    (palette, indexed)
}

fn quantize_rgba_to_256_with_cache_len(
    flat_rgba: &[u8],
    width: usize,
    height: usize,
) -> (Vec<(u8, u8, u8)>, Vec<u8>, usize) {
    let pixel_count = width.saturating_mul(height);
    // Fast lossless path when the image uses <= 256 distinct RGB colors.
    let mut exact_map: HashMap<u32, u8> = HashMap::with_capacity(256);
    let mut exact_palette: Vec<(u8, u8, u8)> = Vec::with_capacity(256);
    let mut exact_indexed = Vec::with_capacity(pixel_count);
    let mut overflow = false;

    for px in flat_rgba.chunks_exact(4).take(pixel_count) {
        let key = ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32);
        if let Some(&idx) = exact_map.get(&key) {
            exact_indexed.push(idx);
        } else if exact_palette.len() < 256 {
            let idx = exact_palette.len() as u8;
            exact_map.insert(key, idx);
            exact_palette.push((px[0], px[1], px[2]));
            exact_indexed.push(idx);
        } else {
            overflow = true;
            break;
        }
    }

    if !overflow {
        return (exact_palette, exact_indexed, 0);
    }

    // Perceptually weighted median-cut over 5-bit-per-channel RGB buckets for > 256 distinct colors.
    let mut hist: HashMap<u16, (u32, u64, u64, u64)> = HashMap::new();
    for px in flat_rgba.chunks_exact(4).take(pixel_count) {
        let qkey = (((px[0] as u16) >> 3) << 10)
            | (((px[1] as u16) >> 3) << 5)
            | ((px[2] as u16) >> 3);
        let entry = hist.entry(qkey).or_insert((0, 0, 0, 0));
        entry.0 += 1;
        entry.1 += px[0] as u64;
        entry.2 += px[1] as u64;
        entry.3 += px[2] as u64;
    }

    let mut buckets: Vec<ColorBucket> = hist
        .into_values()
        .map(|(count, sum_r, sum_g, sum_b)| {
            let c = count as u64;
            ColorBucket {
                r_avg: (sum_r / c) as u8,
                g_avg: (sum_g / c) as u8,
                b_avg: (sum_b / c) as u8,
                count,
                sum_r,
                sum_g,
                sum_b,
            }
        })
        .collect();

    let total_buckets = buckets.len();
    let mut boxes: Vec<(usize, usize)> = vec![(0, total_buckets)];

    while boxes.len() < 256 {
        let mut best_box_idx = None;
        let mut best_score = 0u64;
        let mut best_channel = 0u8;

        for (bi, &(start, end)) in boxes.iter().enumerate() {
            if end - start <= 1 {
                continue;
            }
            let (mut min_r, mut max_r) = (u8::MAX, u8::MIN);
            let (mut min_g, mut max_g) = (u8::MAX, u8::MIN);
            let (mut min_b, mut max_b) = (u8::MAX, u8::MIN);
            let mut pop = 0u64;

            for b in &buckets[start..end] {
                min_r = min_r.min(b.r_avg);
                max_r = max_r.max(b.r_avg);
                min_g = min_g.min(b.g_avg);
                max_g = max_g.max(b.g_avg);
                min_b = min_b.min(b.b_avg);
                max_b = max_b.max(b.b_avg);
                pop += b.count as u64;
            }

            let w_r = (max_r - min_r) as u64 * 2;
            let w_g = (max_g - min_g) as u64 * 4;
            let w_b = (max_b - min_b) as u64 * 3;
            let (channel, weighted_span) = if w_g >= w_r && w_g >= w_b {
                (1u8, w_g)
            } else if w_b >= w_r {
                (2u8, w_b)
            } else {
                (0u8, w_r)
            };

            let sqrt_pop = (pop as f64).sqrt().ceil() as u64;
            let score = weighted_span.saturating_mul(sqrt_pop.max(1));
            if score > best_score {
                best_score = score;
                best_box_idx = Some(bi);
                best_channel = channel;
            }
        }

        let Some(bi) = best_box_idx else {
            break;
        };
        let (start, end) = boxes[bi];
        let slice = &mut buckets[start..end];
        match best_channel {
            0 => slice.sort_unstable_by_key(|b| b.r_avg),
            1 => slice.sort_unstable_by_key(|b| b.g_avg),
            _ => slice.sort_unstable_by_key(|b| b.b_avg),
        }

        let total_pop: u64 = slice.iter().map(|b| b.count as u64).sum();
        let half_pop = total_pop / 2;
        let mut accum = 0u64;
        let mut split_rel = None;
        for (idx, b) in slice.iter().enumerate() {
            accum += b.count as u64;
            if accum >= half_pop {
                if idx + 1 < slice.len() {
                    split_rel = Some((idx + 1).max(1));
                } else {
                    split_rel = Some(idx.max(1));
                }
                break;
            }
        }
        let mid = start + split_rel.unwrap_or(1);
        boxes[bi] = (start, mid);
        boxes.push((mid, end));
    }

    let raw_palette: Vec<(u8, u8, u8)> = boxes
        .iter()
        .map(|&(start, end)| {
            let mut cnt = 0u64;
            let mut sr = 0u64;
            let mut sg = 0u64;
            let mut sb = 0u64;
            for b in &buckets[start..end] {
                cnt += b.count as u64;
                sr += b.sum_r;
                sg += b.sum_g;
                sb += b.sum_b;
            }
            if cnt == 0 {
                (0, 0, 0)
            } else {
                ((sr / cnt) as u8, (sg / cnt) as u8, (sb / cnt) as u8)
            }
        })
        .collect();

    let mut indexed = vec![0u8; pixel_count];
    let mut lookup_cache: HashMap<u32, u8> = HashMap::with_capacity(2048);
    let mut used = vec![false; raw_palette.len()];

    // Serpentine Floyd-Steinberg error diffusion with [-48, 48] error clamp and flat-region guard.
    let mut err_curr = vec![[0.0f32; 3]; width];
    let mut err_next = vec![[0.0f32; 3]; width];

    for y in 0..height {
        let row_base = y * width;
        let left_to_right = (y & 1) == 0;
        for step in 0..width {
            let x = if left_to_right { step } else { width - 1 - step };
            let off = (row_base + x) * 4;
            let orig_r = flat_rgba[off];
            let orig_g = flat_rgba[off + 1];
            let orig_b = flat_rgba[off + 2];

            let r = (orig_r as f32 + err_curr[x][0]).round().clamp(0.0, 255.0) as u8;
            let g = (orig_g as f32 + err_curr[x][1]).round().clamp(0.0, 255.0) as u8;
            let b = (orig_b as f32 + err_curr[x][2]).round().clamp(0.0, 255.0) as u8;

            let key = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            let idx = if let Some(&cached) = lookup_cache.get(&key) {
                cached
            } else {
                let mut best_idx = 0u8;
                let mut best_dist = u32::MAX;
                for (pi, &(pr, pg, pb)) in raw_palette.iter().enumerate() {
                    let dist = redmean_dist_sq(r, g, b, pr, pg, pb);
                    if dist < best_dist {
                        best_dist = dist;
                        best_idx = pi as u8;
                        if dist == 0 {
                            break;
                        }
                    }
                }
                if lookup_cache.len() < MAX_QUANT_LOOKUP_CACHE {
                    lookup_cache.insert(key, best_idx);
                }
                best_idx
            };

            used[idx as usize] = true;
            indexed[row_base + x] = idx;

            let (pr, pg, pb) = raw_palette[idx as usize];
            let er = (r as f32 - pr as f32).clamp(-48.0, 48.0);
            let eg = (g as f32 - pg as f32).clamp(-48.0, 48.0);
            let eb = (b as f32 - pb as f32).clamp(-48.0, 48.0);

            let tiny_err = er.abs() <= 3.0 && eg.abs() <= 3.0 && eb.abs() <= 3.0;
            let same_left = x > 0
                && orig_r == flat_rgba[off - 4]
                && orig_g == flat_rgba[off - 3]
                && orig_b == flat_rgba[off - 2];
            let same_up = y > 0 && {
                let up_off = ((y - 1) * width + x) * 4;
                orig_r == flat_rgba[up_off]
                    && orig_g == flat_rgba[up_off + 1]
                    && orig_b == flat_rgba[up_off + 2]
            };
            let orig_dist = redmean_dist_sq(orig_r, orig_g, orig_b, pr, pg, pb);
            if tiny_err || ((same_left || same_up) && orig_dist <= 64) {
                continue;
            }

            const W7: f32 = 7.0 / 16.0;
            const W3: f32 = 3.0 / 16.0;
            const W5: f32 = 5.0 / 16.0;
            const W1: f32 = 1.0 / 16.0;

            if left_to_right {
                if x + 1 < width {
                    err_curr[x + 1][0] += er * W7;
                    err_curr[x + 1][1] += eg * W7;
                    err_curr[x + 1][2] += eb * W7;
                }
                if y + 1 < height {
                    if x > 0 {
                        err_next[x - 1][0] += er * W3;
                        err_next[x - 1][1] += eg * W3;
                        err_next[x - 1][2] += eb * W3;
                    }
                    err_next[x][0] += er * W5;
                    err_next[x][1] += eg * W5;
                    err_next[x][2] += eb * W5;
                    if x + 1 < width {
                        err_next[x + 1][0] += er * W1;
                        err_next[x + 1][1] += eg * W1;
                        err_next[x + 1][2] += eb * W1;
                    }
                }
            } else {
                if x > 0 {
                    err_curr[x - 1][0] += er * W7;
                    err_curr[x - 1][1] += eg * W7;
                    err_curr[x - 1][2] += eb * W7;
                }
                if y + 1 < height {
                    if x + 1 < width {
                        err_next[x + 1][0] += er * W3;
                        err_next[x + 1][1] += eg * W3;
                        err_next[x + 1][2] += eb * W3;
                    }
                    err_next[x][0] += er * W5;
                    err_next[x][1] += eg * W5;
                    err_next[x][2] += eb * W5;
                    if x > 0 {
                        err_next[x - 1][0] += er * W1;
                        err_next[x - 1][1] += eg * W1;
                        err_next[x - 1][2] += eb * W1;
                    }
                }
            }
        }
        std::mem::swap(&mut err_curr, &mut err_next);
        err_next.fill([0.0, 0.0, 0.0]);
    }

    let cache_len = lookup_cache.len();

    // Compact palette so every emitted `#idx;2;...` entry is actually referenced.
    let mut remap = vec![0u8; raw_palette.len()];
    let mut compact_palette = Vec::with_capacity(raw_palette.len());
    for (old_idx, &color) in raw_palette.iter().enumerate() {
        if used[old_idx] {
            remap[old_idx] = compact_palette.len() as u8;
            compact_palette.push(color);
        }
    }
    for idx in &mut indexed {
        *idx = remap[*idx as usize];
    }

    (compact_palette, indexed, cache_len)
}

/// Renders an image for direct CLI terminal stdout (auto-detecting Kitty / iTerm2 / Sixel / 24-bit ANSI Half-Blocks).
pub fn render_ansi_stdout(bytes: &[u8], max_cols: u16, max_rows: u16) -> Result<String> {
    render_ansi_stdout_with_pref(bytes, max_cols, max_rows, ImageProtocolPreference::Auto)
}

/// Renders an image for direct CLI terminal stdout honoring an explicit or auto-detected protocol preference.
pub fn render_ansi_stdout_with_pref(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    pref: ImageProtocolPreference,
) -> Result<String> {
    let caps = detect_graphics_caps(pref);
    render_ansi_stdout_with_caps(bytes, max_cols, max_rows, caps)
}

fn render_ansi_stdout_with_caps(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    caps: TerminalGraphicsCaps,
) -> Result<String> {
    validate_raster_image_bytes(bytes)?;
    let max_px_w = (max_cols.max(1) as u32)
        .saturating_mul(caps.cell_px.0.max(1) as u32)
        .clamp(64, bitefoundry_html::graphics::MAX_RASTER_DIM_PX);
    let max_px_h = (max_rows.max(1) as u32)
        .saturating_mul(caps.cell_px.1.max(1) as u32)
        .clamp(64, bitefoundry_html::graphics::MAX_RASTER_DIM_PX);
    let (raster_bytes, _is_svg) = maybe_rasterize_svg(bytes, max_px_w, max_px_h)?;
    let bytes = raster_bytes.as_ref();
    let fit = FitBox::contained(max_cols, max_rows);
    match caps.protocol {
        TerminalImageProtocol::ITerm2 => {
            let guessed = image::guess_format(bytes).ok();
            let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
            let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
            if width_px == 0 || height_px == 0 {
                bail!("Image dimensions are 0x0");
            }
            let (cols, _) = compute_halfblock_dims(width_px, height_px, fit, caps.cell_px);
            let encoded_owned;
            let raw_bytes: &[u8] = if matches!(
                guessed,
                Some(
                    ImageFormat::Png
                        | ImageFormat::Jpeg
                        | ImageFormat::Gif
                        | ImageFormat::WebP
                )
            ) && width_px <= MAX_RENDER_PX_W
                && height_px <= MAX_RENDER_PX_H
                && bytes.len() <= MAX_ITERM2_RAW_BYTES
            {
                bytes
            } else {
                let bounded = fit_within_max_bounds(&dyn_img, MAX_RENDER_PX_W, MAX_RENDER_PX_H);
                encoded_owned = encode_png(&bounded)?;
                &encoded_owned
            };
            let b64 = B64_STANDARD.encode(raw_bytes);
            Ok(format!(
                "\x1b]1337;File=inline=1;width={cols};preserveAspectRatio=1:{b64}\x07\n"
            ))
        }
        TerminalImageProtocol::Kitty => {
            let guessed = image::guess_format(bytes).ok();
            let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
            let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
            if width_px == 0 || height_px == 0 {
                bail!("Image dimensions are 0x0");
            }
            let png_owned;
            let png_bytes: &[u8] = if matches!(guessed, Some(ImageFormat::Png))
                && width_px <= MAX_RENDER_PX_W
                && height_px <= MAX_RENDER_PX_H
            {
                bytes
            } else {
                let bounded = fit_within_max_bounds(&dyn_img, MAX_RENDER_PX_W, MAX_RENDER_PX_H);
                png_owned = encode_png(&bounded)?;
                &png_owned
            };
            let b64 = B64_STANDARD.encode(png_bytes);
            let chunks: Vec<&str> = b64
                .as_bytes()
                .chunks(4096)
                .map(|c| std::str::from_utf8(c).expect("base64 is valid ASCII"))
                .collect();
            let total = chunks.len();
            let mut out = String::new();
            for (idx, chunk) in chunks.into_iter().enumerate() {
                let more = if idx + 1 < total { 1 } else { 0 };
                if idx == 0 {
                    out.push_str(&format!("\x1b_Gf=100,a=T,m={more};{chunk}\x1b\\"));
                } else {
                    out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
                }
            }
            out.push('\n');
            Ok(out)
        }
        TerminalImageProtocol::Sixel => {
            let rendered = decode_and_render_for_protocol(bytes, fit, (0, 0, 0), caps)?;
            let mut out = rendered.pixel_payload.unwrap_or_default();
            out.push('\n');
            Ok(out)
        }
        TerminalImageProtocol::Halfblock => {
            let rendered =
                decode_and_render_halfblocks_with_cell_px(bytes, fit, (0, 0, 0), caps.cell_px)?;
            let mut out = String::new();
            for line in &rendered.lines {
                for span in &line.spans {
                    let (tr, tg, tb) = match span.style.fg {
                        Some(Color::Rgb(r, g, b)) => (r, g, b),
                        _ => (255, 255, 255),
                    };
                    let (br, bg_c, bb) = match span.style.bg {
                        Some(Color::Rgb(r, g, b)) => (r, g, b),
                        _ => (0, 0, 0),
                    };
                    out.push_str(&format!(
                        "\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg_c};{bb}m{}",
                        span.content
                    ));
                }
                out.push_str("\x1b[0m\n");
            }
            Ok(out)
        }
    }
}

fn format_label_for(fmt: Option<ImageFormat>) -> String {
    match fmt {
        Some(ImageFormat::Png) => "PNG".to_string(),
        Some(ImageFormat::Jpeg) => "JPEG".to_string(),
        Some(ImageFormat::Gif) => "GIF".to_string(),
        Some(ImageFormat::WebP) => "WEBP".to_string(),
        Some(other) => format!("{other:?}").to_ascii_uppercase(),
        None => "IMAGE".to_string(),
    }
}

fn blend_over_bg(px: Rgba<u8>, bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let a = px[3] as u16;
    if a == 255 {
        return (px[0], px[1], px[2]);
    }
    if a == 0 {
        return bg;
    }
    let inv = 255 - a;
    let r = ((px[0] as u16 * a + bg.0 as u16 * inv + 127) / 255) as u8;
    let g = ((px[1] as u16 * a + bg.1 as u16 * inv + 127) / 255) as u8;
    let b = ((px[2] as u16 * a + bg.2 as u16 * inv + 127) / 255) as u8;
    (r, g, b)
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, ImageFormat::Png)
        .context("Failed to encode PNG")?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, RgbaImage};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn make_test_png(w: u32, h: u32) -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_fn(w, h, |x, y| {
            if y < h / 2 {
                Rgba([(x * 16) as u8, 120, 40, 255])
            } else {
                Rgba([20, 40, 200, 128])
            }
        });
        let dyn_img = DynamicImage::ImageRgba8(img);
        encode_png(&dyn_img).unwrap()
    }

    fn decode_kitty_payload_png(payload: &str) -> DynamicImage {
        let mut b64 = String::new();
        for frame in payload.split("\x1b\\") {
            if let Some((_, chunk)) = frame.split_once(';') {
                b64.push_str(chunk);
            }
        }
        let decoded_png = B64_STANDARD.decode(&b64).unwrap();
        image::load_from_memory(&decoded_png).unwrap()
    }

    #[test]
    fn test_probe_vt_mode_enabled_before_query_write_and_restored() {
        #[derive(Debug, PartialEq, Eq)]
        enum Step {
            SetMode(u32),
            Write(String),
            Flush,
            ReadReply,
        }

        struct RecordingWriter {
            log: Rc<RefCell<Vec<Step>>>,
        }
        impl std::io::Write for RecordingWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.log
                    .borrow_mut()
                    .push(Step::Write(String::from_utf8_lossy(buf).into_owned()));
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.log.borrow_mut().push(Step::Flush);
                Ok(())
            }
        }

        let log = Rc::new(RefCell::new(Vec::<Step>::new()));
        let mut writer = RecordingWriter {
            log: Rc::clone(&log),
        };
        let prev_mode = 0x0018 | WIN_ENABLE_QUICK_EDIT_MODE; // QuickEdit initially ON, VT input OFF

        let reply = write_and_read_windows_probe_with(
            prev_mode,
            "\x1b[16t\x1b[c",
            &mut writer,
            |mode| log.borrow_mut().push(Step::SetMode(mode)),
            || {
                log.borrow_mut().push(Step::ReadReply);
                b"\x1b[6;20;10t\x1b[?61;4c".to_vec()
            },
        );

        assert_eq!(reply, "\x1b[6;20;10t\x1b[?61;4c");
        let steps = log.borrow();
        assert_eq!(steps.len(), 5, "unexpected probe step sequence: {steps:?}");

        // 1. ENABLE_VIRTUAL_TERMINAL_INPUT MUST be set BEFORE write_all(query)
        match steps[0] {
            Step::SetMode(m) => {
                assert_ne!(m & WIN_ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
                assert_ne!(m & WIN_ENABLE_EXTENDED_FLAGS, 0);
                assert_eq!(m & WIN_ENABLE_QUICK_EDIT_MODE, 0);
            }
            ref other => panic!("expected SetMode first, got {other:?}"),
        }
        assert_eq!(steps[1], Step::Write("\x1b[16t\x1b[c".to_string()));
        assert_eq!(steps[2], Step::Flush);
        assert_eq!(steps[3], Step::ReadReply);

        // 2. Drop restores mode with VT input cleared, EXTENDED_FLAGS set, and QUICK_EDIT cleared
        match steps[4] {
            Step::SetMode(m) => {
                assert_eq!(m & WIN_ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
                assert_ne!(m & WIN_ENABLE_EXTENDED_FLAGS, 0);
                assert_eq!(m & WIN_ENABLE_QUICK_EDIT_MODE, 0);
                assert_eq!(m & 0x0018, 0x0018);
            }
            ref other => panic!("expected restore SetMode last, got {other:?}"),
        }
    }

    #[test]
    fn test_cell_px_rounded_division_avoids_truncation() {
        assert_eq!(cell_px_from_window_px(1590, 995, 100, 50), Some((16, 20)));
        assert_eq!(
            parse_csi_win_size_reply(b"\x1b[4;995;1590t", 100, 50),
            Some((16, 20))
        );
    }

    #[test]
    fn test_protocol_detection_wt_requires_da1_stackbite_suppressed_wezterm_kitty() {
        // 1. WT_SESSION alone -> Halfblock statically (never forces Sixel without DA1 ;4)
        let mut wt_only = HashMap::new();
        wt_only.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_only),
            TerminalImageProtocol::Halfblock
        );

        // 2. WT_SESSION + DA1 ;4 -> Sixel; WT_SESSION + DA1 without ;4 -> Halfblock
        let mut caps_sixel = TerminalGraphicsCaps {
            protocol: resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_only),
            ..Default::default()
        };
        apply_probe_reply_to_caps(&mut caps_sixel, b"\x1b[?61;4c", false, true, None);
        assert_eq!(caps_sixel.protocol, TerminalImageProtocol::Sixel);

        let mut caps_no_sixel = TerminalGraphicsCaps {
            protocol: resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_only),
            ..Default::default()
        };
        apply_probe_reply_to_caps(&mut caps_no_sixel, b"\x1b[?61;1c", false, true, None);
        assert_eq!(caps_no_sixel.protocol, TerminalImageProtocol::Halfblock);

        // 3. WT_SESSION + STACKBITE_ROOM_ID -> Halfblock statically (probe results only)
        let mut sb_env = HashMap::new();
        sb_env.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        sb_env.insert("STACKBITE_ROOM_ID", "room-123");
        assert!(is_stackbite_env(&|k| sb_env.get(k).map(|v| (*v).to_string())));
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &sb_env),
            TerminalImageProtocol::Halfblock
        );

        // 4. WezTerm -> Kitty (promoted ahead of iTerm2)
        let mut wez_exec = HashMap::new();
        wez_exec.insert("WEZTERM_EXECUTABLE", "C:\\Program Files\\WezTerm\\wezterm-gui.exe");
        wez_exec.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_exec),
            TerminalImageProtocol::Kitty
        );

        let mut wez_tp = HashMap::new();
        wez_tp.insert("TERM_PROGRAM", "WezTerm");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_tp),
            TerminalImageProtocol::Kitty
        );
    }

    #[test]
    fn test_kitty_single_axis_emits_only_constraining_dimension() {
        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (10, 20),
            cell_px_exact: true,
        };

        // Width-constrained 1600x900 in 80x40 cells -> emits ONLY `c=`, NEVER `,r=`
        let wide_png = make_test_png(1600, 900);
        let wide_rendered =
            decode_and_render_for_protocol(&wide_png, FitBox::contained(80, 40), (0, 0, 0), caps)
                .unwrap();
        let wide_payload = wide_rendered.pixel_payload.as_deref().unwrap();
        let wide_hdr = wide_payload.split(';').next().unwrap();
        assert!(
            wide_hdr.contains(",c="),
            "expected width-constrained Kitty header to contain ,c=: {wide_hdr}"
        );
        assert!(
            !wide_hdr.contains(",r="),
            "width-constrained Kitty header must NOT contain ,r=: {wide_hdr}"
        );

        // Height-constrained 400x1200 in 80x20 cells -> emits ONLY `r=`, NEVER `,c=`
        let tall_png = make_test_png(400, 1200);
        let tall_rendered =
            decode_and_render_for_protocol(&tall_png, FitBox::contained(80, 20), (0, 0, 0), caps)
                .unwrap();
        let tall_payload = tall_rendered.pixel_payload.as_deref().unwrap();
        let tall_hdr = tall_payload.split(';').next().unwrap();
        assert!(
            tall_hdr.contains(",r="),
            "expected height-constrained Kitty header to contain ,r=: {tall_hdr}"
        );
        assert!(
            !tall_hdr.contains(",c="),
            "height-constrained Kitty header must NOT contain ,c=: {tall_hdr}"
        );
    }

    #[test]
    fn test_pixel_scroll_backing_crops_native_source_without_double_scaling() {
        let tall_png = make_test_png(800, 2400);
        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (10, 20),
            cell_px_exact: true,
        };
        // Virtual viewport 40x20 cells (400x400px); native width is 800px
        let rendered = decode_and_render_for_protocol(
            &tall_png,
            FitBox::scrollable(40, 20, 100),
            (0, 0, 0),
            caps,
        )
        .unwrap();
        assert_eq!(rendered.cols, 40);
        assert!(rendered.rows > 20);

        let (payload, draw_c, draw_r) = rendered
            .viewport_pixel_payload(0, 10, 40, 20)
            .expect("expected viewport crop payload");
        assert!(draw_c <= 40 && draw_r <= 20);
        let cropped_img = decode_kitty_payload_png(&payload);
        assert_eq!(
            cropped_img.width(),
            800,
            "cropped viewport PNG must preserve native 800px width, not downscaled 400px buffer"
        );
        assert_eq!(cropped_img.height(), 800);
    }

    #[test]
    fn test_fit_box_no_forced_crop_on_wide_low_terminal_at_100pct() {
        let fit = FitBox::scrollable(176, 25, 100);
        for cell_px in [(10, 20), (8, 16), (9, 19)] {
            for proto in [
                TerminalImageProtocol::Kitty,
                TerminalImageProtocol::ITerm2,
                TerminalImageProtocol::Sixel,
            ] {
                let (_tw, _th, occ_cols, occ_rows) =
                    compute_pixel_fit_for_protocol(1920, 1080, fit, cell_px, proto);
                assert!(
                    occ_rows <= 25 && occ_cols <= 176,
                    "1920x1080 at 100% zoom must fit within 176x25 without forced crop for {proto:?} cell_px={cell_px:?}, got {occ_cols}x{occ_rows}"
                );
            }
            let (hb_w, hb_h) = compute_halfblock_dims(1920, 1080, fit, cell_px);
            assert!(
                hb_w <= 176 && (hb_h / 2) <= 25,
                "halfblock 1920x1080 must fit within 176x25 for cell_px={cell_px:?}, got {hb_w}x{}",
                hb_h / 2
            );
        }
    }

    #[test]
    fn test_tall_screenshot_uses_full_width_past_92_cols_and_zoom_300pct_scales() {
        let cell_px = (10, 20);
        let (tw100, th100, cols100, _rows100) = compute_pixel_fit_for_protocol(
            1200,
            4000,
            FitBox::scrollable(140, 30, 100),
            cell_px,
            TerminalImageProtocol::Kitty,
        );
        assert!(
            cols100 > 92,
            "tall screenshot in 140-col viewport must use full width past old 92-col cap, got {cols100}"
        );

        let (tw200, th200, _cols200, _rows200) = compute_pixel_fit_for_protocol(
            1200,
            4000,
            FitBox::scrollable(140, 30, 200),
            cell_px,
            TerminalImageProtocol::Kitty,
        );
        let (tw300, th300, _cols300, _rows300) = compute_pixel_fit_for_protocol(
            1200,
            4000,
            FitBox::scrollable(140, 30, 300),
            cell_px,
            TerminalImageProtocol::Kitty,
        );

        assert!(tw200 > tw100 && th200 > th100, "200% zoom must scale beyond 100%");
        assert!(
            tw200 > 1920,
            "200% zoom must not be blocked by old 1920px width cap, got {tw200}"
        );
        assert!(
            tw300 > tw200 && th300 > th200,
            "300% zoom must scale strictly beyond 200% (got {tw200}x{th200} -> {tw300}x{th300})"
        );
    }

    #[test]
    fn test_sixel_preserves_ui_accent_suppresses_flat_dither_and_bounds_cache() {
        // Synthetic UI screenshot: ~98% flat gray background (240, 240, 240) + ~2% vivid blue/red accent +
        // small gradient strip so unique RGB count > 256 (forcing median-cut + dithering path).
        let w = 160usize;
        let h = 100usize;
        let mut flat = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let off = (y * w + x) * 4;
                let (r, g, b) = if y == 0 {
                    // 160 unique gradient pixels on row 0
                    ((x * 255 / w) as u8, 40, (255 - x * 255 / w) as u8)
                } else if y == 1 {
                    // another 160 unique gradient pixels on row 1 -> > 300 distinct colors total
                    (40, (x * 255 / w) as u8, ((x * 3) % 256) as u8)
                } else if (20..24).contains(&y) && (10..50).contains(&x) {
                    // Vivid blue UI accent text (~1% pixels)
                    (0, 102, 255)
                } else if (30..34).contains(&y) && (10..50).contains(&x) {
                    // Vivid red UI accent text (~1% pixels)
                    (230, 25, 45)
                } else {
                    // Flat gray UI canvas
                    (240, 240, 240)
                };
                flat[off] = r;
                flat[off + 1] = g;
                flat[off + 2] = b;
                flat[off + 3] = 255;
            }
        }

        let (palette, indexed, cache_len) = quantize_rgba_to_256_with_cache_len(&flat, w, h);
        assert!(palette.len() <= 256);
        assert!(cache_len <= MAX_QUANT_LOOKUP_CACHE);

        // (a) Both vivid blue and vivid red UI accents are preserved accurately in the palette
        let min_blue_dist = palette
            .iter()
            .map(|&(r, g, b)| redmean_dist_sq(0, 102, 255, r, g, b))
            .min()
            .unwrap();
        let min_red_dist = palette
            .iter()
            .map(|&(r, g, b)| redmean_dist_sq(230, 25, 45, r, g, b))
            .min()
            .unwrap();
        assert!(
            min_blue_dist <= 200,
            "blue UI accent lost in palette (min redmean dist = {min_blue_dist})"
        );
        assert!(
            min_red_dist <= 200,
            "red UI accent lost in palette (min redmean dist = {min_red_dist})"
        );

        // (b) Interior flat background region (y = 50..95, x = 10..150) has ZERO dither noise (single index)
        let ref_idx = indexed[50 * w + 10];
        for y in 50..95 {
            for x in 10..150 {
                assert_eq!(
                    indexed[y * w + x],
                    ref_idx,
                    "unexpected dither noise at ({x},{y}) in flat background region"
                );
            }
        }

        // (c) Large smooth gradient with > 20,000 unique RGB colors still respects MAX_QUANT_LOOKUP_CACHE
        let gw = 200usize;
        let gh = 160usize;
        let mut grad = Vec::with_capacity(gw * gh * 4);
        for y in 0..gh {
            for x in 0..gw {
                grad.extend_from_slice(&[
                    ((x * 255) / gw) as u8,
                    ((y * 255) / gh) as u8,
                    (((x * 7 + y * 13) * 3) % 256) as u8,
                    255,
                ]);
            }
        }
        let (_gp, _gi, grad_cache_len) = quantize_rgba_to_256_with_cache_len(&grad, gw, gh);
        assert!(
            grad_cache_len <= MAX_QUANT_LOOKUP_CACHE,
            "lookup_cache exceeded cap: {grad_cache_len} > {MAX_QUANT_LOOKUP_CACHE}"
        );
    }

    #[test]
    fn test_halfblock_aspect_ratio_matches_actual_cell_px() {
        let sq: RgbaImage = ImageBuffer::from_pixel(200, 200, Rgba([60, 140, 220, 255]));
        let png = encode_png(&DynamicImage::ImageRgba8(sq)).unwrap();
        let rendered = decode_and_render_halfblocks_with_cell_px(
            &png,
            FitBox::contained(40, 20),
            (0, 0, 0),
            (9, 22),
        )
        .unwrap();
        let phys_aspect = (rendered.cols as f64 * 9.0) / (rendered.rows as f64 * 22.0);
        assert!(
            (0.90..=1.10).contains(&phys_aspect),
            "expected square physical aspect in 0.90..=1.10 for cell_px=(9,22), got {phys_aspect:.3} ({}x{})",
            rendered.cols,
            rendered.rows
        );
    }

    #[test]
    fn test_inline_halfblock_uses_light_theme_bg_for_transparent_png() {
        let transparent: RgbaImage = ImageBuffer::from_pixel(8, 8, Rgba([0, 0, 0, 0]));
        let png = encode_png(&DynamicImage::ImageRgba8(transparent)).unwrap();
        let light_bg = color_to_rgb(Color::Rgb(245, 247, 250), (20, 22, 26));
        assert_eq!(light_bg, (245, 247, 250));
        assert_eq!(color_to_rgb(Color::Reset, (245, 247, 250)), (245, 247, 250));

        let rendered = decode_and_render_halfblocks_with_cell_px(
            &png,
            FitBox::contained(16, 8),
            light_bg,
            (10, 20),
        )
        .unwrap();
        assert!(!rendered.lines.is_empty());
        for line in &rendered.lines {
            for span in &line.spans {
                assert_eq!(span.style.fg, Some(Color::Rgb(245, 247, 250)));
                assert_eq!(span.style.bg, Some(Color::Rgb(245, 247, 250)));
            }
        }
    }

    #[test]
    fn test_is_image_mime_or_name() {
        assert!(is_image_mime_or_name("image/png", None));
        assert!(is_image_mime_or_name("hostedContent", None));
        assert!(is_image_mime_or_name("application/octet-stream", Some("photo.WEBP")));
        assert!(!is_image_mime_or_name("application/pdf", Some("doc.pdf")));
    }

    #[test]
    fn test_decode_and_render_halfblocks_aspect_coalescing_and_alpha() {
        let png = make_test_png(16, 8);
        let rendered = decode_and_render_halfblocks(&png, 20, 10, (10, 20, 30)).unwrap();
        assert_eq!(rendered.width_px, 16);
        assert_eq!(rendered.height_px, 8);
        assert_eq!(rendered.cols, 16);
        assert_eq!(rendered.rows, 4);
        assert_eq!(rendered.format_label, "PNG");
        assert_eq!(rendered.protocol, TerminalImageProtocol::Halfblock);
        assert!(rendered.pixel_payload.is_none());
        assert_eq!(rendered.lines.len(), 4);

        // Every line must contain half-block characters summing to `cols`
        for line in &rendered.lines {
            let char_count: usize = line
                .spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum();
            assert_eq!(char_count, 16);
            assert!(line.spans.iter().all(|s| s.content.chars().all(|c| c == '▀')));
        }

        // Uniform solid image coalesces every row into a single Span
        let solid: RgbaImage = ImageBuffer::from_pixel(10, 4, Rgba([255, 0, 0, 255]));
        let solid_png = encode_png(&DynamicImage::ImageRgba8(solid)).unwrap();
        let solid_rendered =
            decode_and_render_halfblocks(&solid_png, 20, 10, (0, 0, 0)).unwrap();
        assert_eq!(solid_rendered.lines.len(), 2);
        assert_eq!(solid_rendered.lines[0].spans.len(), 1);
        assert_eq!(solid_rendered.lines[0].spans[0].content, "▀▀▀▀▀▀▀▀▀▀");
    }

    #[test]
    fn test_protocol_auto_detection_matrix() {
        let empty = HashMap::new();
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Kitty, &empty),
            TerminalImageProtocol::Kitty
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Iterm2, &empty),
            TerminalImageProtocol::ITerm2
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Sixel, &empty),
            TerminalImageProtocol::Sixel
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Halfblock, &empty),
            TerminalImageProtocol::Halfblock
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &empty),
            TerminalImageProtocol::Halfblock
        );

        // ONENOTE_IMAGE_PROTOCOL takes precedence over TEAMS_IMAGE_PROTOCOL and overrides TMUX/explicit pref
        let mut env_override = HashMap::new();
        env_override.insert("TMUX", "/tmp/tmux-1000/default,1,0");
        env_override.insert("TEAMS_IMAGE_PROTOCOL", "sixel");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &env_override),
            TerminalImageProtocol::Sixel
        );
        env_override.insert("ONENOTE_IMAGE_PROTOCOL", "kitty");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &env_override),
            TerminalImageProtocol::Kitty
        );
        env_override.insert("ONENOTE_IMAGE_PROTOCOL", "halfblocks");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Kitty, &env_override),
            TerminalImageProtocol::Halfblock
        );

        // TMUX / STY guard forces Halfblock in Auto mode even when KITTY_WINDOW_ID / WT_SESSION are present
        let mut tmux_env = HashMap::new();
        tmux_env.insert("TMUX", "/tmp/tmux-1000/default,1,0");
        tmux_env.insert("KITTY_WINDOW_ID", "1");
        tmux_env.insert("WT_SESSION", "guid-1");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &tmux_env),
            TerminalImageProtocol::Halfblock
        );

        let mut screen_env = HashMap::new();
        screen_env.insert("STY", "1234.pts-0.host");
        screen_env.insert("TERM", "foot");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &screen_env),
            TerminalImageProtocol::Halfblock
        );

        // Kitty & Ghostty detection
        let mut kitty_env = HashMap::new();
        kitty_env.insert("KITTY_WINDOW_ID", "42");
        kitty_env.insert("WT_SESSION", "guid-1");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &kitty_env),
            TerminalImageProtocol::Kitty
        );

        let mut ghostty_env = HashMap::new();
        ghostty_env.insert("TERM_PROGRAM", "Ghostty");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &ghostty_env),
            TerminalImageProtocol::Kitty
        );

        // WezTerm + WT_SESSION both set -> Kitty wins
        let mut wez_wt_env = HashMap::new();
        wez_wt_env.insert("WEZTERM_EXECUTABLE", "C:\\Program Files\\WezTerm\\wezterm-gui.exe");
        wez_wt_env.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_wt_env),
            TerminalImageProtocol::Kitty
        );

        // iTerm2 / Rio / Mintty / Hyper remain ITerm2
        let mut iterm_env = HashMap::new();
        iterm_env.insert("TERM_PROGRAM", "iTerm.app");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &iterm_env),
            TerminalImageProtocol::ITerm2
        );

        // WT_SESSION alone stays Halfblock statically so Auto always probes DA1 ;4
        let mut wt_only = HashMap::new();
        wt_only.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_only),
            TerminalImageProtocol::Halfblock
        );

        // Linux Sixel terminals
        let mut foot_env = HashMap::new();
        foot_env.insert("TERM", "foot");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &foot_env),
            TerminalImageProtocol::Sixel
        );
    }

    #[test]
    fn test_sixel_encoder_solid_and_multiband_and_gradient() {
        let solid: RgbaImage = ImageBuffer::from_pixel(10, 8, Rgba([255, 0, 0, 255]));
        let solid_png = encode_png(&DynamicImage::ImageRgba8(solid)).unwrap();
        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let rendered = decode_and_render_for_protocol(
            &solid_png,
            FitBox::contained(40, 20),
            (0, 0, 0),
            caps,
        )
        .unwrap();
        assert_eq!(rendered.protocol, TerminalImageProtocol::Sixel);
        assert!(rendered.lines.is_empty());
        let payload = rendered.pixel_payload.as_deref().unwrap();
        assert!(payload.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(payload.contains("#0;2;100;0;0"));
        assert!(payload.contains('!'));
        assert!(payload.contains('-'));
        assert!(payload.ends_with("\x1b\\"));
        assert!(!payload.ends_with('\n'));
        assert!(rendered.approx_heap_bytes() > std::mem::size_of::<RenderedTerminalImage>());

        let grad: RgbaImage = ImageBuffer::from_fn(32, 16, |x, y| {
            Rgba([
                ((x * 8) % 256) as u8,
                ((y * 16) % 256) as u8,
                (((x + y) * 5) % 256) as u8,
                if y < 8 { 255 } else { 128 },
            ])
        });
        let grad_png = encode_png(&DynamicImage::ImageRgba8(grad)).unwrap();
        let grad_rendered = decode_and_render_for_protocol(
            &grad_png,
            FitBox::contained(20, 10),
            (12, 24, 36),
            caps,
        )
        .unwrap();
        let grad_payload = grad_rendered.pixel_payload.as_deref().unwrap();
        assert!(grad_payload.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(grad_payload.contains(";2;"));
        assert!(grad_payload.contains('-'));
        assert!(grad_payload.ends_with("\x1b\\"));
    }

    #[test]
    fn test_kitty_and_iterm2_tui_vs_cli_stdout_payloads() {
        let png = make_test_png(16, 8);

        // TUI Kitty payload: fixed image id 4242, q=2, single constraining axis (`c=`), C=1, no trailing newline
        let kitty_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let kitty_tui =
            decode_and_render_for_protocol(&png, FitBox::contained(20, 10), (0, 0, 0), kitty_caps)
                .unwrap();
        let kitty_payload = kitty_tui.pixel_payload.as_deref().unwrap();
        assert!(kitty_payload.starts_with(&format!(
            "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},c={},C=1,m=",
            kitty_tui.cols
        )));
        assert!(!kitty_payload.split(';').next().unwrap().contains(",r="));
        assert!(kitty_payload.ends_with("\x1b\\"));
        assert!(!kitty_payload.ends_with('\n'));
        assert_eq!(
            kitty_delete_preview_sequence(),
            "\x1b_Ga=d,d=i,i=4242,q=2\x1b\\"
        );

        // TUI iTerm2 payload: doNotMoveCursor=1, no trailing newline
        let iterm_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::ITerm2,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let iterm_tui =
            decode_and_render_for_protocol(&png, FitBox::contained(20, 10), (0, 0, 0), iterm_caps)
                .unwrap();
        let iterm_payload = iterm_tui.pixel_payload.as_deref().unwrap();
        assert!(iterm_payload.starts_with(&format!(
            "\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1;doNotMoveCursor=1:",
            iterm_tui.cols, iterm_tui.rows
        )));
        assert!(iterm_payload.ends_with('\x07'));
        assert!(!iterm_payload.ends_with('\n'));

        // CLI stdout payloads: omit C=1 and doNotMoveCursor=1, end with '\n'
        let kitty_cli = render_ansi_stdout_with_caps(&png, 16, 8, kitty_caps).unwrap();
        assert!(kitty_cli.starts_with("\x1b_Gf=100,a=T,m="));
        assert!(!kitty_cli.contains("C=1"));
        assert!(kitty_cli.ends_with("\x1b\\\n"));

        let iterm_cli = render_ansi_stdout_with_caps(&png, 16, 8, iterm_caps).unwrap();
        assert!(iterm_cli.starts_with("\x1b]1337;File=inline=1;"));
        assert!(!iterm_cli.contains("doNotMoveCursor=1"));
        assert!(iterm_cli.ends_with("\x07\n"));

        let sixel_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let sixel_cli = render_ansi_stdout_with_caps(&png, 16, 8, sixel_caps).unwrap();
        assert!(sixel_cli.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(sixel_cli.ends_with("\x1b\\\n"));
    }

    #[test]
    fn test_render_ansi_stdout_protocols() {
        let png = make_test_png(8, 4);

        let half = render_ansi_stdout_with_caps(
            &png,
            16,
            8,
            TerminalGraphicsCaps {
                protocol: TerminalImageProtocol::Halfblock,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(half.contains("\x1b[38;2;"));
        assert!(half.contains("\x1b[48;2;"));
        assert!(half.contains('▀'));
        assert!(half.ends_with("\x1b[0m\n"));

        let iterm = render_ansi_stdout_with_caps(
            &png,
            16,
            8,
            TerminalGraphicsCaps {
                protocol: TerminalImageProtocol::ITerm2,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(iterm.starts_with("\x1b]1337;File=inline=1;width=8;preserveAspectRatio=1:"));
        assert!(iterm.ends_with("\x07\n"));

        let kitty = render_ansi_stdout_with_caps(
            &png,
            16,
            8,
            TerminalGraphicsCaps {
                protocol: TerminalImageProtocol::Kitty,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(kitty.starts_with("\x1b_Gf=100,a=T,m="));
        assert!(kitty.ends_with("\x1b\\\n"));
    }

    #[test]
    fn test_decode_rejects_invalid_bytes() {
        assert!(decode_and_render_halfblocks(b"not-an-image", 20, 10, (0, 0, 0)).is_err());
        assert!(render_ansi_stdout(b"not-an-image", 20, 10).is_err());
    }

    #[test]
    fn test_approx_heap_bytes_scales_with_dimensions_and_is_nonzero() {
        let empty = RenderedTerminalImage {
            width_px: 0,
            height_px: 0,
            cols: 0,
            rows: 0,
            format_label: String::new(),
            protocol: TerminalImageProtocol::Halfblock,
            lines: Vec::new(),
            pixel_payload: None,
            scroll_backing: None,
        };
        assert_eq!(
            empty.approx_heap_bytes(),
            std::mem::size_of::<RenderedTerminalImage>()
        );
        assert!(empty.approx_heap_bytes() > 0);

        let with_payload = RenderedTerminalImage {
            width_px: 10,
            height_px: 10,
            cols: 2,
            rows: 1,
            format_label: "PNG".to_string(),
            protocol: TerminalImageProtocol::Sixel,
            lines: Vec::new(),
            pixel_payload: Some("x".repeat(512)),
            scroll_backing: None,
        };
        assert!(with_payload.approx_heap_bytes() >= empty.approx_heap_bytes() + 512);

        let png = make_test_png(64, 32);
        let small = decode_and_render_halfblocks(&png, 8, 4, (0, 0, 0)).unwrap();
        let large = decode_and_render_halfblocks(&png, 64, 16, (0, 0, 0)).unwrap();

        let small_cells = (small.cols as usize) * (small.rows as usize);
        let large_cells = (large.cols as usize) * (large.rows as usize);
        assert!(large_cells > small_cells);
        assert!(small.approx_heap_bytes() > empty.approx_heap_bytes());
        assert!(large.approx_heap_bytes() > small.approx_heap_bytes());
    }

    #[test]
    fn test_render_ansi_stdout_rejects_invalid_bytes_all_protocols() {
        for proto in [
            TerminalImageProtocol::Halfblock,
            TerminalImageProtocol::ITerm2,
            TerminalImageProtocol::Kitty,
            TerminalImageProtocol::Sixel,
        ] {
            let caps = TerminalGraphicsCaps {
                protocol: proto,
                ..Default::default()
            };
            assert!(render_ansi_stdout_with_caps(b"bad-bytes", 16, 8, caps).is_err());
        }
    }

    #[test]
    fn test_protocol_helpers_and_cell_size_parser() {
        assert_eq!(TerminalImageProtocol::Kitty.next_manual(), TerminalImageProtocol::ITerm2);
        assert_eq!(TerminalImageProtocol::ITerm2.next_manual(), TerminalImageProtocol::Sixel);
        assert_eq!(TerminalImageProtocol::Sixel.next_manual(), TerminalImageProtocol::Halfblock);
        assert_eq!(TerminalImageProtocol::Halfblock.next_manual(), TerminalImageProtocol::Kitty);

        assert!(TerminalImageProtocol::Kitty.is_pixel());
        assert!(TerminalImageProtocol::ITerm2.is_pixel());
        assert!(TerminalImageProtocol::Sixel.is_pixel());
        assert!(!TerminalImageProtocol::Halfblock.is_pixel());

        assert_eq!(parse_cell_pixel_size("10x20"), Some((10, 20)));
        assert_eq!(parse_cell_pixel_size(" 9X18 "), Some((9, 18)));
        assert_eq!(parse_cell_pixel_size("0x16"), None);
        assert_eq!(parse_cell_pixel_size("bad"), None);

        let mut cell_env = HashMap::new();
        cell_env.insert("TEAMS_CELL_PIXEL_SIZE", "9x18");
        assert_eq!(
            resolve_cell_pixel_size_with(|k| cell_env.get(k).map(|v| (*v).to_string())),
            Some((9, 18))
        );
        cell_env.insert("ONENOTE_CELL_PIXEL_SIZE", "12x24");
        assert_eq!(
            resolve_cell_pixel_size_with(|k| cell_env.get(k).map(|v| (*v).to_string())),
            Some((12, 24))
        );
    }

    #[test]
    fn test_terminal_probe_reply_parsers() {
        assert_eq!(
            parse_csi_cell_size_reply(b"\x1b[6;24;12t"),
            Some((12, 24))
        );
        assert_eq!(
            parse_csi_cell_size_reply(b"noise\x1b[6;20;10t\x1b[?62;4c"),
            Some((10, 20))
        );
        assert_eq!(parse_csi_cell_size_reply(b"\x1b[6;0;10t"), None);
        assert_eq!(parse_csi_cell_size_reply(b"\x1b[4;1080;1920t"), None);
        assert_eq!(
            parse_csi_cell_size_reply(b"\x1b[6;500;200t"),
            Some((64, 128))
        );

        assert_eq!(
            parse_csi_win_size_reply(b"\x1b[4;960;1920t", 160, 40),
            Some((12, 24))
        );
        assert_eq!(
            parse_csi_win_size_reply(b"prefix\x1b[4;800;1200t\x1b[?64;4c", 120, 40),
            Some((10, 20))
        );
        assert_eq!(parse_csi_win_size_reply(b"\x1b[4;800;1200t", 0, 40), None);
        assert_eq!(parse_csi_win_size_reply(b"\x1b[4;0;1200t", 120, 40), None);

        assert!(parse_kitty_query_ok(b"\x1b_Gi=31;OK\x1b\\"));
        assert!(parse_kitty_query_ok(b"\x1b[6;20;10t\x1b_Gi=31,s=1;OK\x07\x1b[?62c"));
        assert!(!parse_kitty_query_ok(b"\x1b_Gi=31;ENOENT:not found\x1b\\"));
        assert!(!parse_kitty_query_ok(b"\x1b_Gi=99;OK\x1b\\"));
        assert!(!parse_kitty_query_ok(b"\x1b[?62;4c"));

        assert!(parse_da1_has_sixel(b"\x1b[?62;1;4;6;9;15;22c"));
        assert!(parse_da1_has_sixel(b"\x1b[6;20;10t\x1b[?64;4c"));
        assert!(!parse_da1_has_sixel(b"\x1b[?64;1;6;14;22c"));
        assert!(!parse_da1_has_sixel(b"\x1b[?4;1;6c"));
        assert!(!parse_da1_has_sixel(b""));
    }

    #[test]
    fn test_kitty_preserves_full_resolution_for_jpeg_instead_of_cell_downscale() {
        let rgb = image::ImageBuffer::from_fn(640, 480, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let dyn_img = DynamicImage::ImageRgb8(rgb);
        let mut jpeg_buf = Cursor::new(Vec::new());
        dyn_img.write_to(&mut jpeg_buf, ImageFormat::Jpeg).unwrap();
        let jpeg_bytes = jpeg_buf.into_inner();

        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let rendered = decode_and_render_for_protocol(
            &jpeg_bytes,
            FitBox::contained(20, 10),
            (0, 0, 0),
            caps,
        )
        .unwrap();
        let payload = rendered.pixel_payload.as_deref().unwrap();
        let decoded_img = decode_kitty_payload_png(payload);
        assert_eq!(decoded_img.width(), 640);
        assert_eq!(decoded_img.height(), 480);
    }

    #[test]
    fn test_sixel_floyd_steinberg_dithering_smooth_gradient_and_edge_dims() {
        let mut flat = Vec::with_capacity(32 * 16 * 4);
        for y in 0..16u32 {
            for x in 0..32u32 {
                flat.extend_from_slice(&[
                    (x * 8) as u8,
                    (y * 16) as u8,
                    ((x * 5 + y * 9) % 256) as u8,
                    255,
                ]);
            }
        }
        let (palette, indexed) = quantize_rgba_to_256(&flat, 32, 16);
        assert!(palette.len() <= 256 && palette.len() > 32);
        assert_eq!(indexed.len(), 32 * 16);
        assert!(indexed.iter().all(|&i| (i as usize) < palette.len()));

        for (w, h) in [(1usize, 1usize), (1, 8), (8, 1)] {
            let buf = vec![128u8; w * h * 4];
            let s = encode_sixel_rgba(&buf, w as u32, h as u32);
            assert!(s.starts_with("\x1bP0;1;q\"1;1;"));
            assert!(s.ends_with("\x1b\\"));
        }
    }

    #[test]
    fn test_looks_like_html_and_svg_bytes_and_decode_rejection() {
        let html_doc = b"  \r\n<!DOCTYPE html><html><head><title>Login</title></head><body>Sign in</body></html>";
        let html_bom = b"\xEF\xBB\xBF<html><body>403 Forbidden</body></html>";
        let svg_raw = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"><rect/></svg>";
        let svg_xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- comment -->\n<svg viewBox=\"0 0 10 10\"></svg>";
        let png = make_test_png(8, 8);

        assert!(looks_like_html_bytes(html_doc));
        assert!(looks_like_html_bytes(html_bom));
        assert!(!looks_like_html_bytes(svg_raw));
        assert!(!looks_like_html_bytes(svg_xml));
        assert!(!looks_like_html_bytes(&png));

        assert!(looks_like_svg_bytes(svg_raw));
        assert!(looks_like_svg_bytes(svg_xml));
        assert!(!looks_like_svg_bytes(html_doc));
        assert!(!looks_like_svg_bytes(&png));

        assert!(is_image_mime_or_name("image/svg+xml", None));
        assert!(is_image_mime_or_name("image/svg+xml; charset=utf-8", Some("icon.svg")));
        assert!(!is_image_mime_or_name("text/html; charset=utf-8", Some("photo.png")));
        assert!(is_image_mime_or_name("application/octet-stream", Some("logo.svg")));
        assert!(is_image_mime_or_name("image/png; charset=binary", None));

        let html_err = decode_and_render_halfblocks(html_doc, 20, 10, (0, 0, 0))
            .unwrap_err()
            .to_string();
        assert!(html_err.contains("HTML"), "unexpected html err: {html_err}");

        let svg_rendered = decode_and_render_halfblocks(svg_raw, 20, 10, (0, 0, 0))
            .expect("standalone SVG should render via Pure-Rust resvg");
        assert_eq!(svg_rendered.format_label, "SVG");
        assert!(!svg_rendered.lines.is_empty());
        assert!(svg_rendered
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains('▀'))));

        for proto in [
            TerminalImageProtocol::Kitty,
            TerminalImageProtocol::ITerm2,
            TerminalImageProtocol::Sixel,
            TerminalImageProtocol::Halfblock,
        ] {
            let caps = TerminalGraphicsCaps {
                protocol: proto,
                cell_px: (8, 16),
                cell_px_exact: true,
            };
            let err_h = decode_and_render_for_protocol(
                html_doc,
                FitBox::contained(20, 10),
                (0, 0, 0),
                caps,
            )
            .unwrap_err()
            .to_string();
            assert!(err_h.contains("HTML"));
            let rendered_s = decode_and_render_for_protocol(
                svg_xml,
                FitBox::contained(20, 10),
                (0, 0, 0),
                caps,
            )
            .expect("SVG should render for all terminal protocols");
            assert_eq!(rendered_s.format_label, "SVG");
            assert!(render_ansi_stdout_with_caps(html_doc, 20, 10, caps).is_err());
            assert!(render_ansi_stdout_with_caps(svg_raw, 20, 10, caps).is_ok());
        }
    }

    #[test]
    fn test_erase_pixel_rect_sequence_and_sixel_safe_height() {
        assert_eq!(compute_sixel_safe_pixel_height(8, 20), 156); // 160 -> 26 * 6
        assert_eq!(compute_sixel_safe_pixel_height(4, 19), 72); // 76 -> 12 * 6
        assert_eq!(compute_sixel_safe_pixel_height(6, 16), 96); // exact multiple of 6

        for cell_px in [(9u16, 19u16), (10, 20), (8, 16), (11, 23)] {
            for max_rows in 4..=25u16 {
                for (img_w, img_h) in [(200u32, 600u32), (800, 800), (600, 200), (137, 251)] {
                    let (_tw, th, _occ_cols, occ_rows) = compute_pixel_fit_for_protocol(
                        img_w,
                        img_h,
                        FitBox::contained(40, max_rows),
                        cell_px,
                        TerminalImageProtocol::Sixel,
                    );
                    let band_px = th.div_ceil(6) * 6;
                    assert!(
                        band_px <= (max_rows as u32) * (cell_px.1 as u32),
                        "Sixel 6px band overhung max_rows={max_rows} cell_px={cell_px:?}: th={th} band_px={band_px}"
                    );
                    assert!(occ_rows <= max_rows);
                }
            }
        }

        let seq = erase_pixel_rect_sequence(10, 5, 8, 3, 40, 20);
        assert!(seq.starts_with("\x1b[0m"));
        assert!(seq.contains("\x1b[6;11H          "));
        assert!(seq.contains("\x1b[10;11H          "));
        assert!(!seq.contains("\x1b[11;11H"));

        let clamped = erase_pixel_rect_sequence(38, 19, 5, 5, 40, 20);
        assert_eq!(clamped, "\x1b[0m\x1b[20;39H  ");

        assert!(erase_pixel_rect_sequence(40, 5, 5, 5, 40, 20).is_empty());
        assert!(erase_pixel_rect_sequence(5, 5, 0, 5, 40, 20).is_empty());
        assert_eq!(kitty_delete_all_sequence(), "\x1b_Ga=d,d=A,q=2\x1b\\");
    }

    #[test]
    fn test_restore_windows_console_mode_flags_sets_extended_and_clears_quick_edit() {
        let restored = restore_windows_console_mode_flags(0x0040 | 0x0018);
        assert_eq!(restored & WIN_ENABLE_QUICK_EDIT_MODE, 0);
        assert_ne!(restored & WIN_ENABLE_EXTENDED_FLAGS, 0);
        assert_eq!(restored & 0x0018, 0x0018);

        let from_zero = restore_windows_console_mode_flags(0);
        assert_eq!(from_zero, WIN_ENABLE_EXTENDED_FLAGS);
    }

    #[test]
    fn test_tall_image_viewport_pixel_payload_scrolling() {
        let tall_img: RgbaImage = ImageBuffer::from_fn(200, 1000, |x, y| {
            Rgba([
                ((x * 255) / 200) as u8,
                ((y * 255) / 1000) as u8,
                160,
                255,
            ])
        });
        let tall_png = encode_png(&DynamicImage::ImageRgba8(tall_img)).unwrap();
        for proto in [
            TerminalImageProtocol::Sixel,
            TerminalImageProtocol::Kitty,
            TerminalImageProtocol::ITerm2,
        ] {
            let caps = TerminalGraphicsCaps {
                protocol: proto,
                cell_px: (8, 16),
                cell_px_exact: true,
            };
            let rendered = decode_and_render_for_protocol(
                &tall_png,
                FitBox::scrollable(50, 16, 100),
                (10, 10, 10),
                caps,
            )
            .unwrap();
            assert!(
                rendered.rows > 20,
                "expected tall rendered rows > 20 for {proto:?}, got {}",
                rendered.rows
            );

            let (top_payload, top_c, top_r) = rendered
                .viewport_pixel_payload(0, 0, 50, 16)
                .expect("expected top viewport payload");
            assert!(top_c <= 50 && top_r <= 16);

            let (mid_payload, mid_c, mid_r) = rendered
                .viewport_pixel_payload(0, 10, 50, 16)
                .expect("expected scrolled viewport payload");
            assert!(mid_c <= 50 && mid_r <= 16);
            assert_ne!(top_payload, mid_payload);
        }
    }
}
