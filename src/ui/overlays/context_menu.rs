//! Context Menu Overlay (`.` or right-click) for OneNote pages, blocks, and global actions.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{HitMap, HitTarget},
    overlays::{OverlayKind, OverlayOutcome},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTarget {
    PageItem,
    CanvasBlock,
    Global,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<String>,
    pub action: Action,
}

impl MenuItem {
    fn new(label: impl Into<String>, shortcut: Option<&str>, action: Action) -> Self {
        Self {
            label: label.into(),
            shortcut: shortcut.map(str::to_string),
            action,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextMenu {
    pub context: ContextTarget,
    pub anchor: (u16, u16),
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

impl ContextMenu {
    pub fn build(target: ContextTarget, anchor: (u16, u16), has_resources: bool) -> Self {
        let mut items = match target {
            ContextTarget::PageItem => vec![
                MenuItem::new("New Page", Some("n"), Action::OpenOnenoteCreatePage { level: 0 }),
                MenuItem::new("Append Markdown", Some("a"), Action::OpenOnenoteAppend),
                MenuItem::new("Extract Links", Some("u"), Action::OpenLinkExtractor),
                MenuItem::new("Inspect Page HTML", Some("H"), Action::OpenHtmlPreview),
                MenuItem::new("Open in Browser", Some("o"), Action::OnenoteOpenWeb),
                MenuItem::new("Copy Page Markdown", Some("y"), Action::CopyActivePageMarkdown),
                MenuItem::new("Delete Page", Some("D"), Action::ConfirmDeleteOnenotePage),
            ],
            ContextTarget::CanvasBlock => vec![
                MenuItem::new("Edit Selected Block", Some("e"), Action::OpenOnenoteEditBlock),
                MenuItem::new("Toggle To-Do Checkbox", Some("Space"), Action::OnenoteToggleTodo(None)),
                MenuItem::new("Append Markdown", Some("a"), Action::OpenOnenoteAppend),
                MenuItem::new("Extract Links", Some("u"), Action::OpenLinkExtractor),
                MenuItem::new("Toggle Inline Link URLs", Some("h"), Action::ToggleShowLinkUrls),
                MenuItem::new("Inspect Page HTML", Some("H"), Action::OpenHtmlPreview),
                MenuItem::new("Preview Images", Some("v"), Action::OpenImagePreview),
                MenuItem::new(
                    "Resources & Attachments",
                    Some("A"),
                    Action::OpenOverlay(OverlayKind::Attachments),
                ),
                MenuItem::new("Open in Browser", Some("o"), Action::OnenoteOpenWeb),
            ],
            ContextTarget::Global => vec![
                MenuItem::new("New Page", Some("n"), Action::OpenOnenoteCreatePage { level: 0 }),
                MenuItem::new("New Section", Some("S"), Action::OpenOnenoteCreateSection),
                MenuItem::new("New Notebook", Some("B"), Action::OpenOnenoteCreateNotebook),
                MenuItem::new("Search Pages", Some("/"), Action::OpenSearch),
                MenuItem::new("Refresh All", Some("Ctrl+R"), Action::RefreshAll),
                MenuItem::new(
                    "Settings",
                    Some("Ctrl+,"),
                    Action::OpenOverlay(OverlayKind::Settings),
                ),
            ],
        };

        if matches!(target, ContextTarget::PageItem) && has_resources {
            items.push(MenuItem::new(
                "Resources & Attachments",
                Some("A"),
                Action::OpenOverlay(OverlayKind::Attachments),
            ));
            items.push(MenuItem::new(
                "Preview Images",
                Some("v"),
                Action::OpenImagePreview,
            ));
        }

        Self {
            context: target,
            anchor,
            items,
            selected: 0,
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
        }
    }

    pub fn prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + self.items.len() - 1) % self.items.len();
        }
    }

    pub fn next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    pub fn activate(&self) -> OverlayOutcome {
        match self.items.get(self.selected) {
            Some(item) => OverlayOutcome::CloseAndDispatch(item.action.clone()),
            None => OverlayOutcome::Close,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let width = 34u16.min(area.width.saturating_sub(2)).max(18);
        let height = (self.items.len() as u16 + 2).min(area.height.saturating_sub(2)).max(4);
        let x = self
            .anchor
            .0
            .min(area.x + area.width.saturating_sub(width));
        let y = self
            .anchor
            .1
            .min(area.y + area.height.saturating_sub(height));
        let rect = Rect::new(x, y, width, height);

        hitmap.add(area, HitTarget::OverlayBackdrop);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(" Actions ", theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let height = inner.height as usize;
        let selected = self.selected.min(self.items.len().saturating_sub(1));
        let offset = crate::ui::nav_pane::selection_offset(selected, height, self.items.len());
        for (row_idx, item) in self.items.iter().skip(offset).take(height).enumerate() {
            let idx = offset + row_idx;
            let row_y = inner.y + row_idx as u16;
            let is_sel = idx == selected;
            let style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg)
            };
            let sc = item
                .shortcut
                .as_deref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            let left_w = (inner.width as usize).saturating_sub(sc.len() + 2);
            let left = crate::ui::truncate_width(&item.label, left_w);
            let line = Line::from(vec![
                Span::styled(format!(" {left}"), style),
                Span::styled(
                    sc,
                    if is_sel {
                        style
                    } else {
                        Style::default().bg(theme.bg_elevated).fg(theme.fg_muted)
                    },
                ),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(style),
                Rect::new(inner.x, row_y, inner.width, 1),
            );
            hitmap.add_line(inner.x, row_y, inner.width, HitTarget::OverlayItem { index: idx });
        }
    }
}
