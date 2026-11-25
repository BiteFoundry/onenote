//! Root UI module: 3-tier responsive OneNote layout, rendering pipeline, status bar, and shared helpers.

pub mod action;
pub mod app;
pub mod composer;
pub mod event;
pub mod hitmap;
pub mod keymap;
pub mod nav_pane;
pub mod notification;
pub mod onenote_view;
pub mod overlays;
pub mod theme;
pub mod toast;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use self::{
    app::{AppState, Focus, NarrowPane, UiState},
    hitmap::{ButtonId, HitMap, HitTarget},
    toast::ToastKind,
};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn truncate_width(s: &str, max_w: usize) -> String {
    if max_w == 0 {
        return String::new();
    }
    if s.width() <= max_w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    let budget = max_w.saturating_sub(1);
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

pub fn draw(frame: &mut Frame, app: &mut AppState, ui: &mut UiState) -> Option<(u16, u16)> {
    let area = frame.area();
    ui.term_width = area.width;
    ui.term_height = area.height;
    ui.pending_pixel_graphics.clear();

    let mut hitmap = HitMap::new();
    let theme = ui.theme.clone();

    // Base canvas clear
    frame.render_widget(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().bg(theme.bg_canvas).fg(theme.fg_primary)),
        area,
    );

    if area.width < 12 || area.height < 4 {
        let status = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        draw_status_bar(frame, status, app, ui, &mut hitmap);
        ui.hitmap = hitmap;
        return None;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    let body = rows[0];
    let status = rows[1];

    if area.width < 80 {
        // Narrow mode: compact top switcher + active pane
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(2)])
            .split(body);
        draw_narrow_bar(frame, split[0], ui, &mut hitmap);
        match ui.narrow_pane {
            NarrowPane::Notebooks => nav_pane::draw(frame, split[1], app, ui, &mut hitmap),
            NarrowPane::Pages | NarrowPane::Canvas => {
                onenote_view::draw(frame, split[1], app, ui, &mut hitmap)
            }
        }
    } else if ui.show_nav_rail {
        let nav_w = if area.width >= 120 { 26 } else { 22 };
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(nav_w),
                Constraint::Length(1),
                Constraint::Min(30),
            ])
            .split(body);
        nav_pane::draw(frame, cols[0], app, ui, &mut hitmap);
        draw_vertical_divider(frame, cols[1], &theme);
        onenote_view::draw(frame, cols[2], app, ui, &mut hitmap);
    } else {
        onenote_view::draw(frame, body, app, ui, &mut hitmap);
    }

    draw_status_bar(frame, status, app, ui, &mut hitmap);

    // Render overlay stack (bottom to top).
    // Register a full-screen backdrop before each overlay so only the top-most modal
    // can receive mouse clicks (preventing click-through to lower modals or main UI).
    let mut overlays = std::mem::take(&mut ui.overlays);
    for ov in &mut overlays {
        hitmap.add(area, HitTarget::OverlayBackdrop);
        ov.draw(frame, area, app, ui, &mut hitmap);
    }
    ui.overlays = overlays;

    ui.hitmap = hitmap;
    None
}

fn draw_vertical_divider(frame: &mut Frame, area: Rect, theme: &theme::Theme) {
    let lines: Vec<Line<'static>> = (0..area.height)
        .map(|_| Line::from(Span::styled("│", Style::default().fg(theme.border))))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_narrow_bar(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let pills = [
        (" [1 Notebooks] ", NarrowPane::Notebooks, ButtonId::NarrowNotebooks),
        (" [2 Pages] ", NarrowPane::Pages, ButtonId::NarrowPages),
        (" [3 Note] ", NarrowPane::Canvas, ButtonId::NarrowCanvas),
    ];
    let mut x = area.x;
    let mut spans = Vec::new();
    for (label, pane, btn) in pills {
        let active = ui.narrow_pane == pane;
        let style = if active {
            theme.selection().add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(theme.bg_header).fg(theme.fg_muted)
        };
        let w = (label.width() as u16).min(area.right().saturating_sub(x));
        if w == 0 {
            break;
        }
        spans.push(Span::styled(label, style));
        hitmap.add_line(x, area.y, w, HitTarget::Button(btn));
        x = x.saturating_add(w);
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg_header)),
        area,
    );
}

fn draw_status_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let bg = theme.bg_header;

    let mut left_spans: Vec<Span<'static>> = Vec::new();
    if let Some(toast) = ui.toasts.latest() {
        let fg = match toast.kind {
            ToastKind::Info => theme.accent_soft,
            ToastKind::Success => theme.success,
            ToastKind::Warning => theme.warning,
            ToastKind::Error => theme.error,
        };
        left_spans.push(Span::styled(
            format!(" {} ", toast.message),
            Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
        ));
    } else if let Some(task) = ui.tasks.first_running() {
        let spin = SPINNER_FRAMES[ui.spinner_tick % SPINNER_FRAMES.len()];
        left_spans.push(Span::styled(
            format!(" {spin} {}... ", task.label),
            Style::default().bg(bg).fg(theme.accent_soft),
        ));
    } else {
        let spin = if app.syncing || ui.poll_retrying {
            format!("{} ", SPINNER_FRAMES[ui.spinner_tick % SPINNER_FRAMES.len()])
        } else {
            String::new()
        };
        let focus_tag = match ui.focus {
            Focus::Nav => "Notebooks",
            Focus::Pages => "Pages",
            Focus::Canvas => "Canvas",
            Focus::Overlay => "Modal",
        };
        let user_tag = app
            .profile
            .as_ref()
            .map(|p| format!(" · {}", p.display_name))
            .unwrap_or_default();
        left_spans.push(Span::styled(
            format!(" OneNote · {focus_tag}{user_tag} · {spin}{} ", app.sync_summary),
            Style::default().bg(bg).fg(theme.fg_secondary),
        ));
    }

    let density_label = match ui.density {
        crate::config::DensityMode::Cozy => " [Cozy]",
        crate::config::DensityMode::Compact => " [Cmpct]",
    };
    let right_buttons = [
        (" [r Sync]", ButtonId::Refresh),
        (density_label, ButtonId::DensityToggle),
        (" [/ Search]", ButtonId::Search),
        (" [Ctrl+P]", ButtonId::CommandPalette),
        (" [Ctrl+,]", ButtonId::Settings),
        (" [? Help] ", ButtonId::Help),
    ];
    let right_total: u16 = right_buttons.iter().map(|(l, _)| l.len() as u16).sum();
    let left_w = area.width.saturating_sub(right_total);

    frame.render_widget(
        Paragraph::new(Line::from(left_spans)).style(Style::default().bg(bg)),
        Rect::new(area.x, area.y, left_w, 1),
    );

    let mut rx = area.x.saturating_add(left_w);
    for (label, btn) in right_buttons {
        let w = (label.len() as u16).min(area.right().saturating_sub(rx));
        if w == 0 {
            break;
        }
        frame.render_widget(
            Paragraph::new(Span::styled(
                label,
                Style::default().bg(bg).fg(theme.fg_muted),
            ))
            .style(Style::default().bg(bg)),
            Rect::new(rx, area.y, w, 1),
        );
        hitmap.add_line(rx, area.y, w, HitTarget::Button(btn));
        rx = rx.saturating_add(w);
    }
}
