//! Workspace shortcuts shared by input routing and footer registration.
//!
//! Components handle their own navigation and terminal input. The workspace
//! handles panel controls locally and passes external requests to App.

use crate::tui::foundation::footer::FooterHint;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceShortcut {
    Quit,
    Help,
    NextPanel,
    PreviousPanel,
    Start,
    Poweroff,
    ResourceActions,
    Wizard,
    Refresh,
    Resize,
    Shell,
    ToggleTerminal,
    RemoveImage,
    Leader,
}

impl WorkspaceShortcut {
    pub(crate) fn from_key(key: KeyEvent) -> Option<Self> {
        let no_control = !key.modifiers.contains(KeyModifiers::CONTROL);
        let no_resource_action_modifier = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        Some(match key.code {
            KeyCode::Char('q') if no_control => Self::Quit,
            KeyCode::Char('?') => Self::Help,
            KeyCode::Tab => Self::NextPanel,
            KeyCode::BackTab => Self::PreviousPanel,
            KeyCode::Char('s') => Self::Start,
            KeyCode::Char('S') => Self::Poweroff,
            KeyCode::Char('x') | KeyCode::Enter if no_resource_action_modifier => {
                Self::ResourceActions
            }
            KeyCode::Char('n') | KeyCode::Char('a') => Self::Wizard,
            KeyCode::Char('r') => Self::Refresh,
            KeyCode::Char('R') => Self::Resize,
            KeyCode::Char('t') if no_control => Self::Shell,
            KeyCode::Char('T') => Self::ToggleTerminal,
            KeyCode::Char('D') => Self::RemoveImage,
            KeyCode::Char(' ') if key.modifiers.is_empty() => Self::Leader,
            _ => return None,
        })
    }

    pub(crate) fn hint(self) -> FooterHint {
        match self {
            Self::Quit => FooterHint::new("q", "quit")
                .secondary()
                .on_key(KeyCode::Char('q')),
            Self::Help => FooterHint::new("?", "help")
                .primary()
                .on_key(KeyCode::Char('?')),
            Self::NextPanel | Self::PreviousPanel => FooterHint::new("Tab/⇧Tab", "panels")
                .primary()
                .on_key(if self == Self::PreviousPanel {
                    KeyCode::BackTab
                } else {
                    KeyCode::Tab
                }),
            Self::Start => FooterHint::new("s", "start").on_key(KeyCode::Char('s')),
            Self::Poweroff => FooterHint::new("S", "poweroff").on_key(KeyCode::Char('S')),
            Self::ResourceActions => FooterHint::new("x/⏎", "actions")
                .primary()
                .on_key(KeyCode::Char('x')),
            Self::Wizard => FooterHint::new("n/a", "new")
                .secondary()
                .on_key(KeyCode::Char('n')),
            Self::Refresh => FooterHint::new("r", "refresh")
                .secondary()
                .on_key(KeyCode::Char('r')),
            Self::Resize => FooterHint::new("R", "resize")
                .secondary()
                .on_key(KeyCode::Char('R')),
            Self::Shell => FooterHint::new("t", "shell").on_key(KeyCode::Char('t')),
            Self::ToggleTerminal => FooterHint::new("T", "maximize").on_key(KeyCode::Char('T')),
            Self::RemoveImage => FooterHint::new("D", "delete").on_key(KeyCode::Char('D')),
            Self::Leader => FooterHint::new("Space", "leader").on_key(KeyCode::Char(' ')),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WorkspaceShortcut;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn resource_actions_ignore_modified_terminal_mode_keys() {
        assert_eq!(
            WorkspaceShortcut::from_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(WorkspaceShortcut::ResourceActions)
        );
        assert_eq!(
            WorkspaceShortcut::from_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(WorkspaceShortcut::ResourceActions)
        );
        assert_ne!(
            WorkspaceShortcut::from_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(WorkspaceShortcut::ResourceActions)
        );
        assert_ne!(
            WorkspaceShortcut::from_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            Some(WorkspaceShortcut::ResourceActions)
        );
    }
}
