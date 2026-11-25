//! Interactive HTML Viewer modal (`H` / `[H: HTML]` / `.html` resource / auto-routed HTML).
//!
//! Backed by `bitefoundry_html::HtmlViewSession`:
//! - `0 MB` idle on static HTML; on-demand 8 MiB QuickJS-NG when `<script>` / `on*` handlers exist (dropped on close)
//! - `Tab` / `Shift+Tab` focus interactive controls (`<button>`, `<a>`, `<details>`, `<input>`, `<select>`)
//! - `Enter` / `Space` activate focused control, mouse click via `click_at(line, col)`
//! - Text input typing + `Backspace` + `Enter`/`Esc` to finish editing
//! - `m` toggle Rendered ↔ Raw HTML, `o` serve natively via `NativeHtmlServer::serve_html` & open in browser
//! - `j` / `k` / `↑` / `↓` / `PgUp` / `PgDn` / `g` / `G` / wheel scroll + vertical `Scrollbar`
//! - `Esc` / `q` / `[✕ Esc]` / click-outside close

use std::{collections::HashMap, fmt, sync::Arc};

use bitefoundry_html::{ContentKind, HtmlEffect, HtmlViewSession, SourceKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::{self, FitBox, RenderedTerminalImage, TerminalImageProtocol},
    models::OnenotePageDetail,
    ui::{
        action::Action,
        app::{PendingPixelGraphic, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

struct CachedHtmlGraphic {
    source_bytes: Arc<[u8]>,
    cell_px: (u16, u16),
    bg_rgb: (u8, u8, u8),
    rendered: RenderedTerminalImage,
}

pub struct HtmlPreviewOverlay {
    pub session: HtmlViewSession,
    pub content_rect: Rect,
    pub total_lines: usize,
    pub last_width: u16,
    pub follow_focus: bool,
    graphic_cache: HashMap<(usize, u16, TerminalImageProtocol), CachedHtmlGraphic>,
}

impl Drop for HtmlPreviewOverlay {
    fn drop(&mut self) {
        self.graphic_cache.clear();
    }
}

impl fmt::Debug for HtmlPreviewOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HtmlPreviewOverlay")
            .field("title", &self.session.title())
            .field("raw_mode", &self.session.raw_mode)
            .field("js_active", &self.session.js_active)
            .field("scroll_y", &self.session.scroll_y)
            .field("focused_control", &self.session.focused_control)
            .finish()
    }
}

impl HtmlPreviewOverlay {
    pub fn new(title: Option<String>, raw_html: String) -> Self {
        let mut session = HtmlViewSession::new(title, raw_html);
        // Startup effects (e.g., alert() during initial script) are already captured in session.last_status.
        let _ = session.take_pending_effects();
        Self {
            session,
            content_rect: Rect::default(),
            total_lines: 0,
            last_width: 80,
            follow_focus: false,
            graphic_cache: HashMap::new(),
        }
    }

    pub fn new_markdown(title: Option<String>, raw_md: String) -> Self {
        let session = HtmlViewSession::new_markdown(title, raw_md);
        Self {
            session,
            content_rect: Rect::default(),
            total_lines: 0,
            last_width: 80,
            follow_focus: false,
            graphic_cache: HashMap::new(),
        }
    }

    pub fn from_content(title: Option<String>, kind: ContentKind, raw: String) -> Self {
        match kind {
            ContentKind::Markdown => Self::new_markdown(title, raw),
            _ => Self::new(title, raw),
        }
    }

    pub fn from_onenote_page(detail: &OnenotePageDetail) -> Self {
        let title = if detail.summary.title.trim().is_empty() {
            "OneNote Page".to_string()
        } else {
            detail.summary.title.trim().to_string()
        };
        let html = if detail.raw_xhtml.trim().is_empty() {
            let escaped_title = escape_html(&title);
            let escaped_body = escape_html(&detail.body_plain);
            format!(
                "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{escaped_title}</title></head><body><h1>{escaped_title}</h1><p>{escaped_body}</p></body></html>"
            )
        } else {
            detail.raw_xhtml.clone()
        };
        Self::new(Some(title), html)
    }

    pub fn title(&self) -> String {
        let fallback = match self.session.source_kind() {
            SourceKind::Markdown => "Markdown Preview",
            SourceKind::Html => "HTML Preview",
        };
        self.session
            .title()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(fallback)
            .to_string()
    }

    pub fn raw_html(&self) -> &str {
        self.session.raw_html()
    }

    pub fn serve_html(&self) -> String {
        self.session.serve_html()
    }

    pub fn accepts_text(&self) -> bool {
        self.session.editing_input.is_some()
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(24).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    fn ensure_rendered(&mut self, ui: &UiState) {
        let width = if self.content_rect.width >= 12 {
            self.content_rect.width
        } else {
            self.last_width.max(20)
        };
        let html_theme = ui.theme.to_html_theme();
        let _ = self.session.render(width, &html_theme, ui.show_inline_hrefs);
    }

    pub fn toggle_raw_mode(&mut self) -> bool {
        let raw = self.session.toggle_raw_mode();
        self.session.scroll_y = 0;
        self.follow_focus = false;
        raw
    }

    fn apply_effects(&mut self, effects: Vec<HtmlEffect>) -> OverlayOutcome {
        let mut toast: Option<String> = None;
        let mut open_url: Option<String> = None;

        for effect in effects {
            match effect {
                HtmlEffect::None | HtmlEffect::DomChanged => {}
                HtmlEffect::OpenUrl(url) => {
                    open_url = Some(url);
                }
                HtmlEffect::Toast(msg) => {
                    self.session.last_status = Some(msg.clone());
                    toast = Some(msg);
                }
                HtmlEffect::FormSubmitted { action, fields } => {
                    let pairs = fields
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let msg = if action.trim().is_empty() {
                        format!("Form submitted ({pairs})")
                    } else {
                        format!("Form submitted -> {action} ({pairs})")
                    };
                    self.session.last_status = Some(msg.clone());
                    toast = Some(msg);
                }
            }
        }

        if let Some(url) = open_url {
            OverlayOutcome::Dispatch(Action::OpenUrl(url))
        } else if let Some(msg) = toast {
            OverlayOutcome::Dispatch(Action::ShowToast(msg))
        } else {
            OverlayOutcome::Handled
        }
    }

    pub fn handle(&mut self, action: &Action, ui: &UiState) -> OverlayOutcome {
        if matches!(action, Action::Quit | Action::QuitConfirm) {
            return OverlayOutcome::NotHandled;
        }

        if self.session.editing_input.is_some() {
            return match action {
                Action::Input(ch) => {
                    self.session.input_char(*ch);
                    OverlayOutcome::Handled
                }
                Action::Paste(text) => {
                    for ch in text.chars() {
                        if ch != '\r' && ch != '\n' {
                            self.session.input_char(ch);
                        }
                    }
                    OverlayOutcome::Handled
                }
                Action::Backspace => {
                    self.session.backspace();
                    OverlayOutcome::Handled
                }
                Action::Activate | Action::OverlaySubmit | Action::CloseOverlay => {
                    let effects = self.session.finish_editing();
                    self.apply_effects(effects)
                }
                Action::Tab => {
                    let _ = self.session.finish_editing();
                    self.ensure_rendered(ui);
                    self.session.focus_next_control();
                    self.follow_focus = true;
                    OverlayOutcome::Handled
                }
                Action::PrevFocus => {
                    let _ = self.session.finish_editing();
                    self.ensure_rendered(ui);
                    self.session.focus_prev_control();
                    self.follow_focus = true;
                    OverlayOutcome::Handled
                }
                Action::MouseScroll { delta, .. } => {
                    self.scroll_by(*delta);
                    OverlayOutcome::Handled
                }
                _ => OverlayOutcome::Handled,
            };
        }

        match action {
            Action::CloseOverlay | Action::Input('q') | Action::Input('Q') => {
                OverlayOutcome::Close
            }
            Action::Tab => {
                self.ensure_rendered(ui);
                self.session.focus_next_control();
                self.follow_focus = true;
                OverlayOutcome::Handled
            }
            Action::PrevFocus => {
                self.ensure_rendered(ui);
                self.session.focus_prev_control();
                self.follow_focus = true;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::Input(' ') | Action::OnenoteToggleTodo(None) => {
                self.ensure_rendered(ui);
                let effects = self.session.activate_focused();
                self.apply_effects(effects)
            }
            Action::Input('m')
            | Action::Input('M')
            | Action::Input('r')
            | Action::Input('R')
            | Action::RefreshAll => {
                self.toggle_raw_mode();
                OverlayOutcome::Handled
            }
            Action::OnenoteOpenWeb | Action::Input('o') | Action::Input('O') => {
                OverlayOutcome::Dispatch(Action::ServeHtmlPreviewInBrowser)
            }
            Action::MoveUp | Action::Input('k') => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(2);
                self.follow_focus = false;
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.session.scroll_y = self.session.scroll_y.saturating_add(2);
                self.follow_focus = false;
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(10);
                self.follow_focus = false;
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.session.scroll_y = self.session.scroll_y.saturating_add(10);
                self.follow_focus = false;
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.session.scroll_y = 0;
                self.follow_focus = false;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.session.scroll_y = u16::MAX;
                self.follow_focus = false;
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                self.scroll_by(*delta);
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        if delta < 0 {
            self.session.scroll_y = self
                .session
                .scroll_y
                .saturating_sub(delta.unsigned_abs() as u16);
        } else {
            self.session.scroll_y = self.session.scroll_y.saturating_add(delta as u16);
        }
        self.follow_focus = false;
    }

    pub fn handle_click(&mut self, col: u16, row: u16, ui: &UiState) -> OverlayOutcome {
        let r = self.content_rect;
        if col < r.x || col >= r.right() || row < r.y || row >= r.bottom() {
            if self.session.editing_input.is_some() {
                let effects = self.session.finish_editing();
                return self.apply_effects(effects);
            }
            return OverlayOutcome::Handled;
        }
        self.ensure_rendered(ui);
        let line_idx = self.session.scroll_y as usize + (row - r.y) as usize;
        let local_col = col - r.x;
        let effects = self.session.click_at(line_idx, local_col);
        self.apply_effects(effects)
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, ui: &mut UiState, hitmap: &mut HitMap) {
        let theme = ui.theme.clone();
        // Full-screen backdrop so clicking outside the modal closes HtmlPreview cleanly.
        hitmap.add(area, HitTarget::OverlayBackdrop);

        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let raw_pill = if self.session.raw_mode {
            "[m Render]"
        } else {
            "[m Raw]"
        };
        let serve_pill = "[o Browser]";
        let close_pill = "[✕ Esc]";
        let pills = [
            (raw_pill, ButtonId::HtmlToggleRaw),
            (serve_pill, ButtonId::HtmlServeBrowser),
            (close_pill, ButtonId::ConfirmCancel),
        ];
        let pills_total_w: u16 = pills
            .iter()
            .map(|(lbl, _)| lbl.width() as u16 + 1)
            .sum();
        let show_pills = rect.width > pills_total_w + 10;
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_pills { pills_total_w + 1 } else { 0 })
            as usize;

        let is_markdown = self.session.source_kind() == SourceKind::Markdown;
        let kind_badge = if is_markdown { "MD" } else { "HTML" };
        let mode_badge = if self.session.raw_mode {
            if is_markdown {
                "RAW MD"
            } else {
                "RAW HTML"
            }
        } else {
            "RENDERED"
        };
        let js_badge = if self.session.js_active {
            " · JS"
        } else {
            ""
        };
        let title = format!(
            " {kind_badge} · {} [{mode_badge}{js_badge}] ",
            crate::ui::truncate_width(&self.title(), 32)
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, title_budget),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if show_pills {
            let mut px = rect.right().saturating_sub(1);
            for (lbl, btn) in pills.iter().rev() {
                let w = lbl.width() as u16;
                px = px.saturating_sub(w);
                let pill_rect = Rect::new(px, rect.y, w, 1);
                let style = Style::default()
                    .fg(if *btn == ButtonId::ConfirmCancel {
                        theme.fg
                    } else {
                        theme.accent
                    })
                    .bg(theme.bg_elevated)
                    .add_modifier(Modifier::BOLD);
                frame.render_widget(Paragraph::new(Span::styled(*lbl, style)), pill_rect);
                hitmap.add_line(px, rect.y, w, HitTarget::Button(*btn));
                px = px.saturating_sub(1);
            }
        }

        if inner.width == 0 || inner.height == 0 {
            self.content_rect = Rect::default();
            return;
        }

        let show_footer = inner.height >= 3;
        let viewport_h = if show_footer {
            inner.height.saturating_sub(1)
        } else {
            inner.height
        };

        let html_theme = theme.to_html_theme();
        let initial_w = inner.width.max(12);
        let initial_len = self
            .session
            .render(initial_w, &html_theme, ui.show_inline_hrefs)
            .lines
            .len();

        let show_scrollbar = initial_len > viewport_h as usize && inner.width > 3;
        let avail_w = if show_scrollbar {
            inner.width.saturating_sub(1)
        } else {
            inner.width
        };

        let (total_lines, control_count, controls_meta, mut lines_to_render, graphics) = {
            let rendered = self
                .session
                .render(avail_w, &html_theme, ui.show_inline_hrefs);
            let meta: Vec<(usize, String)> = rendered
                .controls
                .iter()
                .map(|c| (c.line, c.label.clone()))
                .collect();
            (
                rendered.lines.len(),
                rendered.controls.len(),
                meta,
                rendered.lines.clone(),
                rendered.graphics.clone(),
            )
        };
        let focused_line = self
            .session
            .focused_control
            .and_then(|idx| controls_meta.get(idx))
            .map(|(line, _)| *line);
        let focused_label = self
            .session
            .focused_control
            .and_then(|idx| controls_meta.get(idx))
            .map(|(_, label)| label.clone());

        self.content_rect = Rect::new(inner.x, inner.y, avail_w, viewport_h);
        self.last_width = avail_w;
        self.total_lines = total_lines;

        let max_scroll_y = total_lines.saturating_sub(viewport_h as usize) as u16;
        if self.follow_focus {
            if let Some(line) = focused_line {
                let top = self.session.scroll_y as usize;
                let bottom = top + viewport_h as usize;
                if line < top {
                    self.session.scroll_y = line as u16;
                } else if line >= bottom {
                    self.session.scroll_y =
                        line.saturating_sub(viewport_h.saturating_sub(1) as usize) as u16;
                }
            }
            self.follow_focus = false;
        }
        self.session.scroll_y = self.session.scroll_y.min(max_scroll_y);

        // Render intersecting inline graphics (`<svg>`, `<canvas>`, `data:image/*`) into their
        // reserved line slots without shifting any downstream line indices or control coordinates.
        let scroll_top = self.session.scroll_y as usize;
        let scroll_bottom = scroll_top.saturating_add(viewport_h as usize);
        let caps = ui.graphics_caps;
        let proto = caps.protocol;
        let bg_rgb = img_preview::color_to_rgb(theme.bg_elevated, (24, 24, 30));
        let mut pixel_skip_rects: Vec<Rect> = Vec::new();

        for g in &graphics {
            let g_end = g.line_start.saturating_add(g.line_count);
            if g.line_count == 0 || g_end <= scroll_top || g.line_start >= scroll_bottom {
                continue;
            }
            let cache_key = (g.node_id.0, avail_w, proto);
            let needs_decode = self.graphic_cache.get(&cache_key).is_none_or(|entry| {
                !Arc::ptr_eq(&entry.source_bytes, &g.image_bytes)
                    || entry.cell_px != caps.cell_px
                    || entry.bg_rgb != bg_rgb
            });
            if needs_decode {
                let fit = FitBox::contained(avail_w.max(1), (g.line_count as u16).max(1));
                let decoded = if proto == TerminalImageProtocol::Halfblock {
                    img_preview::decode_and_render_halfblocks_with_cell_px(
                        &g.image_bytes,
                        fit,
                        bg_rgb,
                        caps.cell_px,
                    )
                } else {
                    img_preview::decode_and_render_for_protocol(&g.image_bytes, fit, bg_rgb, caps)
                };
                if let Ok(rendered_img) = decoded {
                    self.graphic_cache.insert(
                        cache_key,
                        CachedHtmlGraphic {
                            source_bytes: Arc::clone(&g.image_bytes),
                            cell_px: caps.cell_px,
                            bg_rgb,
                            rendered: rendered_img,
                        },
                    );
                } else {
                    continue;
                }
            }

            let Some(cached) = self.graphic_cache.get(&cache_key) else {
                continue;
            };
            let rendered_img = &cached.rendered;
            let slot_end = g_end.min(lines_to_render.len());
            let slot_len = slot_end.saturating_sub(g.line_start);

            if !proto.is_pixel() || rendered_img.pixel_payload.is_none() {
                for offset in 0..slot_len {
                    lines_to_render[g.line_start + offset] = rendered_img
                        .lines
                        .get(offset)
                        .cloned()
                        .unwrap_or_default();
                }
            } else {
                let vis_start = g.line_start.max(scroll_top);
                let vis_end = g_end.min(scroll_bottom);
                let vis_rows = vis_end.saturating_sub(vis_start) as u16;
                let crop_y = vis_start.saturating_sub(g.line_start) as u16;
                if vis_rows > 0 {
                    if let Some((payload, draw_cols, draw_rows)) =
                        rendered_img.viewport_pixel_payload(0, crop_y, avail_w, vis_rows)
                    {
                        let fit_cols = draw_cols.min(avail_w);
                        let fit_rows = draw_rows.min(vis_rows);
                        if fit_cols > 0 && fit_rows > 0 {
                            for offset in 0..slot_len {
                                lines_to_render[g.line_start + offset] = Line::default();
                            }
                            let screen_y = self.content_rect.y + (vis_start - scroll_top) as u16;
                            let draw_rect =
                                Rect::new(self.content_rect.x, screen_y, fit_cols, fit_rows);
                            let pending_key = format!(
                                "html:n{}:w{}:{}:r{}x{}:c0,{},{},{}:p{:x}",
                                g.node_id.0,
                                avail_w,
                                proto.as_str(),
                                fit_cols,
                                fit_rows,
                                crop_y,
                                avail_w,
                                vis_rows,
                                Arc::as_ptr(&g.image_bytes) as *const () as usize,
                            );
                            ui.pending_pixel_graphics.push(PendingPixelGraphic {
                                cache_key: pending_key,
                                rect: draw_rect,
                                protocol: proto,
                                preencoded_payload: Some(payload),
                            });
                            pixel_skip_rects.push(draw_rect);
                        }
                    }
                }
            }
        }

        frame.render_widget(
            Paragraph::new(lines_to_render)
                .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
                .scroll((self.session.scroll_y, 0)),
            self.content_rect,
        );

        if !pixel_skip_rects.is_empty() {
            let buf = frame.buffer_mut();
            let clip_right = self.content_rect.right().min(buf.area.right());
            let clip_bottom = self.content_rect.bottom().min(buf.area.bottom());
            for draw_rect in pixel_skip_rects {
                let y_end = draw_rect.bottom().min(clip_bottom);
                let x_end = draw_rect.right().min(clip_right);
                for py in draw_rect.y..y_end {
                    for px in draw_rect.x..x_end {
                        if let Some(cell) = buf.cell_mut((px, py)) {
                            cell.set_skip(true);
                        }
                    }
                }
            }
        }

        if show_scrollbar {
            let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, viewport_h);
            let mut sb_state = ScrollbarState::new(max_scroll_y.saturating_add(1) as usize)
                .position(self.session.scroll_y as usize);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(theme.accent))
                .track_style(theme.muted_style());
            frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
        }

        if show_footer {
            let footer_y = inner.y + viewport_h;
            let footer_rect = Rect::new(inner.x, footer_y, inner.width, 1);
            let footer_text = if let Some(ref status) = self.session.last_status {
                format!(" {status} · Tab Focus · Enter/Space Activate · m Raw · o Browser · Esc Close")
            } else if self.session.editing_input.is_some() {
                " Editing input — type text · Backspace delete · Enter/Esc finish ".to_string()
            } else if let Some(lbl) = focused_label {
                let focus_num = self.session.focused_control.map(|i| i + 1).unwrap_or(0);
                format!(
                    " Control {focus_num}/{control_count}: {lbl} · Enter/Space Activate · Tab Next · m Raw · o Browser · Esc Close"
                )
            } else if self.session.raw_mode {
                let raw_label = if is_markdown {
                    "Raw Markdown"
                } else {
                    "Raw HTML"
                };
                format!(
                    " {raw_label} ({total_lines} lines) · m/r Rendered · o Browser · j/k/Wheel Scroll · Esc Close"
                )
            } else {
                format!(
                    " {control_count} controls · Tab Focus · Click/Enter Activate · m Raw · o Browser · j/k Scroll · Esc Close"
                )
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&footer_text, inner.width as usize),
                    theme.muted_style().bg(theme.bg_header),
                ))
                .style(Style::default().bg(theme.bg_header)),
                footer_rect,
            );
        }
    }
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::config::AppConfig;

    fn render_overlay(
        overlay: &mut HtmlPreviewOverlay,
        ui: &mut UiState,
        width: u16,
        height: u16,
    ) -> (HitMap, String) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hitmap = HitMap::new();
        ui.pending_pixel_graphics.clear();
        terminal
            .draw(|frame| {
                let area = frame.area();
                overlay.draw(frame, area, ui, &mut hitmap);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        (hitmap, out)
    }

    #[test]
    fn test_html_preview_onclick_button_click_at_and_raw_toggle() {
        let html = r#"
            <!DOCTYPE html>
            <html>
              <head><title>Interactive Note</title></head>
              <body>
                <h1>Counter Demo</h1>
                <p id="status">Clicks: 0</p>
                <button onclick="document.getElementById('status').textContent = 'Clicks: 1'">Increment</button>
              </body>
            </html>
        "#;
        let mut ui = UiState::from_config(&AppConfig::default());
        let mut overlay = HtmlPreviewOverlay::new(None, html.to_string());
        assert!(overlay.session.js_active, "onclick handler should activate on-demand QuickJS");

        let (hitmap, screen0) = render_overlay(&mut overlay, &mut ui, 100, 28);
        assert!(screen0.contains("Clicks: 0"));
        assert!(screen0.contains("[ Increment ]"));

        // Header pills & backdrop must be registered
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::HtmlToggleRaw)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::HtmlServeBrowser)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::ConfirmCancel)).is_some());
        assert_eq!(hitmap.hit(0, 0), Some(HitTarget::OverlayBackdrop));

        // Locate button control coordinate and click via handle_click (which dispatches click_at)
        let theme = ui.theme.to_html_theme();
        let ctrl = overlay
            .session
            .render(overlay.content_rect.width, &theme, false)
            .controls[0]
            .clone();
        let click_col = overlay.content_rect.x + ctrl.col_start + 1;
        let click_row = overlay.content_rect.y + ctrl.line as u16;
        let click_outcome = overlay.handle_click(click_col, click_row, &ui);
        assert_eq!(click_outcome, OverlayOutcome::Handled);

        let (_, screen1) = render_overlay(&mut overlay, &mut ui, 100, 28);
        assert!(
            screen1.contains("Clicks: 1"),
            "expected onclick DOM mutation to render 'Clicks: 1', got:\n{screen1}"
        );

        // Press 'm' to toggle Rendered -> Raw HTML -> Rendered
        assert_eq!(overlay.handle(&Action::Input('m'), &ui), OverlayOutcome::Handled);
        assert!(overlay.session.raw_mode);
        let (_, raw_screen) = render_overlay(&mut overlay, &mut ui, 100, 28);
        assert!(raw_screen.contains("<button") && raw_screen.contains("RAW HTML"));

        assert_eq!(overlay.handle(&Action::Input('m'), &ui), OverlayOutcome::Handled);
        assert!(!overlay.session.raw_mode);

        // Press 'o' to request native Axum serving in browser
        assert_eq!(
            overlay.handle(&Action::Input('o'), &ui),
            OverlayOutcome::Dispatch(Action::ServeHtmlPreviewInBrowser)
        );
        assert_eq!(
            overlay.handle(&Action::OnenoteOpenWeb, &ui),
            OverlayOutcome::Dispatch(Action::ServeHtmlPreviewInBrowser)
        );

        // Esc / q close modal
        assert_eq!(overlay.handle(&Action::Input('q'), &ui), OverlayOutcome::Close);
        assert_eq!(overlay.handle(&Action::CloseOverlay, &ui), OverlayOutcome::Close);
    }

    #[test]
    fn test_html_preview_static_zero_js_tab_focus_input_and_scrollbar() {
        let mut long_body = String::from(
            r#"<form action="/save"><input type="text" name="note" value="v1" /><button type="submit">Save</button></form>"#,
        );
        for i in 0..40 {
            long_body.push_str(&format!("<p>Line {i} of static HTML content.</p>"));
        }
        let mut ui = UiState::from_config(&AppConfig::default());
        let mut overlay = HtmlPreviewOverlay::new(Some("Static Doc".into()), long_body);
        assert!(!overlay.session.js_active, "static HTML must keep QuickJS at 0 MB idle");

        let _ = render_overlay(&mut overlay, &mut ui, 90, 20);
        assert!(overlay.total_lines > overlay.content_rect.height as usize);

        // Tab focuses text input, Space/Enter enters editing mode, typing updates value
        overlay.handle(&Action::Tab, &ui);
        assert_eq!(overlay.session.focused_control, Some(0));
        overlay.handle(&Action::Activate, &ui);
        assert!(overlay.accepts_text());
        overlay.handle(&Action::Input('!'), &ui);
        overlay.handle(&Action::Backspace, &ui);
        overlay.handle(&Action::Input('2'), &ui);
        // Enter finishes editing
        overlay.handle(&Action::Activate, &ui);
        assert!(!overlay.accepts_text());

        // Tab to submit button & activate with Space (or OnenoteToggleTodo from global keymap)
        overlay.handle(&Action::Tab, &ui);
        assert_eq!(overlay.session.focused_control, Some(1));
        let submit = overlay.handle(&Action::OnenoteToggleTodo(None), &ui);
        assert!(matches!(submit, OverlayOutcome::Dispatch(Action::ShowToast(ref s)) if s.contains("note=v12")));

        // Scroll down/bottom/up
        overlay.handle(&Action::Input('j'), &ui);
        assert_eq!(overlay.session.scroll_y, 2);
        overlay.handle(&Action::ScrollBottom, &ui);
        let _ = render_overlay(&mut overlay, &mut ui, 90, 20);
        assert!(overlay.session.scroll_y > 0);
        overlay.handle(&Action::MouseScroll { delta: -3, column: 20, row: 10 }, &ui);
        overlay.handle(&Action::ScrollTop, &ui);
        assert_eq!(overlay.session.scroll_y, 0);
    }

    #[test]
    fn test_html_preview_inline_svg_renders_halfblock_and_pixel_preserving_button_click() {
        let html = r##"
            <!DOCTYPE html>
            <html>
              <head><title>SVG + Button</title></head>
              <body>
                <p>Header before graphic</p>
                <svg width="80" height="40">
                  <rect width="80" height="40" fill="#e11d48"/>
                </svg>
                <p id="out">Init</p>
                <button onclick="document.getElementById('out').textContent = 'Clicked!'">Run Action</button>
              </body>
            </html>
        "##;

        // 1. Halfblock mode: reserved slot renders `▀` and button below hits exact click_at coordinates
        let mut ui = UiState::from_config(&AppConfig::default());
        ui.graphics_caps = img_preview::TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Halfblock,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let mut overlay = HtmlPreviewOverlay::new(None, html.to_string());
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        let mut hitmap = HitMap::new();
        terminal
            .draw(|frame| {
                ui.pending_pixel_graphics.clear();
                overlay.draw(frame, frame.area(), &mut ui, &mut hitmap);
            })
            .unwrap();

        let html_theme = ui.theme.to_html_theme();
        let (graphic_slot, btn_ctrl) = {
            let r = overlay
                .session
                .render(overlay.content_rect.width, &html_theme, false);
            assert_eq!(r.graphics.len(), 1, "expected 1 inline SVG graphic");
            assert_eq!(r.controls.len(), 1, "expected 1 button control below SVG");
            let g = &r.graphics[0];
            ((g.line_start, g.line_count), r.controls[0].clone())
        };
        assert!(
            btn_ctrl.line >= graphic_slot.0 + graphic_slot.1,
            "button should sit below the graphic slot"
        );

        let buf = terminal.backend().buffer();
        let slot_has_halfblock = (graphic_slot.0..graphic_slot.0 + graphic_slot.1).any(|line_idx| {
            let y = overlay.content_rect.y + line_idx as u16;
            (overlay.content_rect.x..overlay.content_rect.right())
                .any(|x| buf[(x, y)].symbol() == "▀")
        });
        assert!(
            slot_has_halfblock,
            "expected halfblock '▀' inside reserved graphic slot {:?}",
            graphic_slot
        );
        assert!(ui.pending_pixel_graphics.is_empty());

        // Clicking button below the graphic hits exact coordinates and mutates DOM
        let click_col = overlay.content_rect.x + btn_ctrl.col_start + 1;
        let click_row = overlay.content_rect.y + btn_ctrl.line as u16;
        assert_eq!(
            overlay.handle_click(click_col, click_row, &ui),
            OverlayOutcome::Handled
        );
        let (_, screen_after) = render_overlay(&mut overlay, &mut ui, 100, 32);
        assert!(
            screen_after.contains("Clicked!"),
            "expected button click below SVG to mutate DOM to 'Clicked!', got:\n{screen_after}"
        );

        // 2. Pixel protocol (Kitty): queues pending_pixel_graphics and marks covered cells skip=true
        ui.graphics_caps = img_preview::TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let mut covered_cell_skipped = false;
        terminal
            .draw(|frame| {
                ui.pending_pixel_graphics.clear();
                hitmap.clear();
                overlay.draw(frame, frame.area(), &mut ui, &mut hitmap);
                if let Some(pg) = ui.pending_pixel_graphics.first() {
                    covered_cell_skipped = frame.buffer_mut()[(pg.rect.x, pg.rect.y)].skip;
                }
            })
            .unwrap();
        assert_eq!(
            ui.pending_pixel_graphics.len(),
            1,
            "expected 1 pending pixel graphic for Kitty"
        );
        let pg = &ui.pending_pixel_graphics[0];
        assert!(pg.cache_key.starts_with("html:"));
        assert!(pg
            .preencoded_payload
            .as_deref()
            .is_some_and(|p| p.contains("\x1b_G")));
        assert!(
            covered_cell_skipped,
            "expected covered pixel graphic cell to be marked skip=true in frame buffer"
        );
    }
}
