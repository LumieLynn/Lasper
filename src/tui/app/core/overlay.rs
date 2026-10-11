//! State and routing for transient application overlays.
//!
//! Workspace components stay on [`crate::tui::app::App`]. This module owns the
//! short-lived layers that cover the workspace and, importantly, owns their
//! stacking order. Rendering keeps the underlying layers visible; input and
//! shortcut hints belong only to the top layer. A component owns and composes
//! any dialogs nested inside it, such as the user editor's Wayland bindings.

use crate::tui::features::configuration::{ConfigurationAction, ConfigurationView};
use crate::tui::features::wizard::{StepAction as WizardAction, WizardSession};
use crate::tui::features::workspace::overlays::help::HelpOverlay;
use crate::tui::features::workspace::overlays::leader::LeaderOverlay;
use crate::tui::features::workspace::resources::actions::PendingImageRemoval;
use crate::tui::features::workspace::resources::menu::{ResourceAction, ResourceActionMenu};
use crate::tui::foundation::components::confirmation::ConfirmationDialog;
use crate::tui::foundation::footer::{FooterModel, FooterRegistry};
use crate::tui::foundation::input::UiInput;
use crate::tui::protocol::{AppMessage, Component, EventResult};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, Frame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalLayer {
    ResourceActionMenu,
    Leader,
    Wizard,
    Configuration,
    Help,
    QuitConfirmation,
    DeleteConfirmation,
    Dialog,
}

pub(crate) enum OverlayCommand {
    Consumed,
    Configuration(Box<ConfigurationAction>),
    Wizard(Box<WizardAction>),
    Resource(ResourceAction),
    LeaderShell,
    LeaderLogin,
    LeaderX11,
    LeaderConfigure,
    RemoveImage,
    Quit,
}

pub(crate) struct OverlayState {
    pub(crate) configuration: Option<ConfigurationView>,
    pub(crate) help: Option<HelpOverlay>,
    pub(crate) leader: Option<LeaderOverlay>,
    pub(crate) resource_action_menu: Option<ResourceActionMenu>,
    pub(crate) wizard: Option<WizardSession>,
    pub(crate) wizard_opening: Option<crate::tui::features::wizard::workflow::WizardOpening>,
    pub(crate) quit_dialog: Option<ConfirmationDialog>,
    pub(crate) delete_dialog: Option<PendingImageRemoval>,
    pub(crate) active_dialog: Option<Box<dyn Component>>,
    pub(crate) next_configuration_query: u64,
    pub(crate) next_wizard_instance: u64,
}

impl OverlayState {
    pub(crate) fn new() -> Self {
        Self {
            configuration: None,
            help: None,
            leader: None,
            resource_action_menu: None,
            wizard: None,
            wizard_opening: None,
            quit_dialog: None,
            delete_dialog: None,
            active_dialog: None,
            next_configuration_query: 1,
            next_wizard_instance: 1,
        }
    }

    pub(crate) fn next_configuration_query(&mut self) -> u64 {
        let query = self.next_configuration_query;
        self.next_configuration_query = query.checked_add(1).unwrap_or(1);
        query
    }

    /// Live layers in paint order. Taking a snapshot avoids borrowing the
    /// overlay state while its components update their render geometry.
    fn layers(&self) -> [Option<ModalLayer>; 8] {
        [
            self.leader.as_ref().map(|_| ModalLayer::Leader),
            self.resource_action_menu
                .as_ref()
                .map(|_| ModalLayer::ResourceActionMenu),
            self.wizard
                .as_ref()
                .map(|_| ModalLayer::Wizard)
                .or_else(|| self.wizard_opening.as_ref().map(|_| ModalLayer::Wizard)),
            self.configuration
                .as_ref()
                .map(|_| ModalLayer::Configuration),
            self.help.as_ref().map(|_| ModalLayer::Help),
            self.quit_dialog
                .as_ref()
                .map(|_| ModalLayer::QuitConfirmation),
            self.delete_dialog
                .as_ref()
                .map(|_| ModalLayer::DeleteConfirmation),
            self.active_dialog.as_ref().map(|_| ModalLayer::Dialog),
        ]
    }

    /// The input owner, rather than a list of everything that should render.
    pub(crate) fn modal_layer(&self) -> Option<ModalLayer> {
        self.layers().into_iter().flatten().next_back()
    }

    pub(crate) fn leader_active(&self) -> bool {
        self.leader.as_ref().is_some_and(Component::is_focused)
    }

    /// Each layer delegates hints to its current page or nested dialog.
    pub(crate) fn footer_model(&self) -> Option<FooterModel<'static>> {
        let layer = self.modal_layer()?;
        let mut footer = FooterRegistry::new();
        match layer {
            ModalLayer::Leader => {
                if let Some(leader) = &self.leader {
                    leader.register_footer_hints(&mut footer);
                }
            }
            ModalLayer::Help => self.help.as_ref()?.register_footer_hints(&mut footer),
            ModalLayer::Dialog => {
                if let Some(dialog) = &self.active_dialog {
                    dialog.register_footer_hints(&mut footer);
                }
            }
            ModalLayer::DeleteConfirmation | ModalLayer::QuitConfirmation => {}
            ModalLayer::ResourceActionMenu => {
                if let Some(menu) = &self.resource_action_menu {
                    menu.register_footer_hints(&mut footer);
                }
            }
            ModalLayer::Configuration => self
                .configuration
                .as_ref()?
                .register_footer_hints(&mut footer),
            ModalLayer::Wizard => {
                if let Some(wizard) = &self.wizard {
                    wizard.register_footer_hints(&mut footer);
                } else {
                    self.wizard_opening
                        .as_ref()?
                        .register_footer_hints(&mut footer);
                }
            }
        }
        Some(FooterModel::Hints(footer))
    }

    pub(crate) fn open_leader(&mut self) {
        self.leader = Some(LeaderOverlay::new());
    }

    pub(crate) fn open_help(&mut self) {
        self.help = Some(HelpOverlay::new());
    }

    pub(crate) fn close_leader(&mut self) {
        if let Some(mut leader) = self.leader.take() {
            leader.set_focus(false);
        }
    }

    /// Compose live layers from bottom to top. The content area is used for the
    /// leader because it must stay above panels but below the status bar;
    /// every other overlay gets the screen above the shared status bar.
    pub(crate) fn render(&mut self, frame: &mut Frame, content_area: Rect, screen_area: Rect) {
        let foreground = self.modal_layer();
        for layer in self.layers().into_iter().flatten() {
            match layer {
                ModalLayer::Leader => {
                    if let Some(leader) = &mut self.leader {
                        leader.render_layer(frame, content_area, foreground == Some(layer));
                    }
                }
                ModalLayer::ResourceActionMenu => {
                    if let Some(menu) = &mut self.resource_action_menu {
                        menu.render_layer(frame, screen_area, foreground == Some(layer));
                    }
                }
                ModalLayer::Wizard => {
                    if let Some(wizard) = &mut self.wizard {
                        wizard.render(frame, screen_area, foreground == Some(layer));
                    } else if let Some(opening) = &mut self.wizard_opening {
                        opening.render_layer(frame, screen_area, foreground == Some(layer));
                    }
                }
                ModalLayer::Configuration => {
                    if let Some(configuration) = &mut self.configuration {
                        configuration.render_layer(frame, screen_area, foreground == Some(layer));
                    }
                }
                ModalLayer::Help => {
                    if let Some(help) = &mut self.help {
                        help.render_layer(frame, screen_area, foreground == Some(layer));
                    }
                }
                ModalLayer::QuitConfirmation => {
                    if let Some(dialog) = &mut self.quit_dialog {
                        dialog.render_layer(frame, screen_area, foreground == Some(layer));
                    }
                }
                ModalLayer::DeleteConfirmation => {
                    if let Some(dialog) = &mut self.delete_dialog {
                        dialog.render_layer(frame, screen_area, foreground == Some(layer));
                    }
                }
                ModalLayer::Dialog => {
                    if let Some(dialog) = &mut self.active_dialog {
                        dialog.render_layer(frame, screen_area, foreground == Some(layer));
                    }
                }
            }
        }
    }

    /// Convert foreground input into a command for the application. Overlay
    /// widgets own focus and dismissal; App remains responsible for commands
    /// that touch services, terminals, or the runtime model.
    pub(crate) fn handle_input(&mut self, input: UiInput) -> Option<OverlayCommand> {
        match input {
            UiInput::Key(key) => self.handle_key(key),
            UiInput::Mouse(mouse) => self.handle_mouse(mouse),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<OverlayCommand> {
        match self.modal_layer()? {
            ModalLayer::Configuration => {
                let action = self.configuration.as_mut()?.handle_key(key);
                Some(OverlayCommand::Configuration(Box::new(action)))
            }
            ModalLayer::Dialog => self.handle_dialog_key(key),
            ModalLayer::DeleteConfirmation => self.handle_delete_key(key),
            ModalLayer::QuitConfirmation => self.handle_quit_key(key),
            ModalLayer::Wizard => {
                let action = if let Some(wizard) = self.wizard.as_mut() {
                    wizard.handle_key(key)
                } else if matches!(
                    self.wizard_opening.as_mut()?.handle_key(key),
                    EventResult::Ignored
                ) {
                    WizardAction::Close
                } else {
                    WizardAction::None
                };
                Some(OverlayCommand::Wizard(Box::new(action)))
            }
            ModalLayer::Help => {
                if matches!(self.help.as_mut()?.handle_key(key), EventResult::Ignored) {
                    self.help = None;
                }
                Some(OverlayCommand::Consumed)
            }
            ModalLayer::ResourceActionMenu => self.handle_resource_key(key),
            ModalLayer::Leader => self.handle_leader_key(key),
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<OverlayCommand> {
        match self.modal_layer()? {
            ModalLayer::Configuration => {
                let action = self.configuration.as_mut()?.handle_mouse(mouse);
                Some(OverlayCommand::Configuration(Box::new(action)))
            }
            ModalLayer::Dialog => self.handle_dialog_mouse(mouse),
            ModalLayer::Wizard => {
                if let Some(wizard) = self.wizard.as_mut() {
                    let result = wizard.handle_mouse(mouse);
                    Some(self.wizard_result(result))
                } else {
                    Some(OverlayCommand::Consumed)
                }
            }
            ModalLayer::Leader => {
                if matches!(mouse.kind, MouseEventKind::Down(_)) {
                    self.close_leader();
                }
                Some(OverlayCommand::Consumed)
            }
            ModalLayer::Help => {
                let _ = self.help.as_mut()?.handle_mouse(mouse);
                Some(OverlayCommand::Consumed)
            }
            ModalLayer::DeleteConfirmation => {
                let _ = self.delete_dialog.as_mut()?.handle_mouse(mouse);
                Some(OverlayCommand::Consumed)
            }
            ModalLayer::ResourceActionMenu => {
                let _ = self.resource_action_menu.as_mut()?.handle_mouse(mouse);
                Some(OverlayCommand::Consumed)
            }
            ModalLayer::QuitConfirmation => Some(OverlayCommand::Consumed),
        }
    }

    fn handle_dialog_key(&mut self, key: KeyEvent) -> Option<OverlayCommand> {
        let mut dialog = self.active_dialog.take()?;
        match dialog.handle_key(key) {
            EventResult::Message(message) => Some(self.message_command(message)),
            EventResult::Ignored if key.code == KeyCode::Esc => Some(OverlayCommand::Consumed),
            _ => {
                self.active_dialog = Some(dialog);
                Some(OverlayCommand::Consumed)
            }
        }
    }

    fn handle_dialog_mouse(&mut self, mouse: MouseEvent) -> Option<OverlayCommand> {
        let result = self.active_dialog.as_mut()?.handle_mouse(mouse);
        Some(match result {
            EventResult::Message(message) => self.message_command(message),
            _ => OverlayCommand::Consumed,
        })
    }

    fn message_command(&mut self, message: AppMessage) -> OverlayCommand {
        match message {
            AppMessage::Wizard(message) => self
                .wizard
                .as_mut()
                .map(|wizard| {
                    OverlayCommand::Wizard(Box::new(
                        wizard.process_message(AppMessage::Wizard(message)),
                    ))
                })
                .unwrap_or(OverlayCommand::Consumed),
            _ => OverlayCommand::Consumed,
        }
    }

    fn wizard_result(&mut self, result: EventResult) -> OverlayCommand {
        match result {
            EventResult::Message(message) => self.message_command(message),
            _ => OverlayCommand::Consumed,
        }
    }

    fn handle_delete_key(&mut self, key: KeyEvent) -> Option<OverlayCommand> {
        let dialog = self.delete_dialog.as_mut()?;
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Some(OverlayCommand::RemoveImage),
            KeyCode::Char('n') | KeyCode::Esc => {
                self.delete_dialog = None;
                Some(OverlayCommand::Consumed)
            }
            _ => {
                let _ = dialog.handle_key(key);
                Some(OverlayCommand::Consumed)
            }
        }
    }

    fn handle_quit_key(&mut self, key: KeyEvent) -> Option<OverlayCommand> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.quit_dialog = None;
                Some(OverlayCommand::Quit)
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.quit_dialog = None;
                Some(OverlayCommand::Consumed)
            }
            _ => Some(OverlayCommand::Consumed),
        }
    }

    fn handle_resource_key(&mut self, key: KeyEvent) -> Option<OverlayCommand> {
        let menu = self.resource_action_menu.as_mut()?;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.resource_action_menu = None;
                Some(OverlayCommand::Consumed)
            }
            KeyCode::Enter => {
                let action = menu.selected_action();
                self.resource_action_menu = None;
                Some(action.map_or(OverlayCommand::Consumed, OverlayCommand::Resource))
            }
            _ => {
                let _ = menu.handle_key(key);
                Some(OverlayCommand::Consumed)
            }
        }
    }

    fn handle_leader_key(&mut self, key: KeyEvent) -> Option<OverlayCommand> {
        let command = match (key.code, key.modifiers) {
            (KeyCode::Char('t'), modifiers) if modifiers.is_empty() => OverlayCommand::LeaderShell,
            (KeyCode::Char('l'), modifiers) if modifiers.is_empty() => OverlayCommand::LeaderLogin,
            (KeyCode::Char('x'), modifiers) if modifiers.is_empty() => OverlayCommand::LeaderX11,
            (KeyCode::Char('e'), modifiers) if modifiers.is_empty() => {
                OverlayCommand::LeaderConfigure
            }
            _ => OverlayCommand::Consumed,
        };
        self.close_leader();
        Some(command)
    }
}
