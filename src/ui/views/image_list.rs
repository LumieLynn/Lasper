use crate::nspawn::ImageEntry;
use crate::ui::core::{AppMessage, EventResult, ListMessage};
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

pub struct ImageListComponent {
    state: ListState,
    focused: bool,
    active_tab: ImageListTab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageListTab {
    Regular,
    Internal,
}

impl ImageListComponent {
    pub fn new() -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            state,
            focused: false,
            active_tab: ImageListTab::Regular,
        }
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn shows_internal(&self) -> bool {
        self.active_tab == ImageListTab::Internal
    }

    fn switch_tab(&mut self, tab: ImageListTab) -> EventResult {
        self.active_tab = tab;
        EventResult::Consumed
    }

    pub fn handle_key(&mut self, key: KeyEvent, len: usize) -> EventResult {
        match key.code {
            KeyCode::Char('[') | KeyCode::Char(']') => {
                let tab = match self.active_tab {
                    ImageListTab::Regular => ImageListTab::Internal,
                    ImageListTab::Internal => ImageListTab::Regular,
                };
                return self.switch_tab(tab);
            }
            KeyCode::Char('1') if key.modifiers.contains(KeyModifiers::ALT) => {
                return self.switch_tab(ImageListTab::Regular);
            }
            KeyCode::Char('2') if key.modifiers.contains(KeyModifiers::ALT) => {
                return self.switch_tab(ImageListTab::Internal);
            }
            _ => {}
        }

        if len == 0 {
            return EventResult::Ignored;
        }
        let current = self.state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.state.select(Some((current + 1) % len));
                EventResult::Message(AppMessage::List(ListMessage::Next))
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.state
                    .select(Some(if current == 0 { len - 1 } else { current - 1 }));
                EventResult::Message(AppMessage::List(ListMessage::Prev))
            }
            _ => EventResult::Ignored,
        }
    }

    pub fn render(
        &mut self,
        f: &mut Frame,
        area: Rect,
        images: &[ImageEntry],
        selected: usize,
        resize_mode: bool,
    ) {
        let t = theme::theme();
        let block = self.block(area.width, resize_mode);
        if images.is_empty() {
            let message = if self.shows_internal() {
                "  No internal images found"
            } else {
                "  No regular machine images found"
            };
            f.render_widget(
                Paragraph::new(message).block(block.style(Style::default().fg(t.text_secondary))),
                area,
            );
            return;
        }

        self.state.select(Some(selected.min(images.len() - 1)));
        let items: Vec<ListItem> = images
            .iter()
            .map(|image| {
                let selected = self
                    .state
                    .selected()
                    .is_some_and(|idx| images[idx].name == image.name);
                let style = if selected {
                    let style = Style::default().fg(if self.focused {
                        t.list_selected_focused
                    } else {
                        t.list_selected_unfocused
                    });
                    if self.focused {
                        style.add_modifier(Modifier::BOLD)
                    } else {
                        style
                    }
                } else {
                    Style::default().fg(t.list_unselected)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(if selected { ">> " } else { "   " }, style),
                    Span::styled("◆ ", Style::default().fg(t.list_icon_alive)),
                    Span::styled(image.name.clone(), style),
                    Span::styled(format!(" ({})", image.image_type), style),
                    if image.readonly {
                        Span::styled(" [ro]", Style::default().fg(t.text_secondary))
                    } else {
                        Span::raw("")
                    },
                ]))
            })
            .collect();

        let border_color = crate::ui::panel_border_color(resize_mode, self.focused, true);
        let list = List::new(items)
            .block(block.border_style(Style::default().fg(border_color)))
            .highlight_style(Style::default());
        f.render_stateful_widget(list, area, &mut self.state);
    }

    fn block(&self, width: u16, resize_mode: bool) -> Block<'static> {
        let t = theme::theme();
        let labels = if width >= 32 {
            [" Regular ", " Internal "]
        } else {
            [" Reg ", " Int "]
        };
        let mut spans = Vec::with_capacity(3);
        for (index, (tab, label)) in [ImageListTab::Regular, ImageListTab::Internal]
            .into_iter()
            .zip(labels)
            .enumerate()
        {
            let style = if tab == self.active_tab {
                Style::default()
                    .fg(if self.focused {
                        t.tab_active_focused
                    } else {
                        t.tab_active_unfocused
                    })
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.tab_inactive)
            };
            spans.push(Span::styled(label, style));
            if index == 0 {
                spans.push(Span::raw("-"));
            }
        }

        Block::default()
            .title(" Images ")
            .title(Line::from(spans).right_aligned())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(crate::ui::panel_border_color(
                resize_mode,
                self.focused,
                true,
            )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_tabs_switch_even_when_the_current_tab_is_empty() {
        let mut list = ImageListComponent::new();

        assert_eq!(
            list.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), 0),
            EventResult::Consumed
        );
        assert_eq!(list.active_tab, ImageListTab::Internal);
    }
}
