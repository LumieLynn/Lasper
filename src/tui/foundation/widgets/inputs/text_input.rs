use crate::tui::foundation::footer::FooterHint;
use crate::tui::foundation::theme;
use crate::tui::foundation::widgets::control::{Control, ControlContext, ControlOutcome};
use crossterm::event::KeyEvent;
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use tui_input::backend::crossterm::EventHandler;
pub use tui_input::Input;

pub(crate) const INPUT_HINTS: &[FooterHint] = &[
    FooterHint::new("←/→", "cursor").secondary(),
    FooterHint::new("Home/End", "line ends").secondary(),
];

pub(crate) struct InputAppearance<'a> {
    pub(crate) label: &'a str,
    pub(crate) error: Option<&'a str>,
    pub(crate) masked: bool,
}

pub(crate) struct TextInput<'a> {
    input: &'a mut Input,
    appearance: InputAppearance<'a>,
}

impl<'a> TextInput<'a> {
    pub(crate) fn new(input: &'a mut Input, label: &'a str, error: Option<&'a str>) -> Self {
        Self {
            input,
            appearance: InputAppearance {
                label,
                error,
                masked: false,
            },
        }
    }

    pub(crate) fn masked(mut self) -> Self {
        self.appearance.masked = true;
        self
    }
}

impl Control for TextInput<'_> {
    fn render(&mut self, frame: &mut Frame, area: Rect, context: ControlContext) {
        render_input(
            self.input,
            InputAppearance {
                label: self.appearance.label,
                error: self.appearance.error,
                masked: self.appearance.masked,
            },
            context,
            frame,
            area,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> ControlOutcome {
        match self.input.handle_event(&crossterm::event::Event::Key(key)) {
            Some(change) if change.value => ControlOutcome::Changed,
            Some(_) => ControlOutcome::Consumed,
            None => ControlOutcome::Ignored,
        }
    }

    fn hints(&self) -> &'static [FooterHint] {
        INPUT_HINTS
    }

    fn captures_text_input(&self) -> bool {
        true
    }
}

/// Labels and errors are borrowed from the owning view; focus and availability
/// are supplied by the form for this draw.
pub(crate) fn render_input(
    input: &Input,
    appearance: InputAppearance<'_>,
    context: ControlContext,
    frame: &mut Frame,
    area: Rect,
) {
    let area = area.intersection(frame.area());
    let (paragraph, cursor) = if appearance.masked {
        let width = usize::from(area.width.saturating_sub(2));
        let cursor = input.cursor();
        let scroll = cursor.saturating_sub(width.saturating_sub(1));
        let length = input.value().chars().count().saturating_sub(scroll);
        (
            Paragraph::new("*".repeat(length.min(width))),
            cursor.saturating_sub(scroll),
        )
    } else {
        // Leave a cell for the cursor at the end of the visible text.
        let width = usize::from(area.width.saturating_sub(3));
        let scroll = input.visual_scroll(width).min(usize::from(u16::MAX));
        (
            Paragraph::new(input.value()).scroll((0, scroll as u16)),
            input.visual_cursor().saturating_sub(scroll),
        )
    };
    let t = theme::theme();
    let border_style = if context.enabled && appearance.error.is_some() {
        Style::default().fg(t.editor_error)
    } else {
        Style::default().fg(theme::widget_border_color(context.focused, context.enabled))
    };
    let title = match appearance.error {
        Some(error) => format!("{} [ {} ]", appearance.label, error),
        None => appearance.label.to_owned(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(border_style);
    let inner = block.inner(area);
    let text_style = if context.enabled {
        Style::default()
    } else {
        Style::default().fg(t.text_dim)
    };
    frame.render_widget(paragraph.style(text_style).block(block), area);
    if context.focused && context.enabled && !inner.is_empty() {
        let cursor = cursor.min(usize::from(inner.width - 1)) as u16;
        frame.set_cursor_position((inner.x + cursor, inner.y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        backend::{Backend, TestBackend},
        layout::Position,
        Terminal,
    };

    fn body(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (1..buffer.area.width - 1)
            .map(|x| buffer[(x, 1)].symbol())
            .collect()
    }

    #[test]
    fn long_input_scrolls_with_the_cursor_and_returns_to_the_start() {
        theme::init_theme(theme::Theme::dark());
        let mut input = Input::from("abcdefghij");
        let mut terminal = Terminal::new(TestBackend::new(8, 3)).unwrap();
        terminal
            .draw(|frame| {
                TextInput::new(&mut input, "Text", None).render(
                    frame,
                    frame.area(),
                    ControlContext {
                        focused: true,
                        enabled: true,
                    },
                )
            })
            .unwrap();
        assert_eq!(body(&terminal), "fghij ");
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(6, 1)
        );

        input.handle(tui_input::InputRequest::GoToStart);
        terminal
            .draw(|frame| {
                TextInput::new(&mut input, "Text", None).render(
                    frame,
                    frame.area(),
                    ControlContext {
                        focused: true,
                        enabled: true,
                    },
                )
            })
            .unwrap();
        assert_eq!(body(&terminal), "abcdef");
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(1, 1)
        );
    }

    #[test]
    fn wide_text_and_password_masks_use_their_own_display_columns() {
        theme::init_theme(theme::Theme::dark());
        let mut input = Input::from("路径abc");
        let mut terminal = Terminal::new(TestBackend::new(8, 3)).unwrap();
        terminal
            .draw(|frame| {
                TextInput::new(&mut input, "Text", None).render(
                    frame,
                    frame.area(),
                    ControlContext {
                        focused: true,
                        enabled: true,
                    },
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 1)].symbol(), "径");
        assert_eq!(buffer[(3, 1)].symbol(), "a");
        assert_eq!(buffer[(5, 1)].symbol(), "c");
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(6, 1)
        );

        input = Input::from("a密🙂");
        terminal
            .draw(|frame| {
                TextInput::new(&mut input, "Password", None)
                    .masked()
                    .render(
                        frame,
                        frame.area(),
                        ControlContext {
                            focused: true,
                            enabled: true,
                        },
                    )
            })
            .unwrap();
        assert_eq!(body(&terminal), "***   ");
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(4, 1)
        );

        let mut tiny = Terminal::new(TestBackend::new(2, 3)).unwrap();
        tiny.draw(|frame| {
            TextInput::new(&mut input, "Password", None)
                .masked()
                .render(
                    frame,
                    frame.area(),
                    ControlContext {
                        focused: true,
                        enabled: true,
                    },
                )
        })
        .unwrap();
        assert!(!tiny.backend().cursor_visible());
    }
}
