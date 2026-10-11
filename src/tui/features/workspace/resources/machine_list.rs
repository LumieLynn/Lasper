use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::ListItem,
    Frame,
};

use crate::domain::runtime::{MachineEntry, MachineState};
use crate::tui::features::workspace::resources::list::{ResourceList, ResourceListRender};
use crate::tui::foundation::footer::FooterRegistry;
use crate::tui::foundation::theme;
use crate::tui::protocol::EventResult;

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
                    MachineState::Exiting | MachineState::Unknown(_) => {
                        Style::default().fg(t.list_icon_dead)
                    }
                };
                let icon = match &entry.state {
                    MachineState::Running => "● ",
                    MachineState::Starting => "◑ ",
                    MachineState::Exiting => "◐ ",
                    MachineState::Unknown(_) => "? ",
                };
                let mut spans = vec![
                    styles.cursor_span(),
                    Span::styled(icon, icon_style),
                    Span::styled(entry.name.as_str(), styles.text),
                    Span::styled(format!(" ({})", entry.state.label()), styles.text),
                ];
                if let Some(address) = entry.addresses.primary() {
                    spans.push(Span::styled(
                        format!(" - {address}"),
                        Style::default().fg(t.list_addr).add_modifier(Modifier::DIM),
                    ));
                }
                ListItem::new(Line::from(spans))
            },
        );
    }

    pub fn handle_key(&mut self, key: KeyEvent, len: usize, selected: usize) -> EventResult {
        self.list.handle_key(key, len, selected)
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        self.list.handle_mouse(mouse)
    }

    pub(crate) fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        self.list.register_footer_hints(footer);
    }
}
