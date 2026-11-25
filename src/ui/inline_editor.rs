//! Direct in-place OneNote canvas editor (`src/ui/inline_editor.rs`).
//!
//! Renders directly inside the Note Canvas pane (no modal popup) using `ComposerState`
//! for real 2D cursor navigation, UTF-8 / Korean IME hardware cursor placement,
//! Undo/Redo, clipboard paste, and debounced background Graph syncing.
//!
//! Defaults to **Line Live Preview** (`EditorMode::LivePreview`): inactive logical lines
//! render as clean rich Markdown (headings, checkboxes, bullets, blockquotes, horizontal
//! rules, fenced code blocks, bold/italic/code/strike/links), while the active cursor line
//! expands its raw Markdown source with syntax highlighting and exact hardware cursor `(cx, cy)`.
//! Press `Ctrl+M` or click `[Live]`/`[Src]` to toggle full raw Markdown source mode.

use std::time::{Duration, Instant};

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Rendering mode for the in-place OneNote canvas editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorMode {
    /// Obsidian-style Line Live Preview: inactive lines render rich Markdown; active line exposes raw Markdown.
    #[default]
    LivePreview,
    /// Full raw Markdown source mode with syntax highlighting across all lines.
    Source,
}

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
    ModeChanged,
    Ignored,
}

#[derive(Debug, Clone)]
pub struct OnenoteInlineEditor {
    pub page_key: String,
    pub page_title: String,
    pub target: InlineEditTarget,
    pub mode: EditorMode,
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
            mode: EditorMode::LivePreview,
            composer,
            dirty: false,
            dirty_since: None,
            saving: false,
            save_status: InlineSaveStatus::Saved,
            last_editor_area: Rect::default(),
        }
    }

    /// Toggles between `EditorMode::LivePreview` and `EditorMode::Source`.
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            EditorMode::LivePreview => EditorMode::Source,
            EditorMode::Source => EditorMode::LivePreview,
        };
    }

    pub fn should_autosave(&self, now: Instant) -> bool {
        self.dirty
            && !self.saving
            && !matches!(self.save_status, InlineSaveStatus::Conflict(_))
            && self
                .dirty_since
                .is_some_and(|t| {
                    now.saturating_duration_since(t)
                        >= Duration::from_millis(INLINE_AUTOSAVE_DEBOUNCE_MS)
                })
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
            Action::InlineToggleMode => {
                self.toggle_mode();
                InlineEditEvent::ModeChanged
            }
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

    /// Places the composer cursor accurately at the logical line & byte offset matching a
    /// mouse click inside `last_editor_area` (works identically in both `LivePreview` and `Source`).
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
        if self.composer.text().is_empty() {
            self.composer.set_cursor_byte(0);
            return true;
        }

        let body_w = (area.width as usize).max(1);
        let layout = build_editor_layout(
            self.composer.text(),
            body_w,
            self.composer.cursor(),
            self.mode,
            None,
            &Theme::default(),
        );
        let height = area.height as usize;
        let offset = compute_scroll_offset(layout.cursor_row, height, layout.rows.len());
        let target_vis_row = offset + (row - area.y) as usize;
        let target_col = (col - area.x) as usize;

        if let Some(vrow) = layout.rows.get(target_vis_row) {
            let pos = vrow.byte_offset_for_col(target_col);
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

        let (mode_pill, mode_pill_color) = match self.mode {
            EditorMode::LivePreview => (" [Live] ", theme.accent_soft),
            EditorMode::Source => (" [Src] ", theme.warning),
        };
        let save_btn = " [Ctrl+S Save] ";
        let done_btn = " [✓ Esc Done] ";
        let right_w = (UnicodeWidthStr::width(mode_pill)
            + UnicodeWidthStr::width(save_btn)
            + UnicodeWidthStr::width(done_btn)) as u16;
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
                format!(" · Ln {ln}, Col {col} · Ctrl+M Mode · Ctrl+E $EDITOR "),
                Style::default().bg(bg).fg(theme.fg_muted),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(left_line).style(Style::default().bg(bg)),
            Rect::new(area.x, area.y, left_w, 1),
        );

        let mut rx = area.x.saturating_add(left_w);
        let mode_w = (UnicodeWidthStr::width(mode_pill) as u16)
            .min(area.right().saturating_sub(rx));
        if mode_w > 0 {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    mode_pill,
                    Style::default()
                        .bg(bg)
                        .fg(mode_pill_color)
                        .add_modifier(Modifier::BOLD),
                )),
                Rect::new(rx, area.y, mode_w, 1),
            );
            hitmap.add_line(
                rx,
                area.y,
                mode_w,
                HitTarget::Button(ButtonId::InlineToggleMode),
            );
            rx = rx.saturating_add(mode_w);
        }
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

        let body_width = (editor_area.width as usize).max(1);
        self.composer.last_inner_width = body_width.min(u16::MAX as usize) as u16;
        let surface_style = Style::default().bg(theme.bg_canvas).fg(theme.fg_primary);

        if self.composer.text().is_empty() {
            let placeholder = match self.target {
                InlineEditTarget::Append => {
                    "Type Markdown to append (- [ ] todo, ## heading, **bold**)... (Ctrl+S save, Esc done)"
                }
                _ => "Start typing your note... (Markdown supported · Ctrl+M mode · Ctrl+S save · Esc done)",
            };
            let placeholder_line = Line::from(Span::styled(
                composer::truncate_display(placeholder, body_width),
                theme.muted_style().add_modifier(Modifier::ITALIC),
            ));
            frame.render_widget(
                Paragraph::new(vec![placeholder_line]).style(surface_style),
                editor_area,
            );
            return Some((editor_area.x, editor_area.y));
        }

        let layout = build_editor_layout(
            self.composer.text(),
            body_width,
            self.composer.cursor(),
            self.mode,
            self.composer.selection_range(),
            theme,
        );
        let height = editor_area.height as usize;
        let offset = compute_scroll_offset(layout.cursor_row, height, layout.rows.len());

        let visible_lines: Vec<Line<'static>> = layout
            .rows
            .into_iter()
            .skip(offset)
            .take(height)
            .map(|r| r.line)
            .collect();
        frame.render_widget(
            Paragraph::new(visible_lines).style(surface_style),
            editor_area,
        );

        let cursor_y = editor_area
            .y
            .saturating_add(layout.cursor_row.saturating_sub(offset) as u16)
            .min(editor_area.bottom().saturating_sub(1));
        let cursor_x = editor_area
            .x
            .saturating_add(layout.cursor_col as u16)
            .min(editor_area.right().saturating_sub(1));
        Some((cursor_x, cursor_y))
    }
}

// ---------------------------------------------------------------------------
// Line Live Preview & Source Syntax-Highlighted Layout Engine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct CellMap {
    col_start: usize,
    col_width: usize,
    src_byte: usize,
}

#[derive(Debug, Clone)]
struct VisualRow {
    line: Line<'static>,
    cells: Vec<CellMap>,
    src_start: usize,
    src_end: usize,
}

impl VisualRow {
    fn byte_offset_for_col(&self, target_col: usize) -> usize {
        if self.cells.is_empty() {
            return self.src_start;
        }
        for cell in &self.cells {
            if target_col < cell.col_start + cell.col_width {
                return cell.src_byte;
            }
        }
        self.src_end
    }
}

struct EditorLayout {
    rows: Vec<VisualRow>,
    cursor_row: usize,
    cursor_col: usize,
}

#[derive(Debug, Clone, Copy)]
enum GlyphText {
    Char(char),
    Tab,
}

#[derive(Debug, Clone, Copy)]
struct Glyph {
    text: GlyphText,
    width: usize,
    style: Style,
    src_byte: usize,
}

impl Glyph {
    fn from_source_char(ch: char, style: Style, src_byte: usize) -> Self {
        if ch == '\t' {
            Self {
                text: GlyphText::Tab,
                width: 4,
                style,
                src_byte,
            }
        } else {
            Self {
                text: GlyphText::Char(ch),
                width: UnicodeWidthChar::width(ch).unwrap_or(0),
                style,
                src_byte,
            }
        }
    }

    fn synthetic(ch: char, style: Style, src_byte: usize) -> Self {
        Self {
            text: GlyphText::Char(ch),
            width: UnicodeWidthChar::width(ch).unwrap_or(0),
            style,
            src_byte,
        }
    }

    fn push_to(&self, out: &mut String) {
        match self.text {
            GlyphText::Char(ch) => out.push(ch),
            GlyphText::Tab => out.push_str("    "),
        }
    }
}

fn compute_scroll_offset(cursor_row: usize, viewport_h: usize, total_rows: usize) -> usize {
    if viewport_h == 0 {
        return 0;
    }
    let raw = if cursor_row >= viewport_h {
        cursor_row + 1 - viewport_h
    } else {
        0
    };
    raw.min(total_rows.saturating_sub(viewport_h))
}

fn logical_line_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            ranges.push((start, idx));
            start = idx + 1;
        }
    }
    ranges.push((start, text.len()));
    ranges
}

fn build_editor_layout(
    text: &str,
    body_width: usize,
    cursor: usize,
    mode: EditorMode,
    selection: Option<(usize, usize)>,
    theme: &Theme,
) -> EditorLayout {
    let width = body_width.max(1);
    let cursor = cursor.min(text.len());
    let ranges = logical_line_ranges(text);

    let mut rows: Vec<VisualRow> = Vec::with_capacity(ranges.len());
    let mut cursor_pos: Option<(usize, usize)> = None;
    let mut fence_state: Option<(char, usize)> = None;

    for (line_start, line_end) in ranges {
        let raw = &text[line_start..line_end];
        let is_active = cursor >= line_start && cursor <= line_end;
        let hide_delimiters = mode == EditorMode::LivePreview && !is_active;

        // Track fenced code block state across logical lines regardless of active status.
        let fence_kind = if let Some((fch, flen)) = fence_state {
            if is_closing_fence(raw, fch, flen) {
                fence_state = None;
                FenceLineKind::Close
            } else {
                FenceLineKind::Body
            }
        } else if let Some((fch, flen, lang)) = parse_opening_fence(raw) {
            fence_state = Some((fch, flen));
            FenceLineKind::Open(lang)
        } else {
            FenceLineKind::None
        };

        let mut glyphs: Vec<Glyph> = Vec::with_capacity(raw.len());
        let mut cont_prefix: Vec<Glyph> = Vec::new();

        build_logical_line_glyphs(
            raw,
            line_start,
            width,
            fence_kind,
            hide_delimiters,
            theme,
            &mut glyphs,
            &mut cont_prefix,
        );

        // Apply active text selection highlight (preserves foreground syntax colors).
        if let Some((sel_start, sel_end)) = selection {
            let sel_style = theme.selection();
            for g in &mut glyphs {
                if g.src_byte >= sel_start && g.src_byte < sel_end {
                    g.style = g.style.patch(sel_style);
                }
            }
        }

        // Soft-wrap glyphs into VisualRows while computing exact cursor (row, col) on the active line.
        let cont_w: usize = cont_prefix.iter().map(|g| g.width).sum();
        let use_cont = hide_delimiters && !cont_prefix.is_empty() && cont_w < width;

        let mut col = 0usize;
        let mut row_glyphs: Vec<Glyph> = Vec::new();
        let mut row_cells: Vec<CellMap> = Vec::new();
        let mut row_src_start = line_start;

        for g in glyphs {
            if col > 0 && col + g.width > width {
                rows.push(VisualRow {
                    line: glyphs_to_line(&row_glyphs),
                    cells: std::mem::take(&mut row_cells),
                    src_start: row_src_start,
                    src_end: g.src_byte,
                });
                row_glyphs.clear();
                col = 0;
                row_src_start = g.src_byte;

                if use_cont {
                    for cg in &cont_prefix {
                        let mapped = Glyph {
                            src_byte: row_src_start,
                            ..*cg
                        };
                        row_cells.push(CellMap {
                            col_start: col,
                            col_width: mapped.width,
                            src_byte: mapped.src_byte,
                        });
                        col += mapped.width;
                        row_glyphs.push(mapped);
                    }
                }
            }

            if is_active && g.src_byte == cursor && cursor_pos.is_none() {
                cursor_pos = Some((rows.len(), col));
            }
            row_cells.push(CellMap {
                col_start: col,
                col_width: g.width,
                src_byte: g.src_byte,
            });
            col += g.width;
            row_glyphs.push(g);
        }

        if is_active && cursor_pos.is_none() {
            cursor_pos = Some((rows.len(), col));
        }

        rows.push(VisualRow {
            line: glyphs_to_line(&row_glyphs),
            cells: row_cells,
            src_start: row_src_start,
            src_end: line_end,
        });
    }

    let (cursor_row, cursor_col) = cursor_pos.unwrap_or((rows.len().saturating_sub(1), 0));
    EditorLayout {
        rows,
        cursor_row,
        cursor_col,
    }
}

fn glyphs_to_line(glyphs: &[Glyph]) -> Line<'static> {
    if glyphs.is_empty() {
        return Line::from("");
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur_style = glyphs[0].style;
    let mut buf = String::new();
    for g in glyphs {
        if g.style != cur_style && !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), cur_style));
            cur_style = g.style;
        }
        g.push_to(&mut buf);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur_style));
    }
    Line::from(spans)
}

#[derive(Debug, Clone, Copy)]
enum FenceLineKind<'a> {
    None,
    Open(&'a str),
    Body,
    Close,
}

fn parse_opening_fence(raw: &str) -> Option<(char, usize, &str)> {
    let trimmed = raw.trim_start_matches(' ');
    let indent = raw.len() - trimmed.len();
    if indent > 3 {
        return None;
    }
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|&c| c == first).count();
    if count < 3 {
        return None;
    }
    let rest = &trimmed[count..];
    if first == '`' && rest.contains('`') {
        return None;
    }
    Some((first, count, rest.trim()))
}

fn is_closing_fence(raw: &str, fence_char: char, min_len: usize) -> bool {
    let trimmed = raw.trim_start_matches(' ');
    let indent = raw.len() - trimmed.len();
    if indent > 3 {
        return false;
    }
    let count = trimmed.chars().take_while(|&c| c == fence_char).count();
    count >= min_len && trimmed[count..].trim().is_empty()
}

fn is_horizontal_rule(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.len() < 3 {
        return false;
    }
    let mut marker: Option<char> = None;
    let mut count = 0usize;
    for ch in trimmed.chars() {
        if ch == ' ' || ch == '\t' {
            continue;
        }
        if matches!(ch, '-' | '*' | '_') {
            if let Some(m) = marker {
                if m != ch {
                    return false;
                }
            } else {
                marker = Some(ch);
            }
            count += 1;
        } else {
            return false;
        }
    }
    count >= 3
}

/// Returns `(heading_level, content_start_byte_rel)` for `# `..`###### `.
fn parse_heading(raw: &str) -> Option<(usize, usize)> {
    let trimmed = raw.trim_start_matches(' ');
    let indent = raw.len() - trimmed.len();
    if indent > 3 {
        return None;
    }
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let after = &trimmed[hashes..];
    if after.starts_with(' ') && !after[1..].trim().is_empty() {
        Some((hashes, indent + hashes + 1))
    } else {
        None
    }
}

/// Returns `(indent_bytes, checked, content_start_byte_rel)` for `- [ ] ` / `- [x] ` / `* [ ] ` / `+ [x] `.
fn parse_task_item(raw: &str) -> Option<(usize, bool, usize)> {
    let trimmed = raw.trim_start_matches(' ');
    let indent = raw.len() - trimmed.len();
    let mut bytes = trimmed.as_bytes().iter();
    let bullet = *bytes.next()?;
    if !matches!(bullet, b'-' | b'*' | b'+') {
        return None;
    }
    if *bytes.next()? != b' ' {
        return None;
    }
    if *bytes.next()? != b'[' {
        return None;
    }
    let state = *bytes.next()?;
    let checked = match state {
        b' ' => false,
        b'x' | b'X' => true,
        _ => return None,
    };
    if *bytes.next()? != b']' {
        return None;
    }
    match bytes.next() {
        Some(b' ') => Some((indent, checked, indent + 6)),
        None => Some((indent, checked, indent + 5)),
        _ => None,
    }
}

/// Returns `(indent_bytes, content_start_byte_rel)` for `- ` / `* ` / `+ `.
fn parse_bullet_item(raw: &str) -> Option<(usize, usize)> {
    let trimmed = raw.trim_start_matches(' ');
    let indent = raw.len() - trimmed.len();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'-' | b'*' | b'+') && bytes[1] == b' ' {
        Some((indent, indent + 2))
    } else {
        None
    }
}

/// Returns `(indent_bytes, marker_end_byte_rel)` for `1. ` / `2) `.
fn parse_ordered_item(raw: &str) -> Option<(usize, usize)> {
    let trimmed = raw.trim_start_matches(' ');
    let indent = raw.len() - trimmed.len();
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 6 {
        return None;
    }
    let rest = &trimmed[digits..];
    if rest.starts_with(". ") || rest.starts_with(") ") {
        Some((indent, indent + digits + 2))
    } else {
        None
    }
}

/// Returns `(quote_depth, content_start_byte_rel)` for `> ` / `>> `.
fn parse_blockquote(raw: &str) -> Option<(usize, usize)> {
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut depth = 0usize;
    while i < bytes.len() {
        let mut spaces = 0usize;
        while i + spaces < bytes.len() && bytes[i + spaces] == b' ' && spaces < 3 {
            spaces += 1;
        }
        if i + spaces < bytes.len() && bytes[i + spaces] == b'>' {
            i += spaces + 1;
            depth += 1;
            if i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
        } else {
            break;
        }
    }
    if depth > 0 {
        Some((depth, i))
    } else {
        None
    }
}

fn push_raw_slice(s: &str, base_offset: usize, style: Style, out: &mut Vec<Glyph>) {
    for (rel, ch) in s.char_indices() {
        out.push(Glyph::from_source_char(ch, style, base_offset + rel));
    }
}

fn push_synthetic_str(s: &str, style: Style, src_byte: usize, out: &mut Vec<Glyph>) {
    for ch in s.chars() {
        out.push(Glyph::synthetic(ch, style, src_byte));
    }
}

#[allow(clippy::too_many_arguments)]
fn build_logical_line_glyphs(
    raw: &str,
    line_start: usize,
    width: usize,
    fence_kind: FenceLineKind<'_>,
    hide_delimiters: bool,
    theme: &Theme,
    out: &mut Vec<Glyph>,
    cont_prefix: &mut Vec<Glyph>,
) {
    let base_style = Style::default().fg(theme.fg_primary);
    let accent_marker = Style::default()
        .fg(theme.unread)
        .add_modifier(Modifier::BOLD);

    // 1. Fenced code block lines
    match fence_kind {
        FenceLineKind::Open(lang) => {
            if hide_delimiters {
                let label = if lang.is_empty() {
                    "─── code ───".to_string()
                } else {
                    format!("─── {lang} ───")
                };
                let shown = composer::truncate_display(&label, width);
                push_synthetic_str(&shown, theme.muted_style(), line_start, out);
            } else {
                push_raw_slice(
                    raw,
                    line_start,
                    theme.muted_style().add_modifier(Modifier::ITALIC),
                    out,
                );
            }
            return;
        }
        FenceLineKind::Close => {
            if hide_delimiters {
                let rule = "─".repeat(width.min(12).max(1));
                push_synthetic_str(&rule, theme.muted_style(), line_start, out);
            } else {
                push_raw_slice(
                    raw,
                    line_start,
                    theme.muted_style().add_modifier(Modifier::ITALIC),
                    out,
                );
            }
            return;
        }
        FenceLineKind::Body => {
            let code_style = theme.code_style();
            if hide_delimiters && width > 2 {
                let bar_style = if theme.no_color {
                    theme.muted_style()
                } else {
                    theme.muted_style().bg(theme.bg_code)
                };
                push_synthetic_str("▏ ", bar_style, line_start, out);
                cont_prefix.push(Glyph::synthetic('▏', bar_style, line_start));
                cont_prefix.push(Glyph::synthetic(' ', bar_style, line_start));
            }
            push_raw_slice(raw, line_start, code_style, out);
            return;
        }
        FenceLineKind::None => {}
    }

    // 2. Horizontal rule (`---`, `***`, `___`)
    if is_horizontal_rule(raw) {
        if hide_delimiters {
            let rule = "─".repeat(width.min(48).max(1));
            push_synthetic_str(&rule, theme.muted_style(), line_start, out);
        } else {
            push_raw_slice(raw, line_start, theme.muted_style(), out);
        }
        return;
    }

    // 3. Heading (`#`–`######`)
    if let Some((level, content_rel)) = parse_heading(raw) {
        let heading_color = match level {
            1 => theme.unread,
            2 => theme.accent,
            3 => theme.accent_soft,
            _ => theme.fg_primary,
        };
        let heading_style = Style::default()
            .fg(heading_color)
            .add_modifier(Modifier::BOLD);
        if !hide_delimiters {
            push_raw_slice(
                &raw[..content_rel],
                line_start,
                theme.muted_style().add_modifier(Modifier::BOLD),
                out,
            );
        }
        parse_inline_markdown(
            &raw[content_rel..],
            line_start + content_rel,
            heading_style,
            theme,
            hide_delimiters,
            out,
        );
        return;
    }

    // 4. Task checkboxes (`- [ ] ` / `- [x] ` / `* [ ] ` / `+ [x] `)
    if let Some((indent, checked, content_rel)) = parse_task_item(raw) {
        let task_text_style = if checked {
            theme.muted_style().add_modifier(Modifier::CROSSED_OUT)
        } else {
            base_style
        };
        let box_style = if checked {
            Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD)
        } else {
            accent_marker
        };
        if hide_delimiters {
            push_raw_slice(&raw[..indent], line_start, base_style, out);
            let box_ch = if checked { '☑' } else { '☐' };
            out.push(Glyph::synthetic(box_ch, box_style, line_start + indent));
            out.push(Glyph::synthetic(' ', base_style, line_start + content_rel));
        } else {
            push_raw_slice(&raw[..indent], line_start, base_style, out);
            push_raw_slice(&raw[indent..content_rel], line_start + indent, box_style, out);
        }
        parse_inline_markdown(
            &raw[content_rel..],
            line_start + content_rel,
            task_text_style,
            theme,
            hide_delimiters,
            out,
        );
        return;
    }

    // 5. Bullet lists (`- ` / `* ` / `+ `)
    if let Some((indent, content_rel)) = parse_bullet_item(raw) {
        if hide_delimiters {
            push_raw_slice(&raw[..indent], line_start, base_style, out);
            out.push(Glyph::synthetic('•', accent_marker, line_start + indent));
            out.push(Glyph::synthetic(' ', base_style, line_start + content_rel));
        } else {
            push_raw_slice(&raw[..indent], line_start, base_style, out);
            push_raw_slice(
                &raw[indent..content_rel],
                line_start + indent,
                accent_marker,
                out,
            );
        }
        parse_inline_markdown(
            &raw[content_rel..],
            line_start + content_rel,
            base_style,
            theme,
            hide_delimiters,
            out,
        );
        return;
    }

    // 6. Ordered lists (`1. `, `2. `)
    if let Some((indent, content_rel)) = parse_ordered_item(raw) {
        push_raw_slice(&raw[..indent], line_start, base_style, out);
        push_raw_slice(
            &raw[indent..content_rel],
            line_start + indent,
            accent_marker,
            out,
        );
        parse_inline_markdown(
            &raw[content_rel..],
            line_start + content_rel,
            base_style,
            theme,
            hide_delimiters,
            out,
        );
        return;
    }

    // 7. Blockquotes (`> `)
    if let Some((depth, content_rel)) = parse_blockquote(raw) {
        let quote_style = theme.quote_style();
        if hide_delimiters {
            for _ in 0..depth {
                out.push(Glyph::synthetic('┃', accent_marker, line_start));
                out.push(Glyph::synthetic(' ', quote_style, line_start + content_rel));
                cont_prefix.push(Glyph::synthetic('┃', accent_marker, line_start));
                cont_prefix.push(Glyph::synthetic(' ', quote_style, line_start));
            }
        } else {
            push_raw_slice(&raw[..content_rel], line_start, accent_marker, out);
        }
        parse_inline_markdown(
            &raw[content_rel..],
            line_start + content_rel,
            quote_style,
            theme,
            hide_delimiters,
            out,
        );
        return;
    }

    // 8. Standard paragraph line with inline Markdown
    parse_inline_markdown(raw, line_start, base_style, theme, hide_delimiters, out);
}

fn is_escapable_punctuation(b: u8) -> bool {
    matches!(
        b,
        b'\\' | b'`' | b'*' | b'_' | b'~' | b'[' | b']' | b'(' | b')' | b'#' | b'>' | b'-' | b'!'
    )
}

fn prev_char_is_word(s: &str, idx: usize) -> bool {
    s[..idx]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric())
}

fn next_char_is_word(s: &str, idx: usize) -> bool {
    s[idx..]
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric())
}

fn find_closing_delim(s: &str, start: usize, delim: &str, require_word_boundary: bool) -> Option<usize> {
    let bytes = s.as_bytes();
    let dlen = delim.len();
    let dbytes = delim.as_bytes();
    let mut i = start;
    while i + dlen <= bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == b'`' && delim != "`" {
            // Skip inline code span while searching for closing formatting delimiter.
            if let Some(close_bt) = s[i + 1..].find('`') {
                i = i + 1 + close_bt + 1;
                continue;
            }
        }
        if &bytes[i..i + dlen] == dbytes {
            // Ensure inner content is non-empty and doesn't end with whitespace.
            if i > start {
                let before_ws = s[start..i]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace);
                let boundary_ok = !require_word_boundary || !next_char_is_word(s, i + dlen);
                // For single `*` or `_`, avoid matching inside `**` or `__`.
                let single_ok = if dlen == 1 {
                    let prev_same = bytes.get(i.wrapping_sub(1)) == Some(&dbytes[0]);
                    let next_same = bytes.get(i + 1) == Some(&dbytes[0]);
                    !prev_same && !next_same
                } else {
                    true
                };
                if !before_ws && boundary_ok && single_ok {
                    return Some(i);
                }
            }
        }
        let ch = s[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}

/// Parses `[label](url)` or `![alt](url)` starting at byte `start` in `s`.
/// Returns `(is_image, label_start, label_end, url_start, url_end, total_end)`.
fn parse_inline_link(s: &str, start: usize) -> Option<(bool, usize, usize, usize, usize, usize)> {
    let bytes = s.as_bytes();
    let (is_image, bracket_pos) = if s[start..].starts_with("![") {
        (true, start + 1)
    } else if s[start..].starts_with('[') {
        (false, start)
    } else {
        return None;
    };

    let label_start = bracket_pos + 1;
    let mut i = label_start;
    let mut depth = 0usize;
    let mut label_end = None;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == b'[' {
            depth += 1;
        } else if bytes[i] == b']' {
            if depth == 0 {
                label_end = Some(i);
                break;
            }
            depth -= 1;
        }
        i += 1;
    }
    let label_end = label_end?;
    if label_end == label_start {
        return None;
    }
    if bytes.get(label_end + 1) != Some(&b'(') {
        return None;
    }

    let url_start = label_end + 2;
    let mut j = url_start;
    let mut pdepth = 0usize;
    let mut url_end = None;
    while j < bytes.len() {
        if bytes[j] == b'\\' && j + 1 < bytes.len() {
            j += 2;
            continue;
        }
        if bytes[j] == b'(' {
            pdepth += 1;
        } else if bytes[j] == b')' {
            if pdepth == 0 {
                url_end = Some(j);
                break;
            }
            pdepth -= 1;
        }
        j += 1;
    }
    let url_end = url_end?;
    Some((is_image, label_start, label_end, url_start, url_end, url_end + 1))
}

fn parse_inline_markdown(
    s: &str,
    base_offset: usize,
    base_style: Style,
    theme: &Theme,
    hide_delimiters: bool,
    out: &mut Vec<Glyph>,
) {
    let bytes = s.as_bytes();
    let delim_style = theme.muted_style();
    let mut i = 0usize;

    while i < bytes.len() {
        // 1. Backslash escape
        if bytes[i] == b'\\' && i + 1 < bytes.len() && is_escapable_punctuation(bytes[i + 1]) {
            if !hide_delimiters {
                out.push(Glyph::from_source_char('\\', delim_style, base_offset + i));
            }
            let escaped = bytes[i + 1] as char;
            out.push(Glyph::from_source_char(
                escaped,
                base_style,
                base_offset + i + 1,
            ));
            i += 2;
            continue;
        }

        // 2. Inline code `` `...` ``
        if bytes[i] == b'`' {
            if let Some(close_rel) = s[i + 1..].find('`') {
                if close_rel > 0 {
                    let close_idx = i + 1 + close_rel;
                    let mut code_style = theme.code_style();
                    if base_style.add_modifier.contains(Modifier::CROSSED_OUT) {
                        code_style = code_style.add_modifier(Modifier::CROSSED_OUT);
                    }
                    if !hide_delimiters {
                        out.push(Glyph::from_source_char('`', delim_style, base_offset + i));
                    }
                    push_raw_slice(&s[i + 1..close_idx], base_offset + i + 1, code_style, out);
                    if !hide_delimiters {
                        out.push(Glyph::from_source_char(
                            '`',
                            delim_style,
                            base_offset + close_idx,
                        ));
                    }
                    i = close_idx + 1;
                    continue;
                }
            }
        }

        // 3. Links `[label](url)` and Images `![alt](url)`
        if bytes[i] == b'[' || (bytes[i] == b'!' && bytes.get(i + 1) == Some(&b'[')) {
            if let Some((is_image, l_start, l_end, u_start, u_end, total_end)) =
                parse_inline_link(s, i)
            {
                let link_style = if is_image {
                    Style::default()
                        .fg(theme.unread)
                        .add_modifier(Modifier::BOLD)
                } else {
                    base_style
                        .fg(theme.accent)
                        .add_modifier(Modifier::UNDERLINED)
                };
                if hide_delimiters {
                    if is_image {
                        push_synthetic_str("◈ ", link_style, base_offset + i, out);
                    }
                    parse_inline_markdown(
                        &s[l_start..l_end],
                        base_offset + l_start,
                        link_style,
                        theme,
                        true,
                        out,
                    );
                } else {
                    push_raw_slice(&s[i..l_start], base_offset + i, delim_style, out);
                    parse_inline_markdown(
                        &s[l_start..l_end],
                        base_offset + l_start,
                        link_style,
                        theme,
                        false,
                        out,
                    );
                    push_raw_slice(&s[l_end..u_start], base_offset + l_end, delim_style, out);
                    push_raw_slice(&s[u_start..u_end], base_offset + u_start, delim_style, out);
                    push_raw_slice(
                        &s[u_end..total_end],
                        base_offset + u_end,
                        delim_style,
                        out,
                    );
                }
                i = total_end;
                continue;
            }
        }

        // 4. Bold + Italic `***...***`
        if s[i..].starts_with("***")
            && s[i + 3..].chars().next().is_some_and(|c| !c.is_whitespace())
        {
            if let Some(close_idx) = find_closing_delim(s, i + 3, "***", false) {
                let style = base_style.add_modifier(Modifier::BOLD | Modifier::ITALIC);
                if !hide_delimiters {
                    push_raw_slice(&s[i..i + 3], base_offset + i, delim_style, out);
                }
                parse_inline_markdown(
                    &s[i + 3..close_idx],
                    base_offset + i + 3,
                    style,
                    theme,
                    hide_delimiters,
                    out,
                );
                if !hide_delimiters {
                    push_raw_slice(
                        &s[close_idx..close_idx + 3],
                        base_offset + close_idx,
                        delim_style,
                        out,
                    );
                }
                i = close_idx + 3;
                continue;
            }
        }

        // 5. Bold `**...**`
        if s[i..].starts_with("**")
            && !s[i..].starts_with("***")
            && s[i + 2..].chars().next().is_some_and(|c| !c.is_whitespace())
        {
            if let Some(close_idx) = find_closing_delim(s, i + 2, "**", false) {
                let style = base_style.add_modifier(Modifier::BOLD);
                if !hide_delimiters {
                    push_raw_slice(&s[i..i + 2], base_offset + i, delim_style, out);
                }
                parse_inline_markdown(
                    &s[i + 2..close_idx],
                    base_offset + i + 2,
                    style,
                    theme,
                    hide_delimiters,
                    out,
                );
                if !hide_delimiters {
                    push_raw_slice(
                        &s[close_idx..close_idx + 2],
                        base_offset + close_idx,
                        delim_style,
                        out,
                    );
                }
                i = close_idx + 2;
                continue;
            }
        }

        // 6. Bold `__...__` (word-boundary guarded)
        if s[i..].starts_with("__")
            && !prev_char_is_word(s, i)
            && s[i + 2..].chars().next().is_some_and(|c| !c.is_whitespace())
        {
            if let Some(close_idx) = find_closing_delim(s, i + 2, "__", true) {
                let style = base_style.add_modifier(Modifier::BOLD);
                if !hide_delimiters {
                    push_raw_slice(&s[i..i + 2], base_offset + i, delim_style, out);
                }
                parse_inline_markdown(
                    &s[i + 2..close_idx],
                    base_offset + i + 2,
                    style,
                    theme,
                    hide_delimiters,
                    out,
                );
                if !hide_delimiters {
                    push_raw_slice(
                        &s[close_idx..close_idx + 2],
                        base_offset + close_idx,
                        delim_style,
                        out,
                    );
                }
                i = close_idx + 2;
                continue;
            }
        }

        // 7. Strikethrough `~~...~~`
        if s[i..].starts_with("~~")
            && s[i + 2..].chars().next().is_some_and(|c| !c.is_whitespace())
        {
            if let Some(close_idx) = find_closing_delim(s, i + 2, "~~", false) {
                let style = base_style.add_modifier(Modifier::CROSSED_OUT);
                if !hide_delimiters {
                    push_raw_slice(&s[i..i + 2], base_offset + i, delim_style, out);
                }
                parse_inline_markdown(
                    &s[i + 2..close_idx],
                    base_offset + i + 2,
                    style,
                    theme,
                    hide_delimiters,
                    out,
                );
                if !hide_delimiters {
                    push_raw_slice(
                        &s[close_idx..close_idx + 2],
                        base_offset + close_idx,
                        delim_style,
                        out,
                    );
                }
                i = close_idx + 2;
                continue;
            }
        }

        // 8. Italic `*...*`
        if bytes[i] == b'*'
            && bytes.get(i + 1) != Some(&b'*')
            && s[i + 1..].chars().next().is_some_and(|c| !c.is_whitespace())
        {
            if let Some(close_idx) = find_closing_delim(s, i + 1, "*", false) {
                let style = base_style.add_modifier(Modifier::ITALIC);
                if !hide_delimiters {
                    out.push(Glyph::from_source_char('*', delim_style, base_offset + i));
                }
                parse_inline_markdown(
                    &s[i + 1..close_idx],
                    base_offset + i + 1,
                    style,
                    theme,
                    hide_delimiters,
                    out,
                );
                if !hide_delimiters {
                    out.push(Glyph::from_source_char(
                        '*',
                        delim_style,
                        base_offset + close_idx,
                    ));
                }
                i = close_idx + 1;
                continue;
            }
        }

        // 9. Italic `_..._` (word-boundary guarded so snake_case stays literal)
        if bytes[i] == b'_'
            && bytes.get(i + 1) != Some(&b'_')
            && !prev_char_is_word(s, i)
            && s[i + 1..].chars().next().is_some_and(|c| !c.is_whitespace())
        {
            if let Some(close_idx) = find_closing_delim(s, i + 1, "_", true) {
                let style = base_style.add_modifier(Modifier::ITALIC);
                if !hide_delimiters {
                    out.push(Glyph::from_source_char('_', delim_style, base_offset + i));
                }
                parse_inline_markdown(
                    &s[i + 1..close_idx],
                    base_offset + i + 1,
                    style,
                    theme,
                    hide_delimiters,
                    out,
                );
                if !hide_delimiters {
                    out.push(Glyph::from_source_char(
                        '_',
                        delim_style,
                        base_offset + close_idx,
                    ));
                }
                i = close_idx + 1;
                continue;
            }
        }

        // 10. Literal character
        let ch = s[i..].chars().next().unwrap();
        out.push(Glyph::from_source_char(ch, base_style, base_offset + i));
        i += ch.len_utf8();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{OnenotePageSummary, OnenoteScope};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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

    fn buffer_row_text(term: &Terminal<TestBackend>, y: u16, width: u16) -> String {
        let buf = term.backend().buffer();
        let mut out = String::new();
        let mut x = 0u16;
        while x < width {
            let sym = buf[(x, y)].symbol();
            out.push_str(sym);
            let step = UnicodeWidthStr::width(sym).max(1) as u16;
            x = x.saturating_add(step);
        }
        out.trim_end().to_string()
    }

    #[test]
    fn inline_editor_supports_korean_typing_cursor_navigation_undo_and_hardware_cursor() {
        let detail = sample_detail(
            r#"<!DOCTYPE html><html><head><title>Daily Note</title></head><body><div id="div:{1}"><p id="p:{1}">First line</p><p id="p:{2}">Second line</p></div></body></html>"#,
        );
        let mut editor = OnenoteInlineEditor::open_for_page(&detail, Some(0));
        assert!(matches!(editor.target, InlineEditTarget::PageBody { .. }));
        assert_eq!(editor.mode, EditorMode::LivePreview);
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

    #[test]
    fn live_preview_hides_delimiters_on_inactive_lines_and_tracks_exact_cjk_cursor_on_active_line()
    {
        let detail = sample_detail(
            r#"<!DOCTYPE html><html><head><title>Live Preview</title></head><body><div id="div:{1}"><p id="p:{1}">Init</p></div></body></html>"#,
        );
        let mut editor = OnenoteInlineEditor::open_for_page(&detail, None);
        assert_eq!(editor.mode, EditorMode::LivePreview);

        let md = [
            "# Heading One",
            "- [ ] Open task with **bold** and *italic*",
            "- [x] Done task with ~~old~~ and `code`",
            "- Bullet item with [OneNote](https://onenote.com)",
            "> Quoted wisdom",
            "---",
            "```rust",
            "let x = **not_bold**;",
            "```",
            "## 한글 active **라인**",
        ]
        .join("\n");
        editor.composer.load_text(&md);
        // Cursor is at end of the last line ("## 한글 active **라인**").
        let active_line_str = "## 한글 active **라인**";
        let expected_cursor_col = UnicodeWidthStr::width(active_line_str) as u16;

        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let theme = Theme::default();
        let mut hitmap = HitMap::default();
        let mut cursor = None;
        term.draw(|f| {
            cursor = editor.draw(f, f.area(), &theme, &mut hitmap);
        })
        .unwrap();

        let y0 = editor.last_editor_area.y;
        // Row 0: `# Heading One` -> rendered without `# `
        let row0 = buffer_row_text(&term, y0, 80);
        assert_eq!(row0, "Heading One");
        assert!(!row0.contains('#'));

        // Row 1: `- [ ] Open task with **bold** and *italic*` -> `☐ `, no `**` / `*`
        let row1 = buffer_row_text(&term, y0 + 1, 80);
        assert!(row1.starts_with("☐ Open task with bold and italic"), "row1={row1:?}");
        assert!(!row1.contains("- [ ]") && !row1.contains("**") && !row1.contains('*'));

        // Row 2: `- [x] Done task with ~~old~~ and \`code\`` -> `☑ `, no `~~` / `` ` ``
        let row2 = buffer_row_text(&term, y0 + 2, 80);
        assert!(row2.starts_with("☑ Done task with old and code"), "row2={row2:?}");
        assert!(!row2.contains("~~") && !row2.contains('`'));

        // Row 3: Bullet list & link `[OneNote](https://onenote.com)` -> `• ` + underlined label, no URL
        let row3 = buffer_row_text(&term, y0 + 3, 80);
        assert_eq!(row3, "• Bullet item with OneNote");
        assert!(!row3.contains("https://"));

        // Row 4: Blockquote `> Quoted wisdom` -> `┃ Quoted wisdom`
        let row4 = buffer_row_text(&term, y0 + 4, 80);
        assert_eq!(row4, "┃ Quoted wisdom");

        // Row 5: Horizontal rule `---` -> `────...`
        let row5 = buffer_row_text(&term, y0 + 5, 80);
        assert!(row5.starts_with("──────────"), "row5={row5:?}");
        assert!(!row5.contains("---"));

        // Row 6..8: Fenced code block -> subtle marker + literal code (NOT stripping `**`)
        let row6 = buffer_row_text(&term, y0 + 6, 80);
        assert!(row6.contains("rust") && !row6.contains("```"), "row6={row6:?}");
        let row7 = buffer_row_text(&term, y0 + 7, 80);
        assert!(row7.contains("let x = **not_bold**;"), "row7={row7:?}");
        let row8 = buffer_row_text(&term, y0 + 8, 80);
        assert!(row8.starts_with('─') && !row8.contains("```"), "row8={row8:?}");

        // Row 9 (ACTIVE line): exposes raw Markdown `## 한글 active **라인**`!
        let row9 = buffer_row_text(&term, y0 + 9, 80);
        assert_eq!(row9, active_line_str);

        // Exact hardware cursor `(cx, cy)` accounts for ASCII + Korean wide chars!
        let (cx, cy) = cursor.expect("active line must return exact hardware cursor");
        assert_eq!(cy, y0 + 9);
        assert_eq!(cx, editor.last_editor_area.x + expected_cursor_col);

        // Move cursor left across `**` and `인` (3 chars from end) -> check exact CJK column shift
        editor.handle_action(&Action::CursorLeft); // before final `*`
        editor.handle_action(&Action::CursorLeft); // before first `*`
        editor.handle_action(&Action::CursorLeft); // before `인` (width 2)
        let prefix_before_cursor = "## 한글 active **라";
        let expected_mid_col = UnicodeWidthStr::width(prefix_before_cursor) as u16;
        term.draw(|f| {
            cursor = editor.draw(f, f.area(), &theme, &mut hitmap);
        })
        .unwrap();
        let (cx2, cy2) = cursor.unwrap();
        assert_eq!(cy2, y0 + 9);
        assert_eq!(cx2, editor.last_editor_area.x + expected_mid_col);
    }

    #[test]
    fn clicking_inactive_rendered_line_moves_cursor_expands_it_and_collapses_previous_line() {
        let detail = sample_detail(
            r#"<!DOCTYPE html><html><head><title>Click Test</title></head><body><div id="div:{1}"><p id="p:{1}">Init</p></div></body></html>"#,
        );
        let mut editor = OnenoteInlineEditor::open_for_page(&detail, None);
        editor
            .composer
            .load_text("# Title **Bold**\n- [ ] Task 한글\nLast **active** line");
        // Cursor starts on line 2 ("Last **active** line").

        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let theme = Theme::default();
        let mut hitmap = HitMap::default();
        term.draw(|f| {
            editor.draw(f, f.area(), &theme, &mut hitmap);
        })
        .unwrap();

        let y0 = editor.last_editor_area.y;
        let x0 = editor.last_editor_area.x;
        // Before click: line 0 is rendered ("Title Bold"), line 2 is raw ("Last **active** line")
        assert_eq!(buffer_row_text(&term, y0, 80), "Title Bold");
        assert_eq!(buffer_row_text(&term, y0 + 2, 80), "Last **active** line");

        // Click on `B` in rendered "Title Bold" (visual col 6 on row y0)
        assert!(editor.handle_click(x0 + 6, y0));
        // Cursor should land right on 'B' inside `# Title **Bold**` (byte offset 10)
        assert_eq!(&editor.composer.text()[editor.composer.cursor()..editor.composer.cursor() + 4], "Bold");

        // Redraw: line 0 is now expanded to raw `# Title **Bold**`, line 2 collapsed to `Last active line`!
        let mut cursor = None;
        term.draw(|f| {
            cursor = editor.draw(f, f.area(), &theme, &mut hitmap);
        })
        .unwrap();
        assert_eq!(buffer_row_text(&term, y0, 80), "# Title **Bold**");
        assert_eq!(buffer_row_text(&term, y0 + 2, 80), "Last active line");
        let (cx, cy) = cursor.unwrap();
        assert_eq!(cy, y0);
        assert_eq!(cx, x0 + 10); // `# Title **` has display width 10

        // Now click on `한` in rendered `☐ Task 한글` (col 7 = "☐ Task " width 7) on row y0 + 1
        assert!(editor.handle_click(x0 + 7, y0 + 1));
        assert!(editor.composer.text()[editor.composer.cursor()..].starts_with("한글"));

        term.draw(|f| {
            editor.draw(f, f.area(), &theme, &mut hitmap);
        })
        .unwrap();
        // Line 0 collapsed back to rendered, line 1 expanded to raw `- [ ] Task 한글`
        assert_eq!(buffer_row_text(&term, y0, 80), "Title Bold");
        assert_eq!(buffer_row_text(&term, y0 + 1, 80), "- [ ] Task 한글");
    }

    #[test]
    fn ctrl_m_and_mode_pill_toggle_between_live_preview_and_source() {
        let keymap = crate::ui::keymap::KeyMap;
        let ctrl_m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL);
        let action = crate::ui::event::key_to_action(
            ctrl_m,
            crate::ui::app::Focus::Canvas,
            &keymap,
            true,
        );
        assert_eq!(action, Some(Action::InlineToggleMode));

        let detail = sample_detail(
            r#"<!DOCTYPE html><html><head><title>Mode Test</title></head><body><div id="div:{1}"><p id="p:{1}">Init</p></div></body></html>"#,
        );
        let mut editor = OnenoteInlineEditor::open_for_page(&detail, None);
        editor.composer.load_text("# Heading\nSecond line");
        assert_eq!(editor.mode, EditorMode::LivePreview);

        let mut term = Terminal::new(TestBackend::new(80, 16)).unwrap();
        let theme = Theme::default();
        let mut hitmap = HitMap::default();
        term.draw(|f| {
            editor.draw(f, f.area(), &theme, &mut hitmap);
        })
        .unwrap();

        let y0 = editor.last_editor_area.y;
        // Header shows clickable `[Live]` pill and registers `ButtonId::InlineToggleMode`
        let header = buffer_row_text(&term, 0, 80);
        assert!(header.contains("[Live]"), "header={header:?}");
        assert!(
            hitmap
                .region_of(HitTarget::Button(ButtonId::InlineToggleMode))
                .is_some(),
            "[Live] pill must register ButtonId::InlineToggleMode in HitMap"
        );
        // Inactive line 0 is rendered without `# `
        assert_eq!(buffer_row_text(&term, y0, 80), "Heading");

        // Toggle via Ctrl+M action -> Source mode exposes raw `# Heading` on inactive lines too
        assert_eq!(
            editor.handle_action(&action.unwrap()),
            InlineEditEvent::ModeChanged
        );
        assert_eq!(editor.mode, EditorMode::Source);

        hitmap.clear();
        term.draw(|f| {
            editor.draw(f, f.area(), &theme, &mut hitmap);
        })
        .unwrap();
        let header_src = buffer_row_text(&term, 0, 80);
        assert!(header_src.contains("[Src]"), "header_src={header_src:?}");
        assert_eq!(buffer_row_text(&term, y0, 80), "# Heading");

        // Toggle back -> LivePreview
        editor.handle_action(&Action::InlineToggleMode);
        assert_eq!(editor.mode, EditorMode::LivePreview);
    }

    #[test]
    fn live_preview_handles_soft_wrapping_for_cjk_active_cursor_and_inactive_clicks() {
        let detail = sample_detail(
            r#"<!DOCTYPE html><html><head><title>Wrap Test</title></head><body><div id="div:{1}"><p id="p:{1}">Init</p></div></body></html>"#,
        );
        let mut editor = OnenoteInlineEditor::open_for_page(&detail, None);
        // Width = 20 cols:
        // Line 0 (inactive): "# 가나다라마바사아자차카타" -> rendered "가나다라마바사아자차카타" (24 cols) -> wraps to 2 rows (10 chars = 20 cols on row 0, 2 chars = 4 cols on row 1)
        // Line 1 (active):   "**한글** soft wrap 테스트" -> 25 cols -> wraps onto row 2 (20 cols) and row 3 (5 cols)
        editor
            .composer
            .load_text("# 가나다라마바사아자차카타\n**한글** soft wrap 테스트");

        let mut term = Terminal::new(TestBackend::new(20, 12)).unwrap();
        let theme = Theme::default();
        let mut hitmap = HitMap::default();
        let mut cursor = None;
        term.draw(|f| {
            cursor = editor.draw(f, f.area(), &theme, &mut hitmap);
        })
        .unwrap();

        let y0 = editor.last_editor_area.y;
        let x0 = editor.last_editor_area.x;
        // Row 0 & 1: wrapped inactive heading (no `# `)
        let r0 = buffer_row_text(&term, y0, 20);
        let r1 = buffer_row_text(&term, y0 + 1, 20);
        assert_eq!(r0, "가나다라마바사아자차");
        assert_eq!(r1, "카타");

        // Row 2 & 3: wrapped active raw Markdown line (`**한글** soft wrap ` = 19 cols, `테` wraps)
        let r2 = buffer_row_text(&term, y0 + 2, 20);
        let r3 = buffer_row_text(&term, y0 + 3, 20);
        assert_eq!(r2, "**한글** soft wrap");
        assert_eq!(r3, "테스트");
        let (cx, cy) = cursor.unwrap();
        assert_eq!(cy, y0 + 3);
        assert_eq!(cx, x0 + UnicodeWidthStr::width("테스트") as u16);

        // Clicking on '타' on wrapped row 1 (col 2) moves cursor into line 0 right at '타'
        assert!(editor.handle_click(x0 + 2, y0 + 1));
        assert!(
            editor.composer.text()[editor.composer.cursor()..].starts_with("타"),
            "cursor should land on '타', got {:?}",
            &editor.composer.text()[editor.composer.cursor()..]
        );
    }
}
