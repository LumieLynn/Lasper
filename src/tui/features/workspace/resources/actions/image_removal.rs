//! Image removal confirmation and dispatch over application lifecycle services.
use crate::application::{ImageLifecycleService, ImageRemovalOutcome};
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, ImageName};
use crate::tui::features::workspace::resources::actions::ResourceActionServices;
use crate::tui::foundation::components::confirmation::ConfirmationDialog;
use crate::tui::foundation::status::StatusLevel;
use crate::tui::protocol::events::AppEvent;
use crate::tui::protocol::{Component, EventResult};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{layout::Rect, Frame};
use tokio::sync::mpsc::Sender;

pub(crate) struct PendingImageRemoval {
    target: ImageName,
    dialog: ConfirmationDialog,
}

#[derive(Debug)]
pub(crate) struct ImageRemovalCompletion {
    pub target: ImageName,
    pub outcome: Result<ImageRemovalOutcome, String>,
}

impl ImageRemovalCompletion {
    pub(crate) fn status(&self) -> (String, StatusLevel) {
        match &self.outcome {
            Ok(outcome) => outcome_status(self.target.as_str(), outcome),
            Err(error) => (format!("Remove failed: {error}"), StatusLevel::Error),
        }
    }
}

impl PendingImageRemoval {
    pub(crate) fn new(target: ImageName, dialog: ConfirmationDialog) -> Self {
        Self { target, dialog }
    }

    pub(crate) fn target(&self) -> &ImageName {
        &self.target
    }

    pub(crate) fn cleanup_artifacts(&self) -> bool {
        self.dialog.checkbox_checked().unwrap_or(false)
    }
}

impl Component for PendingImageRemoval {
    fn set_focus(&mut self, focused: bool) {
        self.dialog.set_focus(focused);
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.dialog.render(frame, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        self.dialog.handle_key(key)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        self.dialog.handle_mouse(mouse)
    }
}

fn validate(image: &ImageEntry, machine_present: bool) -> Result<(), (String, StatusLevel)> {
    if ImageEntry::is_protected_name(&image.name) {
        return Err((
            "The .host image cannot be removed.".into(),
            StatusLevel::Warn,
        ));
    }
    if machine_present {
        return Err((
            format!("Stop machine '{}' before deleting its image.", image.name),
            StatusLevel::Warn,
        ));
    }
    Ok(())
}

pub(crate) fn prepare(
    image: &ImageEntry,
    machine_present: bool,
) -> Result<PendingImageRemoval, (String, StatusLevel)> {
    validate(image, machine_present)?;
    let target = ImageName::new(image.name.clone())
        .map_err(|error| (error.to_string(), StatusLevel::Error))?;
    let detail = if image.is_hidden() {
        "This hidden image will be removed through systemd's image management path."
    } else if image.readonly {
        "This read-only image and its local data will be removed."
    } else {
        "This image and its local data will be removed."
    };
    let mut dialog = ConfirmationDialog::new(
        "Delete Image",
        format!(
            "Delete '{}'?\n{}\nsystemd also attempts to remove all same-name .nspawn settings.",
            image.name, detail
        ),
    );
    if !image.is_hidden() && MachineName::new(&image.name).is_ok() {
        dialog = dialog.with_checkbox("Remove Lasper NVIDIA state and unit drop-ins", true);
    }
    Ok(PendingImageRemoval::new(target, dialog))
}

pub(crate) fn submit(
    pending: PendingImageRemoval,
    image: Option<&ImageEntry>,
    machine_present: bool,
    lifecycle: &ImageLifecycleService,
    services: &ResourceActionServices,
    tx: Option<Sender<AppEvent>>,
) -> Result<(), (String, StatusLevel)> {
    let target = pending.target().clone();
    let name = pending.target().as_str().to_owned();
    let image = image.filter(|image| image.name == name).ok_or_else(|| {
        (
            format!("Image {name} changed before confirmation; refresh and try again."),
            StatusLevel::Warn,
        )
    })?;
    validate(image, machine_present)?;
    let tx = tx.ok_or_else(|| {
        (
            "Internal error: application event channel is unavailable".into(),
            StatusLevel::Error,
        )
    })?;
    let removal = lifecycle
        .begin_remove(image, pending.cleanup_artifacts())
        .map_err(|error| (error.to_string(), StatusLevel::Warn))?;
    let pm = services.permissions.clone();
    let operation = services.host_operations.begin();
    tokio::spawn(async move {
        let _operation = operation;
        let audit = match pm.request_elevation(format!("Remove image {}", name)).await {
            Ok(audit) => audit,
            Err(error) => {
                let _ = tx
                    .send(AppEvent::ImageRemovalFinished(ImageRemovalCompletion {
                        target,
                        outcome: Err(error.to_string()),
                    }))
                    .await;
                return;
            }
        };
        let result = audit.run(async { removal.run().await }).await;
        let _ = tx
            .send(AppEvent::ImageRemovalFinished(ImageRemovalCompletion {
                target,
                outcome: Ok(result),
            }))
            .await;
    });
    Ok(())
}

fn outcome_status(name: &str, result: &ImageRemovalOutcome) -> (String, StatusLevel) {
    match result {
        crate::application::ImageRemovalOutcome::NotAttempted { reason, .. } => (
            format!("Remove was not attempted: {}", reason),
            crate::tui::foundation::status::StatusLevel::Warn,
        ),
        crate::application::ImageRemovalOutcome::Removed(report) => {
            let unit_warning = match &report.unit {
                crate::application::image_lifecycle::UnitDisableReport::Failed(reason) => {
                    Some(reason.clone())
                }
                _ => None,
            };
            match &report.artifacts {
                crate::application::image_lifecycle::ArtifactCleanupReport::Removed => {
                    match unit_warning {
                        Some(reason) => (
                            format!(
                                "Removed image {} and Lasper artifacts; unit disable warning: {}",
                                name, reason
                            ),
                            crate::tui::foundation::status::StatusLevel::Warn,
                        ),
                        None => (
                            format!("Removed image {} and Lasper artifacts", name),
                            crate::tui::foundation::status::StatusLevel::Success,
                        ),
                    }
                }
                crate::application::image_lifecycle::ArtifactCleanupReport::PreservedAmbiguous(
                    errors,
                )
                | crate::application::image_lifecycle::ArtifactCleanupReport::PartiallyRemoved(
                    errors,
                )
                | crate::application::image_lifecycle::ArtifactCleanupReport::Failed(errors) => (
                    format!(
                        "Removed image {}; cleanup warning: {}{}",
                        name,
                        errors.join("; "),
                        unit_warning
                            .as_deref()
                            .map(|reason| format!("; unit disable: {reason}"))
                            .unwrap_or_default()
                    ),
                    crate::tui::foundation::status::StatusLevel::Warn,
                ),
                _ => match unit_warning {
                    Some(reason) => (
                        format!("Removed image {}; unit disable warning: {}", name, reason),
                        crate::tui::foundation::status::StatusLevel::Warn,
                    ),
                    None => (
                        format!("Removed image {}", name),
                        crate::tui::foundation::status::StatusLevel::Success,
                    ),
                },
            }
        }
        crate::application::ImageRemovalOutcome::Rejected { reason, .. } => (
            format!("Remove rejected: {}", reason),
            crate::tui::foundation::status::StatusLevel::Warn,
        ),
        crate::application::ImageRemovalOutcome::Failed { reason, .. } => (
            format!("Remove failed: {}", reason),
            crate::tui::foundation::status::StatusLevel::Error,
        ),
        crate::application::ImageRemovalOutcome::OutcomeUnknown { reason, .. } => (
            format!("Removal outcome unknown: {}", reason),
            crate::tui::foundation::status::StatusLevel::Warn,
        ),
    }
}
