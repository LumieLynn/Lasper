//! Two-level section navigation. Categories and actual pages have separate
//! identities; collapsing a category does not discard the active page.

use std::collections::BTreeSet;

use crossterm::event::KeyCode;
use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState},
    Frame,
};

use crate::tui::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConfigurationPage {
    X11,
}

impl ConfigurationPage {
    fn label(self) -> &'static str {
        match self {
            Self::X11 => "X11",
        }
    }
    fn category(self) -> Category {
        match self {
            Self::X11 => Category::HostIntegration,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    HostIntegration,
}

impl Category {
    const ALL: &'static [Self] = &[Self::HostIntegration];
    fn label(self) -> &'static str {
        match self {
            Self::HostIntegration => "Host Integration",
        }
    }
    fn pages(self) -> &'static [ConfigurationPage] {
        match self {
            Self::HostIntegration => &[ConfigurationPage::X11],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigationItem {
    Category(Category),
    Page(ConfigurationPage),
}

pub(super) struct ConfigurationNavigation {
    selected: NavigationItem,
    active: ConfigurationPage,
    expanded: BTreeSet<Category>,
    list: ListState,
    hits: Vec<(Rect, NavigationItem)>,
}

impl Default for ConfigurationNavigation {
    fn default() -> Self {
        Self {
            selected: NavigationItem::Page(ConfigurationPage::X11),
            active: ConfigurationPage::X11,
            expanded: BTreeSet::from([Category::HostIntegration]),
            list: ListState::default(),
            hits: Vec::new(),
        }
    }
}

impl ConfigurationNavigation {
    pub(super) fn active_page(&self) -> ConfigurationPage {
        self.active
    }

    fn visible_items(&self) -> Vec<NavigationItem> {
        Category::ALL
            .iter()
            .flat_map(|category| {
                std::iter::once(NavigationItem::Category(*category)).chain(
                    category
                        .pages()
                        .iter()
                        .filter(|_| self.expanded.contains(category))
                        .copied()
                        .map(NavigationItem::Page),
                )
            })
            .collect()
    }

    pub(super) fn move_selection(&mut self, down: bool) {
        let items = self.visible_items();
        let selected = items
            .iter()
            .position(|item| *item == self.selected)
            .unwrap_or(0);
        let next = if down {
            (selected + 1).min(items.len() - 1)
        } else {
            selected.saturating_sub(1)
        };
        self.selected = items[next];
    }

    /// Return true when the user enters a page's content pane.
    pub(super) fn handle_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Left | KeyCode::Char('h') => match self.selected {
                NavigationItem::Page(page) => {
                    self.selected = NavigationItem::Category(page.category())
                }
                NavigationItem::Category(category) => {
                    self.expanded.remove(&category);
                }
            },
            KeyCode::Right | KeyCode::Char('l') => match self.selected {
                NavigationItem::Category(category) => {
                    if !self.expanded.insert(category) {
                        self.selected = NavigationItem::Page(category.pages()[0]);
                    }
                }
                NavigationItem::Page(page) => {
                    self.active = page;
                    return true;
                }
            },
            KeyCode::Enter | KeyCode::Char(' ') => match self.selected {
                NavigationItem::Category(category) => {
                    self.toggle_category(category);
                }
                NavigationItem::Page(page) => {
                    self.active = page;
                    return true;
                }
            },
            _ => {}
        }
        false
    }

    fn toggle_category(&mut self, category: Category) {
        if !self.expanded.remove(&category) {
            self.expanded.insert(category);
        }
    }

    pub(super) fn click(&mut self, position: Position) {
        let Some(item) = self
            .hits
            .iter()
            .find(|(area, _)| area.contains(position))
            .map(|(_, item)| *item)
        else {
            return;
        };
        self.selected = item;
        match item {
            NavigationItem::Category(category) => self.toggle_category(category),
            NavigationItem::Page(page) => self.active = page,
        }
    }

    pub(super) fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        self.hits.clear();
        let block = Block::default()
            .title(" Sections ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(crate::tui::widget_border_color(focused, true)));
        let inner = block.inner(area);
        let items = self.visible_items();
        self.list
            .select(items.iter().position(|item| *item == self.selected));
        let t = theme::theme();
        let entries = items
            .iter()
            .map(|item| {
                let selected = *item == self.selected;
                let (indent, pointer, disclosure, label) = match item {
                    NavigationItem::Category(category) => (
                        "",
                        if selected
                            || matches!(self.selected, NavigationItem::Page(page) if page.category() == *category)
                        {
                            "> "
                        } else {
                            "  "
                        },
                        if self.expanded.contains(category) {
                            "[-] "
                        } else {
                            "[+] "
                        },
                        category.label(),
                    ),
                    NavigationItem::Page(page) => {
                        ("  ", if selected { ">> " } else { "   " }, "", page.label())
                    }
                };
                let mut cursor = Style::default().fg(if focused {
                    t.list_cursor_focused
                } else {
                    t.list_cursor_unfocused
                });
                let mut text = Style::default().fg(if selected {
                    if focused {
                        t.list_selected_focused
                    } else {
                        t.list_selected_unfocused
                    }
                } else {
                    t.list_unselected
                });
                if selected && focused {
                    cursor = cursor.add_modifier(Modifier::BOLD);
                    text = text.add_modifier(Modifier::BOLD);
                }
                ListItem::new(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(pointer, cursor),
                    Span::styled(disclosure, text),
                    Span::styled(label, text),
                ]))
            })
            .collect::<Vec<_>>();
        frame.render_stateful_widget(List::new(entries).block(block), area, &mut self.list);
        for (row, item) in items
            .into_iter()
            .skip(self.list.offset())
            .take(inner.height as usize)
            .enumerate()
        {
            self.hits.push((
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
                item,
            ));
        }
    }
}
