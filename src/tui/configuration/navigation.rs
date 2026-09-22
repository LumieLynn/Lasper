//! Configuration section navigation.
//!
//! The visible tree is backed by a small static route registry rather than a
//! pair of category/page switches. It is intentionally data-driven at the
//! navigation boundary while page-local controls remain owned by each page.
//! This keeps the current two-level UI simple and leaves room for deeper
//! sections without changing selection or collapse semantics.

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
    fn node(self) -> NavigationNodeId {
        match self {
            Self::X11 => NavigationNodeId::X11,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum NavigationNodeId {
    HostIntegration,
    X11,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigationNodeKind {
    Section,
    Page(ConfigurationPage),
}

#[derive(Clone, Copy, Debug)]
struct NavigationNode {
    parent: Option<NavigationNodeId>,
    label: &'static str,
    kind: NavigationNodeKind,
    children: &'static [NavigationNodeId],
}

const ROOT_NODES: &[NavigationNodeId] = &[NavigationNodeId::HostIntegration];
const HOST_INTEGRATION_CHILDREN: &[NavigationNodeId] = &[NavigationNodeId::X11];
const NO_CHILDREN: &[NavigationNodeId] = &[];

fn node(id: NavigationNodeId) -> NavigationNode {
    match id {
        NavigationNodeId::HostIntegration => NavigationNode {
            parent: None,
            label: "Host Integration",
            kind: NavigationNodeKind::Section,
            children: HOST_INTEGRATION_CHILDREN,
        },
        NavigationNodeId::X11 => NavigationNode {
            parent: Some(NavigationNodeId::HostIntegration),
            label: "X11",
            kind: NavigationNodeKind::Page(ConfigurationPage::X11),
            children: NO_CHILDREN,
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VisibleNode {
    id: NavigationNodeId,
    depth: usize,
}

pub(super) struct ConfigurationNavigation {
    selected: NavigationNodeId,
    active: ConfigurationPage,
    expanded: BTreeSet<NavigationNodeId>,
    list: ListState,
    hits: Vec<(Rect, NavigationNodeId)>,
}

impl Default for ConfigurationNavigation {
    fn default() -> Self {
        Self {
            selected: ConfigurationPage::X11.node(),
            active: ConfigurationPage::X11,
            expanded: BTreeSet::from([NavigationNodeId::HostIntegration]),
            list: ListState::default(),
            hits: Vec::new(),
        }
    }
}

impl ConfigurationNavigation {
    pub(super) fn active_page(&self) -> ConfigurationPage {
        self.active
    }

    fn visible_items(&self) -> Vec<VisibleNode> {
        let mut visible = Vec::new();
        for id in ROOT_NODES {
            self.push_visible(*id, 0, &mut visible);
        }
        visible
    }

    fn push_visible(&self, id: NavigationNodeId, depth: usize, visible: &mut Vec<VisibleNode>) {
        visible.push(VisibleNode { id, depth });
        if !self.expanded.contains(&id) {
            return;
        }
        for child in node(id).children {
            self.push_visible(*child, depth + 1, visible);
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
            self.selected = item.id;
        }
    }

    /// Return true when the user enters a page's content pane.
    pub(super) fn handle_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Left | KeyCode::Char('h') => {
                let current = node(self.selected);
                if let Some(parent) = current.parent {
                    self.selected = parent;
                } else {
                    self.expanded.remove(&self.selected);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => match node(self.selected).kind {
                NavigationNodeKind::Section => {
                    if !self.expanded.insert(self.selected) {
                        if let Some(first_child) = node(self.selected).children.first() {
                            self.selected = *first_child;
                        }
                    }
                }
                NavigationNodeKind::Page(page) => {
                    self.active = page;
                    return true;
                }
            },
            KeyCode::Enter | KeyCode::Char(' ') => match node(self.selected).kind {
                NavigationNodeKind::Section => self.toggle_section(self.selected),
                NavigationNodeKind::Page(page) => {
                    self.active = page;
                    return true;
                }
            },
            _ => {}
        }
        false
    }

    fn toggle_section(&mut self, id: NavigationNodeId) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
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
        self.selected = id;
        match node(id).kind {
            NavigationNodeKind::Section => self.toggle_section(id),
            NavigationNodeKind::Page(page) => self.active = page,
        }
    }

    fn is_active_descendant(&self, id: NavigationNodeId) -> bool {
        let mut current = self.active.node();
        while let Some(parent) = node(current).parent {
            if parent == id {
                return true;
            }
            current = parent;
        }
        false
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
                let current = node(item.id);
                let indent = "  ".repeat(item.depth);
                let pointer = if selected || self.is_active_descendant(item.id) {
                    ">".repeat(item.depth + 1) + " "
                } else {
                    " ".repeat(item.depth + 1) + " "
                };
                let disclosure = match current.kind {
                    NavigationNodeKind::Section if self.expanded.contains(&item.id) => "[-] ",
                    NavigationNodeKind::Section => "[+] ",
                    NavigationNodeKind::Page(_) => "",
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
                    Span::styled(current.label, text),
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
