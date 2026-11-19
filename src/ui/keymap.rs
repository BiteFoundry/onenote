//! Configurable keyboard shortcuts for the Microsoft OneNote TUI (`KeyMap`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Logical command bound to a key combo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCommand {
    Quit,
    CommandPalette,
    Settings,
    Help,
    Search,
    Refresh,
    ToggleNavRail,
    ToggleDensity,
    OpenFilePicker,
    OpenContextMenu,
    OpenLinkExtractor,
    ToggleShowLinkUrls,
    OpenImagePreview,
    OpenAttachments,
    NewNotebook,
    NewSection,
    NewPage,
    NewSubpage,
    AppendMarkdown,
    EditBlock,
    ToggleTodo,
    DeletePage,
    OpenWeb,
    NextSection,
    PrevSection,
    CopySelection,
    PasteClipboard,
    ExternalEditor,
}

#[derive(Debug, Clone, Default)]
pub struct KeyMap;

impl KeyMap {
    /// Resolves global shortcuts that work even when text input is NOT active (or Ctrl combos that always work).
    pub fn resolve_global(&self, key: &KeyEvent, text_input: bool) -> Option<KeyCommand> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl {
            return match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => Some(KeyCommand::Quit),
                KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::Char('k') | KeyCode::Char('K') => {
                    Some(KeyCommand::CommandPalette)
                }
                KeyCode::Char(',') => Some(KeyCommand::Settings),
                KeyCode::Char('b') | KeyCode::Char('B') => Some(KeyCommand::ToggleNavRail),
                KeyCode::Char('d') | KeyCode::Char('D') => Some(KeyCommand::ToggleDensity),
                KeyCode::Char('r') | KeyCode::Char('R') => Some(KeyCommand::Refresh),
                KeyCode::Char('f') | KeyCode::Char('F') => Some(KeyCommand::OpenFilePicker),
                KeyCode::Char('v') | KeyCode::Char('V') => Some(KeyCommand::PasteClipboard),
                KeyCode::Char('e') | KeyCode::Char('E') => Some(KeyCommand::ExternalEditor),
                _ => None,
            };
        }

        if alt {
            return match key.code {
                KeyCode::Char('v') | KeyCode::Char('V') => Some(KeyCommand::OpenImagePreview),
                _ => None,
            };
        }

        if key.code == KeyCode::F(5) {
            return Some(KeyCommand::Refresh);
        }

        if text_input {
            return None;
        }

        match key.code {
            KeyCode::Char('?') => Some(KeyCommand::Help),
            KeyCode::Char('/') => Some(KeyCommand::Search),
            KeyCode::Char('.') => Some(KeyCommand::OpenContextMenu),
            KeyCode::Char('u') => Some(KeyCommand::OpenLinkExtractor),
            KeyCode::Char('h') => Some(KeyCommand::ToggleShowLinkUrls),
            KeyCode::Char('v') => Some(KeyCommand::OpenImagePreview),
            KeyCode::Char('A') => Some(KeyCommand::OpenAttachments),
            KeyCode::Char('B') => Some(KeyCommand::NewNotebook),
            KeyCode::Char('S') => Some(KeyCommand::NewSection),
            KeyCode::Char('n') => Some(KeyCommand::NewPage),
            KeyCode::Char('N') => Some(KeyCommand::NewSubpage),
            KeyCode::Char('a') => Some(KeyCommand::AppendMarkdown),
            KeyCode::Char('e') => Some(KeyCommand::EditBlock),
            KeyCode::Char(' ') | KeyCode::Char('x') => Some(KeyCommand::ToggleTodo),
            KeyCode::Char('D') | KeyCode::Delete => Some(KeyCommand::DeletePage),
            KeyCode::Char('o') => Some(KeyCommand::OpenWeb),
            KeyCode::Char(']') => Some(KeyCommand::NextSection),
            KeyCode::Char('[') => Some(KeyCommand::PrevSection),
            KeyCode::Char('y') => Some(KeyCommand::CopySelection),
            _ => None,
        }
    }
}
