//! One deployment transaction: apply effects, settle resources, then publish completion.

mod apply;
mod cleanup;
mod journal;
mod preflight;
mod rollback;

use crate::adapters::provisioning::engine::diagnostics::DeploymentDiagnostics;
use crate::adapters::provisioning::engine::execution::{DeploymentExecution, StoragePreparation};
use crate::adapters::provisioning::engine::failure::DeploymentFailure;
use crate::adapters::provisioning::engine::host::DirectProvisioningCapabilities;
use crate::adapters::provisioning::engine::resources::ApplyReport;
use crate::adapters::rootfs::RootfsTarget;
use crate::application::provisioning::{DeploymentJobContext, DeploymentSecrets};

/// State held from the first attempted effect through cleanup and durable finalization.
struct Transaction<'a> {
    host: DirectProvisioningCapabilities,
    storage: StoragePreparation,
    job: DeploymentJobContext,
    logs: &'a mut DeploymentDiagnostics,
    report: ApplyReport,
    raw_mount_target: Option<RootfsTarget>,
    storage_mount_attempted: bool,
    external_provider_started: bool,
}

pub(crate) async fn run_deployment(
    execution: DeploymentExecution,
    host: DirectProvisioningCapabilities,
    secrets: DeploymentSecrets,
    job: DeploymentJobContext,
    logs: &mut DeploymentDiagnostics,
) -> Result<(), DeploymentFailure> {
    let DeploymentExecution {
        target,
        source,
        storage,
        configuration,
    } = execution;
    let mut transaction = Transaction {
        host,
        storage,
        job,
        logs,
        report: ApplyReport::new(target),
        raw_mount_target: None,
        storage_mount_attempted: false,
        external_provider_started: false,
    };
    let result = apply::apply(source, configuration, secrets, &mut transaction).await;
    cleanup::finish(result, transaction).await
}
