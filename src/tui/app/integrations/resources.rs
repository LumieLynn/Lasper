//! Resource action integration: resolve UI targets, then submit narrow workflows.
use crate::application::machine_lifecycle::MachineOperation;
use crate::application::{MachineRuntimeAction, NspawnUnitAction};
use crate::domain::machine::AllowedSignal;
use crate::tui::app::App;
use crate::tui::features::workspace::resources::actions::{
    image_removal, machine, machine_outcome_status, ImageRemovalCompletion, ResourceActionServices,
};
use crate::tui::foundation::status::StatusLevel;

impl App {
    pub(in crate::tui::app) fn apply_machine_projection(&mut self) {
        let lifecycle = &self.services.machine_lifecycle;
        self.workspace
            .project_machines(|observed| lifecycle.project_machines(observed));
        let machine = self
            .terminal
            .active_session()
            .map(|session| session.machine_name.as_str());
        self.workspace.request_inspection(machine);
    }

    pub fn action_start(&mut self) {
        if let Some(name) = self
            .focused_image_resource()
            .map(|image| image.name.clone())
        {
            self.action_start_image_named(&name);
            return;
        }
        self.set_status(
            "Focus Images to start an image.".into(),
            crate::tui::foundation::status::StatusLevel::Info,
        );
    }

    pub fn action_poweroff(&mut self) {
        if let Some(name) = self
            .focused_machine_resource()
            .map(|entry| entry.name.clone())
        {
            self.action_runtime_named(&name, MachineRuntimeAction::Poweroff);
            return;
        }
        self.set_status(
            "Focus Machines to power off a running machine.".into(),
            crate::tui::foundation::status::StatusLevel::Info,
        );
    }

    pub(crate) fn action_runtime_named(&mut self, name: &str, action: MachineRuntimeAction) {
        let Some(entry) = self.workspace.resources.machine(name).cloned() else {
            self.set_status(
                format!("Machine '{}' changed before the action started.", name),
                crate::tui::foundation::status::StatusLevel::Warn,
            );
            return;
        };
        let name = entry.name.clone();
        match self
            .services
            .machine_lifecycle
            .begin_runtime(&entry, action)
        {
            Ok(operation) => {
                self.submit_machine_operation(operation);
            }
            Err(rejection) => self.set_status(
                format!("{}: {}", name, rejection),
                crate::tui::foundation::status::StatusLevel::Warn,
            ),
        }
    }

    pub fn action_remove(&mut self) {
        if self.ui.overlays.delete_dialog.is_some() {
            self.action_remove_image();
            return;
        }
        self.set_status(
            "Focus Images to remove a machine image.".into(),
            crate::tui::foundation::status::StatusLevel::Info,
        );
    }

    pub(crate) fn action_start_image_named(&mut self, name: &str) {
        let image = match self.workspace.resources.image(name).cloned() {
            Some(image) => image,
            None => {
                self.set_status(
                    format!("Image '{}' changed before the action started.", name),
                    crate::tui::foundation::status::StatusLevel::Warn,
                );
                return;
            }
        };
        if image.is_hidden() {
            self.set_status(
                "Internal images cannot be started directly.".into(),
                crate::tui::foundation::status::StatusLevel::Warn,
            );
            return;
        }
        let name = image.name.clone();
        let is_mstack = image.image_type == "mstack";
        let observed_state = self
            .workspace
            .resources
            .machine(&name)
            .map(|entry| entry.state.clone());
        if let Some(state) = &observed_state {
            self.set_status(
                format!("{} is already {}.", name, state.label()),
                crate::tui::foundation::status::StatusLevel::Info,
            );
            return;
        }
        let operation = match self
            .services
            .machine_lifecycle
            .begin_launch(&image, observed_state)
        {
            Ok(operation) => operation,
            Err(rejection) => {
                self.set_status(
                    format!("{}: {}", name, rejection),
                    crate::tui::foundation::status::StatusLevel::Warn,
                );
                return;
            }
        };
        if self.submit_machine_operation(operation) && is_mstack {
            self.set_status(
                format!(
                    "{} is an OCI application and may not be bootable; systemd will still try to start it.",
                    name
                ),
                crate::tui::foundation::status::StatusLevel::Warn,
            );
        }
    }

    pub(crate) fn action_unit_named(&mut self, name: &str, action: NspawnUnitAction) {
        let Some(image) = self
            .workspace
            .resources
            .image(name)
            .filter(|image| !image.is_hidden())
            .cloned()
        else {
            self.set_status(
                format!("Image '{}' changed before the action started.", name),
                crate::tui::foundation::status::StatusLevel::Warn,
            );
            return;
        };
        let name = image.name.clone();
        match self.services.machine_lifecycle.begin_unit(&image, action) {
            Ok(operation) => {
                self.submit_machine_operation(operation);
            }
            Err(rejection) => self.set_status(
                format!("{}: {}", name, rejection),
                crate::tui::foundation::status::StatusLevel::Warn,
            ),
        }
    }
    pub(in crate::tui::app) fn handle_image_removal_completion(
        &mut self,
        completion: ImageRemovalCompletion,
    ) {
        let (message, level) = completion.status();
        self.set_status(message, level);
        self.refresh();
    }

    pub(in crate::tui::app) fn resource_action_services(&self) -> ResourceActionServices {
        ResourceActionServices {
            permissions: self.permissions.clone(),
            host_operations: self.services.host_operations.clone(),
        }
    }

    pub(in crate::tui::app) fn show_delete_dialog(&mut self) {
        let Some(name) = self
            .focused_image_resource()
            .map(|image| image.name.clone())
        else {
            self.set_status(
                "Focus Images to remove a machine image.".into(),
                StatusLevel::Info,
            );
            return;
        };
        self.show_delete_dialog_for(&name);
    }

    pub(in crate::tui::app) fn show_delete_dialog_for(&mut self, name: &str) {
        let Some(image) = self.workspace.resources.image(name) else {
            self.set_status(
                format!("Image '{name}' changed before the action started."),
                StatusLevel::Warn,
            );
            return;
        };
        let running = self.workspace.resources.machine(name).is_some();
        match image_removal::prepare(image, running) {
            Ok(pending) => self.ui.overlays.delete_dialog = Some(pending),
            Err((message, level)) => self.set_status(message, level),
        }
    }

    pub(in crate::tui::app) fn action_remove_image(&mut self) {
        let Some(pending) = self.ui.overlays.delete_dialog.take() else {
            return;
        };
        let name = pending.target().as_str();
        let image = self.workspace.resources.image(name);
        let running = self.workspace.resources.machine(name).is_some();
        let result = image_removal::submit(
            pending,
            image,
            running,
            &self.services.image_lifecycle,
            &self.resource_action_services(),
            self.ui.app_tx.clone(),
        );
        if let Err((message, level)) = result {
            self.set_status(message, level);
        }
    }

    pub(in crate::tui::app) fn submit_machine_operation(
        &mut self,
        operation: MachineOperation,
    ) -> bool {
        let Some(tx) = self.ui.app_tx.clone() else {
            return false;
        };
        self.apply_machine_projection();
        machine::submit(operation, &self.resource_action_services(), tx);
        true
    }

    pub(in crate::tui::app) fn handle_machine_operation_result(
        &mut self,
        outcome: crate::application::MachineLifecycleOutcome,
    ) {
        machine::reconcile_x11(outcome.action, self.services.x11_access.clone());
        let (message, level) = machine_outcome_status(outcome);
        self.apply_machine_projection();
        self.refresh();
        self.set_status(message, level);
    }

    pub(in crate::tui::app) fn handle_resource_command(
        &mut self,
        action: crate::tui::features::workspace::resources::menu::ResourceAction,
    ) {
        use crate::application::{MachineRuntimeAction, NspawnUnitAction};
        use crate::tui::features::workspace::resources::menu::ResourceAction;

        match action {
            ResourceAction::Configure { target } => self.open_configuration(target),
            ResourceAction::StartImage { image } => self.action_start_image_named(&image),
            ResourceAction::EnableNspawnUnit { image } => {
                self.action_unit_named(&image, NspawnUnitAction::Enable)
            }
            ResourceAction::DisableNspawnUnit { image } => {
                self.action_unit_named(&image, NspawnUnitAction::Disable)
            }
            ResourceAction::DeleteImage { image } => self.show_delete_dialog_for(&image),
            ResourceAction::PoweroffMachine { machine } => {
                self.action_runtime_named(&machine, MachineRuntimeAction::Poweroff)
            }
            ResourceAction::RebootMachine { machine } => {
                self.action_runtime_named(&machine, MachineRuntimeAction::Reboot)
            }
            ResourceAction::TerminateMachine { machine } => {
                self.action_runtime_named(&machine, MachineRuntimeAction::Terminate)
            }
            ResourceAction::KillMachine { machine } => self.action_runtime_named(
                &machine,
                MachineRuntimeAction::Kill {
                    signal: AllowedSignal::Kill,
                },
            ),
        }
    }
    pub(in crate::tui::app) fn open_resource_actions(&mut self) {
        let menu = if let Some(image) = self.focused_image_resource().cloned() {
            let machine_present = self.workspace.resources.machine(&image.name).is_some();
            let removing = self
                .services
                .image_lifecycle
                .active_images()
                .contains(&image.name);
            Some(
                crate::tui::features::workspace::resources::menu::ResourceActionMenu::for_image(
                    &image,
                    machine_present,
                    removing,
                ),
            )
        } else {
            self.focused_machine_resource().map(
                crate::tui::features::workspace::resources::menu::ResourceActionMenu::for_machine,
            )
        };
        if let Some(menu) = menu {
            self.ui.overlays.resource_action_menu = Some(menu);
        }
    }
}
