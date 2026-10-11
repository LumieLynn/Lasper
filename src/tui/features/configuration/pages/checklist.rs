//! Shared state machine for endpoint declaration checklists.

use std::collections::BTreeSet;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::application::configuration::ConfigurationTarget;
use crate::tui::features::configuration::page::ConfigurationPane;
use crate::tui::foundation::scrolling::ScrollAction;
use crate::tui::foundation::selection::navigate_selection;
use crate::tui::foundation::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChecklistFocus {
    Bindings,
    RuntimeAccess,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ChecklistHitAreas {
    pub bindings: Vec<(Rect, usize)>,
    pub checkboxes: Vec<(Rect, usize)>,
    pub access: Rect,
}

pub(super) struct PageChecklist<T> {
    pub focus: ChecklistFocus,
    pub list: ListState,
    pub items: Vec<T>,
    order: Vec<usize>,
    focusable: Vec<bool>,
    pub expanded: BTreeSet<usize>,
    pub hits: ChecklistHitAreas,
    expand_initial_selection: bool,
    viewport_rows: usize,
}

impl<T> Default for PageChecklist<T> {
    fn default() -> Self {
        Self {
            focus: ChecklistFocus::Bindings,
            list: ListState::default(),
            items: Vec::new(),
            order: Vec::new(),
            focusable: Vec::new(),
            expanded: BTreeSet::new(),
            hits: ChecklistHitAreas::default(),
            expand_initial_selection: true,
            viewport_rows: 0,
        }
    }
}

impl<T> PageChecklist<T> {
    pub fn reset(&mut self) {
        self.focus = ChecklistFocus::Bindings;
        self.list = ListState::default();
        self.items.clear();
        self.order.clear();
        self.focusable.clear();
        self.expanded.clear();
        self.hits = ChecklistHitAreas::default();
        self.expand_initial_selection = true;
    }

    pub fn load(&mut self, items: Vec<T>) {
        self.items = items;
        self.order = (0..self.items.len()).collect();
        self.focusable = vec![true; self.items.len()];
        self.list = ListState::default().with_selected((!self.items.is_empty()).then_some(0));
        self.expanded.clear();
        if !self.items.is_empty() {
            self.expanded.insert(0);
        }
        self.focus = ChecklistFocus::Bindings;
        self.expand_initial_selection = true;
    }

    pub fn selected(&self) -> Option<usize> {
        self.list
            .selected()
            .and_then(|position| self.order.get(position).copied())
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() && self.focusable.get(index).copied().unwrap_or(false) {
            if let Some(position) = self.order.iter().position(|item| *item == index) {
                self.list.select(Some(position));
            }
        }
    }

    pub fn set_order(&mut self, order: Vec<usize>, focusable: Vec<bool>) {
        let selected = self.selected();
        debug_assert_eq!(order.len(), self.items.len());
        debug_assert_eq!(focusable.len(), self.items.len());
        self.order = order;
        self.focusable = focusable;
        let replacement = selected
            .filter(|index| self.focusable.get(*index).copied().unwrap_or(false))
            .or_else(|| {
                self.order
                    .iter()
                    .copied()
                    .find(|index| self.focusable.get(*index).copied().unwrap_or(false))
            });
        self.list.select(
            replacement.and_then(|index| self.order.iter().position(|item| *item == index)),
        );
        if self.expand_initial_selection {
            if let Some(index) = replacement {
                self.expanded.insert(index);
                self.expand_initial_selection = false;
            }
        }
    }

    pub fn is_focusable(&self, index: usize) -> bool {
        self.focusable.get(index).copied().unwrap_or(false)
    }

    pub fn bindings_focused(&self) -> bool {
        self.focus == ChecklistFocus::Bindings
    }

    pub fn focus_bindings(&mut self) {
        self.focus = ChecklistFocus::Bindings;
    }

    pub fn focus_runtime(&mut self) {
        self.focus = ChecklistFocus::RuntimeAccess;
    }

    pub fn move_focus(&mut self, action: ScrollAction, runtime_access_available: bool) {
        let runtime_position = self.order.len();
        let current = if self.bindings_focused() {
            self.list.selected()
        } else {
            Some(runtime_position)
        };
        let next = navigate_selection(
            action,
            current,
            self.order.len() + usize::from(runtime_access_available),
            self.viewport_rows,
            false,
            |position| {
                if position == runtime_position {
                    runtime_access_available
                } else {
                    self.is_focusable(self.order[position])
                }
            },
        );
        if let Some(position) = next {
            if position == runtime_position {
                self.focus_runtime();
            } else {
                self.list.select(Some(position));
                self.focus_bindings();
            }
        }
    }

    pub fn toggle_selected_expansion(&mut self) {
        let Some(selected) = self.selected() else {
            return;
        };
        self.expand_initial_selection = false;
        if !self.expanded.remove(&selected) {
            self.expanded.insert(selected);
        }
    }

    pub fn set_selected_expanded(&mut self, expanded: bool) {
        let Some(selected) = self.selected() else {
            return;
        };
        self.expand_initial_selection = false;
        if expanded {
            self.expanded.insert(selected);
        } else {
            self.expanded.remove(&selected);
        }
    }
}

pub(super) enum ChecklistBody<'a> {
    Message(&'a str),
    Entries(Vec<Vec<Line<'static>>>),
}

pub(super) fn render_endpoint_checklist<T>(
    frame: &mut Frame,
    area: Rect,
    title: &'static str,
    pane: ConfigurationPane,
    target: &ConfigurationTarget,
    checklist: &mut PageChecklist<T>,
    body: ChecklistBody<'_>,
) -> ChecklistHitAreas {
    let mut hits = ChecklistHitAreas::default();
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(
            Style::default().fg(crate::tui::foundation::theme::widget_border_color(
                pane == ConfigurationPane::Content,
                true,
            )),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Min(0),
        ratatui::layout::Constraint::Length(3),
    ])
    .split(inner);
    checklist.viewport_rows = usize::from(rows[0].height);

    match body {
        ChecklistBody::Message(message) => {
            frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), rows[0]);
        }
        ChecklistBody::Entries(entries) => {
            let entries = entries.into_iter().enumerate().collect::<Vec<_>>();
            let ordered_entries = checklist
                .order
                .iter()
                .filter_map(|index| entries.get(*index).cloned())
                .collect::<Vec<_>>();
            let heights = ordered_entries
                .iter()
                .map(|(_, lines)| lines.len())
                .collect::<Vec<_>>();
            frame.render_stateful_widget(
                List::new(
                    ordered_entries
                        .into_iter()
                        .map(|(index, lines)| {
                            let style = if checklist.selected() == Some(index) {
                                Style::default().fg(if checklist.bindings_focused() {
                                    theme::theme().list_highlight_symbol
                                } else {
                                    theme::theme().text_secondary
                                })
                            } else if !checklist.is_focusable(index) {
                                Style::default().fg(theme::theme().text_dim)
                            } else {
                                Style::default()
                            };
                            ListItem::new(Text::from(lines)).style(style)
                        })
                        .collect::<Vec<_>>(),
                )
                .highlight_symbol(">> ")
                .highlight_style(if checklist.bindings_focused() {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
                rows[0],
                &mut checklist.list,
            );
            let mut y = rows[0].y;
            for (display_index, height) in heights.iter().enumerate().skip(checklist.list.offset())
            {
                let height = (*height as u16).min(rows[0].bottom().saturating_sub(y));
                if height == 0 {
                    break;
                }
                if let Some(index) = checklist.order.get(display_index).copied() {
                    if checklist.is_focusable(index) {
                        hits.bindings
                            .push((Rect::new(rows[0].x, y, rows[0].width, height), index));
                        hits.checkboxes
                            .push((Rect::new(rows[0].x, y, 7.min(rows[0].width), 1), index));
                    }
                }
                y += height;
            }
        }
    }

    render_runtime_access(frame, rows[1], target, pane, checklist, &mut hits);
    hits
}

fn render_runtime_access<T>(
    frame: &mut Frame,
    area: Rect,
    target: &ConfigurationTarget,
    pane: ConfigurationPane,
    checklist: &PageChecklist<T>,
    hits: &mut ChecklistHitAreas,
) {
    let enabled = matches!(target, ConfigurationTarget::Machine(_));
    let focused = enabled
        && pane == ConfigurationPane::Content
        && checklist.focus == ChecklistFocus::RuntimeAccess;
    hits.access = area;
    let label = if enabled {
        " Runtime access... "
    } else {
        " Runtime access is available for running machines "
    };
    frame.render_widget(
        Paragraph::new(label)
            .alignment(ratatui::layout::Alignment::Center)
            .style(if !enabled {
                Style::default().fg(theme::theme().text_dim)
            } else if focused {
                Style::default()
                    .fg(theme::theme().button_focused_fg)
                    .bg(theme::theme().button_focused_bg)
            } else {
                Style::default().fg(theme::theme().button_unfocused_fg)
            })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(ratatui::widgets::BorderType::Rounded)
                    .border_style(Style::default().fg(if !enabled {
                        theme::theme().border_disabled
                    } else if focused {
                        theme::theme().button_border_focused
                    } else {
                        theme::theme().button_border_unfocused
                    })),
            ),
        area,
    );
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(in crate::tui::features::configuration) struct ChecklistTestState {
    pub selected: Option<usize>,
    pub expanded: std::collections::BTreeSet<usize>,
    pub modal_open: bool,
    pub bindings: Vec<(Rect, usize)>,
    pub checkboxes: Vec<(Rect, usize)>,
    pub access: Rect,
}
