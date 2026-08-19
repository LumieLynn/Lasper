//! Runtime composition for the image lifecycle vertical slice.

use super::image_lifecycle::{
    ArtifactCleanupReport, ArtifactOwnership, ImageControl, ImageControlOutcome,
    ImageLifecycleService, ImageRemoveRequest, ImageRemoveTransport, ImageRuntime,
    ManagedArtifactCleanup, UnitDisableReport,
};
use super::{NspawnManager, OperationRegistry, PermissionLevel};
use crate::nspawn::adapters::comm::backend::ContainerBackend;
use crate::nspawn::adapters::comm::daemon_backend::DaemonBackend;
use crate::nspawn::adapters::comm::dbus::DbusBackend;
use crate::nspawn::models::{ContainerEntry, ImageName, MachineName};
use crate::nspawn::sys::command::CommandRunner;
use crate::nspawn::sys::daemon::ElevatedDaemon;
use crate::nspawn::sys::ExecutionContext;
use std::sync::Arc;

pub(crate) fn compose_image_lifecycle(
    manager: Arc<dyn NspawnManager>,
    level: PermissionLevel,
    cli_mode: bool,
    exec_ctx: &ExecutionContext,
) -> ImageLifecycleService {
    let runtime: Arc<dyn ImageRuntime> = Arc::new(LegacyImageRuntime(manager));
    let route = match select_image_control_route(level, cli_mode) {
        ImageControlRouteKind::Daemon(transport) => ImageControlRoute::Daemon {
            daemon: exec_ctx
                .daemon_ref()
                .cloned()
                .expect("elevated image lifecycle requires daemon"),
            dbus: DaemonBackend::new(
                exec_ctx
                    .daemon_ref()
                    .cloned()
                    .expect("elevated image lifecycle requires daemon"),
            ),
            system_operations: exec_ctx.system_operations.clone(),
            transport,
        },
        ImageControlRouteKind::LocalCli => ImageControlRoute::LocalCli {
            runner: exec_ctx.local_cmd.clone(),
            system_operations: exec_ctx.system_operations.clone(),
        },
        ImageControlRouteKind::DirectDbus => ImageControlRoute::DirectDbus {
            dbus: DbusBackend::new(),
            fallback_runner: exec_ctx.local_cmd.clone(),
            fallback_operations: exec_ctx.system_operations.clone(),
        },
    };
    let control: Arc<dyn ImageControl> = Arc::new(RoutedImageControl { route });
    let cleanup: Arc<dyn ManagedArtifactCleanup> = Arc::new(StoreArtifactCleanup {
        systemd_unit: exec_ctx.systemd_unit.clone(),
        nvidia_state: exec_ctx.nvidia_state.clone(),
        system_operations: exec_ctx.system_operations.clone(),
    });
    ImageLifecycleService::new(runtime, control, cleanup, OperationRegistry::new())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageControlRouteKind {
    DirectDbus,
    LocalCli,
    Daemon(ImageRemoveTransport),
}

fn select_image_control_route(level: PermissionLevel, cli_mode: bool) -> ImageControlRouteKind {
    match (level, cli_mode) {
        (PermissionLevel::Elevated, false) => {
            ImageControlRouteKind::Daemon(ImageRemoveTransport::Dbus)
        }
        (PermissionLevel::Elevated, true) => {
            ImageControlRouteKind::Daemon(ImageRemoveTransport::Cli)
        }
        (PermissionLevel::User | PermissionLevel::Root, false) => ImageControlRouteKind::DirectDbus,
        (PermissionLevel::User | PermissionLevel::Root, true) => ImageControlRouteKind::LocalCli,
    }
}

fn direct_remove_may_fallback(outcome: &ImageControlOutcome) -> bool {
    matches!(outcome, ImageControlOutcome::NotAttempted { .. })
}

struct LegacyImageRuntime(Arc<dyn NspawnManager>);

#[async_trait::async_trait]
impl ImageRuntime for LegacyImageRuntime {
    async fn list_machines(&self) -> Result<Vec<ContainerEntry>, String> {
        self.0
            .list_machines()
            .await
            .map_err(|error| error.to_string())
    }
}

enum ImageControlRoute {
    DirectDbus {
        dbus: DbusBackend,
        fallback_runner: Arc<dyn CommandRunner>,
        fallback_operations: super::SystemOperationStore,
    },
    LocalCli {
        runner: Arc<dyn CommandRunner>,
        system_operations: super::SystemOperationStore,
    },
    Daemon {
        daemon: Arc<ElevatedDaemon>,
        dbus: DaemonBackend,
        system_operations: super::SystemOperationStore,
        transport: ImageRemoveTransport,
    },
}

struct RoutedImageControl {
    route: ImageControlRoute,
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
                if ContainerBackend::is_available(dbus).await {
                    ContainerBackend::disable(dbus, machine.as_str()).await
                } else {
                    fallback_operations.disable(machine.as_str()).await
                }
            }
            ImageControlRoute::LocalCli {
                system_operations, ..
            } => system_operations.disable(machine.as_str()).await,
            ImageControlRoute::Daemon {
                dbus,
                system_operations,
                transport,
                ..
            } => match transport {
                ImageRemoveTransport::Dbus => {
                    ContainerBackend::disable(dbus, machine.as_str()).await
                }
                ImageRemoveTransport::Cli => system_operations.disable(machine.as_str()).await,
            },
        };
        match result {
            Ok(()) => UnitDisableReport::Disabled,
            Err(error) => UnitDisableReport::Failed(error.to_string()),
        }
    }

    async fn remove_image(&self, image: &ImageName) -> ImageControlOutcome {
        match &self.route {
            ImageControlRoute::DirectDbus {
                dbus,
                fallback_runner,
                ..
            } => match dbus.remove_image_outcome(image).await {
                outcome if direct_remove_may_fallback(&outcome) => {
                    super::system_operation::execute_cli_image_remove_with_runner(
                        image.clone(),
                        fallback_runner.as_ref(),
                    )
                    .await
                }
                outcome => outcome,
            },
            ImageControlRoute::LocalCli { runner, .. } => {
                super::system_operation::execute_cli_image_remove_with_runner(
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
        }
    }
}

struct StoreArtifactCleanup {
    systemd_unit: crate::nspawn::adapters::config::SystemdUnitStore,
    nvidia_state: crate::nspawn::platform::nvidia::NvidiaStateStore,
    system_operations: super::SystemOperationStore,
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
    use crate::nspawn::ops::image_lifecycle::ImageRemovalRejection;

    #[test]
    fn image_control_route_covers_authority_and_cli_modes() {
        assert_eq!(
            select_image_control_route(PermissionLevel::User, false),
            ImageControlRouteKind::DirectDbus
        );
        assert_eq!(
            select_image_control_route(PermissionLevel::Root, false),
            ImageControlRouteKind::DirectDbus
        );
        assert_eq!(
            select_image_control_route(PermissionLevel::User, true),
            ImageControlRouteKind::LocalCli
        );
        assert_eq!(
            select_image_control_route(PermissionLevel::Root, true),
            ImageControlRouteKind::LocalCli
        );
        assert_eq!(
            select_image_control_route(PermissionLevel::Elevated, false),
            ImageControlRouteKind::Daemon(ImageRemoveTransport::Dbus)
        );
        assert_eq!(
            select_image_control_route(PermissionLevel::Elevated, true),
            ImageControlRouteKind::Daemon(ImageRemoveTransport::Cli)
        );
    }

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
