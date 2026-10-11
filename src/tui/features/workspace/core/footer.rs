//! Combine panel-owned hints with the workspace's current controls.

use crate::tui::features::workspace::core::shortcuts::WorkspaceShortcut;
use crate::tui::features::workspace::terminal_panel::{TerminalManager, TerminalPanel};
use crate::tui::features::workspace::Workspace;
use crate::tui::features::workspace::WorkspaceFocus;
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crossterm::event::KeyCode;

impl Workspace {
    pub(crate) fn register_footer_hints(
        &self,
        footer: &mut FooterRegistry,
        terminal: &TerminalManager,
        removing_images: &std::collections::HashSet<String>,
    ) {
        let image = self.focused_image();
        let image_selected = image.is_some();
        let image_removable =
            image.is_some_and(|image| self.resources.image_removable(image, removing_images));
        let shell_available = match self.focus() {
            WorkspaceFocus::Machines | WorkspaceFocus::MachineInspector => {
                self.focused_machine().is_some_and(|machine| {
                    machine.state == crate::domain::runtime::MachineState::Running
                })
            }
            WorkspaceFocus::Images | WorkspaceFocus::ImageInspector => {
                image.is_some_and(|image| self.resources.image_has_running_machine(image))
            }
            WorkspaceFocus::Terminal => self.resources.selected_machine().is_some_and(|machine| {
                machine.state == crate::domain::runtime::MachineState::Running
            }),
        };
        let terminal_insert = self.focus().is_terminal() && terminal.in_insert_mode();
        if self.layout.is_resizing() && !terminal_insert {
            self.register_resize_hints(footer, terminal.is_showing());
            return;
        }

        match self.focus() {
            WorkspaceFocus::Machines => self.resources.register_footer_hints(footer, false),
            WorkspaceFocus::Images => self.resources.register_footer_hints(footer, true),
            WorkspaceFocus::MachineInspector | WorkspaceFocus::ImageInspector => {
                self.inspector.register_footer_hints(footer)
            }
            WorkspaceFocus::Terminal => {
                TerminalPanel.register_footer_hints(footer, terminal);
            }
        }
        // Insert mode consumes workspace shortcuts before they reach its
        // input handler. Only the terminal's own escape/tab hints apply.
        if terminal_insert {
            return;
        }

        use WorkspaceShortcut as Shortcut;
        match self.focus() {
            WorkspaceFocus::Machines => {
                footer.register(Shortcut::Poweroff.hint());
                footer.register(Shortcut::ResourceActions.hint());
                footer.register(Shortcut::Wizard.hint());
            }
            WorkspaceFocus::Images => {
                if image_selected && !self.resources.images_show_internal() {
                    footer.register(Shortcut::Start.hint());
                    footer.register(Shortcut::ResourceActions.hint());
                }
                if image_removable {
                    let hint = Shortcut::RemoveImage.hint();
                    footer.register(if self.resources.images_show_internal() {
                        hint.with_label("delete internal")
                    } else {
                        hint
                    });
                }
                footer.register(Shortcut::Refresh.hint());
            }
            WorkspaceFocus::Terminal if terminal.is_showing() => {
                let hint = Shortcut::ToggleTerminal.hint();
                footer.register(if terminal.maximized {
                    hint.with_label("restore split")
                } else {
                    hint
                });
            }
            _ => {}
        }
        if shell_available {
            footer.register(Shortcut::Shell.hint());
        }
        footer.register(Shortcut::NextPanel.hint());
        if !self.focus().is_terminal() {
            footer.register(Shortcut::Leader.hint());
        }
        footer.register(Shortcut::Help.hint());
        footer.register(Shortcut::Quit.hint());
        footer.register(Shortcut::Resize.hint());
    }

    fn register_resize_hints(&self, footer: &mut FooterRegistry, terminal_showing: bool) {
        footer.register(
            FooterHint::new("Esc/R/q", "exit resize")
                .primary()
                .on_key(KeyCode::Esc),
        );
        footer.register(FooterHint::new("←/h | →/l", "list width"));
        if self.focus().is_machine_list() || self.focus().is_image_list() {
            footer.register(FooterHint::new("↓/j | ↑/k", "machine/image split"));
        } else if terminal_showing {
            footer.register(FooterHint::new("↓/j | ↑/k", "detail taller/shorter"));
        }
        footer.register(WorkspaceShortcut::NextPanel.hint());
    }
}
