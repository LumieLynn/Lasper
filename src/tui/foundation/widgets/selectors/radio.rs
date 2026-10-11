use crate::tui::foundation::footer::FooterHint;
use crate::tui::foundation::widgets::control::{Control, ControlContext, ControlOutcome};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

const RADIO_HINTS: &[FooterHint] = &[FooterHint::new("←/→", "select")];

#[derive(Default)]
pub(crate) struct RadioState {
    selected_idx: usize,
    scroll: usize,
}

impl RadioState {
    pub(crate) fn new(selected_idx: usize) -> Self {
        Self {
            selected_idx,
            scroll: 0,
        }
    }

    pub(crate) fn selected_idx(&self) -> usize {
        self.selected_idx
    }
}

pub(crate) struct RadioControl<'a, T> {
    state: &'a mut RadioState,
    label: &'a str,
    options: &'a [T],
}

impl<'a, T: AsRef<str>> RadioControl<'a, T> {
    pub(crate) fn new(state: &'a mut RadioState, label: &'a str, options: &'a [T]) -> Self {
        state.selected_idx = state.selected_idx.min(options.len().saturating_sub(1));
        Self {
            state,
            label,
            options,
        }
    }
}

impl<T: AsRef<str>> Control for RadioControl<'_, T> {
    fn render(&mut self, f: &mut Frame, area: Rect, context: ControlContext) {
        let area = area.intersection(f.area());
        let style = Style::default().fg(crate::tui::foundation::theme::widget_border_color(
            context.focused,
            context.enabled,
        ));

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(self.label)
            .border_style(style);
        let inner = block.inner(area);
        let t = crate::tui::foundation::theme::theme();
        let mut spans = Vec::new();
        let mut width = 0;
        let mut selected_start = 0;
        let mut selected_end = 0;
        for (i, opt) in self.options.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("   "));
                width += 3;
            }
            let symbol = if i == self.state.selected_idx {
                "(●)"
            } else {
                "(○)"
            };
            let option = format!("{} {}", symbol, opt.as_ref());
            let option_style = if i == self.state.selected_idx && context.enabled {
                selected_start = width;
                selected_end = width + option.width();
                Style::default()
                    .fg(t.list_highlight_symbol)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            width += option.width();
            spans.push(Span::styled(option, option_style));
        }

        let visible_width = usize::from(inner.width);
        if width <= visible_width {
            self.state.scroll = 0;
        } else if context.enabled {
            if selected_start < self.state.scroll || selected_end - selected_start > visible_width {
                self.state.scroll = selected_start;
            } else if selected_end > self.state.scroll.saturating_add(visible_width) {
                self.state.scroll = selected_end.saturating_sub(visible_width);
            }
        }
        self.state.scroll = self
            .state
            .scroll
            .min(width.saturating_sub(visible_width))
            .min(usize::from(u16::MAX));

        let text_style = if context.enabled {
            Style::default()
        } else {
            Style::default()
                .fg(crate::tui::foundation::theme::theme().border_disabled)
                .add_modifier(Modifier::DIM)
        };
        let paragraph = Paragraph::new(Line::from(spans))
            .style(text_style)
            .scroll((0, self.state.scroll as u16))
            .block(block);
        f.render_widget(paragraph, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> ControlOutcome {
        match key.code {
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Up | KeyCode::Char('k') => {
                if self.state.selected_idx > 0 {
                    self.state.selected_idx -= 1;
                    return ControlOutcome::Changed;
                }
                ControlOutcome::Ignored
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Down | KeyCode::Char('j') => {
                if self.state.selected_idx < self.options.len().saturating_sub(1) {
                    self.state.selected_idx += 1;
                    return ControlOutcome::Changed;
                }
                ControlOutcome::Ignored
            }
            _ => ControlOutcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> ControlOutcome {
        let inner = Block::default().borders(Borders::ALL).inner(area);
        if mouse.kind != MouseEventKind::Down(MouseButton::Left)
            || !inner.contains((mouse.column, mouse.row).into())
            || mouse.row != inner.y
        {
            return ControlOutcome::Ignored;
        }
        let column = self.state.scroll + usize::from(mouse.column - inner.x);
        let mut start = 0;
        for (index, option) in self.options.iter().enumerate() {
            let end = start + 4 + option.as_ref().width();
            if (start..end).contains(&column) {
                let changed = self.state.selected_idx != index;
                self.state.selected_idx = index;
                return if changed {
                    ControlOutcome::Changed
                } else {
                    ControlOutcome::Consumed
                };
            }
            start = end + 3;
        }
        ControlOutcome::Ignored
    }

    fn hints(&self) -> &'static [FooterHint] {
        RADIO_HINTS
    }

    fn is_focusable(&self) -> bool {
        !self.options.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn pointer_selection_uses_cell_widths_and_ignores_option_spacing() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut state = RadioState::new(0);
        let options = ["设备", "all"];
        let mut radio = RadioControl::new(&mut state, "Choice", &options);
        let mut terminal = Terminal::new(TestBackend::new(30, 3)).unwrap();
        terminal
            .draw(|frame| {
                radio.render(
                    frame,
                    frame.area(),
                    ControlContext {
                        focused: true,
                        enabled: true,
                    },
                )
            })
            .unwrap();
        let click = |column| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        // Border + "(○) " + four cells of text + three cells of separation.
        assert_eq!(
            radio.handle_mouse(click(12), Rect::new(0, 0, 30, 3)),
            ControlOutcome::Changed
        );
        assert_eq!(radio.state.selected_idx(), 1);
        assert_eq!(
            radio.handle_mouse(click(10), Rect::new(0, 0, 30, 3)),
            ControlOutcome::Ignored
        );
        assert_eq!(radio.state.selected_idx(), 1);
        assert_eq!(
            radio.handle_mouse(click(5), Rect::new(0, 0, 30, 3)),
            ControlOutcome::Changed
        );
        assert_eq!(radio.state.selected_idx(), 0);
    }

    #[test]
    fn selected_option_stays_visible_after_navigation_and_resize() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut state = RadioState::new(0);
        let options = ["first", "last"];
        let mut radio = RadioControl::new(&mut state, "Choice", &options);
        let mut terminal = Terminal::new(TestBackend::new(12, 3)).unwrap();
        radio.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        terminal
            .draw(|frame| {
                radio.render(
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
        let visible = (1..11)
            .map(|column| buffer[(column, 1)].symbol())
            .collect::<String>();
        assert_eq!(visible, "  (●) last");
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 7,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            radio.handle_mouse(click, Rect::new(0, 0, 12, 3)),
            ControlOutcome::Consumed
        );
        assert_eq!(radio.state.selected_idx(), 1);

        let mut wide = Terminal::new(TestBackend::new(40, 3)).unwrap();
        wide.draw(|frame| {
            radio.render(
                frame,
                frame.area(),
                ControlContext {
                    focused: true,
                    enabled: true,
                },
            )
        })
        .unwrap();
        assert_eq!(
            radio.handle_mouse(MouseEvent { column: 5, ..click }, Rect::new(0, 0, 40, 3)),
            ControlOutcome::Changed
        );
        assert_eq!(radio.state.selected_idx(), 0);
        assert!(
            !RadioControl::new(&mut RadioState::new(1), "Empty", &[] as &[&str]).is_focusable()
        );
        assert_eq!(
            RadioControl::new(&mut RadioState::new(9), "Choice", &["one"])
                .state
                .selected_idx(),
            0
        );
    }
}
