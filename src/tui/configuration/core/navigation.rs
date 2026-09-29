//! Navigation derived from registered page descriptors.

use std::collections::BTreeSet;

use crossterm::event::KeyCode;
use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState},
    Frame,
};

use crate::tui::configuration::page::{
    ConfigurationPageDescriptor, ConfigurationPageId, ConfigurationSection,
};
use crate::tui::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum NavigationNodeId {
    Section(ConfigurationSection),
    Page(ConfigurationPageId),
}

#[derive(Clone, Copy, Debug)]
struct VisibleNode {
    id: NavigationNodeId,
    depth: usize,
}

pub(super) struct ConfigurationNavigation {
    pages: Vec<ConfigurationPageDescriptor>,
    selected: NavigationNodeId,
    current_page: ConfigurationPageId,
    expanded: BTreeSet<ConfigurationSection>,
    list: ListState,
    hits: Vec<(Rect, NavigationNodeId)>,
}

impl ConfigurationNavigation {
    pub(super) fn new(pages: &[ConfigurationPageDescriptor]) -> Self {
        let pages = pages.to_vec();
        let first = pages
            .first()
            .expect("configuration workspace requires at least one registered page");
        Self {
            selected: NavigationNodeId::Page(first.id),
            current_page: first.id,
            expanded: BTreeSet::from([first.section]),
            pages,
            list: ListState::default(),
            hits: Vec::new(),
        }
    }

    pub(super) fn current_page(&self) -> ConfigurationPageId {
        self.current_page
    }

    fn sections(&self) -> Vec<ConfigurationSection> {
        let mut sections = Vec::new();
        for page in &self.pages {
            if !sections.contains(&page.section) {
                sections.push(page.section);
            }
        }
        sections
    }

    fn visible_items(&self) -> Vec<VisibleNode> {
        let mut visible = Vec::new();
        for section in self.sections() {
            visible.push(VisibleNode {
                id: NavigationNodeId::Section(section),
                depth: 0,
            });
            if self.expanded.contains(&section) {
                visible.extend(
                    self.pages
                        .iter()
                        .filter(|page| page.section == section)
                        .map(|page| VisibleNode {
                            id: NavigationNodeId::Page(page.id),
                            depth: 1,
                        }),
                );
            }
        }
        visible
    }

    fn parent(&self, id: NavigationNodeId) -> Option<NavigationNodeId> {
        match id {
            NavigationNodeId::Section(_) => None,
            NavigationNodeId::Page(page) => self
                .pages
                .iter()
                .find(|descriptor| descriptor.id == page)
                .map(|descriptor| NavigationNodeId::Section(descriptor.section)),
        }
    }

    fn children(
        &self,
        section: ConfigurationSection,
    ) -> impl Iterator<Item = ConfigurationPageId> + '_ {
        self.pages
            .iter()
            .filter(move |page| page.section == section)
            .map(|page| page.id)
    }

    fn label(&self, id: NavigationNodeId) -> &'static str {
        match id {
            NavigationNodeId::Section(section) => section.label,
            NavigationNodeId::Page(page) => self
                .pages
                .iter()
                .find(|descriptor| descriptor.id == page)
                .map(|descriptor| descriptor.label)
                .expect("registered navigation page has a descriptor"),
        }
    }

    pub(super) fn move_selection(&mut self, down: bool) {
        let items = self.visible_items();
        let selected = items
            .iter()
            .position(|item| item.id == self.selected)
            .unwrap_or(0);
        let next = if down {
            (selected + 1).min(items.len().saturating_sub(1))
        } else {
            selected.saturating_sub(1)
        };
        if let Some(item) = items.get(next) {
            self.select(item.id);
        }
    }

    fn select(&mut self, id: NavigationNodeId) {
        self.selected = id;
        if let NavigationNodeId::Page(page) = id {
            self.current_page = page;
        }
    }

    /// Return true when the selected page asks to enter its content pane.
    pub(super) fn handle_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(parent) = self.parent(self.selected) {
                    self.select(parent);
                } else if let NavigationNodeId::Section(section) = self.selected {
                    self.expanded.remove(&section);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => match self.selected {
                NavigationNodeId::Section(section) => {
                    if !self.expanded.insert(section) {
                        let first = self.children(section).next();
                        if let Some(first) = first {
                            self.select(NavigationNodeId::Page(first));
                        }
                    }
                }
                NavigationNodeId::Page(_) => return true,
            },
            KeyCode::Enter | KeyCode::Char(' ') => match self.selected {
                NavigationNodeId::Section(section) => self.toggle_section(section),
                NavigationNodeId::Page(_) => return true,
            },
            _ => {}
        }
        false
    }

    fn toggle_section(&mut self, section: ConfigurationSection) {
        if !self.expanded.remove(&section) {
            self.expanded.insert(section);
        }
    }

    pub(super) fn click(&mut self, position: Position) {
        let Some(id) = self
            .hits
            .iter()
            .find(|(area, _)| area.contains(position))
            .map(|(_, id)| *id)
        else {
            return;
        };
        self.select(id);
        if let NavigationNodeId::Section(section) = id {
            self.toggle_section(section);
        }
    }

    fn is_current_section(&self, section: ConfigurationSection) -> bool {
        self.pages
            .iter()
            .any(|page| page.id == self.current_page && page.section == section)
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
            .select(items.iter().position(|item| item.id == self.selected));
        let t = theme::theme();
        let entries = items
            .iter()
            .map(|item| {
                let selected = item.id == self.selected;
                let active_parent = matches!(item.id, NavigationNodeId::Section(section) if self.is_current_section(section));
                let indent = "  ".repeat(item.depth);
                let pointer = if selected || active_parent {
                    ">".repeat(item.depth + 1) + " "
                } else {
                    " ".repeat(item.depth + 1) + " "
                };
                let disclosure = match item.id {
                    NavigationNodeId::Section(section) if self.expanded.contains(&section) => "[-] ",
                    NavigationNodeId::Section(_) => "[+] ",
                    NavigationNodeId::Page(_) => "",
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
                    Span::styled(self.label(item.id), text),
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
                item.id,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_accepts_page_owned_ids_and_new_sections_without_display_branches() {
        let host = ConfigurationSection {
            key: "host",
            label: "Host Integration",
        };
        let network = ConfigurationSection {
            key: "network",
            label: "Networking",
        };
        let first = ConfigurationPageId::new("first");
        let second = ConfigurationPageId::new("second");
        let additional = ConfigurationPageId::new("veth");
        let mut navigation = ConfigurationNavigation::new(&[
            ConfigurationPageDescriptor {
                id: first,
                section: host,
                label: "First",
            },
            ConfigurationPageDescriptor {
                id: second,
                section: host,
                label: "Second",
            },
            ConfigurationPageDescriptor {
                id: additional,
                section: network,
                label: "Veth",
            },
        ]);
        assert_eq!(navigation.current_page(), first);
        navigation.move_selection(true);
        assert_eq!(navigation.current_page(), second);
        navigation.move_selection(true);
        assert_eq!(navigation.selected, NavigationNodeId::Section(network));
        assert_eq!(navigation.label(navigation.selected), "Networking");
        navigation.handle_key(KeyCode::Right);
        navigation.move_selection(true);
        assert_eq!(navigation.current_page(), additional);
        navigation.handle_key(KeyCode::Left);
        navigation.handle_key(KeyCode::Char(' '));
        assert_eq!(navigation.current_page(), additional);
        assert!(!navigation
            .visible_items()
            .iter()
            .any(|node| node.id == NavigationNodeId::Page(additional)));
    }
}
