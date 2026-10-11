//! Explain configuration omissions before the user chooses a deployment source.

use crate::config::BootstrapDiagnostic;
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::components::text_block::{TextBlockControl, TextBlockState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::theme;
use crate::tui::foundation::widgets::inputs::button::ButtonControl;
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum WarningControl {
    Message,
    Continue,
}

const MESSAGE_MIN_HEIGHT: u16 = 3;

pub(crate) struct BootstrapWarningDialog {
    content: String,
    text: TextBlockState,
    form: FormState<WarningControl>,
}

impl BootstrapWarningDialog {
    pub(crate) fn from_diagnostics(diagnostics: Vec<BootstrapDiagnostic>) -> Option<Self> {
        if diagnostics.is_empty() {
            return None;
        }
        let path = crate::config::config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "lasper.toml".into());
        let mut content = format!(
            "Some bootstrap settings in {path} could not be used.\nValid profiles and other application settings remain available.\n"
        );
        for diagnostic in diagnostics {
            log::warn!(
                "Bootstrap config {}: {}",
                diagnostic.setting,
                diagnostic.reason
            );
            content.push_str(&format!(
                "\n{}\n{}\n",
                diagnostic.setting, diagnostic.reason
            ));
        }
        content.push_str("\nFix the configuration and restart Lasper to load your changes.");
        let mut form = FormState::new(WarningControl::Message);
        form.set_focus(true);
        Some(Self {
            content,
            text: TextBlockState::default(),
            form,
        })
    }

    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, WarningControl>) -> R) -> R {
        let mut message = TextBlockControl::new(&mut self.text, " Warnings ", &self.content);
        let mut continue_button = ButtonControl::new("Continue");
        visit(&mut Form::new(
            &mut self.form,
            [
                FormItem::field(WarningControl::Message, &mut message).fill(MESSAGE_MIN_HEIGHT),
                FormItem::field(WarningControl::Continue, &mut continue_button),
            ],
        ))
    }

    fn block() -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Bootstrap Configuration ")
    }

    fn preferred_height(&mut self, width: u16) -> u16 {
        let bounds = Rect::new(0, 0, width, u16::MAX);
        let inner = Self::block().inner(bounds);
        let message_height = TextBlockControl::new(&mut self.text, " Warnings ", &self.content)
            .preferred_height(inner.width);
        self.with_form(|form| form.minimum_height())
            .saturating_add(message_height.saturating_sub(MESSAGE_MIN_HEIGHT))
            .saturating_add(bounds.height - inner.height)
    }

    fn acknowledge() -> EventResult {
        EventResult::Message(AppMessage::Wizard(
            WizardMessage::AcknowledgeConfigurationWarning,
        ))
    }

    fn finish_input(outcome: FormOutcome<WarningControl>) -> EventResult {
        match outcome {
            FormOutcome::Activated(WarningControl::Continue) => Self::acknowledge(),
            FormOutcome::Ignored => EventResult::Ignored,
            _ => EventResult::Consumed,
        }
    }
}

impl Component for BootstrapWarningDialog {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        self.form.register_footer_hints(footer);
        footer.register(FooterHint::new("Enter", "continue").on_key(KeyCode::Enter));
        footer.register(FooterHint::new("Esc", "close").on_key(KeyCode::Esc));
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width.min(90);
        let height = self.preferred_height(width).min(area.height);
        let dialog = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let block =
            Self::block().border_style(Style::default().fg(theme::theme().dialog_border_warn));
        let inner = block.inner(dialog);
        frame.render_widget(Clear, dialog);
        frame.render_widget(block, dialog);
        self.with_form(|form| form.render(frame, inner));
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if !self.form.is_focused() || key.kind == KeyEventKind::Release {
            return EventResult::Consumed;
        }
        if key.modifiers.is_empty() && matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
            return Self::acknowledge();
        }
        Self::finish_input(self.with_form(|form| form.handle_key(key)))
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        Self::finish_input(self.with_form(|form| form.handle_mouse(mouse)))
    }

    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
        self.with_form(|_| ());
    }

    fn is_focused(&self) -> bool {
        self.form.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn an_empty_diagnostic_list_does_not_create_a_dialog() {
        assert!(BootstrapWarningDialog::from_diagnostics(Vec::new()).is_none());
    }

    #[test]
    fn warnings_scroll_in_a_small_viewport_and_continue_by_keyboard_or_mouse() {
        theme::init_theme(theme::Theme::dark());
        let mut dialog = BootstrapWarningDialog::from_diagnostics(vec![BootstrapDiagnostic {
            setting: "bootstrap.methods.dnf5.profiles.\"incomplete\"".into(),
            reason: "Profile ignored: repository=\"host\" is required".into(),
        }])
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        terminal
            .draw(|frame| dialog.render(frame, frame.area()))
            .unwrap();
        assert_eq!(
            dialog.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            EventResult::Consumed
        );
        terminal
            .draw(|frame| dialog.render(frame, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("restart Lasper"));
        let button = dialog.form.control_area(WarningControl::Continue).unwrap();
        assert_eq!(
            dialog.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: button.x + 1,
                row: button.y + 1,
                modifiers: KeyModifiers::NONE,
            }),
            BootstrapWarningDialog::acknowledge()
        );
        for code in [KeyCode::Enter, KeyCode::Esc] {
            assert_eq!(
                dialog.handle_key(KeyEvent::new(code, KeyModifiers::NONE)),
                BootstrapWarningDialog::acknowledge()
            );
        }
    }
}
