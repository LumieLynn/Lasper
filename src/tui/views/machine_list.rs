use crossterm::event::KeyEvent;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::ListItem,
    Frame,
};

use crate::nspawn::{MachineEntry, MachineState};
use crate::tui::core::EventResult;
use crate::tui::theme;
use crate::tui::widgets::lists::resource_list::{ResourceList, ResourceListRender};

pub struct MachineListComponent {
    list: ResourceList,
}

impl MachineListComponent {
    pub fn new() -> Self {
        Self {
            list: ResourceList::new(" Machines "),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_with_data(
        &mut self,
        f: &mut Frame,
        area: Rect,
        entries: &[MachineEntry],
        selected: usize,
        focused: bool,
        resize_mode: bool,
    ) {
        self.list.set_focus(focused);
        let t = theme::theme();
        self.list.render(
            f,
            area,
            entries,
            ResourceListRender {
                selected,
                resize_mode,
                empty_message: "No running machines found",
                trailing_title: None,
            },
            |entry, styles| {
                let icon_style = match &entry.state {
                    MachineState::Running | MachineState::Starting => {
                        Style::default().fg(t.list_icon_alive)
                    }
                    MachineState::Exiting | MachineState::Off => {
                        Style::default().fg(t.list_icon_dead)
                    }
                };
                let icon = match &entry.state {
                    MachineState::Running => "● ",
                    MachineState::Starting => "◑ ",
                    MachineState::Exiting => "◐ ",
                    MachineState::Off => "○ ",
                };
                let mut spans = vec![
                    styles.cursor_span(),
                    Span::styled(icon, icon_style),
                    Span::styled(entry.name.as_str(), styles.text),
                    Span::styled(format!(" ({})", entry.state.label()), styles.text),
                ];
                if let Some(address) = &entry.address {
                    spans.push(Span::styled(
                        format!(" - {address}"),
                        Style::default().fg(t.list_addr).add_modifier(Modifier::DIM),
                    ));
                }
                ListItem::new(Line::from(spans))
            },
        );
    }

    /// Handles navigation keys and returns the corresponding AppMessage.
    /// j/↓ → ListNext, k/↑ → ListPrev. All other keys are Ignored.
    pub fn handle_key(&mut self, key: KeyEvent, len: usize) -> EventResult {
        self.list.handle_key(key, len)
    }
}
