//! Terminal keyboard & mouse event -> `Action` translation.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::ui::{
    action::Action,
    app::Focus,
    hitmap::HitMap,
    keymap::{KeyCommand, KeyMap},
    overlays::OverlayKind,
};

pub fn key_to_action(
    key: KeyEvent,
    focus: Focus,
    keymap: &KeyMap,
    text_input: bool,
) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    if let Some(cmd) = keymap.resolve_global(&key, text_input) {
        return Some(match cmd {
            KeyCommand::Quit => Action::QuitConfirm,
            KeyCommand::CommandPalette => Action::OpenOverlay(OverlayKind::CommandPalette),
            KeyCommand::Settings => Action::OpenOverlay(OverlayKind::Settings),
            KeyCommand::Help => Action::OpenOverlay(OverlayKind::Help),
            KeyCommand::Search => Action::OpenSearch,
            KeyCommand::Refresh => Action::RefreshAll,
            KeyCommand::ToggleNavRail => Action::ToggleNavRail,
            KeyCommand::ToggleDensity => Action::ToggleDensity,
            KeyCommand::OpenFilePicker => Action::OpenFilePicker,
            KeyCommand::OpenContextMenu => Action::OpenContextMenu {
                col: 12,
                row: 5,
            },
            KeyCommand::OpenLinkExtractor => Action::OpenLinkExtractor,
            KeyCommand::ToggleShowLinkUrls => Action::ToggleShowLinkUrls,
            KeyCommand::OpenImagePreview => Action::OpenImagePreview,
            KeyCommand::OpenHtmlPreview => Action::OpenHtmlPreview,
            KeyCommand::OpenAttachments => Action::OpenOverlay(OverlayKind::Attachments),
            KeyCommand::NewNotebook => Action::OpenOnenoteCreateNotebook,
            KeyCommand::NewSection => Action::OpenOnenoteCreateSection,
            KeyCommand::NewPage => Action::OpenOnenoteCreatePage { level: 0 },
            KeyCommand::AppendMarkdown => Action::OpenOnenoteAppend,
            KeyCommand::EditBlock => Action::OpenOnenoteEditBlock,
            KeyCommand::ToggleTodo => Action::OnenoteToggleTodo(None),
            KeyCommand::DeletePage => Action::ConfirmDeleteOnenotePage,
            KeyCommand::OpenWeb => Action::OnenoteOpenWeb,
            KeyCommand::NextSection => Action::OnenoteNextSection,
            KeyCommand::PrevSection => Action::OnenotePrevSection,
            KeyCommand::CopySelection => Action::CopyActivePageMarkdown,
            KeyCommand::PasteClipboard => Action::PasteClipboard,
            KeyCommand::ExternalEditor => Action::RequestExternalEditor,
        });
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if focus == Focus::Overlay {
        if ctrl && key.code == KeyCode::Enter || alt && key.code == KeyCode::Enter {
            return Some(Action::OverlaySubmit);
        }
        if ctrl && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S')) {
            return Some(Action::OverlaySubmit);
        }
        return match key.code {
            KeyCode::Esc => Some(Action::CloseOverlay),
            KeyCode::Enter if shift => Some(Action::Newline),
            KeyCode::Enter => Some(Action::Activate),
            KeyCode::Tab => Some(Action::Tab),
            KeyCode::BackTab => Some(Action::PrevFocus),
            KeyCode::Backspace => Some(Action::Backspace),
            KeyCode::Delete => Some(Action::Delete),
            KeyCode::Up => Some(Action::MoveUp),
            KeyCode::Down => Some(Action::MoveDown),
            KeyCode::Left => Some(Action::CursorLeft),
            KeyCode::Right => Some(Action::CursorRight),
            KeyCode::Home => Some(Action::CursorHome),
            KeyCode::End => Some(Action::CursorEnd),
            KeyCode::PageUp => Some(Action::PageUp),
            KeyCode::PageDown => Some(Action::PageDown),
            KeyCode::Char(ch) if !ctrl && !alt => Some(Action::Input(ch)),
            _ => None,
        };
    }

    match key.code {
        KeyCode::Esc => Some(Action::CloseOverlay),
        KeyCode::Tab => Some(Action::NextFocus),
        KeyCode::BackTab => Some(Action::PrevFocus),
        KeyCode::Enter => Some(Action::Activate),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::MoveDown),
        KeyCode::Left => Some(Action::PrevFocus),
        KeyCode::Right => Some(Action::NextFocus),
        KeyCode::PageUp => Some(Action::PageUp),
        KeyCode::PageDown => Some(Action::PageDown),
        KeyCode::Home | KeyCode::Char('g') => Some(Action::ScrollTop),
        KeyCode::End | KeyCode::Char('G') => Some(Action::ScrollBottom),
        KeyCode::Char('1') => Some(Action::SetFocus(Focus::Nav)),
        KeyCode::Char('2') => Some(Action::SetFocus(Focus::Pages)),
        KeyCode::Char('3') => Some(Action::SetFocus(Focus::Canvas)),
        KeyCode::Char('q') => Some(Action::QuitConfirm),
        _ => None,
    }
}

pub fn mouse_to_action(mouse: MouseEvent, hitmap: &HitMap) -> Option<Action> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let target = hitmap.hit(mouse.row, mouse.column)?;
            Some(Action::MouseClick {
                target,
                col: mouse.column,
                row: mouse.row,
            })
        }
        MouseEventKind::Down(MouseButton::Right) => {
            Some(Action::MouseRightClick {
                col: mouse.column,
                row: mouse.row,
            })
        }
        _ => None,
    }
}
