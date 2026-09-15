use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::application::configuration::{X11BindingChange, X11BindingDeclaration};
use crate::tui::core::{Component, EventResult};
use crate::tui::theme;
use crate::tui::widgets::inputs::path_box::PathBox;
use crate::tui::widgets::selectors::checkbox::Checkbox;

const SOURCE: usize = 0;
const TARGET: usize = 1;
const READ_ONLY: usize = 2;
const APPLY: usize = 3;
const CANCEL: usize = 4;
const CONTROL_COUNT: usize = 5;

pub(super) enum EditorOutcome {
    None,
    Cancel,
    Submit(X11BindingChange),
}

#[derive(Default)]
struct EditorHits {
    source: Rect,
    target: Rect,
    readonly: Rect,
    apply: Rect,
    cancel: Rect,
}

pub(super) struct X11BindingEditor {
    line: usize,
    source: PathBox,
    target: PathBox,
    readonly: Checkbox,
    focus: usize,
    error: Option<String>,
    hits: EditorHits,
}

impl X11BindingEditor {
    pub(super) fn new(binding: &X11BindingDeclaration, draft: Option<&X11BindingChange>) -> Self {
        let (source, target, readonly) = match draft {
            Some(X11BindingChange::Update {
                source,
                guest_target,
                readonly,
                ..
            }) => (source.clone(), guest_target.clone(), *readonly),
            _ => (
                binding.source.clone(),
                binding.guest_target.clone(),
                binding.readonly,
            ),
        };
        let mut editor = Self {
            line: binding.line,
            source: PathBox::new(" Host source ", source.display().to_string())
                .with_validator(validate_source),
            target: PathBox::new(" Guest target ", target.display().to_string())
                .with_validator(validate_target),
            readonly: Checkbox::new("Read-only bind", readonly),
            focus: SOURCE,
            error: None,
            hits: EditorHits::default(),
        };
        editor.update_focus();
        editor
    }

    fn update_focus(&mut self) {
        self.source.set_focus(self.focus == SOURCE);
        self.target.set_focus(self.focus == TARGET);
        self.readonly.set_focus(self.focus == READ_ONLY);
    }

    fn move_focus(&mut self, reverse: bool) {
        self.focus = if reverse {
            (self.focus + CONTROL_COUNT - 1) % CONTROL_COUNT
        } else {
            (self.focus + 1) % CONTROL_COUNT
        };
        self.update_focus();
    }

    fn submit(&mut self) -> EditorOutcome {
        if let Err(error) = self.source.validate().and_then(|_| self.target.validate()) {
            self.error = Some(error);
            return EditorOutcome::None;
        }
        self.error = None;
        EditorOutcome::Submit(X11BindingChange::Update {
            line: self.line,
            source: PathBuf::from(self.source.value()),
            guest_target: PathBuf::from(self.target.value()),
            readonly: self.readonly.checked(),
        })
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> EditorOutcome {
        match key.code {
            KeyCode::Esc => return EditorOutcome::Cancel,
            KeyCode::Tab => {
                self.move_focus(false);
                return EditorOutcome::None;
            }
            KeyCode::BackTab => {
                self.move_focus(true);
                return EditorOutcome::None;
            }
            KeyCode::Enter if self.focus == CANCEL => return EditorOutcome::Cancel,
            KeyCode::Enter => return self.submit(),
            _ => {}
        }
        let result = match self.focus {
            SOURCE => self.source.handle_key(key),
            TARGET => self.target.handle_key(key),
            READ_ONLY => self.readonly.handle_key(key),
            _ => EventResult::Ignored,
        };
        match result {
            EventResult::FocusNext => self.move_focus(false),
            EventResult::FocusPrev => self.move_focus(true),
            EventResult::Consumed => self.error = None,
            EventResult::Ignored | EventResult::Message(_) => {}
        }
        EditorOutcome::None
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) -> EditorOutcome {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return EditorOutcome::None;
        }
        let position = (mouse.column, mouse.row).into();
        self.focus = if self.hits.source.contains(position) {
            SOURCE
        } else if self.hits.target.contains(position) {
            TARGET
        } else if self.hits.readonly.contains(position) {
            self.readonly.handle_key(KeyEvent::from(KeyCode::Char(' ')));
            READ_ONLY
        } else if self.hits.apply.contains(position) {
            return self.submit();
        } else if self.hits.cancel.contains(position) {
            return EditorOutcome::Cancel;
        } else {
            return EditorOutcome::None;
        };
        self.update_focus();
        EditorOutcome::None
    }

    pub(super) fn render(&mut self, frame: &mut Frame, area: Rect) {
        let width = 76.min(area.width);
        let height = 18.min(area.height);
        let dialog = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .title(format!(" Edit X11 bind | line {} ", self.line))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::theme().dialog_border));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        let rows = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(inner);
        self.hits.source = rows[0];
        self.hits.target = rows[1];
        self.hits.readonly = rows[2];
        self.source.render(frame, rows[0]);
        self.target.render(frame, rows[1]);
        self.readonly.render(frame, rows[2]);
        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(error.as_str())
                    .style(Style::default().fg(theme::theme().editor_error)),
                rows[3],
            );
        }
        let buttons = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[4]);
        self.hits.apply = buttons[0];
        self.hits.cancel = buttons[1];
        render_button(frame, buttons[0], "Apply to draft", self.focus == APPLY);
        render_button(frame, buttons[1], "Cancel", self.focus == CANCEL);
    }
}

fn validate_source(value: &str) -> Result<(), String> {
    if value.len() > 4096 {
        return Err("Host source exceeds the editor limit".into());
    }
    let path = Path::new(value);
    let directory = Path::new("/tmp/.X11-unix");
    let valid_socket = path.parent() == Some(directory)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix('X'))
            .is_some_and(|number| {
                let number = number.strip_suffix('_').unwrap_or(number);
                !number.is_empty()
                    && number.bytes().all(|byte| byte.is_ascii_digit())
                    && number.parse::<u16>().is_ok()
            });
    if path == directory || valid_socket {
        Ok(())
    } else {
        Err("Use /tmp/.X11-unix or /tmp/.X11-unix/X<number>[_]".into())
    }
}

fn validate_target(value: &str) -> Result<(), String> {
    if value.len() > 4096 {
        return Err("Guest target exceeds the editor limit".into());
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        Err("Guest target must be an absolute path".into())
    } else if path == Path::new("/") {
        Err("Guest target cannot be /".into())
    } else if value.contains('%') || value.chars().any(char::is_control) {
        Err("Guest target cannot contain specifiers or control characters".into())
    } else if path.components().any(|component| {
        !matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Normal(_)
        )
    }) {
        Err("Guest target cannot contain '.' or '..' components".into())
    } else {
        Ok(())
    }
}

fn render_button(frame: &mut Frame, area: Rect, label: &str, focused: bool) {
    let t = theme::theme();
    frame.render_widget(
        Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(if focused {
                Style::default()
                    .fg(t.button_focused_fg)
                    .bg(t.button_focused_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.button_unfocused_fg)
            })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(if focused {
                        t.button_border_focused
                    } else {
                        t.button_border_unfocused
                    })),
            ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::configuration::X11BindingScope;
    use crossterm::event::KeyModifiers;

    fn binding() -> X11BindingDeclaration {
        X11BindingDeclaration {
            line: 7,
            source: "/tmp/.X11-unix".into(),
            guest_target: "/mnt/x11".into(),
            readonly: true,
            options: vec!["idmap".into()],
            scope: X11BindingScope::Directory,
        }
    }

    #[test]
    fn editor_submits_only_its_finite_binding_identity() {
        let mut editor = X11BindingEditor::new(&binding(), None);
        let result = editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            result,
            EditorOutcome::Submit(X11BindingChange::Update { line: 7, .. })
        ));
    }

    #[test]
    fn invalid_source_remains_in_the_editor() {
        let mut editor = X11BindingEditor::new(&binding(), None);
        editor.source.set_value("/run/user/1000/X0".into());
        assert!(matches!(
            editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            EditorOutcome::None
        ));
        assert!(editor.error.is_some());
    }
}
