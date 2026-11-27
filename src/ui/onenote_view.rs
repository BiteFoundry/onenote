//! Native Microsoft OneNote TUI Renderer (`src/ui/onenote_view.rs`):
//! - Row 0: Colored Horizontal Section Ribbon Bar (`[ ▌Section1 ] [ ▌Section2 ] ... [+Section S]`)
//! - Row 1: Clickable OneNote Action Toolbar (`HitMap`)
//! - Split Body:
//!   - Wide (`inner.width >= 76`): Left Page List sub-column (`26..=32` cols) with subpage indentation
//!     (`level 0` / `level 1` / `level >= 2`) + Right Rich Interactive Note Canvas
//!   - Narrow (`inner.width < 76`): Compact active-page selector bar + Rich Interactive Note Canvas
//! - Rich Block Rendering:
//!   - Interactive `☐`/`☑` Todo checkboxes (`HitTarget::OnenoteTodoToggle`) & colored tags (`★`, `❓`, `❗`, Highlight)
//!   - Headings (`h1`..`h6`) with subtle underline on `h1`/`h2`
//!   - Nested ordered/unordered lists & quotes
//!   - Crisp Unicode box-drawn tables (`┌──┬──┐` / `│  │  │` / `├──┼──┤` / `└──┴──┘`)
//!   - Bordered code blocks with language badge
//!   - Inline image chips + half-block terminal thumbnails (`img_preview`) & `<object>` attachment chips

use std::collections::HashMap;

use bitefoundry_html::layout::{
    render_wrapped_table, wrap_styled_spans, TableBorder, TableSpec, WrapPrefix,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::RenderedTerminalImage,
    models::{
        OnenoteBlock, OnenoteBlockKind, OnenotePageDetail, OnenotePageSummary, OnenoteSection,
        OnenoteTagKind, RichSpan,
    },
    ui::{
        app::{AppState, Focus, UiState},
        
        hitmap::{ButtonId, HitMap, HitTarget},
        nav_pane::{onenote_badge_color, selection_offset, ONENOTE_BRAND_BG, ONENOTE_PURPLE},
        theme::Theme,
    },
};

const SPLIT_MIN_WIDTH: u16 = 76;

/// A rendered canvas line with optional whole-line and inline clickable `HitTarget`s.
#[derive(Debug, Clone)]
pub struct RenderedLine {
    pub line: Line<'static>,
    pub hit: Option<HitTarget>,
    pub inline_hits: Vec<(u16, u16, HitTarget)>,
}

impl RenderedLine {
    pub fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            hit: None,
            inline_hits: Vec::new(),
        }
    }

    pub fn clickable(line: Line<'static>, hit: HitTarget) -> Self {
        Self {
            line,
            hit: Some(hit),
            inline_hits: Vec::new(),
        }
    }

    pub fn with_inline(
        line: Line<'static>,
        hit: Option<HitTarget>,
        inline_hits: Vec<(u16, u16, HitTarget)>,
    ) -> Self {
        Self {
            line,
            hit,
            inline_hits,
        }
    }
}

pub fn render_scrolled_lines(
    frame: &mut Frame,
    area: Rect,
    lines: &[RenderedLine],
    offset: usize,
    height: usize,
    hitmap: &mut HitMap,
) {
    for (row_idx, rendered) in lines.iter().skip(offset).take(height).enumerate() {
        let y = area.y + row_idx as u16;
        let row_rect = Rect::new(area.x, y, area.width, 1);
        frame.render_widget(Paragraph::new(rendered.line.clone()), row_rect);
        if let Some(hit) = rendered.hit {
            hitmap.add_line(area.x, y, area.width, hit);
        }
        for &(col_off, width, target) in &rendered.inline_hits {
            let x = area.x.saturating_add(col_off);
            if x < area.x + area.width {
                let w = width.min((area.x + area.width).saturating_sub(x));
                hitmap.add_line(x, y, w, target);
            }
        }
    }
}

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) -> Option<(u16, u16)> {
    let theme = ui.theme.clone();
    let focused = matches!(ui.focus, Focus::Canvas | Focus::Pages);

    let nb_name = app
        .active_onenote_notebook()
        .map(|nb| nb.display_name.as_str())
        .unwrap_or("Microsoft OneNote");
    let sec_name = app
        .active_onenote_section()
        .map(|s| s.display_name.as_str())
        .unwrap_or("No section");
    let pane_title = format!(" 📓 {nb_name} › {sec_name} ");

    let title_style = if focused {
        Style::default()
            .fg(ONENOTE_PURPLE)
            .add_modifier(Modifier::BOLD)
    } else {
        theme.title(false)
    };
    let border_style = if focused {
        Style::default().fg(ONENOTE_PURPLE)
    } else {
        theme.border_style(false)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(Span::styled(
            crate::ui::truncate_width(&pane_title, area.width.saturating_sub(2) as usize),
            title_style,
        )))
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    // Row 0: Colored Horizontal Section Ribbon Bar
    let ribbon_area = Rect::new(inner.x, inner.y, inner.width, 1);
    let sections = app.sections_for_active_notebook();
    draw_section_ribbon(
        frame,
        ribbon_area,
        &sections,
        app.active_section_key.as_deref(),
        &theme,
        hitmap,
    );
    if inner.height == 1 {
        return None;
    }

    // Row 1: Clickable Action Toolbar
    let toolbar_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    draw_action_toolbar(frame, toolbar_area, &theme, hitmap);
    if inner.height <= 2 {
        return None;
    }

    // Split Body (Row 2..)
    let body_area = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(2),
    );

    if body_area.width >= SPLIT_MIN_WIDTH {
        let page_col_w = ((body_area.width as usize * 30) / 100).clamp(26, 32) as u16;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(page_col_w), Constraint::Min(30)])
            .split(body_area);

        draw_page_list_column(frame, cols[0], app, ui, &theme, hitmap);
        draw_note_canvas(frame, cols[1], app, ui, &theme, hitmap)
    } else {
        // Narrow mode (< 76 cols): compact active-page selector bar + full-width Note Canvas
        if body_area.height >= 3 {
            let selector_rect = Rect::new(body_area.x, body_area.y, body_area.width, 1);
            let canvas_rect = Rect::new(
                body_area.x,
                body_area.y + 1,
                body_area.width,
                body_area.height - 1,
            );
            draw_compact_page_bar(frame, selector_rect, app, &theme, hitmap);
            draw_note_canvas(frame, canvas_rect, app, ui, &theme, hitmap)
        } else {
            draw_note_canvas(frame, body_area, app, ui, &theme, hitmap)
        }
    }
}

/// Renders Row 0: `[ ▌Section1 ] [ ▌Section2 ] ... [+Section S]`.
pub fn draw_section_ribbon(
    frame: &mut Frame,
    area: Rect,
    sections: &[&OnenoteSection],
    active_section_key: Option<&str>,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    if area.width == 0 {
        return;
    }

    let add_label = "[+Section S]";
    let add_w = UnicodeWidthStr::width(add_label) as u16;

    if sections.is_empty() {
        let hint = " No sections yet ";
        let hint_w = UnicodeWidthStr::width(hint) as u16;
        let mut spans = vec![Span::styled(hint, theme.muted_style())];
        if area.width > hint_w + add_w {
            spans.push(Span::styled(
                add_label,
                Style::default()
                    .fg(ONENOTE_PURPLE)
                    .add_modifier(Modifier::BOLD),
            ));
            hitmap.add_line(
                area.x + hint_w,
                area.y,
                add_w,
                HitTarget::Button(ButtonId::OnenoteNewSection),
            );
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let active_idx = active_section_key
        .and_then(|k| sections.iter().position(|s| s.section_key == k))
        .unwrap_or(0);

    let labels: Vec<String> = sections.iter().map(|s| ribbon_tab_label(s)).collect();
    // Rendered tab: "[ " (2) + "▌" (1) + label (W) + " ]" (2) = W + 5; stride with " " = W + 6
    let strides: Vec<usize> = labels
        .iter()
        .map(|l| UnicodeWidthStr::width(l.as_str()) + 6)
        .collect();
    let (start_idx, end_idx) =
        ribbon_window(&strides, active_idx, area.width as usize, add_w as usize);
    let hidden_after = sections.len() - end_idx;

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut x = area.x;

    // Overflow cues: `‹` / `›` act as the nearest hidden tab, so a click or
    // wheel on them moves to it and scrolls the ribbon.
    if start_idx > 0 {
        spans.push(Span::styled(
            format!("{RIBBON_CUE_LEFT} "),
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        ));
        hitmap.add_line(x, area.y, 1, HitTarget::OnenoteSectionTab(start_idx - 1));
        x = x.saturating_add(RIBBON_CUE_W);
    }

    for (idx, sec) in sections.iter().enumerate().take(end_idx).skip(start_idx) {
        let is_active = active_section_key == Some(sec.section_key.as_str());
        let short_name = labels[idx].clone();
        let tab_w = (strides[idx] - 1) as u16;
        if x.saturating_sub(area.x) + tab_w > area.width {
            break;
        }

        let badge_color = onenote_badge_color(sec.color.as_deref(), idx);
        if is_active {
            let active_bg = ONENOTE_BRAND_BG;
            spans.push(Span::styled(
                "[ ",
                Style::default()
                    .fg(Color::White)
                    .bg(active_bg)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                "▌",
                Style::default()
                    .fg(badge_color)
                    .bg(active_bg)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!("{short_name} ]"),
                Style::default()
                    .fg(Color::White)
                    .bg(active_bg)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled("[ ", theme.muted_style()));
            spans.push(Span::styled("▌", Style::default().fg(badge_color)));
            spans.push(Span::styled(
                short_name,
                Style::default().fg(theme.fg),
            ));
            spans.push(Span::styled(" ]", theme.muted_style()));
        }
        spans.push(Span::raw(" "));
        hitmap.add_line(x, area.y, tab_w, HitTarget::OnenoteSectionTab(idx));
        x = x.saturating_add(tab_w + 1);
    }

    if hidden_after > 0 && x.saturating_sub(area.x) + RIBBON_CUE_W <= area.width {
        spans.push(Span::styled(
            format!("{RIBBON_CUE_RIGHT} "),
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        ));
        hitmap.add_line(x, area.y, 1, HitTarget::OnenoteSectionTab(end_idx));
        x = x.saturating_add(RIBBON_CUE_W);
    }

    if x.saturating_sub(area.x) + add_w <= area.width {
        spans.push(Span::styled(
            add_label,
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        ));
        hitmap.add_line(
            x,
            area.y,
            add_w,
            HitTarget::Button(ButtonId::OnenoteNewSection),
        );
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

const RIBBON_CUE_LEFT: char = '‹';
const RIBBON_CUE_RIGHT: char = '›';
/// Columns an overflow cue takes: the glyph and a 1-col gap.
const RIBBON_CUE_W: u16 = 2;

/// Ribbon tab text: `Group / Section` for a section inside a section group,
/// each part shortened so one tab stays compact.
fn ribbon_tab_label(sec: &OnenoteSection) -> String {
    let name = truncate_clean(&sec.display_name, 16);
    match sec.section_group_name() {
        Some(group) => format!("{} / {name}", truncate_clean(group, 10)),
        None => name,
    }
}

/// The `[start, end)` range of tabs (given their strides) the ribbon shows in
/// `width` columns next to the `add_w`-wide add button. All tabs when they
/// fit; otherwise room for both overflow cues is reserved and the window ends
/// at the active tab when it lies past the first screenful, then extends right.
fn ribbon_window(
    strides: &[usize],
    active_idx: usize,
    width: usize,
    add_w: usize,
) -> (usize, usize) {
    let total: usize = strides.iter().sum();
    if total + add_w <= width || strides.is_empty() {
        return (0, strides.len());
    }
    let budget = width.saturating_sub(add_w + 2 * RIBBON_CUE_W as usize);
    let active_idx = active_idx.min(strides.len() - 1);
    let mut start = active_idx;
    let mut used = strides[active_idx];
    while start > 0 && used + strides[start - 1] <= budget {
        start -= 1;
        used += strides[start];
    }
    let mut end = active_idx + 1;
    while end < strides.len() && used + strides[end] <= budget {
        used += strides[end];
        end += 1;
    }
    (start, end)
}

/// Renders Row 1: Action Toolbar.
fn draw_action_toolbar(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    let buttons: [(&str, ButtonId); 11] = [
        ("[+Pg n]", ButtonId::OnenoteNewPage),
        ("[Add a]", ButtonId::OnenoteAppend),
        ("[Edit e]", ButtonId::OnenoteEditBlock),
        ("[Todo x]", ButtonId::OnenoteToggleTodo),
        ("[Url u]", ButtonId::OnenoteLinks),
        ("[Href h]", ButtonId::OnenoteToggleHrefs),
        ("[Img v]", ButtonId::ImagePreview),
        ("[H: HTML]", ButtonId::HtmlPreview),
        ("[Att A]", ButtonId::Attachments),
        ("[Web o]", ButtonId::OnenoteOpenWeb),
        ("[Del D]", ButtonId::OnenoteDeletePage),
    ];

    let total_raw_w: u16 = buttons
        .iter()
        .map(|(lbl, _)| UnicodeWidthStr::width(*lbl) as u16)
        .sum();
    let gap: u16 = if total_raw_w + (buttons.len().saturating_sub(1) as u16) <= area.width {
        1
    } else {
        0
    };

    let mut spans = Vec::new();
    let mut x = area.x;
    let btn_style = Style::default()
        .fg(ONENOTE_PURPLE)
        .add_modifier(Modifier::BOLD);

    for (idx, &(label, btn_id)) in buttons.iter().enumerate() {
        let w = UnicodeWidthStr::width(label) as u16;
        if x.saturating_sub(area.x) + w > area.width {
            break;
        }
        let style = if btn_id == ButtonId::OnenoteDeletePage {
            Style::default().fg(theme.error)
        } else if btn_id == ButtonId::HtmlPreview {
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD)
        } else {
            btn_style
        };
        spans.push(Span::styled(label, style));
        hitmap.add_line(x, area.y, w, HitTarget::Button(btn_id));
        x = x.saturating_add(w);
        if gap > 0 && idx + 1 < buttons.len() && x.saturating_sub(area.x) < area.width {
            spans.push(Span::raw(" "));
            x = x.saturating_add(gap);
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Formats a page row label with authentic OneNote subpage indentation (`level 0`, `1`, `2+`).
pub fn format_page_row_label(page: &OnenotePageSummary) -> (&'static str, String) {
    let title = if page.title.trim().is_empty() {
        "(Untitled page)".to_string()
    } else {
        page.title.trim().to_string()
    };
    match page.level {
        lvl if lvl <= 0 => {
            let prefix = if page.has_todo { "☐ " } else { "• " };
            (prefix, title)
        }
        1 => {
            let prefix = if page.has_todo { "  └─ ☐ " } else { "  └─ " };
            (prefix, title)
        }
        _ => {
            let prefix = if page.has_todo {
                "    └─ ☐ "
            } else {
                "    └─ "
            };
            (prefix, title)
        }
    }
}

fn draw_page_list_column(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    _ui: &UiState,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    let title = format!(" Pages ({}) ", app.pages.len());
    let block = Block::default()
        .borders(Borders::RIGHT | Borders::TOP)
        .title(Line::from(Span::styled(
            crate::ui::truncate_width(&title, area.width.saturating_sub(1) as usize),
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        )))
        .border_style(theme.border_style(false));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    hitmap.add(area, HitTarget::PanePages);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if app.pages.is_empty() {
        let msg = if app.loading_onenote_pages {
            "Loading pages..."
        } else {
            "No pages (press n)"
        };
        let wrapped = wrap_styled_spans(
            &[Span::styled(msg, theme.muted_style())],
            inner.width as usize,
            WrapPrefix::empty(),
        );
        frame.render_widget(Paragraph::new(wrapped), inner);
        return;
    }

    let active_idx = app
        .active_page_key
        .as_deref()
        .and_then(|k| app.pages.iter().position(|p| p.page_key == k))
        .unwrap_or(0);
    let height = inner.height as usize;
    let offset = selection_offset(active_idx, height, app.pages.len());
    let width = inner.width as usize;

    let mut lines = Vec::with_capacity(height);
    for (idx, page) in app
        .pages
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
    {
        let is_active = app.active_page_key.as_deref() == Some(page.page_key.as_str());
        let (prefix, clean_title) = format_page_row_label(page);
        let avail_w = width.saturating_sub(UnicodeWidthStr::width(prefix)).max(3);
        let body = truncate_clean(&clean_title, avail_w);
        let pad = width
            .saturating_sub(UnicodeWidthStr::width(prefix) + UnicodeWidthStr::width(body.as_str()));

        let row_style = if is_active {
            Style::default()
                .fg(Color::White)
                .bg(ONENOTE_BRAND_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        let prefix_style = if is_active {
            row_style
        } else if page.has_todo {
            Style::default().fg(ONENOTE_PURPLE)
        } else {
            theme.muted_style()
        };

        lines.push(Line::from(vec![
            Span::styled(prefix, prefix_style),
            Span::styled(body, row_style),
            Span::styled(" ".repeat(pad), row_style),
        ]));
        hitmap.add_line(
            inner.x,
            inner.y + (lines.len() as u16 - 1),
            inner.width,
            HitTarget::OnenotePageItem(idx),
        );
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_compact_page_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    theme: &Theme,
    hitmap: &mut HitMap,
) {
    if app.pages.is_empty() {
        frame.render_widget(
            Paragraph::new(" No pages (n: new page)").style(theme.muted_style()),
            area,
        );
        return;
    }

    let total = app.pages.len();
    let idx = app
        .active_page_key
        .as_deref()
        .and_then(|k| app.pages.iter().position(|p| p.page_key == k))
        .unwrap_or(0);
    let prev_idx = (idx + total - 1) % total;
    let next_idx = (idx + 1) % total;

    let prev_btn = "[◀ K]";
    let next_btn = "[J ▶]";
    let prev_w = UnicodeWidthStr::width(prev_btn) as u16;
    let next_w = UnicodeWidthStr::width(next_btn) as u16;
    let mid_budget = area
        .width
        .saturating_sub(prev_w + next_w + 2)
        .max(6) as usize;
    let page_title = app
        .pages
        .get(idx)
        .map(|p| p.title.as_str())
        .unwrap_or("(Untitled)");
    let mid_label = format!(" 📄 {}/{}: {} ", idx + 1, total, page_title);
    let mid_trunc = crate::ui::truncate_width(&mid_label, mid_budget);

    let spans = vec![
        Span::styled(
            prev_btn,
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(mid_trunc.clone(), Style::default().fg(theme.fg)),
        Span::styled(
            next_btn,
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    hitmap.add_line(area.x, area.y, prev_w, HitTarget::OnenotePageItem(prev_idx));
    let next_x = area.x
        + prev_w
        + UnicodeWidthStr::width(mid_trunc.as_str()) as u16;
    if next_x < area.x + area.width {
        hitmap.add_line(
            next_x,
            area.y,
            next_w.min(area.x + area.width - next_x),
            HitTarget::OnenotePageItem(next_idx),
        );
    }
}

fn draw_note_canvas(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    ui: &mut UiState,
    theme: &Theme,
    hitmap: &mut HitMap,
) -> Option<(u16, u16)> {
    hitmap.add(area, HitTarget::PaneCanvas);
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    if let Some(ref mut editor) = ui.inline_editor {
        if app.active_page_key.as_deref() == Some(editor.page_key.as_str()) {
            return editor.draw(frame, inner, theme, hitmap);
        }
    }

    let Some(detail) = app.active_page_detail().cloned() else {
        let msg = if app.loading_onenote_detail {
            "Loading OneNote page content..."
        } else if app.notebooks.is_empty() && app.syncing {
            "Syncing OneNote notebooks and sections..."
        } else if app.notebooks.is_empty() {
            "Select a notebook in the left sidebar or press B to create one."
        } else if app.pages.is_empty() && app.loading_onenote_pages {
            "Loading pages..."
        } else if app.pages.is_empty() {
            "No pages in this section yet (press n to create a page)."
        } else {
            "Select a page (j/k or click) or press r to refresh."
        };
        let wrapped = wrap_styled_spans(
            &[Span::styled(msg, theme.muted_style())],
            inner.width as usize,
            WrapPrefix::empty(),
        );
        frame.render_widget(Paragraph::new(wrapped), inner);
        return None;
    };

    if !detail.blocks.is_empty() && ui.onenote_selected_block >= detail.blocks.len() {
        ui.onenote_selected_block = detail.blocks.len() - 1;
    }

    // Lazily fetch/decode inline terminal images when inline_images is enabled
    let width = inner.width as usize;
    let thumb_cols = width.saturating_sub(4).clamp(24, 72) as u16;
    let thumb_rows = 16u16;
    let inline_bg = ui.inline_image_bg_rgb();
    let (cw, ch) = ui.graphics_caps.cell_px;
    let mut cached_thumbs: HashMap<usize, RenderedTerminalImage> = HashMap::new();

    if ui.inline_images {
        for block in &detail.blocks {
            if let OnenoteBlockKind::Image { resource_index, .. } = &block.kind {
                if let Some(res) = detail.resources.get(*resource_index) {
                    let key = format!(
                        "thumb:{}:{}:{}x{}:{}x{}:{:02x}{:02x}{:02x}",
                        detail.summary.page_key,
                        resource_index,
                        thumb_cols,
                        thumb_rows,
                        cw,
                        ch,
                        inline_bg.0,
                        inline_bg.1,
                        inline_bg.2
                    );
                    if let Some(img) = render_terminal_image(&app.image_cache, &key) {
                        cached_thumbs.insert(*resource_index, img);
                    } else {
                        let url = res
                            .fullres_url
                            .as_deref()
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or(&res.resource_url)
                            .to_string();
                        app.request_onenote_image(
                            key,
                            url,
                            crate::img_preview::FitBox::contained(thumb_cols, thumb_rows),
                            inline_bg,
                            ui.graphics_caps,
                            true,
                        );
                    }
                }
            }
        }
    }

    let (lines, block_line_ranges) = build_page_canvas_lines_with_opts(
        &detail,
        Some(ui.onenote_selected_block),
        &cached_thumbs,
        theme,
        width,
        ui.show_inline_hrefs,
    );

    let height = inner.height as usize;
    let total_lines = lines.len();

    // Auto-scroll to keep the selected block visible when navigated by keyboard
    if ui.onenote_follow_selection {
        if let Some(&(start_line, end_line)) = block_line_ranges.get(&ui.onenote_selected_block) {
            ensure_range_visible(&mut ui.onenote_scroll, total_lines, start_line, end_line, height);
        }
        ui.onenote_follow_selection = false;
    }

    let max_off = total_lines.saturating_sub(height);
    let offset = ui.onenote_scroll.min(max_off);
    ui.onenote_scroll = offset;

    render_scrolled_lines(frame, inner, &lines, offset, height, hitmap);
    None
}

/// Helper wrapping terminal image lookup from `app.image_cache`.
pub fn render_terminal_image(
    cache: &HashMap<String, RenderedTerminalImage>,
    cache_key: &str,
) -> Option<RenderedTerminalImage> {
    cache.get(cache_key).cloned()
}

fn ensure_range_visible(
    scroll: &mut usize,
    total_lines: usize,
    start_line: usize,
    end_line: usize,
    viewport_h: usize,
) {
    if viewport_h == 0 || total_lines <= viewport_h {
        *scroll = 0;
        return;
    }
    let max_off = total_lines - viewport_h;
    let mut cur_off = (*scroll).min(max_off);
    if start_line < cur_off {
        cur_off = start_line;
    } else if end_line > cur_off + viewport_h {
        cur_off = end_line.saturating_sub(viewport_h).min(start_line);
    }
    *scroll = cur_off.min(max_off);
}

/// Pure builder returning `(lines, block_index -> (start_line, end_line))` for rendering & unit testing.
pub fn build_page_canvas_lines(
    detail: &OnenotePageDetail,
    selected_block: Option<usize>,
    cached_thumbs: &HashMap<usize, RenderedTerminalImage>,
    theme: &Theme,
    width: usize,
) -> (Vec<RenderedLine>, HashMap<usize, (usize, usize)>) {
    build_page_canvas_lines_with_opts(detail, selected_block, cached_thumbs, theme, width, false)
}

pub fn build_page_canvas_lines_with_opts(
    detail: &OnenotePageDetail,
    selected_block: Option<usize>,
    cached_thumbs: &HashMap<usize, RenderedTerminalImage>,
    theme: &Theme,
    width: usize,
    show_inline_hrefs: bool,
) -> (Vec<RenderedLine>, HashMap<usize, (usize, usize)>) {
    let width = width.max(20);
    let mut lines: Vec<RenderedLine> = Vec::new();
    let mut ranges: HashMap<usize, (usize, usize)> = HashMap::new();

    // Page Title Header with inline clickable `[H: HTML]` pill
    let page_title = if detail.summary.title.trim().is_empty() {
        "(Untitled page)"
    } else {
        detail.summary.title.trim()
    };
    let html_pill = "[H: HTML]";
    let html_pill_w = UnicodeWidthStr::width(html_pill);
    let title_budget = width.saturating_sub(2 + html_pill_w + 2).max(8);
    let shown_title = truncate_clean(page_title, title_budget);
    let pill_col = (2 + UnicodeWidthStr::width(shown_title.as_str()) + 2) as u16;
    let mut title_spans = vec![
        Span::styled(
            "# ",
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            shown_title,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let mut title_hits = Vec::new();
    if (pill_col as usize) + html_pill_w <= width {
        title_spans.push(Span::raw("  "));
        title_spans.push(Span::styled(
            html_pill,
            Style::default()
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD),
        ));
        title_hits.push((
            pill_col,
            html_pill_w as u16,
            HitTarget::Button(ButtonId::HtmlPreview),
        ));
    }
    lines.push(RenderedLine::with_inline(
        Line::from(title_spans),
        None,
        title_hits,
    ));

    // Metadata line: `{notebook_name} › {section_name} · Modified {last_modified} · {blocks.len()} blocks`
    let nb = detail
        .summary
        .notebook_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("My Notebooks");
    let sec = detail
        .summary
        .section_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Section");
    let modified = detail
        .summary
        .last_modified_at
        .as_deref()
        .map(short_timestamp)
        .unwrap_or_else(|| "-".to_string());
    let meta = format!(
        "{nb} › {sec} · Updated {modified} · {} blocks",
        detail.blocks.len()
    );
    lines.push(RenderedLine::plain(Line::from(Span::styled(
        crate::ui::truncate_width(&meta, width),
        theme.muted_style(),
    ))));
    lines.push(RenderedLine::plain(Line::from(Span::styled(
        "─".repeat(width.min(72)),
        Style::default().fg(ONENOTE_BRAND_BG),
    ))));

    if detail.blocks.is_empty() {
        let fallback_src = if !detail.body_plain.trim().is_empty() {
            detail.body_plain.as_str()
        } else if !detail.body_markdown.trim().is_empty() {
            detail.body_markdown.as_str()
        } else {
            ""
        };
        if !fallback_src.is_empty() {
            let html_theme = theme.to_html_theme();
            let render_width = width.saturating_sub(2).clamp(12, u16::MAX as usize) as u16;
            for mut line in bitefoundry_html::markdown::render_markdown(
                fallback_src,
                render_width,
                &html_theme,
                show_inline_hrefs,
            ) {
                if !line.spans.is_empty() {
                    line.spans.insert(0, Span::raw("  "));
                }
                lines.push(RenderedLine::plain(line));
            }
        } else {
            let indent = [Span::raw("  ")];
            for line in wrap_styled_spans(
                &[Span::styled(
                    "Empty page. Press `a` to append Markdown or `n` to create a new page.",
                    theme.muted_style(),
                )],
                width,
                WrapPrefix {
                    first: &indent,
                    cont: &indent,
                },
            ) {
                lines.push(RenderedLine::plain(line));
            }
        }
        return (lines, ranges);
    }

    for (block_idx, block) in detail.blocks.iter().enumerate() {
        let start = lines.len();
        let is_selected = selected_block == Some(block_idx);
        let expanded_storage;
        let effective_block = if show_inline_hrefs {
            expanded_storage = expand_block_hrefs(block);
            &expanded_storage
        } else {
            block
        };
        render_block_into(
            &mut lines,
            block_idx,
            effective_block,
            is_selected,
            cached_thumbs,
            theme,
            width,
        );
        // Subtle spacing line between major blocks
        lines.push(RenderedLine::plain(Line::from("")));
        let end = lines.len();
        ranges.insert(block_idx, (start, end));
    }

    (lines, ranges)
}

fn expand_block_hrefs(block: &OnenoteBlock) -> OnenoteBlock {
    let kind = match &block.kind {
        OnenoteBlockKind::Heading { level, spans } => OnenoteBlockKind::Heading {
            level: *level,
            spans: expand_inline_href_spans(spans, true),
        },
        OnenoteBlockKind::Paragraph { spans } => OnenoteBlockKind::Paragraph {
            spans: expand_inline_href_spans(spans, true),
        },
        OnenoteBlockKind::ListItem {
            ordered,
            index,
            depth,
            spans,
        } => OnenoteBlockKind::ListItem {
            ordered: *ordered,
            index: *index,
            depth: *depth,
            spans: expand_inline_href_spans(spans, true),
        },
        OnenoteBlockKind::Quote { spans } => OnenoteBlockKind::Quote {
            spans: expand_inline_href_spans(spans, true),
        },
        OnenoteBlockKind::Table { headers, rows } => OnenoteBlockKind::Table {
            headers: headers
                .iter()
                .map(|c| expand_inline_href_spans(c, true))
                .collect(),
            rows: rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|c| expand_inline_href_spans(c, true))
                        .collect()
                })
                .collect(),
        },
        other => other.clone(),
    };
    OnenoteBlock {
        element_id: block.element_id.clone(),
        tag: block.tag.clone(),
        raw_html: block.raw_html.clone(),
        kind,
    }
}

fn tag_badge(tag: Option<&OnenoteTagKind>, theme: &Theme) -> Option<(&'static str, Style, bool)> {
    match tag? {
        OnenoteTagKind::Todo { completed: false } => Some((
            "☐ ",
            Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD),
            true,
        )),
        OnenoteTagKind::Todo { completed: true } => Some((
            "☑ ",
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD),
            true,
        )),
        OnenoteTagKind::Important => Some((
            "★ ",
            Style::default()
                .fg(Color::Rgb(245, 194, 66))
                .add_modifier(Modifier::BOLD),
            false,
        )),
        OnenoteTagKind::Question => Some((
            "❓ ",
            Style::default()
                .fg(Color::Rgb(86, 212, 221))
                .add_modifier(Modifier::BOLD),
            false,
        )),
        OnenoteTagKind::Critical => Some((
            "❗ ",
            Style::default()
                .fg(Color::Rgb(242, 95, 92))
                .add_modifier(Modifier::BOLD),
            false,
        )),
        OnenoteTagKind::Highlight => Some((
            "🖍 ",
            Style::default()
                .fg(Color::Rgb(255, 226, 102))
                .add_modifier(Modifier::BOLD),
            false,
        )),
        OnenoteTagKind::Idea => Some((
            "💡 ",
            Style::default()
                .fg(Color::Rgb(245, 194, 66))
                .add_modifier(Modifier::BOLD),
            false,
        )),
        OnenoteTagKind::Custom(_) => Some(("🏷 ", theme.accent_style(), false)),
    }
}

fn render_block_into(
    out: &mut Vec<RenderedLine>,
    block_idx: usize,
    block: &OnenoteBlock,
    selected: bool,
    cached_thumbs: &HashMap<usize, RenderedTerminalImage>,
    theme: &Theme,
    width: usize,
) {
    let sel_marker = if selected { "▸ " } else { "  " };
    let sel_marker_style = if selected {
        Style::default()
            .fg(ONENOTE_PURPLE)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let row_bg = if matches!(block.tag, Some(OnenoteTagKind::Highlight)) {
        Some(Color::Rgb(72, 60, 16))
    } else if selected {
        Some(theme.selection_bg)
    } else {
        None
    };

    let todo_completed = matches!(block.tag, Some(OnenoteTagKind::Todo { completed: true }));
    let badge_info = tag_badge(block.tag.as_ref(), theme);
    let block_hit = HitTarget::OnenoteBlock(block_idx);

    match &block.kind {
        OnenoteBlockKind::Heading { level, spans } => {
            let heading_color = match *level {
                1 => ONENOTE_PURPLE,
                2 => Color::Rgb(110, 168, 255),
                3 => Color::Rgb(86, 212, 221),
                _ => theme.fg,
            };
            let prefix_hashes = format!("{} ", "#".repeat((*level).clamp(1, 6) as usize));
            let mut base_style = Style::default()
                .fg(heading_color)
                .add_modifier(Modifier::BOLD);
            if let Some(bg) = row_bg {
                base_style = base_style.bg(bg);
            }
            if todo_completed {
                base_style = base_style.add_modifier(Modifier::DIM | Modifier::CROSSED_OUT);
            }

            render_rich_text_lines(
                out,
                block_idx,
                sel_marker,
                sel_marker_style,
                &prefix_hashes,
                base_style,
                None,
                badge_info,
                spans,
                base_style,
                row_bg,
                theme,
                width,
            );

            if *level <= 2 {
                let max_seg_w = spans_plain_text(spans)
                    .split('\n')
                    .map(|s| UnicodeWidthStr::width(s.trim_end_matches('\r')))
                    .max()
                    .unwrap_or(0);
                let text_w = max_seg_w + (*level as usize) + 1;
                let rule_char = if *level == 1 { "═" } else { "─" };
                let max_rule_w = width.saturating_sub(4);
                let rule_len = text_w.min(max_rule_w).max(6.min(max_rule_w));
                let rule = rule_char.repeat(rule_len);
                out.push(RenderedLine::clickable(
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(rule, Style::default().fg(heading_color)),
                    ]),
                    block_hit,
                ));
            }
        }
        OnenoteBlockKind::Paragraph { spans } => {
            let base_style = default_rich_base_style(todo_completed, row_bg, theme);
            render_rich_text_lines(
                out,
                block_idx,
                sel_marker,
                sel_marker_style,
                "",
                Style::default(),
                None,
                badge_info,
                spans,
                base_style,
                row_bg,
                theme,
                width,
            );
        }
        OnenoteBlockKind::ListItem {
            ordered,
            index,
            depth,
            spans,
        } => {
            let max_indent = width.saturating_sub(14);
            let indent_w = ((*depth).min(6) as usize * 2).min(max_indent);
            let indent = " ".repeat(indent_w);
            let bullet = if *ordered {
                format!("{indent}{}. ", index.unwrap_or(1))
            } else {
                format!("{indent}• ")
            };
            let mut bullet_style = Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD);
            if let Some(bg) = row_bg {
                bullet_style = bullet_style.bg(bg);
            }
            let base_style = default_rich_base_style(todo_completed, row_bg, theme);
            render_rich_text_lines(
                out,
                block_idx,
                sel_marker,
                sel_marker_style,
                &bullet,
                bullet_style,
                None,
                badge_info,
                spans,
                base_style,
                row_bg,
                theme,
                width,
            );
        }
        OnenoteBlockKind::Quote { spans } => {
            let mut bar_style = Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD);
            if let Some(bg) = row_bg {
                bar_style = bar_style.bg(bg);
            }
            let base_style = default_rich_base_style(todo_completed, row_bg, theme);
            render_rich_text_lines(
                out,
                block_idx,
                sel_marker,
                sel_marker_style,
                "│ ",
                bar_style,
                Some(("│ ", bar_style)),
                badge_info,
                spans,
                base_style,
                row_bg,
                theme,
                width,
            );
        }
        OnenoteBlockKind::Table { headers, rows } => {
            let sel_w = UnicodeWidthStr::width(sel_marker);
            for line in
                render_unicode_table(headers, rows, selected, theme, width.saturating_sub(sel_w))
            {
                let mut prefixed = vec![Span::styled(sel_marker, sel_marker_style)];
                prefixed.extend(line.spans);
                out.push(RenderedLine::clickable(Line::from(prefixed), block_hit));
            }
        }
        OnenoteBlockKind::CodeBlock { lang, code } => {
            let sel_w = UnicodeWidthStr::width(sel_marker);
            let box_w = width.saturating_sub(sel_w + 2).min(width.saturating_sub(sel_w)).max(6);
            let lang_raw = lang
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("code");
            let lang_label = truncate_clean(lang_raw, box_w.saturating_sub(5).max(1));
            let top_label = format!("─ {lang_label} ");
            let top_rem = box_w.saturating_sub(UnicodeWidthStr::width(top_label.as_str()) + 2);
            let border_st = if selected {
                Style::default().fg(ONENOTE_PURPLE)
            } else {
                theme.muted_style()
            };
            let code_bg = Color::Rgb(28, 30, 38);
            let code_st = Style::default().fg(Color::Rgb(220, 228, 240)).bg(code_bg);

            out.push(RenderedLine::clickable(
                Line::from(vec![
                    Span::styled(sel_marker, sel_marker_style),
                    Span::styled(format!("┌{top_label}{}┐", "─".repeat(top_rem)), border_st),
                ]),
                block_hit,
            ));

            let inner_code_w = box_w.saturating_sub(4).max(2);
            let wrapped = wrap_styled_spans(
                &[Span::styled(code.clone(), code_st)],
                inner_code_w,
                WrapPrefix::empty(),
            );
            for line in wrapped {
                let pad = inner_code_w.saturating_sub(line.width());
                let mut row_spans = vec![
                    Span::styled(sel_marker, sel_marker_style),
                    Span::styled("│ ", border_st.bg(code_bg)),
                ];
                row_spans.extend(line.spans);
                if pad > 0 {
                    row_spans.push(Span::styled(" ".repeat(pad), code_st));
                }
                row_spans.push(Span::styled(" │", border_st.bg(code_bg)));
                out.push(RenderedLine::clickable(Line::from(row_spans), block_hit));
            }

            out.push(RenderedLine::clickable(
                Line::from(vec![
                    Span::styled(sel_marker, sel_marker_style),
                    Span::styled(
                        format!("└{}┘", "─".repeat(box_w.saturating_sub(2))),
                        border_st,
                    ),
                ]),
                block_hit,
            ));
        }
        OnenoteBlockKind::Image {
            resource_index,
            alt,
            width: img_w,
            height: img_h,
            ..
        } => {
            let dim_tag = match (img_w, img_h) {
                (Some(w), Some(h)) if *w > 0 && *h > 0 => format!(" ({w}x{h})"),
                _ => String::new(),
            };
            let alt_tag = alt
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|a| format!(" \"{a}\""))
                .unwrap_or_default();
            let chip = format!(
                "[IMG #{}{dim_tag}{alt_tag} — v preview]",
                resource_index + 1
            );
            let mut chip_style = Style::default()
                .fg(ONENOTE_PURPLE)
                .add_modifier(Modifier::BOLD);
            if let Some(bg) = row_bg {
                chip_style = chip_style.bg(bg);
            }
            let sel_w = UnicodeWidthStr::width(sel_marker);
            let first_p = [Span::styled(sel_marker, sel_marker_style)];
            let cont_p = [Span::styled(" ".repeat(sel_w), sel_marker_style)];
            let off = sel_w as u16;
            for line in wrap_styled_spans(
                &[Span::styled(chip, chip_style)],
                width,
                WrapPrefix {
                    first: &first_p,
                    cont: &cont_p,
                },
            ) {
                let hit_w = (line.width() as u16).saturating_sub(off);
                let inline_hits = if hit_w > 0 {
                    vec![(off, hit_w, HitTarget::OnenoteResource(*resource_index))]
                } else {
                    Vec::new()
                };
                out.push(RenderedLine::with_inline(line, Some(block_hit), inline_hits));
            }

            if let Some(thumb) = cached_thumbs.get(resource_index) {
                for img_line in &thumb.lines {
                    let mut spans = vec![Span::styled("    ", Style::default())];
                    spans.extend(img_line.spans.clone());
                    out.push(RenderedLine::with_inline(
                        Line::from(spans),
                        Some(block_hit),
                        vec![(
                            4,
                            thumb.cols,
                            HitTarget::OnenoteResource(*resource_index),
                        )],
                    ));
                }
            }
        }
        OnenoteBlockKind::Attachment {
            resource_index,
            name,
            mime_type,
            ..
        } => {
            let is_html = crate::ui::app::is_html_resource_meta(Some(mime_type), Some(name));
            let is_md = crate::ui::app::is_markdown_resource_meta(Some(mime_type), Some(name));
            let chip = if is_html {
                format!("[HTML {name} ({mime_type}) — Enter/click preview]")
            } else if is_md {
                format!("[MD {name} ({mime_type}) — Enter/click preview]")
            } else {
                format!("[FILE {name} ({mime_type}) — A download]")
            };
            let mut chip_style = theme.accent_style().add_modifier(Modifier::BOLD);
            if let Some(bg) = row_bg {
                chip_style = chip_style.bg(bg);
            }
            let sel_w = UnicodeWidthStr::width(sel_marker);
            let first_p = [Span::styled(sel_marker, sel_marker_style)];
            let cont_p = [Span::styled(" ".repeat(sel_w), sel_marker_style)];
            let off = sel_w as u16;
            for line in wrap_styled_spans(
                &[Span::styled(chip, chip_style)],
                width,
                WrapPrefix {
                    first: &first_p,
                    cont: &cont_p,
                },
            ) {
                let hit_w = (line.width() as u16).saturating_sub(off);
                let inline_hits = if hit_w > 0 {
                    vec![(off, hit_w, HitTarget::OnenoteResource(*resource_index))]
                } else {
                    Vec::new()
                };
                out.push(RenderedLine::with_inline(line, Some(block_hit), inline_hits));
            }
        }
        OnenoteBlockKind::Divider => {
            out.push(RenderedLine::clickable(
                Line::from(vec![
                    Span::styled(sel_marker, sel_marker_style),
                    Span::styled(
                        "─".repeat(width.saturating_sub(4).min(64)),
                        theme.muted_style(),
                    ),
                ]),
                block_hit,
            ));
        }
    }
}

fn default_rich_base_style(todo_completed: bool, row_bg: Option<Color>, theme: &Theme) -> Style {
    let mut base_style = if todo_completed {
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::DIM | Modifier::CROSSED_OUT)
    } else {
        Style::default().fg(theme.fg)
    };
    if let Some(bg) = row_bg {
        base_style = base_style.bg(bg);
    }
    base_style
}

#[allow(clippy::too_many_arguments)]
fn render_rich_text_lines(
    out: &mut Vec<RenderedLine>,
    block_idx: usize,
    sel_marker: &'static str,
    sel_marker_style: Style,
    lead_prefix: &str,
    lead_style: Style,
    cont_lead: Option<(&str, Style)>,
    badge_info: Option<(&'static str, Style, bool)>,
    spans: &[RichSpan],
    base_style: Style,
    row_bg: Option<Color>,
    theme: &Theme,
    width: usize,
) {
    let block_hit = HitTarget::OnenoteBlock(block_idx);
    let badge_w = badge_info
        .as_ref()
        .map(|(txt, _, _)| UnicodeWidthStr::width(*txt))
        .unwrap_or(0);
    let lead_w = UnicodeWidthStr::width(lead_prefix);

    let mut first_prefix = vec![Span::styled(sel_marker, sel_marker_style)];
    let mut first_inline_hits = Vec::new();
    let mut col_off = UnicodeWidthStr::width(sel_marker) as u16;

    if !lead_prefix.is_empty() {
        first_prefix.push(Span::styled(lead_prefix.to_string(), lead_style));
        col_off = col_off.saturating_add(lead_w as u16);
    }
    if let Some((b_txt, mut b_st, is_todo)) = badge_info {
        if let Some(bg) = row_bg {
            b_st = b_st.bg(bg);
        }
        let bw = UnicodeWidthStr::width(b_txt) as u16;
        if is_todo {
            first_inline_hits.push((col_off, bw.max(2), HitTarget::OnenoteTodoToggle(block_idx)));
        }
        first_prefix.push(Span::styled(b_txt, b_st));
    }

    let mut cont_prefix = vec![Span::styled(sel_marker, sel_marker_style)];
    let mut pad_st = Style::default();
    if let Some(bg) = row_bg {
        pad_st = pad_st.bg(bg);
    }
    if let Some((c_txt, c_st)) = cont_lead {
        cont_prefix.push(Span::styled(c_txt.to_string(), c_st));
        if badge_w > 0 {
            cont_prefix.push(Span::styled(" ".repeat(badge_w), pad_st));
        }
    } else if lead_w + badge_w > 0 {
        cont_prefix.push(Span::styled(" ".repeat(lead_w + badge_w), pad_st));
    }

    let wrapped = wrap_rich_spans(
        spans,
        width,
        WrapPrefix {
            first: &first_prefix,
            cont: &cont_prefix,
        },
        base_style,
        theme,
    );

    for (line_idx, line) in wrapped.into_iter().enumerate() {
        let inline_hits = if line_idx == 0 {
            std::mem::take(&mut first_inline_hits)
        } else {
            Vec::new()
        };
        out.push(RenderedLine::with_inline(line, Some(block_hit), inline_hits));
    }
}

fn style_for_rich_span(span: &RichSpan, base_style: Style, theme: &Theme) -> Style {
    let mut st = base_style;
    if span.style.bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    if span.style.italic {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if span.style.strike {
        st = st.add_modifier(Modifier::CROSSED_OUT);
    }
    if span.style.underline || span.style.link_url.is_some() {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    if span.style.link_url.is_some() {
        st = st.fg(theme.accent);
    }
    if span.style.code {
        st = st.fg(Color::Rgb(245, 194, 107)).bg(Color::Rgb(36, 38, 48));
    }
    st
}

fn rich_spans_to_ratatui(
    spans: &[RichSpan],
    base_style: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    spans
        .iter()
        .map(|sp| Span::styled(sp.text.clone(), style_for_rich_span(sp, base_style, theme)))
        .collect()
}

fn expand_inline_href_spans(spans: &[RichSpan], show_inline_hrefs: bool) -> Vec<RichSpan> {
    if !show_inline_hrefs {
        return spans.to_vec();
    }
    let mut out = Vec::with_capacity(spans.len() * 2);
    for sp in spans {
        out.push(sp.clone());
        if let Some(url) = sp.style.link_url.as_deref() {
            let trimmed = url.trim();
            if !trimmed.is_empty() && !sp.text.contains(trimmed) {
                out.push(RichSpan {
                    text: format!(" ({trimmed})"),
                    style: crate::models::TextStyleFlags::default(),
                });
            }
        }
    }
    out
}

fn wrap_rich_spans(
    spans: &[RichSpan],
    max_width: usize,
    prefix: WrapPrefix<'_>,
    base_style: Style,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let styled = rich_spans_to_ratatui(spans, base_style, theme);
    wrap_styled_spans(&styled, max_width, prefix)
}

/// Renders a crisp Unicode box-drawn table (`┌──┬──┐` / `│  │  │` / `├──┼──┤` / `└──┴──┘`) fit to `max_width`
/// via the shared `bitefoundry_html::layout::render_wrapped_table` primitive (lossless multi-line cells,
/// strict `line.width() <= max_width`, stacked fallback on ultra-narrow viewports).
pub fn render_unicode_table(
    headers: &[Vec<RichSpan>],
    rows: &[Vec<Vec<RichSpan>>],
    selected: bool,
    theme: &Theme,
    max_width: usize,
) -> Vec<Line<'static>> {
    let col_count = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if col_count == 0 {
        return Vec::new();
    }

    let border_style = if selected {
        Style::default().fg(ONENOTE_PURPLE)
    } else {
        theme.muted_style()
    };
    let header_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let cell_style = Style::default().fg(theme.fg);

    let has_non_empty_header = headers
        .iter()
        .any(|cell| cell.iter().any(|sp| !sp.text.trim().is_empty()));
    let styled_headers: Vec<Vec<Span<'static>>> = if has_non_empty_header {
        (0..col_count)
            .map(|c| {
                headers
                    .get(c)
                    .map(|cell| rich_spans_to_ratatui(cell, header_style, theme))
                    .unwrap_or_default()
            })
            .collect()
    } else {
        Vec::new()
    };

    let styled_rows: Vec<Vec<Vec<Span<'static>>>> = rows
        .iter()
        .map(|row| {
            (0..col_count)
                .map(|c| {
                    row.get(c)
                        .map(|cell| rich_spans_to_ratatui(cell, cell_style, theme))
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect();

    let spec = TableSpec {
        headers: &styled_headers,
        rows: &styled_rows,
        alignments: &[],
        border: TableBorder::Box,
        prefix: &[],
        border_style,
        header_style,
        cell_style,
    };
    render_wrapped_table(&spec, max_width)
}

/// Converts a single `OnenoteBlock` back into clean Markdown for surgical block editing (`e`).
pub fn block_to_markdown(block: &OnenoteBlock) -> String {
    let todo_prefix = match &block.tag {
        Some(OnenoteTagKind::Todo { completed: false }) => "- [ ] ",
        Some(OnenoteTagKind::Todo { completed: true }) => "- [x] ",
        _ => "",
    };

    match &block.kind {
        OnenoteBlockKind::Heading { level, spans } => {
            format!(
                "{} {}",
                "#".repeat((*level).clamp(1, 6) as usize),
                spans_to_markdown(spans)
            )
        }
        OnenoteBlockKind::Paragraph { spans } => {
            format!("{todo_prefix}{}", spans_to_markdown(spans))
        }
        OnenoteBlockKind::ListItem {
            ordered,
            index,
            depth,
            spans,
        } => {
            let indent = "  ".repeat(*depth as usize);
            if !todo_prefix.is_empty() {
                format!("{indent}{todo_prefix}{}", spans_to_markdown(spans))
            } else if *ordered {
                format!("{indent}{}. {}", index.unwrap_or(1), spans_to_markdown(spans))
            } else {
                format!("{indent}- {}", spans_to_markdown(spans))
            }
        }
        OnenoteBlockKind::Quote { spans } => format!("> {}", spans_to_markdown(spans)),
        OnenoteBlockKind::CodeBlock { lang, code } => {
            let l = lang.as_deref().unwrap_or("");
            format!("```{l}\n{code}\n```")
        }
        OnenoteBlockKind::Table { headers, rows } => {
            let cols = headers
                .len()
                .max(rows.iter().map(Vec::len).max().unwrap_or(0));
            if cols == 0 {
                return String::new();
            }
            let mut md = Vec::new();
            let h_cells: Vec<String> = (0..cols)
                .map(|c| headers.get(c).map(|s| spans_to_markdown(s)).unwrap_or_default())
                .collect();
            md.push(format!("| {} |", h_cells.join(" | ")));
            md.push(format!("| {} |", vec!["---"; cols].join(" | ")));
            for r in rows {
                let r_cells: Vec<String> = (0..cols)
                    .map(|c| r.get(c).map(|s| spans_to_markdown(s)).unwrap_or_default())
                    .collect();
                md.push(format!("| {} |", r_cells.join(" | ")));
            }
            md.join("\n")
        }
        OnenoteBlockKind::Image { alt, src, .. } => {
            format!("![{}]({src})", alt.as_deref().unwrap_or("image"))
        }
        OnenoteBlockKind::Attachment { name, src, .. } => format!("[{name}]({src})"),
        OnenoteBlockKind::Divider => "---".to_string(),
    }
}

fn spans_plain_text(spans: &[RichSpan]) -> String {
    let mut out = String::new();
    for s in spans {
        out.push_str(&s.text);
    }
    out
}

fn spans_to_markdown(spans: &[RichSpan]) -> String {
    crate::parser::onenote::spans_to_markdown(spans)
}

fn truncate_clean(text: &str, max_width: usize) -> String {
    crate::ui::truncate_width(text, max_width)
}

fn short_timestamp(iso: &str) -> String {
    if let (Some(date), Some(time)) = (iso.get(0..10), iso.get(11..16)) {
        format!("{date} {time}")
    } else {
        iso.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OnenoteResource, OnenoteScope};

    fn sample_summary(title: &str, level: i32, has_todo: bool) -> OnenotePageSummary {
        OnenotePageSummary {
            page_key: format!("me|p-{title}"),
            id: format!("p-{title}"),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Meeting Notes".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            title: title.to_string(),
            level,
            order: 0,
            created_at: Some("2026-11-15T09:00:00Z".to_string()),
            last_modified_at: Some("2026-11-15T14:20:00Z".to_string()),
            content_url: None,
            web_url: Some("https://onedrive.live.com/onenote".to_string()),
            preview_text: None,
            has_todo,
        }
    }

    #[test]
    fn subpage_indentation_formats_levels_authentically() {
        let p0 = sample_summary("Root Page", 0, false);
        let p0_todo = sample_summary("Todo Page", 0, true);
        let p1 = sample_summary("Sub Page", 1, false);
        let p2 = sample_summary("Nested Page", 2, false);

        assert_eq!(format_page_row_label(&p0), ("• ", "Root Page".to_string()));
        assert_eq!(
            format_page_row_label(&p0_todo),
            ("☐ ", "Todo Page".to_string())
        );
        assert_eq!(
            format_page_row_label(&p1),
            ("  └─ ", "Sub Page".to_string())
        );
        assert_eq!(
            format_page_row_label(&p2),
            ("    └─ ", "Nested Page".to_string())
        );
    }

    #[test]
    fn unicode_box_table_renders_headers_and_rows_cleanly() {
        let headers = vec![
            vec![RichSpan::plain("Service")],
            vec![RichSpan::plain("Status")],
        ];
        let rows = vec![
            vec![
                vec![RichSpan::plain("Teams")],
                vec![RichSpan::plain("Healthy")],
            ],
            vec![
                vec![RichSpan::plain("OneNote")],
                vec![RichSpan::plain("Deployed")],
            ],
        ];
        let rendered = render_unicode_table(&headers, &rows, false, &Theme::default(), 60);
        let plain: String = rendered
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(plain.contains('┌') && plain.contains('┬') && plain.contains('┐'));
        assert!(plain.contains("Service") && plain.contains("OneNote") && plain.contains("Deployed"));
        assert!(plain.contains('├') && plain.contains('┼') && plain.contains('┤'));
        assert!(plain.contains('└') && plain.contains('┴') && plain.contains('┘'));
    }

    #[test]
    fn note_canvas_renders_tags_todo_hit_targets_code_and_resources() {
        let detail = OnenotePageDetail {
            summary: sample_summary("v0.1.0 Release Notes", 0, true),
            raw_xhtml: String::new(),
            body_markdown: String::new(),
            body_plain: String::new(),
            blocks: vec![
                OnenoteBlock {
                    element_id: Some("h1:1".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::Heading {
                        level: 1,
                        spans: vec![RichSpan::plain("Key Goals")],
                    },
                },
                OnenoteBlock {
                    element_id: Some("p:2".to_string()),
                    tag: Some(OnenoteTagKind::Todo { completed: false }),
                    raw_html: None,
                    kind: OnenoteBlockKind::Paragraph {
                        spans: vec![RichSpan::plain("Complete OneNote TUI")],
                    },
                },
                OnenoteBlock {
                    element_id: Some("p:3".to_string()),
                    tag: Some(OnenoteTagKind::Todo { completed: true }),
                    raw_html: None,
                    kind: OnenoteBlockKind::Paragraph {
                        spans: vec![RichSpan::plain("Verify XHTML parser")],
                    },
                },
                OnenoteBlock {
                    element_id: Some("pre:4".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::CodeBlock {
                        lang: Some("rust".to_string()),
                        code: "let ok = true;".to_string(),
                    },
                },
                OnenoteBlock {
                    element_id: Some("img:5".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::Image {
                        resource_index: 0,
                        src: "https://graph.microsoft.com/v1.0/me/onenote/resources/r1/content"
                            .to_string(),
                        alt: Some("Architecture".to_string()),
                        width: Some(800),
                        height: Some(600),
                    },
                },
            ],
            resources: vec![OnenoteResource {
                element_id: Some("img:5".to_string()),
                resource_url:
                    "https://graph.microsoft.com/v1.0/me/onenote/resources/r1/content".to_string(),
                fullres_url: None,
                mime_type: "image/png".to_string(),
                file_name: Some("arch.png".to_string()),
                is_image: true,
                alt_text: Some("Architecture".to_string()),
            }],
            primary_div_id: Some("div:main".to_string()),
            synced_at: None,
        };

        let (lines, ranges) = build_page_canvas_lines(
            &detail,
            Some(1),
            &HashMap::new(),
            &Theme::default(),
            80,
        );
        assert_eq!(ranges.len(), 5);

        let all_text: String = lines
            .iter()
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_text.contains("v0.1.0 Release Notes"));
        assert!(all_text.contains("☐ Complete OneNote TUI"));
        assert!(all_text.contains("☑ Verify XHTML parser"));
        assert!(all_text.contains("─ rust "));
        assert!(all_text.contains("[IMG #1"));

        // Verify block #1 has inline HitTarget::OnenoteTodoToggle(1)
        let has_todo_hit = lines.iter().any(|r| {
            r.inline_hits
                .iter()
                .any(|&(_, _, t)| t == HitTarget::OnenoteTodoToggle(1))
        });
        assert!(has_todo_hit);

        assert_eq!(
            block_to_markdown(&detail.blocks[1]),
            "- [ ] Complete OneNote TUI"
        );
        assert_eq!(
            block_to_markdown(&detail.blocks[2]),
            "- [x] Verify XHTML parser"
        );
    }

    #[test]
    fn fallback_markdown_and_outbound_underscore_callout() {
        // 1. Plain-text fallback when blocks is empty renders Markdown via bitefoundry_html::markdown
        let detail = OnenotePageDetail {
            summary: sample_summary("Fallback Page", 0, false),
            raw_xhtml: String::new(),
            body_markdown: String::new(),
            body_plain: "# H1\n**bold**".to_string(),
            blocks: Vec::new(),
            resources: Vec::new(),
            primary_div_id: None,
            synced_at: None,
        };

        let (lines, ranges) = build_page_canvas_lines(
            &detail,
            None,
            &HashMap::new(),
            &Theme::default(),
            80,
        );
        assert!(ranges.is_empty());
        // Skip the 3 header lines (page title `# Fallback Page`, metadata, divider) to inspect body
        let body_text: String = lines
            .iter()
            .skip(3)
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body_text.contains("H1"), "expected H1 in fallback:\n{body_text}");
        assert!(body_text.contains("bold"), "expected bold in fallback:\n{body_text}");
        assert!(!body_text.contains("# "), "raw '#' leaked in fallback:\n{body_text}");
        assert!(!body_text.contains("**"), "raw '**' leaked in fallback:\n{body_text}");

        // 2. Outbound Markdown -> OneNote XHTML via pulldown-cmark:
        //    underscore _italic_ / __bold__, backslash escapes, and GFM Callout (> [!note])
        let outbound_md = "_italic_ __bold__ \\*escaped\\*\n\n> [!note]\n> Callout body text\n\n> [!warning] Inline warning";
        let xhtml = crate::parser::markdown_to_onenote_html_fragment(outbound_md, &[]);
        assert!(
            xhtml.contains("<em>italic</em>"),
            "expected <em>italic</em>, got: {xhtml}"
        );
        assert!(
            xhtml.contains("<strong>bold</strong>"),
            "expected <strong>bold</strong>, got: {xhtml}"
        );
        assert!(
            xhtml.contains("*escaped*") && !xhtml.contains("\\*escaped"),
            "expected backslash escape resolved, got: {xhtml}"
        );
        assert!(
            xhtml.contains(r#"data-callout="note""#)
                && xhtml.contains("border-left:4px solid")
                && xhtml.contains("<strong>NOTE</strong>")
                && xhtml.contains("Callout body text"),
            "expected styled GFM note callout, got: {xhtml}"
        );
        assert!(
            xhtml.contains(r#"data-callout="warning""#)
                && xhtml.contains("<strong>WARNING</strong>")
                && xhtml.contains("Inline warning"),
            "expected styled GFM warning callout, got: {xhtml}"
        );
    }

    #[test]
    fn heading_90_char_korean_and_embedded_newline_wraps_losslessly_at_width_50() {
        let part1 = "가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사";
        let part2 = "둘째줄한글제목내용가나다라마바사아자차카타파하끝토큰마무리테스트긴제목표시확인완료추가검증문구";
        let full_heading = format!("{part1}\n{part2}");
        assert!(
            full_heading.chars().count() >= 90,
            "heading must be >= 90 chars, got {}",
            full_heading.chars().count()
        );

        let detail = OnenotePageDetail {
            summary: sample_summary("Heading Wrap Test", 0, false),
            raw_xhtml: String::new(),
            body_markdown: String::new(),
            body_plain: String::new(),
            blocks: vec![OnenoteBlock {
                element_id: Some("h1:1".to_string()),
                tag: None,
                raw_html: None,
                kind: OnenoteBlockKind::Heading {
                    level: 1,
                    spans: vec![RichSpan::plain(full_heading.clone())],
                },
            }],
            resources: Vec::new(),
            primary_div_id: None,
            synced_at: None,
        };

        let width = 50usize;
        let (lines, ranges) = build_page_canvas_lines(
            &detail,
            None,
            &HashMap::new(),
            &Theme::default(),
            width,
        );
        let &(start, end) = ranges.get(&0).expect("heading block range missing");
        let block_lines = &lines[start..end];

        for (idx, rl) in block_lines.iter().enumerate() {
            assert!(
                rl.line.width() <= width,
                "heading line {idx} exceeded width {width}: {} ({:?})",
                rl.line.width(),
                rl.line
            );
        }

        // Exclude underline rule (`═`) and trailing blank separator line.
        let text_lines: Vec<String> = block_lines
            .iter()
            .map(|rl| {
                rl.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .filter(|s| !s.trim().is_empty() && !s.contains('═'))
            .collect();

        assert!(
            text_lines.len() >= 4,
            "expected multi-line wrapped heading at width=50, got {} lines: {:?}",
            text_lines.len(),
            text_lines
        );

        // Embedded `\n` must start `part2` on a distinct line after `part1`.
        let part2_line_idx = text_lines
            .iter()
            .position(|l| l.contains("둘째줄한글"))
            .expect("second paragraph after embedded newline missing");
        assert!(
            part2_line_idx > 0,
            "embedded newline must place second part on a subsequent line: {:?}",
            text_lines
        );
        assert!(
            !text_lines[part2_line_idx].contains("카타파하둘째줄"),
            "embedded newline must not be flattened into a single line: {:?}",
            text_lines
        );

        // Lossless character check across all wrapped heading lines.
        let stripped: String = text_lines
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if i == 0 {
                    s.strip_prefix("  # ").unwrap_or(s)
                } else {
                    s.strip_prefix("    ").unwrap_or(s)
                }
            })
            .collect::<String>()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let expected: String = full_heading
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(stripped, expected, "heading text must wrap losslessly");
    }

    #[test]
    fn wide_6_col_table_with_8_line_cell_preserves_all_lines_and_width_invariant() {
        let headers: Vec<Vec<RichSpan>> = (1..=6)
            .map(|c| vec![RichSpan::plain(format!("C{c}"))])
            .collect();
        let eight_line_cell = (1..=8)
            .map(|n| format!("L{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = vec![vec![
            vec![RichSpan::plain("A1")],
            vec![RichSpan::plain(eight_line_cell)],
            vec![RichSpan::plain("A3")],
            vec![RichSpan::plain("A4")],
            vec![RichSpan::plain("A5")],
            vec![RichSpan::plain("END6")],
        ]];

        let width = 60usize;
        let rendered = render_unicode_table(&headers, &rows, false, &Theme::default(), width);
        assert!(!rendered.is_empty());

        for (idx, line) in rendered.iter().enumerate() {
            assert!(
                line.width() <= width,
                "6-col table line {idx} exceeded width {width}: {} ({:?})",
                line.width(),
                line
            );
        }

        let plain: String = rendered
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        // All 6 columns (headers + cells) must be preserved without right-column clipping.
        for tok in ["C1", "C2", "C3", "C4", "C5", "C6", "A1", "A3", "A4", "A5", "END6"] {
            assert!(
                plain.contains(tok),
                "missing 6-col table token '{tok}' at width={width}:\n{plain}"
            );
        }

        // Lines 5-8 of the 8-line cell must ALL survive (old `.take(4)` silently dropped them).
        for n in 1..=8 {
            let marker = format!("L{n}");
            assert!(
                plain.contains(&marker),
                "cell line '{marker}' was truncated in 8-line cell:\n{plain}"
            );
        }

        // Also check the full canvas integration (with 2-col selection gutter) keeps `line.width() <= 60`.
        let detail = OnenotePageDetail {
            summary: sample_summary("Wide Table", 0, false),
            raw_xhtml: String::new(),
            body_markdown: String::new(),
            body_plain: String::new(),
            blocks: vec![OnenoteBlock {
                element_id: Some("tbl:1".to_string()),
                tag: None,
                raw_html: None,
                kind: OnenoteBlockKind::Table { headers, rows },
            }],
            resources: Vec::new(),
            primary_div_id: None,
            synced_at: None,
        };
        let (canvas_lines, _) = build_page_canvas_lines(
            &detail,
            Some(0),
            &HashMap::new(),
            &Theme::default(),
            width,
        );
        for (idx, rl) in canvas_lines.iter().enumerate() {
            assert!(
                rl.line.width() <= width,
                "canvas table line {idx} exceeded width {width}: {} ({:?})",
                rl.line.width(),
                rl.line
            );
        }
    }

    #[test]
    fn deep_list_chips_code_quote_and_empty_state_satisfy_width_invariant() {
        let detail = OnenotePageDetail {
            summary: sample_summary("Invariants", 0, false),
            raw_xhtml: String::new(),
            body_markdown: String::new(),
            body_plain: String::new(),
            blocks: vec![
                OnenoteBlock {
                    element_id: Some("li:1".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::ListItem {
                        ordered: true,
                        index: Some(12),
                        depth: 6,
                        spans: vec![RichSpan::plain(
                            "깊은중첩리스트항목텍스트가좁은폭에서도절대우측경계를넘지않고전량표시된다",
                        )],
                    },
                },
                OnenoteBlock {
                    element_id: Some("q:2".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::Quote {
                        spans: vec![RichSpan::plain("인용문첫줄\n인용문둘째줄긴내용확인테스트")],
                    },
                },
                OnenoteBlock {
                    element_id: Some("pre:3".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::CodeBlock {
                        lang: Some("rust".to_string()),
                        code: "\tlet very_long_identifier_name = \"한글코드문자열테스트\";".to_string(),
                    },
                },
                OnenoteBlock {
                    element_id: Some("img:4".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::Image {
                        resource_index: 0,
                        src: "https://example.com/img.png".to_string(),
                        alt: Some("아주긴이미지대체텍스트설명문구확인용".to_string()),
                        width: Some(1920),
                        height: Some(1080),
                    },
                },
                OnenoteBlock {
                    element_id: Some("att:5".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: OnenoteBlockKind::Attachment {
                        resource_index: 1,
                        src: "https://example.com/report.md".to_string(),
                        name: "quarterly_architecture_review_2026_final_v3.md".to_string(),
                        mime_type: "text/markdown".to_string(),
                    },
                },
            ],
            resources: Vec::new(),
            primary_div_id: None,
            synced_at: None,
        };

        for width in [24usize, 36, 50] {
            let (lines, ranges) = build_page_canvas_lines(
                &detail,
                Some(0),
                &HashMap::new(),
                &Theme::default(),
                width,
            );
            for (idx, rl) in lines.iter().enumerate() {
                assert!(
                    rl.line.width() <= width,
                    "width={width} line {idx} overflowed: {} > {width} ({:?})",
                    rl.line.width(),
                    rl.line
                );
            }

            // Image and Attachment chips must wrap into multiple rows at narrow width
            // AND preserve `HitTarget::OnenoteResource` on every wrapped row.
            for &(blk_idx, res_idx) in &[(3usize, 0usize), (4usize, 1usize)] {
                let &(start, end) = ranges.get(&blk_idx).unwrap();
                let chip_rows: Vec<&RenderedLine> = lines[start..end]
                    .iter()
                    .filter(|rl| {
                        !rl.line
                            .spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                            .trim()
                            .is_empty()
                    })
                    .collect();
                assert!(
                    chip_rows.len() >= 2 || width >= 50,
                    "expected wrapped chip rows at width={width} for block {blk_idx}"
                );
                for row in chip_rows {
                    assert!(
                        row.inline_hits
                            .iter()
                            .any(|&(_, _, t)| t == HitTarget::OnenoteResource(res_idx)),
                        "wrapped chip row missing HitTarget::OnenoteResource({res_idx}) at width={width}"
                    );
                }
            }
        }

        // Empty-state prompt must wrap cleanly within width.
        let empty_detail = OnenotePageDetail {
            summary: sample_summary("Empty", 0, false),
            raw_xhtml: String::new(),
            body_markdown: String::new(),
            body_plain: String::new(),
            blocks: Vec::new(),
            resources: Vec::new(),
            primary_div_id: None,
            synced_at: None,
        };
        let (empty_lines, _) = build_page_canvas_lines(
            &empty_detail,
            None,
            &HashMap::new(),
            &Theme::default(),
            28,
        );
        for (idx, rl) in empty_lines.iter().enumerate() {
            assert!(
                rl.line.width() <= 28,
                "empty page line {idx} overflowed width 28: {}",
                rl.line.width()
            );
        }
    }
}
