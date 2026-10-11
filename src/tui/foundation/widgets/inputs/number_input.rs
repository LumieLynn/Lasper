use crate::tui::foundation::footer::FooterHint;
use crate::tui::foundation::widgets::control::{Control, ControlContext, ControlOutcome};
use crate::tui::foundation::widgets::inputs::text_input::{
    render_input, Input, InputAppearance, INPUT_HINTS,
};
use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};
use tui_input::backend::crossterm::EventHandler;

pub(crate) struct NumberInput<'a> {
    input: &'a mut Input,
    label: &'a str,
    error: Option<&'a str>,
    max: u32,
}

impl<'a> NumberInput<'a> {
    pub(crate) fn new(
        input: &'a mut Input,
        label: &'a str,
        error: Option<&'a str>,
        max: u32,
    ) -> Self {
        Self {
            input,
            label,
            error,
            max,
        }
    }
}

impl Control for NumberInput<'_> {
    fn render(&mut self, frame: &mut Frame, area: Rect, context: ControlContext) {
        render_input(
            self.input,
            InputAppearance {
                label: self.label,
                error: self.error,
                masked: false,
            },
            context,
            frame,
            area,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> ControlOutcome {
        edit_number(self.input, self.max, key)
    }

    fn hints(&self) -> &'static [FooterHint] {
        INPUT_HINTS
    }

    fn captures_text_input(&self) -> bool {
        true
    }
}

fn edit_number(input: &mut Input, max: u32, key: KeyEvent) -> ControlOutcome {
    // Rejected edits preserve both the value and its cursor.
    let mut candidate = input.clone();
    if candidate
        .handle_event(&crossterm::event::Event::Key(key))
        .is_none()
    {
        return ControlOutcome::Ignored;
    }
    if !candidate.value().is_empty()
        && !candidate
            .value()
            .parse::<u32>()
            .is_ok_and(|number| number <= max)
    {
        return ControlOutcome::Consumed;
    }
    let changed = candidate.value() != input.value();
    *input = candidate;
    if changed {
        ControlOutcome::Changed
    } else {
        ControlOutcome::Consumed
    }
}

pub(crate) fn validate_number(value: &str, min: u32, max: u32) -> Result<u32, String> {
    if value.is_empty() {
        return Err("Cannot be empty".into());
    }
    let number = value.parse::<u32>().map_err(|_| "Invalid number")?;
    if number > max {
        return Err(format!("Max value is {max}"));
    }
    if number < min {
        return Err(format!("Min value is {min}"));
    }
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn rejected_edits_leave_the_value_and_cursor_unchanged() {
        let mut input = Input::from("65535");
        for code in [KeyCode::Char('x'), KeyCode::Char('9')] {
            assert_eq!(
                NumberInput::new(&mut input, "Port", None, 65535)
                    .handle_key(KeyEvent::new(code, KeyModifiers::NONE)),
                ControlOutcome::Consumed
            );
            assert_eq!(input.value(), "65535");
            assert_eq!(input.cursor(), 5);
        }
        NumberInput::new(&mut input, "Port", None, 65535)
            .handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        NumberInput::new(&mut input, "Port", None, 65535)
            .handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(input.value(), "6555");
        assert_eq!(input.cursor(), 3);
    }
}
