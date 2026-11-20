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

use std::fmt;

use bitefoundry_html::{HtmlEffect, HtmlViewSession};
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
    models::OnenotePageDetail,
    ui::{
        action::Action,
        app::UiState,
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

pub struct HtmlPreviewOverlay {
    pub session: HtmlViewSession,
    pub content_rect: Rect,
    pub total_lines: usize,
    pub last_width: u16,
    pub follow_focus: bool,
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
        self.session
            .title()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("HTML Preview")
            .to_string()
    }

    pub fn raw_html(&self) -> &str {
        self.session.raw_html()
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
            Action::Input('m') | Action::Input('M') => {
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

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
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

        let mode_badge = if self.session.raw_mode {
            "RAW HTML"
        } else {
            "RENDERED"
        };
        let js_badge = if self.session.js_active {
            " · JS"
        } else {
            ""
        };
        let title = format!(
            " HTML · {} [{mode_badge}{js_badge}] ",
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

        let (total_lines, control_count, controls_meta, lines_to_render) = {
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

        frame.render_widget(
            Paragraph::new(lines_to_render)
                .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
                .scroll((self.session.scroll_y, 0)),
            self.content_rect,
        );

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
                format!(
                    " Raw HTML ({total_lines} lines) · m Rendered · o Browser · j/k/Wheel Scroll · Esc Close"
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

    fn render_overlay(overlay: &mut HtmlPreviewOverlay, ui: &UiState, width: u16, height: u16) -> (HitMap, String) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut hitmap = HitMap::new();
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
        let ui = UiState::from_config(&AppConfig::default());
        let mut overlay = HtmlPreviewOverlay::new(None, html.to_string());
        assert!(overlay.session.js_active, "onclick handler should activate on-demand QuickJS");

        let (hitmap, screen0) = render_overlay(&mut overlay, &ui, 100, 28);
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

        let (_, screen1) = render_overlay(&mut overlay, &ui, 100, 28);
        assert!(
            screen1.contains("Clicks: 1"),
            "expected onclick DOM mutation to render 'Clicks: 1', got:\n{screen1}"
        );

        // Press 'm' to toggle Rendered -> Raw HTML -> Rendered
        assert_eq!(overlay.handle(&Action::Input('m'), &ui), OverlayOutcome::Handled);
        assert!(overlay.session.raw_mode);
        let (_, raw_screen) = render_overlay(&mut overlay, &ui, 100, 28);
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
        let ui = UiState::from_config(&AppConfig::default());
        let mut overlay = HtmlPreviewOverlay::new(Some("Static Doc".into()), long_body);
        assert!(!overlay.session.js_active, "static HTML must keep QuickJS at 0 MB idle");

        let _ = render_overlay(&mut overlay, &ui, 90, 20);
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
        let _ = render_overlay(&mut overlay, &ui, 90, 20);
        assert!(overlay.session.scroll_y > 0);
        overlay.handle(&Action::MouseScroll { delta: -3, column: 20, row: 10 }, &ui);
        overlay.handle(&Action::ScrollTop, &ui);
        assert_eq!(overlay.session.scroll_y, 0);
    }
}
