//! Runtime composition for the image lifecycle vertical slice.

use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::process::CommandRunner;
use crate::adapters::runtime::dbus::DbusBackend;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::system_operation::SystemOperationStore;
use crate::application::image_lifecycle::{
    ArtifactCleanupReport, ArtifactOwnership, ImageControl, ImageControlOutcome,
    ImageLifecycleService, ImageRemoveRequest, ImageRemoveTransport, ImageRuntime,
    ManagedArtifactCleanup, UnitDisableReport,
};
use crate::application::machine_lifecycle::{
    MachineAction, MachineControlOutcome, MachineControlRequest, MachineControlTransport,
};
use crate::application::{OperationRegistry, RuntimeCatalog};
use crate::nspawn::models::{ImageName, MachineEntry, MachineName};
use std::sync::Arc;

pub(crate) struct ImageLifecycleAdapters {
    pub(crate) local_cmd: Arc<dyn CommandRunner>,
    pub(crate) system_operations: SystemOperationStore,
    pub(crate) nspawn: crate::adapters::config::NspawnConfigStore,
    pub(crate) systemd_unit: crate::adapters::config::SystemdUnitStore,
    pub(crate) nvidia_state: crate::adapters::platform::nvidia::NvidiaStateStore,
}

pub(crate) enum ImageLifecycleRoute {
    DirectDbus,
    LocalCli,
    Elevated {
        daemon: Arc<ElevatedDaemon>,
        transport: ImageRemoveTransport,
    },
}

pub(crate) fn compose_image_lifecycle(
    runtime_catalog: Arc<RuntimeCatalog>,
    registry: Arc<OperationRegistry>,
    route: ImageLifecycleRoute,
    adapters: ImageLifecycleAdapters,
) -> ImageLifecycleService {
    let ImageLifecycleAdapters {
        local_cmd,
        system_operations,
        nspawn,
        systemd_unit,
        nvidia_state,
    } = adapters;
    let runtime: Arc<dyn ImageRuntime> = Arc::new(CatalogImageRuntime(runtime_catalog));
    let route = match route {
        ImageLifecycleRoute::Elevated { daemon, transport } => ImageControlRoute::Daemon {
            daemon,
            system_operations: system_operations.clone(),
            transport,
        },
        ImageLifecycleRoute::LocalCli => ImageControlRoute::LocalCli {
            runner: local_cmd.clone(),
            system_operations: system_operations.clone(),
        },
        ImageLifecycleRoute::DirectDbus => ImageControlRoute::DirectDbus {
            dbus: DbusBackend::new(),
            fallback_runner: local_cmd,
            fallback_operations: system_operations.clone(),
        },
    };
    let control: Arc<dyn ImageControl> = Arc::new(RoutedImageControl { route, nspawn });
    let cleanup: Arc<dyn ManagedArtifactCleanup> = Arc::new(StoreArtifactCleanup {
        systemd_unit,
        nvidia_state,
        system_operations,
    });
    ImageLifecycleService::new(runtime, control, cleanup, registry)
}

fn direct_remove_may_fallback(outcome: &ImageControlOutcome) -> bool {
    matches!(outcome, ImageControlOutcome::NotAttempted { .. })
}

struct CatalogImageRuntime(Arc<RuntimeCatalog>);

#[async_trait::async_trait]
impl ImageRuntime for CatalogImageRuntime {
    async fn list_machines(&self) -> Result<Vec<MachineEntry>, String> {
        self.0
            .machines()
            .await
            .map(|query| query.value)
            .map_err(|error| error.to_string())
    }
}

enum ImageControlRoute {
    DirectDbus {
        dbus: DbusBackend,
        fallback_runner: Arc<dyn CommandRunner>,
        fallback_operations: SystemOperationStore,
    },
    LocalCli {
        runner: Arc<dyn CommandRunner>,
        system_operations: SystemOperationStore,
    },
    Daemon {
        daemon: Arc<ElevatedDaemon>,
        system_operations: SystemOperationStore,
        transport: ImageRemoveTransport,
    },
}

struct RoutedImageControl {
    route: ImageControlRoute,
    nspawn: crate::adapters::config::NspawnConfigStore,
}

#[async_trait::async_trait]
impl ImageControl for RoutedImageControl {
    async fn disable_unit(&self, machine: &MachineName) -> UnitDisableReport {
        let result = match &self.route {
            ImageControlRoute::DirectDbus {
                dbus,
                fallback_operations,
                ..
            } => {
                if RuntimeSource::is_available(dbus).await {
                    dbus.disable(machine.as_str()).await
                } else {
                    fallback_operations.disable(machine.as_str()).await
                }
            }
            ImageControlRoute::LocalCli {
                system_operations, ..
            } => system_operations.disable(machine.as_str()).await,
            ImageControlRoute::Daemon {
                daemon,
                system_operations,
                transport,
                ..
            } => match transport {
                ImageRemoveTransport::Dbus => disable_unit_via_daemon(daemon, machine).await,
                ImageRemoveTransport::Cli => system_operations.disable(machine.as_str()).await,
            },
        };
        match result {
            Ok(()) => UnitDisableReport::Disabled,
            Err(error) => UnitDisableReport::Failed(error.to_string()),
        }
    }

    async fn remove_image(&self, image: &ImageName) -> ImageControlOutcome {
        let outcome = match &self.route {
            ImageControlRoute::DirectDbus {
                dbus,
                fallback_runner,
                ..
            } => match dbus.remove_image_outcome(image).await {
                outcome if direct_remove_may_fallback(&outcome) => {
                    crate::adapters::system_operation::execute_cli_image_remove_with_runner(
                        image.clone(),
                        fallback_runner.as_ref(),
                    )
                    .await
                }
                outcome => outcome,
            },
            ImageControlRoute::LocalCli { runner, .. } => {
                crate::adapters::system_operation::execute_cli_image_remove_with_runner(
                    image.clone(),
                    runner.as_ref(),
                )
                .await
            }
            ImageControlRoute::Daemon {
                daemon, transport, ..
            } => {
                let request = ImageRemoveRequest {
                    image: image.clone(),
                    transport: *transport,
                };
                match daemon.image_remove(request).await {
                    Ok(outcome) => outcome,
                    Err(error) => ImageControlOutcome::OutcomeUnknown {
                        reason: format!("daemon response was lost: {error}"),
                    },
                }
            }
        };

        if matches!(&outcome, ImageControlOutcome::Removed) {
            match self.nspawn.cleanup_sidecar_locks(image.as_str()).await {
                Ok(true) => log::debug!(
                    "Removed stale nspawn sidecar locks for image {}",
                    image.as_str()
                ),
                Ok(false) => {}
                Err(error) => log::warn!(
                    "Image {} was removed, but its nspawn sidecar locks could not be cleaned: {}",
                    image.as_str(),
                    error
                ),
            }
        }

        outcome
    }
}

async fn disable_unit_via_daemon(
    daemon: &ElevatedDaemon,
    machine: &MachineName,
) -> crate::nspawn::errors::Result<()> {
    let outcome = daemon
        .machine_control(MachineControlRequest {
            machine: machine.clone(),
            action: MachineAction::Disable,
            transport: MachineControlTransport::Dbus,
        })
        .await
        .map_err(|error| crate::nspawn::errors::NspawnError::Runtime(error.to_string()))?;
    match outcome {
        MachineControlOutcome::Succeeded => Ok(()),
        other => Err(crate::nspawn::errors::NspawnError::Runtime(format!(
            "disable unit was not completed: {other:?}"
        ))),
    }
}

struct StoreArtifactCleanup {
    systemd_unit: crate::adapters::config::SystemdUnitStore,
    nvidia_state: crate::adapters::platform::nvidia::NvidiaStateStore,
    system_operations: SystemOperationStore,
}

#[async_trait::async_trait]
impl ManagedArtifactCleanup for StoreArtifactCleanup {
    async fn cleanup(&self, machine: &MachineName) -> ArtifactCleanupReport {
        let mut removed = Vec::new();
        let mut ambiguous = Vec::new();
        let mut failed = Vec::new();
        let mut unit_drop_ins_removed = false;
        match self
            .systemd_unit
            .remove_owned_overrides(machine.as_str())
            .await
        {
            Ok(ownership) if ownership.contains(&ArtifactOwnership::ProvenOwned) => {
                unit_drop_ins_removed = true;
                removed.push("systemd unit drop-ins".to_string());
                if ownership.contains(&ArtifactOwnership::AmbiguousLegacy) {
                    ambiguous.push("ambiguous legacy systemd unit drop-ins were preserved".into());
                }
            }
            Ok(ownership) if ownership.contains(&ArtifactOwnership::AmbiguousLegacy) => {
                ambiguous.push("ambiguous legacy systemd unit drop-ins were preserved".into());
            }
            Ok(_) => {}
            Err(error) => failed.push(format!("systemd unit drop-ins: {error}")),
        }
        match self.nvidia_state.remove_owned(machine.as_str()).await {
            Ok(ArtifactOwnership::ProvenOwned) => removed.push("NVIDIA state".to_string()),
            Ok(ArtifactOwnership::AmbiguousLegacy) => {
                ambiguous.push("ambiguous legacy NVIDIA state was preserved".into());
            }
            Ok(ArtifactOwnership::NotPresent) => {}
            Err(error) => failed.push(format!("NVIDIA state: {error}")),
        }
        if unit_drop_ins_removed {
            if let Err(error) = self.system_operations.reload_daemon().await {
                failed.push(format!("systemd daemon reload: {error}"));
            }
        }
        match (removed.is_empty(), failed.is_empty(), ambiguous.is_empty()) {
            (true, true, true) => ArtifactCleanupReport::NotApplicable,
            (true, true, false) => ArtifactCleanupReport::PreservedAmbiguous(ambiguous),
            (false, true, true) => ArtifactCleanupReport::Removed,
            (false, _, _) => {
                failed.extend(ambiguous);
                ArtifactCleanupReport::PartiallyRemoved(failed)
            }
            (true, false, _) => {
                failed.extend(ambiguous);
                ArtifactCleanupReport::Failed(failed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::image_lifecycle::ImageRemovalRejection;

    #[test]
    fn direct_remove_falls_back_only_when_no_attempt_was_made() {
        assert!(direct_remove_may_fallback(
            &ImageControlOutcome::NotAttempted {
                reason: "D-Bus unavailable".into(),
            }
        ));
        for outcome in [
            ImageControlOutcome::Removed,
            ImageControlOutcome::Rejected {
                rejection: ImageRemovalRejection::Busy,
                reason: "busy".into(),
            },
            ImageControlOutcome::Failed {
                reason: "failed".into(),
            },
            ImageControlOutcome::OutcomeUnknown {
                reason: "response lost".into(),
            },
        ] {
            assert!(!direct_remove_may_fallback(&outcome));
        }
    }
}
