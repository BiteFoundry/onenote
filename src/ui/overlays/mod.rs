//! Overlay stack & HelpOverlay (`?`) for standalone Microsoft OneNote TUI.

pub mod attachments;
pub mod command_palette;
pub mod confirm;
pub mod context_menu;
pub mod file_picker;
pub mod image_preview;
pub mod links;
pub mod onenote_form;
pub mod path_input;
pub mod search;
pub mod settings;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{
        attachments::AttachmentsState, command_palette::CommandPalette, confirm::ConfirmDialog,
        context_menu::ContextMenu, file_picker::FilePickerOverlay,
        image_preview::ImagePreviewOverlay, links::LinkExtractorOverlay,
        onenote_form::OnenoteFormOverlay, path_input::PathInputState, search::SearchOverlay,
        settings::SettingsState,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    ContextMenu,
    CommandPalette,
    Settings,
    Search,
    Attachments,
    PathInput,
    FilePicker,
    Links,
    OnenoteForm,
    ImagePreview,
    Help,
    Confirm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    Handled,
    Close,
    Dispatch(Action),
    CloseAndDispatch(Action),
    NotHandled,
}

#[derive(Debug, Clone)]
pub enum Overlay {
    ContextMenu(ContextMenu),
    CommandPalette(CommandPalette),
    Settings(SettingsState),
    Search(SearchOverlay),
    Attachments(AttachmentsState),
    PathInput(PathInputState),
    FilePicker(FilePickerOverlay),
    Links(LinkExtractorOverlay),
    OnenoteForm(OnenoteFormOverlay),
    ImagePreview(ImagePreviewOverlay),
    Help(HelpOverlay),
    Confirm(ConfirmDialog),
}

impl Overlay {
    pub fn kind(&self) -> OverlayKind {
        match self {
            Overlay::ContextMenu(_) => OverlayKind::ContextMenu,
            Overlay::CommandPalette(_) => OverlayKind::CommandPalette,
            Overlay::Settings(_) => OverlayKind::Settings,
            Overlay::Search(_) => OverlayKind::Search,
            Overlay::Attachments(_) => OverlayKind::Attachments,
            Overlay::PathInput(_) => OverlayKind::PathInput,
            Overlay::FilePicker(_) => OverlayKind::FilePicker,
            Overlay::Links(_) => OverlayKind::Links,
            Overlay::OnenoteForm(_) => OverlayKind::OnenoteForm,
            Overlay::ImagePreview(_) => OverlayKind::ImagePreview,
            Overlay::Help(_) => OverlayKind::Help,
            Overlay::Confirm(_) => OverlayKind::Confirm,
        }
    }

    pub fn accepts_text(&self) -> bool {
        match self {
            Overlay::Search(_)
            | Overlay::CommandPalette(_)
            | Overlay::PathInput(_)
            | Overlay::FilePicker(_) => true,
            Overlay::OnenoteForm(form) => form.accepts_text(),
            _ => false,
        }
    }

    pub fn context_menu(menu: ContextMenu) -> Self {
        Overlay::ContextMenu(menu)
    }

    pub fn command_palette() -> Self {
        Overlay::CommandPalette(CommandPalette::new())
    }

    pub fn settings() -> Self {
        Overlay::Settings(SettingsState::default())
    }

    pub fn search() -> Self {
        Overlay::Search(SearchOverlay::default())
    }

    pub fn attachments() -> Self {
        Overlay::Attachments(AttachmentsState::default())
    }

    pub fn path_input() -> Self {
        Overlay::PathInput(PathInputState::new())
    }

    pub fn file_picker() -> Self {
        Overlay::FilePicker(FilePickerOverlay::new())
    }

    pub fn links(overlay: LinkExtractorOverlay) -> Self {
        Overlay::Links(overlay)
    }

    pub fn onenote_form(form: OnenoteFormOverlay) -> Self {
        Overlay::OnenoteForm(form)
    }

    pub fn help() -> Self {
        Overlay::Help(HelpOverlay)
    }

    pub fn confirm(message: impl Into<String>, on_confirm: Option<Action>) -> Self {
        Overlay::Confirm(ConfirmDialog::new(message, on_confirm))
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, ui: &UiState) -> OverlayOutcome {
        match self {
            Overlay::ContextMenu(menu) => match action {
                Action::MoveUp => {
                    menu.prev();
                    OverlayOutcome::Handled
                }
                Action::MoveDown => {
                    menu.next();
                    OverlayOutcome::Handled
                }
                Action::Activate => menu.activate(),
                Action::CloseOverlay => OverlayOutcome::Close,
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::CommandPalette(palette) => match action {
                Action::Tab => palette.handle(&Action::MoveDown, app, ui),
                Action::PrevFocus => palette.handle(&Action::MoveUp, app, ui),
                other => palette.handle(other, app, ui),
            },
            Overlay::Settings(settings) => match action {
                Action::Tab => settings.handle(&Action::MoveDown, app),
                Action::PrevFocus => settings.handle(&Action::MoveUp, app),
                other => settings.handle(other, app),
            },
            Overlay::Search(search) => search.handle(action, app),
            Overlay::Attachments(tray) => tray.handle(action, app),
            Overlay::PathInput(input) => input.handle(action),
            Overlay::FilePicker(picker) => picker.handle(action),
            Overlay::Links(links) => links.handle(action),
            Overlay::OnenoteForm(form) => form.handle(action),
            Overlay::ImagePreview(preview) => preview.handle(action),
            Overlay::Help(_) => match action {
                Action::CloseOverlay | Action::Activate | Action::Input('q') => {
                    OverlayOutcome::Close
                }
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::Confirm(dialog) => dialog.handle(action),
        }
    }

    pub fn select(&mut self, index: usize) {
        match self {
            Overlay::ContextMenu(menu) => menu.select(index),
            Overlay::CommandPalette(palette) => palette.select(index),
            Overlay::Settings(settings) => settings.select(index),
            Overlay::Search(search) => search.select(index),
            Overlay::Attachments(tray) => tray.select(index),
            Overlay::FilePicker(picker) => picker.select(index),
            Overlay::Links(links) => links.select(index),
            Overlay::OnenoteForm(form) => form.select(index),
            Overlay::ImagePreview(_)
            | Overlay::PathInput(_)
            | Overlay::Help(_)
            | Overlay::Confirm(_) => {}
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        match self {
            Overlay::ContextMenu(menu) => menu.draw(frame, area, ui, hitmap),
            Overlay::CommandPalette(palette) => palette.draw(frame, area, app, ui, hitmap),
            Overlay::Settings(settings) => settings.draw(frame, area, app, ui, hitmap),
            Overlay::Search(search) => search.draw(frame, area, app, ui, hitmap),
            Overlay::Attachments(tray) => tray.draw(frame, area, app, ui, hitmap),
            Overlay::PathInput(input) => input.draw(frame, area, ui, hitmap),
            Overlay::FilePicker(picker) => picker.draw(frame, area, ui, hitmap),
            Overlay::Links(links) => links.draw(frame, area, ui, hitmap),
            Overlay::OnenoteForm(form) => form.draw(frame, area, ui, hitmap),
            Overlay::ImagePreview(preview) => preview.draw(frame, area, app, ui, hitmap),
            Overlay::Help(help) => help.draw(frame, area, ui, hitmap),
            Overlay::Confirm(dialog) => dialog.draw(frame, area, ui, hitmap),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HelpOverlay;

impl HelpOverlay {
    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(area, 78, 24);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Microsoft OneNote TUI · Keyboard Shortcuts · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let lines = vec![
            Line::from(Span::styled("Navigation", theme.title(true))),
            Line::from("  1 / 2 / 3 or Tab      Switch focus (Notebooks / Pages / Note Canvas)"),
            Line::from("  [ / ]                 Previous / Next section tab"),
            Line::from("  j / k / Up / Down     Navigate notebooks, pages, or blocks"),
            Line::from("  PgUp / PgDn / g / G   Scroll note canvas"),
            Line::from("  Ctrl+P / Ctrl+K       Unified Command Palette & Omni-Switcher"),
            Line::from("  /                     Search OneNote pages (SQLite FTS5)"),
            Line::from(""),
            Line::from(Span::styled("OneNote Authoring & Actions", theme.title(true))),
            Line::from("  n / N                 Create new page / subpage (Markdown + Ctrl+V image)"),
            Line::from("  a                     Append Markdown block (Ctrl+V image · Ctrl+F file)"),
            Line::from("  e / Enter             Edit selected block surgically"),
            Line::from("  Space / x             Toggle selected to-do checkbox"),
            Line::from("  B / S                 Create new notebook / section"),
            Line::from("  D / Del               Delete selected page (with confirmation)"),
            Line::from("  o / y                 Open in browser / Copy page Markdown"),
            Line::from(""),
            Line::from(Span::styled("Reader, Links & Media", theme.title(true))),
            Line::from("  u / h                 Link Extractor modal / Toggle inline href URLs"),
            Line::from("  v / Alt+v / A         HD Image Preview / Resources & Attachments tray"),
            Line::from("  Ctrl+F                Interactive File Picker (z = zoxide jump)"),
            Line::from("  . / Right-click       Context Menu"),
            Line::from("  Ctrl+B / Ctrl+D       Toggle sidebar / Cozy-Compact density"),
            Line::from("  Ctrl+, / Ctrl+R       Settings / Refresh all · Ctrl+Q Quit"),
        ];
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            inner,
        );
    }
}

pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(area.height.saturating_sub(h) / 2),
            Constraint::Length(h),
            Constraint::Min(0),
        ])
        .split(area);
    let horiz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(area.width.saturating_sub(w) / 2),
            Constraint::Length(w),
            Constraint::Min(0),
        ])
        .split(vert[1]);
    horiz[1]
}
