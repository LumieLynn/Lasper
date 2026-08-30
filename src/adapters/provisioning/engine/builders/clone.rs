//! Container cloning deployment implementation.

use async_trait::async_trait;

use crate::adapters::error::{NspawnError, Result};
use crate::adapters::provisioning::engine::{
    check_deployment_cancellation, send_deploy_log, AppliedResource, ApplyReport, DeployLogEvent,
    Deployer, DeploymentCancellation,
};
use crate::application::provisioning::ResourceApplyStatus;
use crate::application::provisioning::{DeploymentResource, MachineProvisioningConfig};
use crate::domain::machine::MachineName;

pub struct CloneDeployer {
    pub source_name: String,
    pub system_operations: crate::adapters::system_operation::SystemOperationStore,
    pub nspawn: crate::adapters::config::NspawnConfigStore,
    pub systemd_unit: crate::adapters::config::SystemdUnitStore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClonedConfigStatus {
    ClonedBySystemd,
    ClonedAfterSourceChange,
    NotPresent,
}

impl ClonedConfigStatus {
    fn label(self) -> &'static str {
        match self {
            Self::ClonedBySystemd => "cloned by systemd",
            Self::ClonedAfterSourceChange => "cloned by systemd after the source changed",
            Self::NotPresent => "not present",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CloneApplyResult {
    config: ClonedConfigStatus,
    service_override: Option<ResourceApplyStatus>,
}

impl CloneApplyResult {
    fn summary(self) -> String {
        let service_override = match self.service_override {
            Some(ResourceApplyStatus::Created) => "cloned",
            Some(ResourceApplyStatus::Unchanged) => "already identical",
            Some(ResourceApplyStatus::ReplacedOwned) => "replaced",
            Some(ResourceApplyStatus::ConflictUnknownOwner) => "conflict",
            None => "not present",
        };
        format!(
            "image cloned; .nspawn settings {}; Lasper service override {service_override}",
            self.config.label()
        )
    }
}

#[async_trait]
impl Deployer for CloneDeployer {
    fn is_external_storage_managed(&self) -> bool {
        true
    }

    fn requires_post_config(&self) -> bool {
        false
    }

    fn source_stage_resources(&self, target: &MachineName) -> Vec<DeploymentResource> {
        vec![
            DeploymentResource::ExternalImage(target.clone()),
            DeploymentResource::NspawnConfig(target.clone()),
            DeploymentResource::SystemdOverride(target.clone()),
        ]
    }

    async fn deploy(
        &self,
        name: &str,
        _cfg: &MachineProvisioningConfig,
        _rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
        cancellation: &DeploymentCancellation,
        report: &mut ApplyReport,
    ) -> Result<()> {
        check_deployment_cancellation(cancellation)?;
        let source_config = self.nspawn.inspect(&self.source_name).await?;
        let source_has_override = if MachineName::new(&self.source_name).is_ok() {
            let source_unit = self.systemd_unit.read(&self.source_name).await?;
            source_unit.drop_ins.iter().any(|drop_in| {
                std::path::Path::new(&drop_in.path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    == Some("override.conf")
            })
        } else {
            false
        };
        check_deployment_cancellation(cancellation)?;

        send_deploy_log(
            &logs,
            format!("Cloning image {} to {}...", self.source_name, name),
        )
        .await;

        self.system_operations
            .clone_image(&self.source_name, name)
            .await?;
        report.record_created(AppliedResource::ExternalImage);
        check_deployment_cancellation(cancellation)?;

        let config_result = verify_systemd_cloned_config(
            &self.nspawn,
            &self.source_name,
            name,
            source_config.as_ref(),
            report,
        )
        .await?;
        check_deployment_cancellation(cancellation)?;

        let override_apply = if source_has_override {
            let apply = self
                .systemd_unit
                .clone_override(&self.source_name, name)
                .await?;
            report.record_apply(AppliedResource::SystemdOverride, apply)?;
            check_deployment_cancellation(cancellation)?;
            Some(apply)
        } else {
            None
        };

        self.system_operations
            .reload_daemon()
            .await
            .map_err(|error| {
                NspawnError::Runtime(format!("Failed to reload systemd after clone: {error}"))
            })?;
        check_deployment_cancellation(cancellation)?;

        let result = CloneApplyResult {
            config: config_result,
            service_override: override_apply,
        };
        send_deploy_log(&logs, format!("Clone result: {}.", result.summary())).await;

        Ok(())
    }
}

async fn verify_systemd_cloned_config(
    nspawn: &crate::adapters::config::NspawnConfigStore,
    source_name: &str,
    destination: &str,
    source_before: Option<&crate::adapters::config::nspawn_file::NspawnConfig>,
    report: &mut ApplyReport,
) -> Result<ClonedConfigStatus> {
    let destination_config = nspawn.inspect(destination).await?;
    match (source_before, destination_config.as_ref()) {
        (Some(source), Some(destination)) if source.content == destination.content => {
            Ok(ClonedConfigStatus::ClonedBySystemd)
        }
        (Some(_), None) => Err(NspawnError::Runtime(
            "machinectl clone completed without copying the source .nspawn settings".into(),
        )),
        (Some(_), Some(_)) => {
            report.block_external_image_removal(
                "the cloned .nspawn settings do not match the source snapshot",
            );
            Err(NspawnError::Runtime(
                "machinectl clone produced .nspawn settings that do not match the source".into(),
            ))
        }
        (None, None) => Ok(ClonedConfigStatus::NotPresent),
        (None, Some(destination)) => {
            let source_after = nspawn.inspect(source_name).await?;
            if source_after
                .as_ref()
                .is_some_and(|source| source.content == destination.content)
            {
                Ok(ClonedConfigStatus::ClonedAfterSourceChange)
            } else {
                report.block_external_image_removal(
                    "unexpected .nspawn settings appeared while cloning",
                );
                Err(NspawnError::Runtime(
                    "unexpected .nspawn settings appeared while cloning the image".into(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_result_distinguishes_provider_and_lasper_artifacts() {
        let complete = CloneApplyResult {
            config: ClonedConfigStatus::ClonedBySystemd,
            service_override: Some(ResourceApplyStatus::Created),
        };
        assert_eq!(
            complete.summary(),
            "image cloned; .nspawn settings cloned by systemd; Lasper service override cloned"
        );

        let image_only = CloneApplyResult {
            config: ClonedConfigStatus::NotPresent,
            service_override: None,
        };
        assert_eq!(
            image_only.summary(),
            "image cloned; .nspawn settings not present; Lasper service override not present"
        );
    }

    #[test]
    fn clone_declares_every_resource_systemd_or_lasper_may_copy() {
        let target = MachineName::new("test").unwrap();
        let deployer = CloneDeployer {
            source_name: "base".into(),
            system_operations: crate::adapters::system_operation::SystemOperationStore::direct(
                std::sync::Arc::new(crate::adapters::process::DefaultCommandRunner),
            ),
            nspawn: crate::adapters::config::NspawnConfigStore::direct(),
            systemd_unit: crate::adapters::config::SystemdUnitStore::direct(),
        };

        assert_eq!(
            deployer.source_stage_resources(&target),
            vec![
                DeploymentResource::ExternalImage(target.clone()),
                DeploymentResource::NspawnConfig(target.clone()),
                DeploymentResource::SystemdOverride(target),
            ]
        );
    }
}
