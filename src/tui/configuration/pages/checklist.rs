//! Shared state machine for endpoint declaration checklists.

use std::collections::BTreeSet;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::super::core::ConfigurationPane;
use crate::application::configuration::ConfigurationTarget;
use crate::tui::theme;

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
    pub expanded: BTreeSet<usize>,
    pub hits: ChecklistHitAreas,
}

impl<T> Default for PageChecklist<T> {
    fn default() -> Self {
        Self {
            focus: ChecklistFocus::Bindings,
            list: ListState::default(),
            items: Vec::new(),
            expanded: BTreeSet::new(),
            hits: ChecklistHitAreas::default(),
        }
    }
}

impl<T> PageChecklist<T> {
    pub fn reset(&mut self) {
        self.focus = ChecklistFocus::Bindings;
        self.list = ListState::default();
        self.items.clear();
        self.expanded.clear();
        self.hits = ChecklistHitAreas::default();
    }

    pub fn load(&mut self, items: Vec<T>) {
        self.items = items;
        self.list = ListState::default().with_selected((!self.items.is_empty()).then_some(0));
        self.expanded.clear();
        if !self.items.is_empty() {
            self.expanded.insert(0);
        }
        self.focus = ChecklistFocus::Bindings;
    }

    pub fn selected(&self) -> Option<usize> {
        self.list.selected()
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.list.select(Some(index));
        }
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

    pub fn move_focus(&mut self, down: bool, runtime_access_available: bool) {
        match (self.focus, down) {
            (ChecklistFocus::Bindings, true) => {
                let current = self.selected().unwrap_or(0);
                if current + 1 < self.items.len() {
                    self.select(current + 1);
                } else if runtime_access_available {
                    self.focus_runtime();
                }
            }
            (ChecklistFocus::Bindings, false) => {
                self.select(self.selected().unwrap_or(0).saturating_sub(1));
            }
            (ChecklistFocus::RuntimeAccess, false) if !self.items.is_empty() => {
                self.select(self.items.len() - 1);
                self.focus_bindings();
            }
            _ => {}
        }
    }

    pub fn toggle_selected_expansion(&mut self) {
        let Some(selected) = self.selected() else {
            return;
        };
        if !self.expanded.remove(&selected) {
            self.expanded.insert(selected);
        }
    }

    pub fn set_selected_expanded(&mut self, expanded: bool) {
        let Some(selected) = self.selected() else {
            return;
        };
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
        .border_style(Style::default().fg(crate::tui::widget_border_color(
            pane == ConfigurationPane::Content,
            true,
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Min(0),
        ratatui::layout::Constraint::Length(3),
    ])
    .split(inner);

    match body {
        ChecklistBody::Message(message) => {
            frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), rows[0]);
        }
        ChecklistBody::Entries(entries) => {
            let heights = entries.iter().map(Vec::len).collect::<Vec<_>>();
            frame.render_stateful_widget(
                List::new(
                    entries
                        .into_iter()
                        .enumerate()
                        .map(|(index, lines)| {
                            let style = if checklist.list.selected() == Some(index) {
                                Style::default().fg(if checklist.bindings_focused() {
                                    theme::theme().list_highlight_symbol
                                } else {
                                    theme::theme().text_secondary
                                })
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
            for (index, height) in heights.iter().enumerate().skip(checklist.list.offset()) {
                let height = (*height as u16).min(rows[0].bottom().saturating_sub(y));
                if height == 0 {
                    break;
                }
                hits.bindings
                    .push((Rect::new(rows[0].x, y, rows[0].width, height), index));
                hits.checkboxes
                    .push((Rect::new(rows[0].x, y, 7.min(rows[0].width), 1), index));
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
