use crate::nspawn::models::{BindMount, IdmapSuffix};
use crate::nspawn::platform::nvidia::profile::NvidiaPassthroughMode;
use crate::ui::core::{AppMessage, Component, EventResult, FocusTracker, WizardMessage};
use crate::ui::widgets::lists::editable_list::EditableList;
use crate::ui::widgets::lists::selectable_list::SelectableList;
use crate::ui::widgets::selectors::checkbox::Checkbox;
use crate::ui::wizard::context::{PassthroughConfig, UnclassifiedFile, WizardContext};
use crate::ui::wizard::steps::StepComponent;
use crate::ui::wizard::StepAction;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    widgets::Paragraph,
    Frame,
};

macro_rules! active_comps {
    ($self:ident) => {{
        // Read fields before any mutable borrows to avoid borrow conflicts
        let nvidia_enabled = $self.nvidia_enabled;
        let is_categorized = matches!(
            $self.nvidia_mode,
            $crate::nspawn::platform::nvidia::profile::NvidiaPassthroughMode::Categorized
        );
        let uc_empty = $self.unclassified_list.items().is_empty();
        let has_uc = nvidia_enabled && is_categorized && !uc_empty;

        let mut comps: Vec<&mut dyn Component> = vec![&mut $self.nvidia_toggle];
        if has_uc {
            comps.push(&mut $self.unclassified_list);
        }
        comps.push(&mut $self.bind_list);
        comps
    }};
}

impl_wizard_nav!(DevicesStepView, active_comps);

pub struct DevicesStepView {
    bind_list: EditableList<BindMount>,
    unclassified_list: SelectableList<UnclassifiedFile>,
    nvidia_toggle: Checkbox,
    nvidia_enabled: bool,
    nvidia_mode: NvidiaPassthroughMode,
    nvidia_toolkit_installed: bool,
    focus: FocusTracker,
}

impl DevicesStepView {
    pub fn new(
        initial_data: &PassthroughConfig,
        unclassified_files: &[UnclassifiedFile],
        nvidia_toolkit_installed: bool,
    ) -> Self {
        let nvidia_enabled = initial_data.nvidia_gpu;
        let nvidia_mode = initial_data
            .nvidia_profile
            .as_ref()
            .map(|p| p.mode.clone())
            .unwrap_or(NvidiaPassthroughMode::Mirror);
        let bind_list = EditableList::new(
            " Configured Bind Mounts ",
            initial_data.bind_mounts.clone(),
            |bm| {
                let suffix_display = if bm.suffix == IdmapSuffix::None {
                    String::new()
                } else {
                    format!(" [{}]", bm.suffix.label())
                };
                format!(
                    "  {}:{} ({}){}",
                    bm.source,
                    bm.target,
                    if bm.readonly { "ro" } else { "rw" },
                    suffix_display
                )
            },
            |idx| AppMessage::Wizard(WizardMessage::BindMountRemoved(idx)),
        );

        let unclassified_list = SelectableList::new(
            " Unclassified CDI Files (E to reclassify) ",
            unclassified_files.to_vec(),
            |uf| {
                let cat_label = uf
                    .assigned_category
                    .as_ref()
                    .map(|c| c.label())
                    .unwrap_or("Unclassified");
                let dest = if uf.custom_destination.is_empty() {
                    &uf.default_container_path
                } else {
                    &uf.custom_destination
                };
                let mode = if uf.readonly { "ro" } else { "rw" };
                format!("  {} -> {} ({}) [{}]", uf.host_path, dest, cat_label, mode)
            },
        );

        let has_uc = nvidia_enabled
            && nvidia_mode == NvidiaPassthroughMode::Categorized
            && !unclassified_files.is_empty();

        let mut view = Self {
            bind_list,
            unclassified_list,
            nvidia_toggle: Checkbox::new(" NVIDIA GPU Passthrough", nvidia_enabled)
                .with_enabled(nvidia_toolkit_installed),
            nvidia_enabled,
            nvidia_mode,
            nvidia_toolkit_installed,
            focus: FocusTracker::new(),
        };

        if has_uc {
            view.focus.active_idx = 1; // focus unclassified list
        }
        view.update_focus();
        view
    }

    fn has_unclassified(&self) -> bool {
        self.nvidia_enabled
            && self.nvidia_mode == NvidiaPassthroughMode::Categorized
            && !self.unclassified_list.items().is_empty()
    }
}

impl Component for DevicesStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let has_uc = self.has_unclassified();

        let mut constraints = vec![Constraint::Length(3)]; // NVIDIA toggle
        if has_uc {
            constraints.push(Constraint::Min(5)); // Unclassified list
        }
        constraints.push(Constraint::Min(0)); // Bind list
        constraints.push(Constraint::Length(1)); // Footer

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(area);

        self.update_focus();
        self.nvidia_toggle.render(f, chunks[0]);

        let mut next = 1;
        if has_uc {
            self.unclassified_list.render(f, chunks[next]);
            next += 1;
        }
        self.bind_list.render(f, chunks[next]);

        let footer = " [Tab] switch focus, [Space] toggle NVIDIA, [A]dd/[E]dit/[D]elete, [Enter] next ";
        f.render_widget(
            Paragraph::new(footer).style(Style::default().fg(Color::Yellow)),
            chunks[next + 1],
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        // Custom keys for focused components (before macro delegation)
        if self.bind_list.is_focused() {
            match key.code {
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    return EventResult::Message(AppMessage::Wizard(
                        WizardMessage::OpenBindDialog,
                    ));
                }
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    if let Some(bm) = self.bind_list.selected_item() {
                        let idx = self.bind_list.selected();
                        return EventResult::Message(AppMessage::Wizard(
                            WizardMessage::OpenBindEditDialog(idx, bm.clone()),
                        ));
                    }
                }
                _ => {}
            }
        }

        if self.unclassified_list.is_focused() {
            match key.code {
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    if let Some(idx) = self.unclassified_list.selected_idx() {
                        if let Some(uf) = self.unclassified_list.selected_item().cloned() {
                            return EventResult::Message(AppMessage::Wizard(
                                WizardMessage::OpenUnclassifiedEditDialog(idx, uf),
                            ));
                        }
                    }
                }
                _ => {}
            }
        }

        // NVIDIA toggle: intercept Space to control dialog flow
        if self.nvidia_toggle.is_focused() && key.code == KeyCode::Char(' ') {
            let was_checked = self.nvidia_toggle.checked();
            let _ = self.nvidia_toggle.handle_key(key);
            let now_checked = self.nvidia_toggle.checked();
            if !was_checked && now_checked {
                return EventResult::Message(AppMessage::Wizard(
                    WizardMessage::OpenNvidiaConfigDialog,
                ));
            }
            if was_checked && !now_checked {
                self.nvidia_enabled = false;
                self.update_focus();
                return EventResult::Consumed;
            }
            return EventResult::Consumed;
        }

        delegate_wizard_navigation!(self, key, active_comps)
    }

    fn set_focus(&mut self, focused: bool) {
        wizard_set_focus!(self, focused, active_comps);
    }

    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl StepComponent for DevicesStepView {
    fn handle_message(&mut self, msg: &AppMessage) -> StepAction {
        match msg {
            AppMessage::Wizard(WizardMessage::BindMountAdded(bm)) => {
                self.bind_list.add_item(bm.clone());
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::BindMountUpdated(idx, bm)) => {
                self.bind_list.update_item(*idx, bm.clone());
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::NvidiaConfigSaved(result)) => {
                self.nvidia_enabled = true;
                self.nvidia_mode = result.mode.clone();
                self.nvidia_toggle = Checkbox::new(" NVIDIA GPU Passthrough", true)
                    .with_enabled(self.nvidia_toolkit_installed);
                if self.has_unclassified() {
                    self.focus.active_idx = 1;
                }
                self.update_focus();
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::UnclassifiedFileUpdated(idx, uf)) => {
                self.unclassified_list.update_item(*idx, uf.clone());
                self.update_focus();
                StepAction::CloseDialog
            }
            AppMessage::Wizard(WizardMessage::DialogCancel) => {
                if !self.nvidia_enabled {
                    self.nvidia_toggle = Checkbox::new(" NVIDIA GPU Passthrough", false)
                        .with_enabled(self.nvidia_toolkit_installed);
                }
                StepAction::CloseDialog
            }
            _ => StepAction::None,
        }
    }

    fn commit_to_context(&self, ctx: &mut WizardContext) {
        ctx.passthrough.bind_mounts = self.bind_list.items().to_vec();
        ctx.passthrough.unclassified_files = self.unclassified_list.items().to_vec();
        ctx.passthrough.nvidia_gpu = self.nvidia_enabled;
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}
