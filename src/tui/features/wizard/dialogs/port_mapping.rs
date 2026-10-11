use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

use crate::domain::provisioning::PortForward;
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::footer::FooterRegistry;
use crate::tui::foundation::widgets::inputs::button::ButtonControl;
use crate::tui::foundation::widgets::inputs::number_input::{validate_number, NumberInput};
use crate::tui::foundation::widgets::inputs::text_input::Input;
use crate::tui::foundation::widgets::selectors::radio::{RadioControl, RadioState};
use crate::tui::foundation::{geometry::centered_rect, theme};
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};

#[derive(Clone, Copy, PartialEq, Eq)]
enum PortControl {
    Host,
    Container,
    Protocol,
    Submit,
    Cancel,
}

pub struct PortMappingBox {
    host_port: Input,
    container_port: Input,
    host_error: Option<String>,
    container_error: Option<String>,
    protocol: RadioState,
    form: FormState<PortControl>,
    on_submit: Box<dyn Fn(PortForward) -> AppMessage>,
}

impl PortMappingBox {
    pub fn new(on_submit: impl Fn(PortForward) -> AppMessage + 'static) -> Self {
        Self {
            host_port: Input::from("0"),
            container_port: Input::from("0"),
            host_error: None,
            container_error: None,
            protocol: RadioState::default(),
            form: FormState::new(PortControl::Host),
            on_submit: Box::new(on_submit),
        }
    }

    pub fn with_port(mut self, port: &PortForward) -> Self {
        self.host_port = Input::from(port.host.to_string());
        self.container_port = Input::from(port.container.to_string());
        self.protocol = RadioState::new(usize::from(port.proto == "udp"));
        self
    }

    /// The only declaration of fields and actions. Controls borrow the editing
    /// state for this visit, and carry neither focus state nor callbacks.
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, PortControl>) -> R) -> R {
        let mut host = NumberInput::new(
            &mut self.host_port,
            "Host Port",
            self.host_error.as_deref(),
            65535,
        );
        let mut container = NumberInput::new(
            &mut self.container_port,
            "Container Port",
            self.container_error.as_deref(),
            65535,
        );
        let mut protocol = RadioControl::new(&mut self.protocol, "Protocol", &["tcp", "udp"]);
        let mut submit = ButtonControl::new("OK");
        let mut cancel = ButtonControl::new("Cancel");
        let mut form = Form::new(
            &mut self.form,
            [
                FormItem::field(PortControl::Host, &mut host),
                FormItem::field(PortControl::Container, &mut container),
                FormItem::field(PortControl::Protocol, &mut protocol),
                FormItem::spacer(),
                FormItem::row([
                    FormItem::field(PortControl::Submit, &mut submit).with_width(60),
                    FormItem::field(PortControl::Cancel, &mut cancel).with_width(60),
                ]),
            ],
        )
        .with_actions(PortControl::Submit, PortControl::Cancel);
        visit(&mut form)
    }

    fn try_submit(&mut self) -> Option<AppMessage> {
        let host = validate_number(self.host_port.value(), 1, 65535);
        let container = validate_number(self.container_port.value(), 1, 65535);
        self.host_error = host.as_ref().err().cloned();
        self.container_error = container.as_ref().err().cloned();
        Some((self.on_submit)(PortForward {
            host: host.ok()? as u16,
            container: container.ok()? as u16,
            proto: if self.protocol.selected_idx() == 1 {
                "udp"
            } else {
                "tcp"
            }
            .into(),
        }))
    }

    fn finish_input(&mut self, outcome: FormOutcome<PortControl>) -> EventResult {
        match outcome {
            FormOutcome::Activated(PortControl::Submit) => self
                .try_submit()
                .map_or(EventResult::Consumed, EventResult::Message),
            FormOutcome::Activated(PortControl::Cancel) => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel))
            }
            FormOutcome::Ignored => EventResult::Ignored,
            _ => EventResult::Consumed,
        }
    }
}

impl Component for PortMappingBox {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        self.form.register_footer_hints(footer);
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let dialog_area = centered_rect(30, 40, area);
        frame.render_widget(Clear, dialog_area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Add Port Forward ")
            .border_style(Style::default().fg(theme::theme().dialog_border));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);
        self.with_form(|form| form.render(frame, inner));
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let outcome = self.with_form(|form| form.handle_key(key));
        self.finish_input(outcome)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        let outcome = self.with_form(|form| form.handle_mouse(mouse));
        self.finish_input(outcome)
    }

    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }

    fn is_focused(&self) -> bool {
        self.form.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::{backend::TestBackend, Terminal};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn editor() -> PortMappingBox {
        let mut editor =
            PortMappingBox::new(|port| AppMessage::Wizard(WizardMessage::PortForwardAdded(port)));
        editor.set_focus(true);
        editor
    }

    fn button(terminal: &Terminal<TestBackend>, label: &str) -> MouseEvent {
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let line: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if let Some(x) = line.find(label) {
                return MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: line[..x].chars().count() as u16,
                    row: y,
                    modifiers: KeyModifiers::NONE,
                };
            }
        }
        panic!("missing {label} button");
    }

    #[test]
    fn keyboard_and_pointer_submission_share_validation_and_values() {
        theme::init_theme(theme::Theme::dark());
        let mut editor = editor();
        assert_eq!(
            editor.handle_key(key(KeyCode::Enter)),
            EventResult::Consumed
        );
        assert!(editor.host_error.is_some());
        assert!(editor.container_error.is_some());

        for character in "8080".chars() {
            editor.handle_key(key(KeyCode::Char(character)));
        }
        editor.handle_key(key(KeyCode::Tab));
        for character in "80".chars() {
            editor.handle_key(key(KeyCode::Char(character)));
        }
        editor.handle_key(key(KeyCode::Tab));
        editor.handle_key(key(KeyCode::Right));
        let expected = AppMessage::Wizard(WizardMessage::PortForwardAdded(PortForward {
            host: 8080,
            container: 80,
            proto: "udp".into(),
        }));
        assert_eq!(
            editor.handle_key(key(KeyCode::Enter)),
            EventResult::Message(expected)
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| editor.render(frame, frame.area()))
            .unwrap();
        let submit = button(&terminal, "OK");
        assert_eq!(
            editor.handle_mouse(submit),
            EventResult::Message(AppMessage::Wizard(WizardMessage::PortForwardAdded(
                PortForward {
                    host: 8080,
                    container: 80,
                    proto: "udp".into()
                }
            )))
        );

        editor.host_port.reset();
        assert_eq!(editor.handle_mouse(submit), EventResult::Consumed);
        assert!(editor.host_error.is_some());
    }

    #[test]
    fn compact_form_reveals_cancel_and_recovers_after_resize() {
        theme::init_theme(theme::Theme::dark());
        let mut editor = editor();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| editor.render(frame, frame.area()))
            .unwrap();
        editor.handle_key(key(KeyCode::BackTab));
        terminal
            .draw(|frame| editor.render(frame, frame.area()))
            .unwrap();
        assert_eq!(
            editor.handle_mouse(button(&terminal, "Cancel")),
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel))
        );

        let mut large = Terminal::new(TestBackend::new(120, 40)).unwrap();
        large
            .draw(|frame| editor.render(frame, frame.area()))
            .unwrap();
        let rendered: String = large
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Host Port"));
        assert!(rendered.contains("Container Port"));
        assert_eq!(
            editor.handle_key(key(KeyCode::Enter)),
            EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel))
        );
    }
}
