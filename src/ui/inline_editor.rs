//! Direct in-place OneNote canvas editor (`src/ui/inline_editor.rs`).
//!
//! Renders directly inside the Note Canvas pane (no modal popup) using `ComposerState`
//! for real 2D cursor navigation, UTF-8 / Korean IME hardware cursor placement,
//! Undo/Redo, clipboard paste, and debounced background Graph syncing.

use std::time::{Duration, Instant};

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    models::OnenotePageDetail,
    parser,
    ui::{
        action::Action,
        composer::{self, ComposerState},
        hitmap::{ButtonId, HitMap, HitTarget},
        nav_pane::ONENOTE_PURPLE,
        theme::Theme,
    },
};

/// Debounce window before automatically flushing dirty inline edits to Microsoft Graph.
pub const INLINE_AUTOSAVE_DEBOUNCE_MS: u64 = 1_500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineEditTarget {
    /// Edit the full page Markdown body directly in place.
    PageBody { base_markdown: String },
    /// Surgically edit a single Graph block when the page has embedded resources or multi-div regions.
    Block {
        element_id: String,
        block_index: usize,
        base_content: String,
    },
    /// Append new Markdown directly at the end of a page with resources/multi-div regions.
    Append,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineSaveStatus {
    Saved,
    Editing,
    Syncing,
    Conflict(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineEditEvent {
    TextChanged,
    CursorChanged,
    Ignored,
}

#[derive(Debug, Clone)]
pub struct OnenoteInlineEditor {
    pub page_key: String,
    pub page_title: String,
    pub target: InlineEditTarget,
    pub composer: ComposerState,
    pub dirty: bool,
    pub dirty_since: Option<Instant>,
    pub saving: bool,
    pub save_status: InlineSaveStatus,
    pub last_editor_area: Rect,
}

impl OnenoteInlineEditor {
    /// Opens an in-place editor for `detail`.
    /// Prefers full-page Markdown editing when the page has no embedded media and supports
    /// safe body replacement; falls back to surgical block edit (or inline append) otherwise.
    pub fn open_for_page(detail: &OnenotePageDetail, preferred_block: Option<usize>) -> Self {
        let can_edit_full_body = detail.resources.is_empty()
            && (detail.body_markdown.trim().is_empty()
                || parser::build_replace_page_body_commands(detail, None, "__probe__").is_ok());

        let mut composer = ComposerState::new();
        let target = if can_edit_full_body {
            composer.load_text(&detail.body_markdown);
            if let Some(blk_idx) = preferred_block {
                if let Some(blk) = detail.blocks.get(blk_idx) {
                    let needle = parser::block_baseline_content(blk);
                    if !needle.trim().is_empty() {
                        if let Some(pos) = composer.text().find(needle.trim()) {
                            let target_end = pos + needle.trim().len();
                            // Position cursor at the end of the clicked block's line.
                            composer.set_cursor_byte(target_end);
                        }
                    }
                }
            }
            InlineEditTarget::PageBody {
                base_markdown: detail.body_markdown.clone(),
            }
        } else if let Some(blk_idx) = preferred_block {
            if let Some(blk) = detail.blocks.get(blk_idx) {
                if let Some(eid) = blk.element_id.clone() {
                    let base = parser::block_baseline_content(blk);
                    composer.load_text(&base);
                    InlineEditTarget::Block {
                        element_id: eid,
                        block_index: blk_idx,
                        base_content: base,
                    }
                } else {
                    InlineEditTarget::Append
                }
            } else {
                InlineEditTarget::Append
            }
        } else {
            InlineEditTarget::Append
        };

        Self {
            page_key: detail.summary.page_key.clone(),
            page_title: detail.summary.title.clone(),
            target,
            composer,
            dirty: false,
            dirty_since: None,
            saving: false,
            save_status: InlineSaveStatus::Saved,
            last_editor_area: Rect::default(),
        }
    }

    pub fn should_autosave(&self, now: Instant) -> bool {
        self.dirty
            && !self.saving
            && !matches!(self.save_status, InlineSaveStatus::Conflict(_))
            && self
                .dirty_since
                .is_some_and(|t| now.saturating_duration_since(t) >= Duration::from_millis(INLINE_AUTOSAVE_DEBOUNCE_MS))
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        if !matches!(self.save_status, InlineSaveStatus::Conflict(_)) {
            self.dirty_since = Some(Instant::now());
            self.save_status = InlineSaveStatus::Editing;
        }
    }

    pub fn mark_syncing(&mut self) {
        self.saving = true;
        self.dirty = false;
        self.dirty_since = None;
        self.save_status = InlineSaveStatus::Syncing;
    }

    pub fn mark_saved(&mut self, new_baseline: Option<String>) {
        self.saving = false;
        if let Some(base) = new_baseline {
            match &mut self.target {
                InlineEditTarget::PageBody { base_markdown } => *base_markdown = base,
                InlineEditTarget::Block { base_content, .. } => *base_content = base,
                InlineEditTarget::Append => {}
            }
        }
        if !self.dirty {
            self.save_status = InlineSaveStatus::Saved;
        }
    }

    pub fn mark_conflict(&mut self, msg: String) {
        self.saving = false;
        self.dirty = true;
        self.dirty_since = None;
        self.save_status = InlineSaveStatus::Conflict(msg);
    }

    pub fn mark_error(&mut self, msg: String) {
        self.saving = false;
        self.dirty = true;
        self.dirty_since = None;
        self.save_status = InlineSaveStatus::Error(msg);
    }

    pub fn handle_action(&mut self, action: &Action) -> InlineEditEvent {
        match action {
            Action::Input(ch) => {
                self.composer.insert_char(*ch);
                self.mark_dirty();
                InlineEditEvent::TextChanged
            }
            Action::Paste(text) => {
                self.composer.insert_str(text);
                self.mark_dirty();
                InlineEditEvent::TextChanged
            }
            Action::PasteClipboard => {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    if let Ok(text) = cb.get_text() {
                        if !text.is_empty() {
                            self.composer.insert_str(&composer::normalize(&text));
                            self.mark_dirty();
                            return InlineEditEvent::TextChanged;
                        }
                    }
                }
                InlineEditEvent::Ignored
            }
            Action::InsertNewline | Action::Newline | Action::Activate => {
                self.composer.newline();
                self.mark_dirty();
                InlineEditEvent::TextChanged
            }
            Action::Tab => {
                self.composer.insert_str("  ");
                self.mark_dirty();
                InlineEditEvent::TextChanged
            }
            Action::Backspace => {
                self.composer.backspace();
                self.mark_dirty();
                InlineEditEvent::TextChanged
            }
            Action::Delete => {
                self.composer.delete_forward();
                self.mark_dirty();
                InlineEditEvent::TextChanged
            }
            Action::Undo => {
                if self.composer.undo() {
                    self.mark_dirty();
                    InlineEditEvent::TextChanged
                } else {
                    InlineEditEvent::Ignored
                }
            }
            Action::Redo => {
                if self.composer.redo() {
                    self.mark_dirty();
                    InlineEditEvent::TextChanged
                } else {
                    InlineEditEvent::Ignored
                }
            }
            Action::CursorLeft => {
                self.composer.move_left(false);
                InlineEditEvent::CursorChanged
            }
            Action::CursorRight => {
                self.composer.move_right(false);
                InlineEditEvent::CursorChanged
            }
            Action::CursorUp | Action::MoveUp => {
                self.composer.move_up(false);
                InlineEditEvent::CursorChanged
            }
            Action::CursorDown | Action::MoveDown => {
                self.composer.move_down(false);
                InlineEditEvent::CursorChanged
            }
            Action::CursorHome => {
                self.composer.move_home(false);
                InlineEditEvent::CursorChanged
            }
            Action::CursorEnd => {
                self.composer.move_end(false);
                InlineEditEvent::CursorChanged
            }
            Action::PageUp => {
                for _ in 0..8 {
                    self.composer.move_up(false);
                }
                InlineEditEvent::CursorChanged
            }
            Action::PageDown => {
                for _ in 0..8 {
                    self.composer.move_down(false);
                }
                InlineEditEvent::CursorChanged
            }
            Action::ScrollTop => {
                self.composer.move_doc_start(false);
                InlineEditEvent::CursorChanged
            }
            Action::ScrollBottom => {
                self.composer.move_doc_end(false);
                InlineEditEvent::CursorChanged
            }
            _ => InlineEditEvent::Ignored,
        }
    }

    /// Places the composer cursor near a mouse click inside `last_editor_area`.
    pub fn handle_click(&mut self, col: u16, row: u16) -> bool {
        let area = self.last_editor_area;
        if area.width == 0
            || area.height == 0
            || col < area.x
            || col >= area.right()
            || row < area.y
            || row >= area.bottom()
        {
            return false;
        }
        let body_w = (area.width as usize).max(1);
        let (visual, cursor_row, _) =
            composer::text_layout(self.composer.text(), body_w, self.composer.cursor());
        let height = area.height as usize;
        let offset = if cursor_row >= height {
            (cursor_row + 1 - height).min(visual.len().saturating_sub(height))
        } else {
            0
        };
        let target_vis_row = offset + (row - area.y) as usize;
        let target_col = (col - area.x) as usize;
        if let Some(&(start, end)) = visual.get(target_vis_row) {
            let slice = self.composer.text().get(start..end).unwrap_or("");
            let mut pos = start;
            let mut width = 0usize;
            for ch in slice.chars() {
                let cw = if ch == '\t' {
                    4
                } else {
                    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
                };
                if width + cw > target_col {
                    break;
                }
                width += cw;
                pos += ch.len_utf8();
            }
            self.composer.set_cursor_byte(pos);
            return true;
        }
        self.composer.move_doc_end(false);
        true
    }

    /// Renders the in-place canvas editor into `area` and returns the hardware cursor `(x, y)`.
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        hitmap: &mut HitMap,
    ) -> Option<(u16, u16)> {
        if area.width == 0 || area.height < 2 {
            return None;
        }
        let mode_label = match &self.target {
            InlineEditTarget::PageBody { .. } => "Direct Edit".to_string(),
            InlineEditTarget::Block { block_index, .. } => {
                format!("Block #{}", block_index + 1)
            }
            InlineEditTarget::Append => "Quick Append".to_string(),
        };
        let (status_text, status_color) = match &self.save_status {
            InlineSaveStatus::Saved => ("✓ Saved", theme.success),
            InlineSaveStatus::Editing => ("● Editing", theme.warning),
            InlineSaveStatus::Syncing => ("⟳ Syncing...", theme.accent_soft),
            InlineSaveStatus::Conflict(_) => ("⚠ Conflict", theme.error),
            InlineSaveStatus::Error(_) => ("⚠ Save error", theme.error),
        };
        let (ln, col) = self.composer.cursor_line_col_1based();
        let bg = theme.bg_header;

        let done_btn = " [✓ Esc Done] ";
        let save_btn = " [Ctrl+S Save] ";
        let right_w =
            (UnicodeWidthStr::width(done_btn) + UnicodeWidthStr::width(save_btn)) as u16;
        let left_w = area.width.saturating_sub(right_w);

        let left_line = Line::from(vec![
            Span::styled(
                format!(" ✎ {mode_label} "),
                Style::default()
                    .bg(ONENOTE_PURPLE)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default().bg(bg)),
            Span::styled(
                status_text,
                Style::default()
                    .bg(bg)
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · Ln {ln}, Col {col} · Ctrl+E $EDITOR "),
                Style::default().bg(bg).fg(theme.fg_muted),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(left_line).style(Style::default().bg(bg)),
            Rect::new(area.x, area.y, left_w, 1),
        );

        let mut rx = area.x.saturating_add(left_w);
        let save_w = (UnicodeWidthStr::width(save_btn) as u16)
            .min(area.right().saturating_sub(rx));
        if save_w > 0 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    save_btn,
                    Style::default()
                        .bg(bg)
                        .fg(ONENOTE_PURPLE)
                        .add_modifier(Modifier::BOLD),
                )),
                Rect::new(rx, area.y, save_w, 1),
            );
            hitmap.add_line(rx, area.y, save_w, HitTarget::Button(ButtonId::InlineSave));
            rx = rx.saturating_add(save_w);
        }
        let done_w = (UnicodeWidthStr::width(done_btn) as u16)
            .min(area.right().saturating_sub(rx));
        if done_w > 0 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    done_btn,
                    Style::default()
                        .bg(bg)
                        .fg(theme.success)
                        .add_modifier(Modifier::BOLD),
                )),
                Rect::new(rx, area.y, done_w, 1),
            );
            hitmap.add_line(rx, area.y, done_w, HitTarget::Button(ButtonId::InlineDone));
        }

        // Row 1: Page title header line
        let mut content_y = area.y.saturating_add(1);
        if area.height >= 4 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    crate::ui::truncate_width(&self.page_title, area.width as usize),
                    Style::default()
                        .fg(ONENOTE_PURPLE)
                        .add_modifier(Modifier::BOLD),
                ))),
                Rect::new(area.x, content_y, area.width, 1),
            );
            content_y = content_y.saturating_add(1);
        }

        let editor_h = area.bottom().saturating_sub(content_y);
        if editor_h == 0 {
            return None;
        }
        let editor_area = Rect::new(area.x, content_y, area.width, editor_h);
        self.last_editor_area = editor_area;
        hitmap.add(editor_area, HitTarget::InlineEditorBody);

        let placeholder = match self.target {
            InlineEditTarget::Append => {
                "Type Markdown to append (- [ ] todo, ## heading, **bold**)... (Ctrl+S save, Esc done)"
            }
            _ => "Start typing your note... (Markdown supported · Ctrl+S save · Esc done)",
        };
        let surface_style = Style::default().bg(theme.bg_canvas).fg(theme.fg_primary);
        self.composer
            .draw_editor(frame, editor_area, theme, true, placeholder, surface_style)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OnenotePageSummary, OnenoteScope};
    use ratatui::{backend::TestBackend, Terminal};

    fn sample_detail(xhtml: &str) -> OnenotePageDetail {
        let summary = OnenotePageSummary {
            page_key: "me|p-1".to_string(),
            id: "p-1".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Notes".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Personal".to_string()),
            title: "Daily Note".to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: None,
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        };
        parser::parse_onenote_page_xhtml(summary, xhtml)
    }

    #[test]
    fn inline_editor_supports_korean_typing_cursor_navigation_undo_and_hardware_cursor() {
        let detail = sample_detail(
            r#"<!DOCTYPE html><html><head><title>Daily Note</title></head><body><div id="div:{1}"><p id="p:{1}">First line</p><p id="p:{2}">Second line</p></div></body></html>"#,
        );
        let mut editor = OnenoteInlineEditor::open_for_page(&detail, Some(0));
        assert!(matches!(editor.target, InlineEditTarget::PageBody { .. }));
        assert!(!editor.dirty);

        // Type Korean + ASCII directly with full cursor movement & undo
        editor.handle_action(&Action::InsertNewline);
        editor.handle_action(&Action::Paste("한글 메모 테스트".to_string()));
        assert!(editor.dirty);
        assert!(editor.composer.text().contains("한글 메모 테스트"));

        editor.handle_action(&Action::CursorLeft);
        editor.handle_action(&Action::Backspace);
        assert!(editor.composer.text().contains("한글 메모 테트"));

        editor.handle_action(&Action::Undo);
        assert!(editor.composer.text().contains("한글 메모 테스트"));

        // Draw returns concrete hardware cursor coordinates inside editor area for IME anchoring
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let theme = Theme::default();
        let mut hitmap = HitMap::default();
        let mut cursor = None;
        term.draw(|f| {
            let area = f.area();
            cursor = editor.draw(f, area, &theme, &mut hitmap);
        })
        .unwrap();
        let (cx, cy) = cursor.expect("inline editor must return hardware cursor for IME");
        assert!(cy >= editor.last_editor_area.y && cy < editor.last_editor_area.bottom());
        assert!(cx >= editor.last_editor_area.x && cx < editor.last_editor_area.right());
    }

    #[test]
    fn inline_editor_falls_back_to_block_edit_when_page_has_images() {
        let detail = sample_detail(
            r#"<!DOCTYPE html><html><head><title>With Image</title></head><body><div id="div:{1}"><p id="p:{1}">Caption text</p><img src="https://graph.microsoft.com/v1.0/me/onenote/resources/r1/$value" alt="shot" /></div></body></html>"#,
        );
        assert!(!detail.resources.is_empty());
        let editor = OnenoteInlineEditor::open_for_page(&detail, Some(0));
        assert!(matches!(
            editor.target,
            InlineEditTarget::Block {
                ref element_id,
                block_index: 0,
                ..
            } if element_id == "p:{1}"
        ));
        assert_eq!(editor.composer.text(), "Caption text");
    }
}
