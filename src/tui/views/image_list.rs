use crate::domain::runtime::ImageEntry;
use crate::tui::core::EventResult;
use crate::tui::theme;
use crate::tui::views::title_tabs::{
    bordered_title_tab_hitboxes, clicked_title_tab, clip_title_tabs_before, TitleTabHitbox,
};
use crate::tui::widgets::lists::resource_list::{ResourceList, ResourceListRender};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::ListItem,
    Frame,
};
use unicode_width::UnicodeWidthStr;

pub struct ImageListComponent {
    list: ResourceList,
    active_tab: ImageListTab,
    tab_hitboxes: Vec<TitleTabHitbox<ImageListTab>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageListTab {
    Regular,
    Internal,
}

impl ImageListComponent {
    pub fn new() -> Self {
        Self {
            list: ResourceList::new(" Images "),
            active_tab: ImageListTab::Regular,
            tab_hitboxes: Vec::new(),
        }
    }

    pub fn set_focus(&mut self, focused: bool) {
        self.list.set_focus(focused);
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

        self.list.handle_key(key, len)
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        match clicked_title_tab(&self.tab_hitboxes, mouse) {
            Some(tab) => self.switch_tab(tab),
            None => EventResult::Ignored,
        }
    }

    pub fn render(
        &mut self,
        f: &mut Frame,
        area: Rect,
        images: &[ImageEntry],
        selected: usize,
        removing: &std::collections::HashSet<String>,
        resize_mode: bool,
    ) {
        let t = theme::theme();
        let labels = Self::tab_labels(area.width);
        self.tab_hitboxes = bordered_title_tab_hitboxes(
            area,
            Alignment::Right,
            &[
                (ImageListTab::Regular, labels[0].width()),
                (ImageListTab::Internal, labels[1].width()),
            ],
            1,
        );
        let first_unobscured = area
            .x
            .saturating_add(1)
            .saturating_add(u16::try_from(" Images ".width()).unwrap_or(u16::MAX));
        clip_title_tabs_before(&mut self.tab_hitboxes, first_unobscured);
        let trailing_title = self.tab_title(labels);
        let empty_message = if self.shows_internal() {
            "No internal images found"
        } else {
            "No regular images found"
        };
        self.list.render(
            f,
            area,
            images,
            ResourceListRender {
                selected,
                resize_mode,
                empty_message,
                trailing_title: Some(trailing_title),
            },
            |image, styles| {
                ListItem::new(Line::from(vec![
                    styles.cursor_span(),
                    Span::styled("◆ ", Style::default().fg(t.list_icon_alive)),
                    Span::styled(image.name.as_str(), styles.text),
                    Span::styled(format!(" ({})", image.image_type), styles.text),
                    if removing.contains(&image.name) {
                        Span::styled(" [removing]", Style::default().fg(t.warning))
                    } else {
                        Span::raw("")
                    },
                    if image.readonly {
                        Span::styled(" [ro]", Style::default().fg(t.text_secondary))
                    } else {
                        Span::raw("")
                    },
                ]))
            },
        );
    }

    fn tab_labels(width: u16) -> [&'static str; 2] {
        if width >= 32 {
            [" Regular ", " Internal "]
        } else {
            [" Reg ", " Int "]
        }
    }

    fn tab_title(&self, labels: [&'static str; 2]) -> Line<'static> {
        let t = theme::theme();
        let mut spans = Vec::with_capacity(3);
        for (index, (tab, label)) in [ImageListTab::Regular, ImageListTab::Internal]
            .into_iter()
            .zip(labels)
            .enumerate()
        {
            let style = if tab == self.active_tab {
                Style::default()
                    .fg(if self.list.is_focused() {
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

        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{MouseButton, MouseEventKind};
    use ratatui::{backend::TestBackend, Terminal};
    use std::collections::HashSet;

    #[test]
    fn image_tabs_switch_even_when_the_current_tab_is_empty() {
        let mut list = ImageListComponent::new();

        assert_eq!(
            list.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), 0),
            EventResult::Consumed
        );
        assert_eq!(list.active_tab, ImageListTab::Internal);
    }

    #[test]
    fn rendered_image_tabs_are_clickable() {
        crate::tui::theme::init_theme(crate::tui::theme::Theme::dark());
        let mut list = ImageListComponent::new();
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();

        terminal
            .draw(|frame| list.render(frame, frame.area(), &[], 0, &HashSet::new(), false))
            .unwrap();
        let internal = list
            .tab_hitboxes
            .iter()
            .find(|hitbox| hitbox.value == ImageListTab::Internal)
            .unwrap()
            .area;

        assert_eq!(
            list.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: internal.x,
                row: internal.y,
                modifiers: KeyModifiers::NONE,
            }),
            EventResult::Consumed
        );
        assert!(list.shows_internal());
    }
}
