//! Run all host preparation under one direct call or one daemon request.

use crate::adapters::config::{NspawnConfigStore, SystemdUnitStore};
use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::error::NspawnError;
use crate::adapters::platform::nvidia::{
    ensure_gpu_passthrough, NvidiaPreparationOutcome, NvidiaStateStore,
};
use crate::adapters::rootfs::RootfsStore;
use crate::adapters::system_operation::SystemOperationStore;
use crate::adapters::trusted_state::TrustedStateRoot;
use crate::application::machine_lifecycle::{
    MachineControlTransport, MachinePreparationError, MachineStartPreparation,
    MachineStartPreparationRequest,
};
use crate::application::RuntimeCatalog;
use crate::domain::machine::MachineName;
use crate::domain::nvidia::NvidiaCdiSource;
use std::future::Future;
use std::sync::Arc;

pub(super) enum StartPreparationRoute {
    Direct {
        host: DirectStartPreparation,
        system_operations: SystemOperationStore,
    },
    Elevated {
        daemon: Arc<ElevatedDaemon>,
        nvidia_cdi_source: NvidiaCdiSource,
        transport: MachineControlTransport,
    },
}

pub(super) struct RoutedStartPreparation {
    pub(super) route: StartPreparationRoute,
    pub(super) runtime: Arc<RuntimeCatalog>,
}

#[async_trait::async_trait]
impl MachineStartPreparation for RoutedStartPreparation {
    async fn prepare(&self, machine: &MachineName) -> Result<(), MachinePreparationError> {
        let result = match &self.route {
            StartPreparationRoute::Direct {
                host,
                system_operations,
            } => {
                host.prepare(machine, system_operations.reload_daemon())
                    .await
            }
            StartPreparationRoute::Elevated {
                daemon,
                nvidia_cdi_source,
                transport,
            } => {
                daemon
                    .prepare_machine_start(MachineStartPreparationRequest {
                        machine: machine.clone(),
                        nvidia_cdi_source: nvidia_cdi_source.clone(),
                        transport: *transport,
                    })
                    .await
            }
        };
        self.runtime.invalidate();
        result
    }
}

/// Only direct stores can enter this executor. Elevated preparation invokes it
/// inside the daemon, retaining one resource reservation across all stages.
pub(crate) struct DirectStartPreparation {
    nspawn: NspawnConfigStore,
    systemd_unit: SystemdUnitStore,
    nvidia_state: NvidiaStateStore,
    rootfs: RootfsStore,
    nvidia_cdi_source: NvidiaCdiSource,
    skip_permission_denied: bool,
}

impl DirectStartPreparation {
    pub(crate) fn new(
        state_root: TrustedStateRoot,
        nvidia_cdi_source: NvidiaCdiSource,
        skip_permission_denied: bool,
    ) -> Self {
        Self {
            nspawn: NspawnConfigStore::direct(),
            systemd_unit: SystemdUnitStore::direct(),
            nvidia_state: NvidiaStateStore::direct(state_root),
            rootfs: RootfsStore::direct(),
            nvidia_cdi_source,
            skip_permission_denied,
        }
    }

    /// The selected systemd backend supplies the lazy reload operation. It is
    /// polled only after NVIDIA synchronization reports that a reload is needed.
    pub(crate) async fn prepare(
        &self,
        machine: &MachineName,
        reload: impl Future<Output = Result<(), NspawnError>>,
    ) -> Result<(), MachinePreparationError> {
        self.nvidia_cdi_source
            .validate()
            .map_err(MachinePreparationError::invalid_configuration)?;
        let result = ensure_gpu_passthrough(
            machine.as_str(),
            &self.nspawn,
            &self.systemd_unit,
            &self.nvidia_state,
            &self.rootfs,
            &self.nvidia_cdi_source,
        )
        .await;
        reload_after_nvidia_preparation(result, machine, reload, self.skip_permission_denied).await
    }
}

async fn reload_after_nvidia_preparation(
    result: Result<NvidiaPreparationOutcome, NspawnError>,
    machine: &MachineName,
    reload: impl Future<Output = Result<(), NspawnError>>,
    skip_permission_denied: bool,
) -> Result<(), MachinePreparationError> {
    match result {
        Ok(NvidiaPreparationOutcome::NotConfigured) => Ok(()),
        Ok(NvidiaPreparationOutcome::Synchronized) => reload.await.map_err(map_preparation_error),
        Err(NspawnError::PermissionDenied) if skip_permission_denied => {
            log::warn!(
                "skipping NVIDIA pre-start synchronization for {}: NVIDIA state could not be read without elevated privileges",
                machine,
            );
            Ok(())
        }
        Err(error) => Err(map_preparation_error(error)),
    }
}

fn map_preparation_error(error: NspawnError) -> MachinePreparationError {
    if let NspawnError::SystemOperationOutcomeUnknown(reason) = error {
        return MachinePreparationError::outcome_unknown(reason);
    }
    if matches!(&error, NspawnError::GenericIo(source) | NspawnError::Io(_, source) if source.kind() == std::io::ErrorKind::TimedOut)
    {
        return MachinePreparationError::outcome_unknown(error.to_string());
    }
    let permission_denied = error.is_polkit_rejection()
        || matches!(&error, NspawnError::PermissionDenied)
        || matches!(&error, NspawnError::GenericIo(source) | NspawnError::Io(_, source) if source.kind() == std::io::ErrorKind::PermissionDenied);
    let invalid_configuration = matches!(
        &error,
        NspawnError::Validation(_) | NspawnError::InvalidConfig(_)
    );
    let message = error.to_string();
    if permission_denied {
        MachinePreparationError::permission_denied(message)
    } else if invalid_configuration {
        MachinePreparationError::invalid_configuration(message)
    } else {
        MachinePreparationError::failed(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;

    #[tokio::test]
    async fn startup_without_nvidia_does_not_reload_the_manager() {
        let runner = MockCommandRunner::new();
        let store = SystemOperationStore::direct(Arc::new(runner));
        reload_after_nvidia_preparation(
            Ok(NvidiaPreparationOutcome::NotConfigured),
            &MachineName::new("test").unwrap(),
            store.reload_daemon(),
            false,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn nvidia_synchronization_requires_a_successful_reload_on_every_attempt() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .times(2)
            .withf(|program, args, _| {
                program == "systemctl"
                    && args.iter().map(String::as_str).eq(["--", "daemon-reload"])
            })
            .returning(|_, _, _| {
                Ok(Output {
                    status: std::process::ExitStatus::from_raw(1 << 8),
                    stdout: vec![],
                    stderr: b"Reload failed".to_vec(),
                })
            });
        let store = SystemOperationStore::direct(Arc::new(runner));
        for _ in 0..2 {
            let error = reload_after_nvidia_preparation(
                Ok(NvidiaPreparationOutcome::Synchronized),
                &MachineName::new("test").unwrap(),
                store.reload_daemon(),
                true,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("Reload failed"));
        }
    }

    #[tokio::test]
    async fn only_the_normal_nvidia_read_preflight_can_skip_permission_denial() {
        let store = SystemOperationStore::direct(Arc::new(MockCommandRunner::new()));
        let machine = MachineName::new("test").unwrap();
        reload_after_nvidia_preparation(
            Err(NspawnError::PermissionDenied),
            &machine,
            store.reload_daemon(),
            true,
        )
        .await
        .unwrap();
        let error = reload_after_nvidia_preparation(
            Err(NspawnError::PermissionDenied),
            &machine,
            store.reload_daemon(),
            false,
        )
        .await
        .unwrap_err();
        assert!(error.is_permission_denied());
        let error = reload_after_nvidia_preparation(
            Err(NspawnError::InvalidConfig("invalid NVIDIA markers".into())),
            &machine,
            store.reload_daemon(),
            true,
        )
        .await
        .unwrap_err();
        assert!(error.is_invalid_configuration());
    }

    #[test]
    fn preparation_errors_keep_host_semantics_at_the_adapter_boundary() {
        let permission = map_preparation_error(NspawnError::PermissionDenied);
        assert!(permission.is_permission_denied());
        assert!(!permission.is_invalid_configuration());

        let invalid = map_preparation_error(NspawnError::InvalidConfig("bad unit".into()));
        assert!(invalid.is_invalid_configuration());
        assert!(!invalid.is_permission_denied());

        let failed = map_preparation_error(NspawnError::Runtime("probe failed".into()));
        assert!(!failed.is_invalid_configuration());
        assert!(!failed.is_permission_denied());
        assert_eq!(failed.to_string(), "Runtime error: probe failed");
    }

    #[tokio::test]
    async fn reload_deadline_preserves_uncertain_completion() {
        let error = reload_after_nvidia_preparation(
            Ok(NvidiaPreparationOutcome::Synchronized),
            &MachineName::new("test").unwrap(),
            async {
                Err(NspawnError::SystemOperationOutcomeUnknown(
                    "reload deadline".into(),
                ))
            },
            false,
        )
        .await
        .unwrap_err();
        assert!(error.is_outcome_unknown());

        let error = map_preparation_error(NspawnError::Io(
            "systemd-nspawn".into(),
            std::io::Error::new(std::io::ErrorKind::TimedOut, "rootfs cleanup deadline"),
        ));
        assert!(error.is_outcome_unknown());
    }
}
