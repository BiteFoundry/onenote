//! Page Resources & Attachments tray overlay (`A`) for downloading or previewing embedded OneNote images and files.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::OnenotePageDetail,
    ui::{
        action::Action,
        app::UiState,
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRow {
    pub resource_index: usize,
    pub name: String,
    pub mime_type: String,
    pub is_image: bool,
    pub is_html: bool,
    pub is_markdown: bool,
}

impl ResourceRow {
    pub fn label(&self) -> String {
        let icon = if self.is_html {
            "[HTML]"
        } else if self.is_markdown {
            "[MD]"
        } else if self.is_image {
            "[IMG]"
        } else {
            "[FILE]"
        };
        format!("{icon} {} ({})", self.name, self.mime_type)
    }
}

pub fn rows_from_onenote_page(detail: &OnenotePageDetail) -> Vec<ResourceRow> {
    detail
        .resources
        .iter()
        .enumerate()
        .map(|(resource_index, res)| {
            let name = res
                .file_name
                .as_deref()
                .or(res.alt_text.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if res.is_image {
                        format!("image-{}", resource_index + 1)
                    } else {
                        format!("resource-{}", resource_index + 1)
                    }
                });
            let kind = bitefoundry_html::sniff_content_kind(
                Some(&res.mime_type),
                res.file_name.as_deref().or(Some(name.as_str())),
                None,
            );
            let is_html = kind == bitefoundry_html::ContentKind::Html;
            let is_markdown = kind == bitefoundry_html::ContentKind::Markdown;
            ResourceRow {
                resource_index,
                name,
                mime_type: res.mime_type.clone(),
                is_image: res.is_image && !is_html && !is_markdown,
                is_html,
                is_markdown,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct AttachmentsState {
    pub selected: usize,
}

impl AttachmentsState {
    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn handle(&mut self, action: &Action, detail: Option<&OnenotePageDetail>) -> OverlayOutcome {
        let items = detail.map(rows_from_onenote_page).unwrap_or_default();
        match action {
            Action::MoveUp => {
                self.selected = self.selected.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                if !items.is_empty() && self.selected + 1 < items.len() {
                    self.selected += 1;
                }
                OverlayOutcome::Handled
            }
            Action::Activate => {
                let idx = self.selected.min(items.len().saturating_sub(1));
                match items.get(idx) {
                    Some(row) if row.is_html || row.is_markdown => OverlayOutcome::CloseAndDispatch(
                        Action::OpenHtmlResource(row.resource_index),
                    ),
                    Some(row) => OverlayOutcome::CloseAndDispatch(Action::DownloadResource(
                        row.resource_index,
                    )),
                    None => OverlayOutcome::Close,
                }
            }
            Action::Input('d') => {
                let idx = self.selected.min(items.len().saturating_sub(1));
                match items.get(idx) {
                    Some(row) => OverlayOutcome::CloseAndDispatch(Action::DownloadResource(
                        row.resource_index,
                    )),
                    None => OverlayOutcome::Close,
                }
            }
            Action::Input('v')
            | Action::OpenImagePreview
            | Action::OpenHtmlPreview
            | Action::Input('H') => {
                let idx = self.selected.min(items.len().saturating_sub(1));
                match items.get(idx) {
                    Some(row) if row.is_html || row.is_markdown => OverlayOutcome::CloseAndDispatch(
                        Action::OpenHtmlResource(row.resource_index),
                    ),
                    Some(row) if row.is_image => OverlayOutcome::CloseAndDispatch(
                        Action::OpenImagePreviewAt(row.resource_index),
                    ),
                    _ => OverlayOutcome::Handled,
                }
            }
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        detail: Option<&OnenotePageDetail>,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        hitmap.add(area, HitTarget::OverlayBackdrop);
        let items = detail.map(rows_from_onenote_page).unwrap_or_default();
        let rect = centered_rect(area, 72, (items.len() as u16 + 5).clamp(7, 20));
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                format!(
                    " Page Resources ({}) · Enter/v Preview · d Download · Esc Close ",
                    items.len()
                ),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "No embedded images or attachments on this page.",
                    theme.muted_style(),
                )),
                inner,
            );
            return;
        }

        let height = inner.height as usize;
        let selected = self.selected.min(items.len() - 1);
        let offset = crate::ui::nav_pane::selection_offset(selected, height, items.len());
        for (row_idx, row) in items.iter().skip(offset).take(height).enumerate() {
            let idx = offset + row_idx;
            let y = inner.y + row_idx as u16;
            let style = if idx == selected {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg)
            };
            let text = crate::ui::truncate_width(&row.label(), inner.width as usize);
            frame.render_widget(
                Paragraph::new(Span::styled(text, style)).style(style),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: idx });
        }
    }
}
