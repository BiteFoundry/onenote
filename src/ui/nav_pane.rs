//! Left Navigation Rail (`nav_pane.rs`) — Microsoft OneNote Notebooks (Personal + Team/Group Shared).

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::{
    models::OnenoteNotebook,
    ui::{
        app::{AppState, Focus, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        truncate_width,
    },
};

/// OneNote brand violet accent color.
pub const ONENOTE_PURPLE: Color = Color::Rgb(119, 25, 170);
/// Subtle purple-tinted highlight background for active OneNote selections.
pub const ONENOTE_BRAND_BG: Color = Color::Rgb(48, 28, 66);

/// Maps a OneNote notebook/section hex color (or index fallback) to a crisp terminal `Color`.
pub fn onenote_badge_color(hex: Option<&str>, fallback_idx: usize) -> Color {
    if let Some(raw) = hex.map(str::trim).filter(|s| !s.is_empty()) {
        let clean = raw.trim_start_matches('#');
        if clean.len() == 6 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&clean[0..2], 16),
                u8::from_str_radix(&clean[2..4], 16),
                u8::from_str_radix(&clean[4..6], 16),
            ) {
                return Color::Rgb(r, g, b);
            }
        }
        match raw.to_ascii_lowercase().as_str() {
            "blue" => return Color::Rgb(56, 139, 253),
            "green" => return Color::Rgb(63, 185, 80),
            "yellow" => return Color::Rgb(210, 153, 34),
            "orange" => return Color::Rgb(219, 109, 40),
            "red" | "crimson" => return Color::Rgb(248, 81, 73),
            "purple" | "magenta" => return Color::Rgb(163, 113, 247),
            "teal" | "cyan" => return Color::Rgb(57, 197, 207),
            _ => {}
        }
    }
    const PALETTE: [Color; 7] = [
        Color::Rgb(163, 113, 247),
        Color::Rgb(56, 139, 253),
        Color::Rgb(63, 185, 80),
        Color::Rgb(210, 153, 34),
        Color::Rgb(248, 81, 73),
        Color::Rgb(57, 197, 207),
        Color::Rgb(219, 109, 40),
    ];
    PALETTE[fallback_idx % PALETTE.len()]
}

/// Computes the vertical scroll offset so `selected` stays visible inside a viewport of `height` rows.
pub fn selection_offset(selected: usize, height: usize, total: usize) -> usize {
    if height == 0 || total <= height {
        return 0;
    }
    if selected < height {
        0
    } else {
        (selected + 1).saturating_sub(height).min(total - height)
    }
}

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let focused = ui.focus == Focus::Nav;

    let bg_block = Block::default()
        .borders(Borders::NONE)
        .style(Style::default().bg(theme.bg_sidebar).fg(theme.fg_primary));
    frame.render_widget(bg_block, area);

    if area.width < 4 || area.height < 2 {
        return;
    }

    // Header row
    let header_style = if focused {
        Style::default()
            .bg(theme.bg_header)
            .fg(ONENOTE_PURPLE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(theme.bg_header)
            .fg(theme.fg_primary)
            .add_modifier(Modifier::BOLD)
    };
    let title = format!(" Notebooks ({}) ", app.notebooks.len());
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_width(&title, area.width as usize),
            header_style,
        )))
        .style(Style::default().bg(theme.bg_header)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let list_y = area.y + 1;
    let list_h = area.height.saturating_sub(2) as usize;

    if app.notebooks.is_empty() {
        if list_h > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " No notebooks yet",
                    theme.muted_style(),
                )))
                .style(Style::default().bg(theme.bg_sidebar)),
                Rect::new(area.x, list_y, area.width, 1),
            );
        }
    } else {
        let selected_idx = app
            .active_notebook_key
            .as_deref()
            .and_then(|k| app.notebooks.iter().position(|n| n.notebook_key == k))
            .unwrap_or(0);
        let offset = selection_offset(selected_idx, list_h, app.notebooks.len());

        for (row_offset, nb) in app.notebooks.iter().skip(offset).take(list_h).enumerate() {
            let idx = offset + row_offset;
            let row_y = list_y + row_offset as u16;
            let is_active = app.active_notebook_key.as_deref() == Some(nb.notebook_key.as_str());
            draw_notebook_row(frame, Rect::new(area.x, row_y, area.width, 1), nb, idx, is_active, focused, app, ui);
            hitmap.add_line(area.x, row_y, area.width, HitTarget::NavRow(idx));
        }
    }

    // Bottom quick button: `[+ Notebook B]`
    let footer_y = area.y + area.height.saturating_sub(1);
    if footer_y > area.y {
        let btn_label = " [+ Notebook B] ";
        let w = (btn_label.len() as u16).min(area.width);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_width(btn_label, area.width as usize),
                Style::default()
                    .bg(theme.bg_sidebar)
                    .fg(theme.accent_soft),
            )))
            .style(Style::default().bg(theme.bg_sidebar)),
            Rect::new(area.x, footer_y, area.width, 1),
        );
        hitmap.add_line(area.x, footer_y, w, HitTarget::Button(ButtonId::OnenoteNewNotebook));
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_notebook_row(
    frame: &mut Frame,
    rect: Rect,
    nb: &OnenoteNotebook,
    idx: usize,
    is_active: bool,
    focused: bool,
    app: &AppState,
    ui: &UiState,
) {
    let theme = &ui.theme;
    let badge_color = onenote_badge_color(nb.color.as_deref(), idx);
    let sec_count = app
        .sections
        .iter()
        .filter(|s| {
            s.scope.scope_key() == nb.scope.scope_key()
                && s.notebook_id.as_deref() == Some(nb.id.as_str())
        })
        .count();

    let row_bg = if is_active {
        if theme.no_color {
            Color::Reset
        } else if focused {
            ONENOTE_BRAND_BG
        } else {
            theme.bg_selected
        }
    } else {
        theme.bg_sidebar
    };

    let marker = if is_active { "▌" } else { " " };
    let scope_tag = if nb.scope.is_personal() { "" } else { " [Team]" };
    let count_suffix = if sec_count > 0 {
        format!(" ({sec_count})")
    } else {
        String::new()
    };

    let max_name_w = (rect.width as usize).saturating_sub(3 + count_suffix.len());
    let name_display = truncate_width(&format!("{}{}", nb.display_name, scope_tag), max_name_w);

    let name_style = if is_active {
        let mut st = Style::default()
            .bg(row_bg)
            .fg(theme.fg_primary)
            .add_modifier(Modifier::BOLD);
        if theme.no_color {
            st = st.add_modifier(Modifier::REVERSED);
        }
        st
    } else {
        Style::default().bg(row_bg).fg(theme.fg_secondary)
    };

    let line = Line::from(vec![
        Span::styled(marker, Style::default().bg(row_bg).fg(badge_color)),
        Span::styled("● ", Style::default().bg(row_bg).fg(badge_color)),
        Span::styled(name_display, name_style),
        Span::styled(count_suffix, Style::default().bg(row_bg).fg(theme.fg_muted)),
    ]);

    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(row_bg)),
        rect,
    );
}
