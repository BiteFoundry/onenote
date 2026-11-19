//! Unified Omni-Switcher & Command Palette (`Ctrl+P` / `Ctrl+K`).
//!
//! Prefix modes:
//! - (none): Notebooks, sections, active section pages, and commands
//! - `>`: commands only (`>New Page`, `>Sync`, `>Cycle Theme`, ...)
//! - `#`: notebooks & sections only
//! - `/`: hands off query to `SearchOverlay` on Enter

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
    overlays::{centered_rect, OverlayKind, OverlayOutcome},
};

#[derive(Debug, Clone)]
pub struct PaletteItem {
    pub label: String,
    pub shortcut: String,
    pub action: Action,
    pub priority: i32,
}

fn palette_commands() -> Vec<PaletteItem> {
    let cmds = [
        ("New OneNote Page", "n", Action::OpenOnenoteCreatePage { level: 0 }),
        ("New OneNote Subpage", "N", Action::OpenOnenoteCreatePage { level: 1 }),
        ("Append Markdown Block", "a", Action::OpenOnenoteAppend),
        ("Edit Selected Block", "e", Action::OpenOnenoteEditBlock),
        ("Toggle Selected To-Do Checkbox", "Space / x", Action::OnenoteToggleTodo(None)),
        ("New Notebook", "B", Action::OpenOnenoteCreateNotebook),
        ("New Section", "S", Action::OpenOnenoteCreateSection),
        ("Delete Active Page", "D / Del", Action::ConfirmDeleteOnenotePage),
        ("Open Active Page in Browser", "o", Action::OnenoteOpenWeb),
        ("Copy Page Markdown", "y", Action::CopyActivePageMarkdown),
        ("Search OneNote Pages (FTS5)", "/", Action::OpenSearch),
        ("Extract Links from Page", "u", Action::OpenLinkExtractor),
        ("Toggle Inline Link URLs (href)", "h", Action::ToggleShowLinkUrls),
        ("Preview Embedded Images", "v / Alt+v", Action::OpenImagePreview),
        ("Page Resources & Attachments", "A", Action::OpenOverlay(OverlayKind::Attachments)),
        ("Open File Picker (zoxide)", "Ctrl+F", Action::OpenFilePicker),
        ("Sync / Refresh All", "Ctrl+R / F5", Action::RefreshAll),
        ("Toggle Left Sidebar", "Ctrl+B", Action::ToggleNavRail),
        ("Toggle Density (Cozy / Compact)", "Ctrl+D", Action::ToggleDensity),
        ("Cycle Color Theme", "", Action::CycleTheme),
        ("Cycle Image Protocol", "", Action::CycleImageProtocol),
        ("Toggle Inline Image Previews", "", Action::ToggleInlineImages),
        ("Toggle Notifications", "", Action::ToggleNotify),
        ("Check for Updates", "", Action::CheckForUpdates),
        ("Install Self-Update", "", Action::ApplySelfUpdate),
        ("Settings", "Ctrl+,", Action::OpenOverlay(OverlayKind::Settings)),
        ("Keyboard Shortcuts Help", "?", Action::OpenOverlay(OverlayKind::Help)),
        ("Quit OneNote", "Ctrl+Q", Action::QuitConfirm),
    ];

    cmds.into_iter()
        .map(|(label, shortcut, action)| PaletteItem {
            label: label.to_string(),
            shortcut: shortcut.to_string(),
            action,
            priority: 10,
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct CommandPalette {
    pub query: String,
    pub selected: usize,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn filtered_items(&self, app: &AppState) -> Vec<PaletteItem> {
        let raw = self.query.trim();
        let (mode, needle) = if let Some(rest) = raw.strip_prefix('>') {
            ('>', rest.trim().to_ascii_lowercase())
        } else if let Some(rest) = raw.strip_prefix('#') {
            ('#', rest.trim().to_ascii_lowercase())
        } else {
            (' ', raw.to_ascii_lowercase())
        };

        let mut all = Vec::new();
        if mode != '>' {
            for nb in &app.notebooks {
                all.push(PaletteItem {
                    label: format!("Notebook: {}", nb.display_name),
                    shortcut: nb.scope.owner_label(),
                    action: Action::SelectNotebook(nb.notebook_key.clone()),
                    priority: 30,
                });
            }
            for sec in &app.sections {
                let nb_prefix = sec
                    .notebook_name
                    .as_deref()
                    .map(|n| format!("{n} › "))
                    .unwrap_or_default();
                all.push(PaletteItem {
                    label: format!("Section: {nb_prefix}{}", sec.display_name),
                    shortcut: "Section".to_string(),
                    action: Action::SelectSection(sec.section_key.clone()),
                    priority: 25,
                });
            }
            if mode == ' ' {
                for p in app.pages.iter().take(30) {
                    all.push(PaletteItem {
                        label: format!("Page: {}", p.title),
                        shortcut: "Page".to_string(),
                        action: Action::SelectPage(p.page_key.clone()),
                        priority: 20,
                    });
                }
            }
        }
        if mode != '#' {
            all.extend(palette_commands());
        }

        if needle.is_empty() {
            return all;
        }

        let mut scored: Vec<(i32, PaletteItem)> = all
            .into_iter()
            .filter_map(|item| {
                let hay = item.label.to_ascii_lowercase();
                if hay.starts_with(&needle) {
                    Some((item.priority + 100, item))
                } else if hay.contains(&needle) {
                    Some((item.priority + 50, item))
                } else if fuzzy_subsequence(&hay, &needle) {
                    Some((item.priority, item))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
        scored.into_iter().map(|(_, i)| i).collect()
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, _ui: &UiState) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.query.push_str(&text.replace(['\r', '\n'], " "));
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                let _ = self.query.pop();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::MoveUp => {
                self.selected = self.selected.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                let len = self.filtered_items(app).len();
                if len > 0 && self.selected + 1 < len {
                    self.selected += 1;
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                if let Some(search_q) = self.query.trim().strip_prefix('/') {
                    let q = search_q.trim().to_string();
                    if !q.is_empty() {
                        return OverlayOutcome::CloseAndDispatch(Action::SearchLocalPages(q));
                    }
                    return OverlayOutcome::CloseAndDispatch(Action::OpenSearch);
                }
                let items = self.filtered_items(app);
                let idx = self.selected.min(items.len().saturating_sub(1));
                match items.get(idx) {
                    Some(item) => OverlayOutcome::CloseAndDispatch(item.action.clone()),
                    None => OverlayOutcome::Close,
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
        let rect = centered_rect(
            area,
            area.width.saturating_sub(8).min(84).max(40),
            area.height.saturating_sub(4).min(22).max(10),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Command Palette (> commands · # notebooks/sections · / search) ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" > ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                Span::styled(self.query.clone(), Style::default().fg(theme.fg)),
                Span::styled("▌", Style::default().fg(theme.accent)),
            ])),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let items = self.filtered_items(app);
        let list_y = inner.y + 2;
        let avail_h = inner.height.saturating_sub(2) as usize;
        if avail_h == 0 {
            return;
        }

        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled("No matching commands or items.", theme.muted_style())),
                Rect::new(inner.x, list_y, inner.width, 1),
            );
            return;
        }

        let selected = self.selected.min(items.len() - 1);
        let offset = crate::ui::nav_pane::selection_offset(selected, avail_h, items.len());
        for (i, item) in items.iter().skip(offset).take(avail_h).enumerate() {
            let idx = offset + i;
            let row_y = list_y + i as u16;
            let is_sel = idx == selected;
            let style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg)
            };
            let right = if item.shortcut.is_empty() {
                String::new()
            } else {
                format!(" [{}]", item.shortcut)
            };
            let left_w = (inner.width as usize).saturating_sub(right.len() + 2);
            let left = crate::ui::truncate_width(&item.label, left_w);
            let line = Line::from(vec![
                Span::styled(format!(" {left}"), style),
                Span::styled(
                    right,
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

fn fuzzy_subsequence(haystack: &str, needle: &str) -> bool {
    let mut h_chars = haystack.chars();
    for nc in needle.chars() {
        if !h_chars.any(|hc| hc == nc) {
            return false;
        }
    }
    true
}
