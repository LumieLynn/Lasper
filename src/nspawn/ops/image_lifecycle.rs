//! Application-owned image lifecycle workflow.

use super::registry::{
    OperationRegistry, ResourceClaim, ResourceConflict, ResourceKey, ResourceReservation,
};
use crate::nspawn::models::{ContainerEntry, ImageEntry, ImageName, MachineName};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageRemovalRejection {
    Protected,
    InvalidTarget,
    Busy,
    NotFound,
    AlreadyRunning,
    PermissionDenied,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ImageControlOutcome {
    Removed,
    NotAttempted {
        reason: String,
    },
    Rejected {
        rejection: ImageRemovalRejection,
        reason: String,
    },
    Failed {
        reason: String,
    },
    OutcomeUnknown {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImageRemoveTransport {
    Dbus,
    Cli,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageRemoveRequest {
    pub image: ImageName,
    pub transport: ImageRemoveTransport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitDisableReport {
    Disabled,
    /// Kept for the later machine-unit capability that can observe a prior
    /// disabled state without issuing a second mutation.
    #[allow(dead_code)]
    AlreadyDisabled,
    Failed(String),
    NotRun,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactOwnership {
    ProvenOwned,
    AmbiguousLegacy,
    NotPresent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactCleanupReport {
    NotRequested,
    NotRunBecausePrimaryNotRemoved,
    NotApplicable,
    Removed,
    PreservedAmbiguous(Vec<String>),
    PartiallyRemoved(Vec<String>),
    Failed(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRemovalReport {
    pub unit: UnitDisableReport,
    pub artifacts: ArtifactCleanupReport,
    pub runtime_refresh_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageRemovalOutcome {
    Removed(ImageRemovalReport),
    NotAttempted {
        reason: String,
        report: ImageRemovalReport,
    },
    Rejected {
        rejection: ImageRemovalRejection,
        reason: String,
        report: ImageRemovalReport,
    },
    Failed {
        reason: String,
        report: ImageRemovalReport,
    },
    OutcomeUnknown {
        reason: String,
        report: ImageRemovalReport,
    },
}

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait ImageRuntime: Send + Sync + 'static {
    async fn list_machines(&self) -> Result<Vec<ContainerEntry>, String>;
}

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait ImageControl: Send + Sync + 'static {
    async fn disable_unit(&self, machine: &MachineName) -> UnitDisableReport;
    async fn remove_image(&self, image: &ImageName) -> ImageControlOutcome;
}

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait ManagedArtifactCleanup: Send + Sync + 'static {
    async fn cleanup(&self, machine: &MachineName) -> ArtifactCleanupReport;
}

pub struct ImageRemovalOperation {
    runtime: Arc<dyn ImageRuntime>,
    control: Arc<dyn ImageControl>,
    cleanup: Arc<dyn ManagedArtifactCleanup>,
    name: ImageName,
    cleanup_artifacts: bool,
    _reservation: ResourceReservation,
}

impl std::fmt::Debug for ImageRemovalOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageRemovalOperation")
            .field("name", &self.name)
            .field("cleanup_artifacts", &self.cleanup_artifacts)
            .finish_non_exhaustive()
    }
}

impl ImageRemovalOperation {
    pub async fn run(self) -> ImageRemovalOutcome {
        let machine = (!ImageEntry::is_hidden_name(self.name.as_str()))
            .then(|| MachineName::new(self.name.as_str()).ok())
            .flatten();
        let mut report = ImageRemovalReport {
            unit: if machine.is_some() {
                UnitDisableReport::NotRun
            } else {
                UnitDisableReport::NotApplicable
            },
            artifacts: if self.cleanup_artifacts {
                ArtifactCleanupReport::NotRunBecausePrimaryNotRemoved
            } else {
                ArtifactCleanupReport::NotRequested
            },
            runtime_refresh_required: false,
        };

        let machines = match self.runtime.list_machines().await {
            Ok(machines) => machines,
            Err(reason) => return ImageRemovalOutcome::Failed { reason, report },
        };
        if machines
            .iter()
            .any(|entry| entry.name == self.name.as_str() && entry.state.is_running())
        {
            return ImageRemovalOutcome::Rejected {
                rejection: ImageRemovalRejection::AlreadyRunning,
                reason: format!(
                    "stop machine '{}' before deleting its image",
                    self.name.as_str()
                ),
                report,
            };
        }

        if let Some(machine) = &machine {
            report.unit = self.control.disable_unit(machine).await;
        }

        match self.control.remove_image(&self.name).await {
            ImageControlOutcome::Removed => {
                report.runtime_refresh_required = true;
                if self.cleanup_artifacts {
                    report.artifacts = match machine {
                        Some(machine) => self.cleanup.cleanup(&machine).await,
                        None => ArtifactCleanupReport::NotApplicable,
                    };
                }
                ImageRemovalOutcome::Removed(report)
            }
            ImageControlOutcome::NotAttempted { reason } => {
                ImageRemovalOutcome::NotAttempted { reason, report }
            }
            ImageControlOutcome::Rejected { rejection, reason } => ImageRemovalOutcome::Rejected {
                rejection,
                reason,
                report,
            },
            ImageControlOutcome::Failed { reason } => {
                ImageRemovalOutcome::Failed { reason, report }
            }
            ImageControlOutcome::OutcomeUnknown { reason } => {
                report.runtime_refresh_required = true;
                ImageRemovalOutcome::OutcomeUnknown { reason, report }
            }
        }
    }
}

#[derive(Clone)]
pub struct ImageLifecycleService {
    runtime: Arc<dyn ImageRuntime>,
    control: Arc<dyn ImageControl>,
    cleanup: Arc<dyn ManagedArtifactCleanup>,
    registry: Arc<OperationRegistry>,
}

impl ImageLifecycleService {
    pub fn new(
        runtime: Arc<dyn ImageRuntime>,
        control: Arc<dyn ImageControl>,
        cleanup: Arc<dyn ManagedArtifactCleanup>,
        registry: Arc<OperationRegistry>,
    ) -> Self {
        Self {
            runtime,
            control,
            cleanup,
            registry,
        }
    }

    pub fn active_images(&self) -> HashSet<String> {
        self.registry.active_image_names()
    }

    pub fn begin_remove(
        &self,
        image: &ImageEntry,
        cleanup_artifacts: bool,
    ) -> Result<ImageRemovalOperation, ImageRemovalRejection> {
        if ImageEntry::is_protected_name(&image.name) {
            return Err(ImageRemovalRejection::Protected);
        }
        let name =
            ImageName::new(image.name.clone()).map_err(|_| ImageRemovalRejection::InvalidTarget)?;
        let reservation = self
            .registry
            .reserve_image_removal(
                name.as_str(),
                [ResourceClaim::exclusive(ResourceKey::for_image(&name))],
            )
            .map_err(|ResourceConflict { .. }| ImageRemovalRejection::Busy)?;
        Ok(ImageRemovalOperation {
            runtime: self.runtime.clone(),
            control: self.control.clone(),
            cleanup: self.cleanup.clone(),
            name,
            cleanup_artifacts,
            _reservation: reservation,
        })
    }
}

pub(crate) fn map_native_error(error: crate::nspawn::errors::NspawnError) -> ImageControlOutcome {
    use crate::nspawn::errors::NspawnError;
    let reason = error.to_string();
    match error {
        NspawnError::Validation(_) | NspawnError::ContainerAlreadyRunning(_) => {
            ImageControlOutcome::Rejected {
                rejection: if reason.contains("host image") {
                    ImageRemovalRejection::Protected
                } else if reason.to_ascii_lowercase().contains("busy") {
                    ImageRemovalRejection::Busy
                } else if matches!(error, NspawnError::ContainerAlreadyRunning(_)) {
                    ImageRemovalRejection::AlreadyRunning
                } else {
                    ImageRemovalRejection::InvalidTarget
                },
                reason,
            }
        }
        NspawnError::PermissionDenied => ImageControlOutcome::Rejected {
            rejection: ImageRemovalRejection::PermissionDenied,
            reason,
        },
        NspawnError::ContainerNotFound(_) => ImageControlOutcome::Rejected {
            rejection: ImageRemovalRejection::NotFound,
            reason,
        },
        NspawnError::Dbus(zbus::Error::MethodError(name, detail, _)) => {
            let rejection = match name.as_str() {
                "org.freedesktop.machine1.NoSuchImage" | "System.Error.ENOENT" => {
                    Some(ImageRemovalRejection::NotFound)
                }
                "System.Error.EBUSY" => Some(ImageRemovalRejection::Busy),
                "org.freedesktop.DBus.Error.AccessDenied"
                | "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired"
                | "org.freedesktop.PolicyKit1.Error.NotAuthorized"
                | "org.freedesktop.PolicyKit1.Error.AuthorizationFailed"
                | "org.freedesktop.PolicyKit1.Error.Failed"
                | "System.Error.EACCES"
                | "System.Error.EPERM" => Some(ImageRemovalRejection::PermissionDenied),
                "org.freedesktop.DBus.Error.InvalidArgs" | "System.Error.EINVAL" => {
                    Some(ImageRemovalRejection::InvalidTarget)
                }
                _ => None,
            };
            match rejection {
                Some(rejection) => ImageControlOutcome::Rejected {
                    rejection,
                    reason: detail.unwrap_or(reason),
                },
                None => ImageControlOutcome::Failed { reason },
            }
        }
        NspawnError::Io(_, _) | NspawnError::GenericIo(_) | NspawnError::Dbus(_) => {
            ImageControlOutcome::OutcomeUnknown { reason }
        }
        _ => ImageControlOutcome::Failed { reason },
    }
}

impl std::fmt::Display for ImageRemovalRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protected => f.write_str("the .host image cannot be removed"),
            Self::InvalidTarget => f.write_str("the selected image is not a valid target"),
            Self::Busy => f.write_str("the image is already being modified"),
            Self::NotFound => f.write_str("the image no longer exists"),
            Self::AlreadyRunning => f.write_str("the image has a running machine"),
            Self::PermissionDenied => f.write_str("permission to remove the image was denied"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::{ContainerState, ImageEntry};

    fn image(name: &str) -> ImageEntry {
        ImageEntry {
            name: name.into(),
            image_type: "directory".into(),
            readonly: false,
            usage: None,
            dbus_object_path: None,
        }
    }

    fn service(
        runtime: MockImageRuntime,
        control: MockImageControl,
        cleanup: MockManagedArtifactCleanup,
    ) -> ImageLifecycleService {
        ImageLifecycleService::new(
            Arc::new(runtime),
            Arc::new(control),
            Arc::new(cleanup),
            OperationRegistry::new(),
        )
    }

    #[tokio::test]
    async fn running_machine_is_rejected_before_host_mutations() {
        let mut runtime = MockImageRuntime::new();
        runtime.expect_list_machines().returning(|| {
            Ok(vec![ContainerEntry {
                name: "ubuntu".into(),
                state: ContainerState::Running,
                address: None,
                all_addresses: vec![],
            }])
        });
        let mut control = MockImageControl::new();
        control.expect_disable_unit().never();
        control.expect_remove_image().never();
        let mut cleanup = MockManagedArtifactCleanup::new();
        cleanup.expect_cleanup().never();
        let operation = service(runtime, control, cleanup)
            .begin_remove(&image("ubuntu"), true)
            .unwrap();

        assert!(matches!(
            operation.run().await,
            ImageRemovalOutcome::Rejected {
                rejection: ImageRemovalRejection::AlreadyRunning,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn cleanup_failure_does_not_erase_primary_success() {
        let mut runtime = MockImageRuntime::new();
        runtime.expect_list_machines().returning(|| Ok(vec![]));
        let mut control = MockImageControl::new();
        control
            .expect_disable_unit()
            .returning(|_| UnitDisableReport::Disabled);
        control
            .expect_remove_image()
            .returning(|_| ImageControlOutcome::Removed);
        let mut cleanup = MockManagedArtifactCleanup::new();
        cleanup
            .expect_cleanup()
            .returning(|_| ArtifactCleanupReport::Failed(vec!["state file".into()]));
        let operation = service(runtime, control, cleanup)
            .begin_remove(&image("ubuntu"), true)
            .unwrap();

        assert!(matches!(
            operation.run().await,
            ImageRemovalOutcome::Removed(ImageRemovalReport {
                artifacts: ArtifactCleanupReport::Failed(_),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn failed_removal_is_not_replayed_by_the_service() {
        let mut runtime = MockImageRuntime::new();
        runtime.expect_list_machines().returning(|| Ok(vec![]));
        let mut control = MockImageControl::new();
        control
            .expect_disable_unit()
            .returning(|_| UnitDisableReport::AlreadyDisabled);
        control
            .expect_remove_image()
            .once()
            .returning(|_| ImageControlOutcome::Failed {
                reason: "machined failed".into(),
            });
        let mut cleanup = MockManagedArtifactCleanup::new();
        cleanup.expect_cleanup().never();
        let operation = service(runtime, control, cleanup)
            .begin_remove(&image("ubuntu"), true)
            .unwrap();

        assert!(matches!(
            operation.run().await,
            ImageRemovalOutcome::Failed { .. }
        ));
    }
}
