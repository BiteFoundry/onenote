//! OneNote FTS5 Search Overlay (`/`):
//! Queries local SQLite FTS5 (`storage.search_onenote_pages`) live as you type,
//! and `Enter` jumps directly to the selected notebook/section/page.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone, Default)]
pub struct SearchOverlay {
    pub query: String,
    pub selected: usize,
}

impl SearchOverlay {
    pub fn with_query(query: String) -> Self {
        Self { query, selected: 0 }
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::SearchLocalPages(self.query.clone()))
            }
            Action::Paste(text) => {
                self.query.push_str(&text.replace(['\r', '\n'], " "));
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::SearchLocalPages(self.query.clone()))
            }
            Action::Backspace => {
                let _ = self.query.pop();
                self.selected = 0;
                OverlayOutcome::Dispatch(Action::SearchLocalPages(self.query.clone()))
            }
            Action::MoveUp => {
                self.selected = self.selected.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                if !app.search_results.is_empty() && self.selected + 1 < app.search_results.len() {
                    self.selected += 1;
                }
                OverlayOutcome::Handled
            }
            Action::RefreshAll => OverlayOutcome::Dispatch(Action::RefreshAll),
            Action::Activate | Action::OverlaySubmit => {
                if !self.query.trim().is_empty() && !app.search_results.is_empty() {
                    let idx = self.selected.min(app.search_results.len() - 1);
                    OverlayOutcome::CloseAndDispatch(Action::JumpToSearchHit(idx))
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        hitmap.add(area, HitTarget::OverlayBackdrop);
        let rect = centered_rect(
            area,
            area.width.saturating_sub(8).min(92).max(40),
            area.height.saturating_sub(4).min(22).max(9),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Search OneNote Pages (SQLite FTS5) · Enter Jump · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Input line
        let prompt = format!(" / {}", self.query);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prompt, Style::default().fg(theme.fg).add_modifier(Modifier::BOLD)),
                Span::styled("▌", Style::default().fg(theme.accent)),
            ])),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let results_y = inner.y + 2;
        let avail_h = inner.height.saturating_sub(2) as usize;
        if avail_h == 0 {
            return;
        }

        if self.query.trim().is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "Type keywords to search across cached OneNote page titles and bodies...",
                    theme.muted_style(),
                )),
                Rect::new(inner.x, results_y, inner.width, 1),
            );
            return;
        }

        if app.search_results.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "No matching pages in local cache. Press Ctrl+R to sync more pages.",
                    theme.muted_style(),
                )),
                Rect::new(inner.x, results_y, inner.width, 1),
            );
            return;
        }

        let selected = self.selected.min(app.search_results.len() - 1);
        let offset =
            crate::ui::nav_pane::selection_offset(selected, avail_h, app.search_results.len());
        for (i, hit) in app
            .search_results
            .iter()
            .skip(offset)
            .take(avail_h)
            .enumerate()
        {
            let idx = offset + i;
            let row_y = results_y + i as u16;
            let is_sel = idx == selected;
            let style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg)
            };
            let loc = match (hit.notebook_name.as_deref(), hit.section_name.as_deref()) {
                (Some(n), Some(s)) => format!(" [{n} › {s}]"),
                (None, Some(s)) => format!(" [{s}]"),
                _ => String::new(),
            };
            let snip = if hit.snippet.trim().is_empty() {
                String::new()
            } else {
                format!(" — {}", hit.snippet.replace(['\r', '\n'], " "))
            };
            let line_text = crate::ui::truncate_width(
                &format!(" {}{}{}", hit.title, loc, snip),
                inner.width as usize,
            );
            frame.render_widget(
                Paragraph::new(Span::styled(line_text, style)).style(style),
                Rect::new(inner.x, row_y, inner.width, 1),
            );
            hitmap.add_line(inner.x, row_y, inner.width, HitTarget::OverlayItem { index: idx });
        }
    }
}
