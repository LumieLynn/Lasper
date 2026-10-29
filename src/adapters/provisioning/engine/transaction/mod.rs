//! One deployment transaction: apply effects, settle resources, then publish completion.

mod apply;
mod cleanup;
mod journal;
mod preflight;
mod rollback;

use crate::adapters::provisioning::engine::diagnostics::DeploymentDiagnostics;
use crate::adapters::provisioning::engine::execution::{DeploymentExecution, DeploymentMethod};
use crate::adapters::provisioning::engine::failure::DeploymentFailure;
use crate::adapters::provisioning::engine::host::DirectProvisioningCapabilities;
use crate::adapters::provisioning::engine::resources::ApplyReport;
use crate::adapters::storage::rootfs::RawRootfsMount;
use crate::application::provisioning::{DeploymentJobContext, DeploymentResult, DeploymentSecrets};

/// State held from the first attempted effect through cleanup and durable finalization.
struct Transaction<'a> {
    host: DirectProvisioningCapabilities,
    method: DeploymentMethod,
    job: DeploymentJobContext,
    logs: &'a mut DeploymentDiagnostics,
    report: ApplyReport,
    output: DeploymentResult,
    raw_mount: Option<RawRootfsMount>,
    storage_mount_attempted: bool,
    external_provider_started: bool,
}

pub(crate) async fn run_deployment(
    execution: DeploymentExecution,
    host: DirectProvisioningCapabilities,
    secrets: DeploymentSecrets,
    job: DeploymentJobContext,
    logs: &mut DeploymentDiagnostics,
) -> Result<DeploymentResult, DeploymentFailure> {
    let DeploymentExecution { target, method } = execution;
    let mut transaction = Transaction {
        host,
        method,
        job,
        logs,
        report: ApplyReport::new(target),
        output: DeploymentResult::default(),
        raw_mount: None,
        storage_mount_attempted: false,
        external_provider_started: false,
    };
    let result = apply::apply(secrets, &mut transaction).await;
    cleanup::finish(result, transaction).await
}

#[cfg(test)]
mod tests;
