//! Apply the selected source and requested guest/host configuration in stage order.

use super::{
    journal::{persist_applying, persist_committed},
    preflight::inspect_deployment_sidecars,
    Transaction,
};
use crate::adapters::inspection::accounts::local_accounts;
use crate::adapters::provisioning::engine::execution::{
    check_deployment_cancellation, StoragePreparation, SystemConfiguration,
};
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::adapters::provisioning::engine::host::HostResultExt;
use crate::adapters::provisioning::engine::resources::AppliedResource;
use crate::adapters::provisioning::engine::sources::Deployer;
use crate::adapters::storage::rootfs::{RawRootfsMount, RootfsTarget};
use crate::application::provisioning::{
    DeploymentResource, DeploymentSecrets, DeploymentStage, ResourceApplyStatus,
    ResourceDisposition,
};
use crate::domain::machine::accounts::LocalAccountObservation;
use crate::domain::machine::PrivateUsersMode;

pub(super) async fn apply(
    source: Box<dyn Deployer>,
    configuration: Option<SystemConfiguration>,
    mut secrets: DeploymentSecrets,
    transaction: &mut Transaction<'_>,
) -> OperationResult<()> {
    let Transaction {
        host,
        storage,
        job,
        logs,
        report,
        output,
        raw_mount,
        storage_mount_attempted,
        external_provider_started,
    } = transaction;
    let target = report.target.clone();
    let name = target.to_string();
    let cancellation = job.cancellation();
    let system_operations = host.system_operations.clone();
    logs.message(format!("=== Deploying '{name}' ==="));
    check_deployment_cancellation(&cancellation)?;
    for warning in inspect_deployment_sidecars(&name, host).await? {
        log::warn!(
            "[AUDIT] [Container: {}] [Step: Preflight] {}",
            name,
            warning
        );
        logs.message(format!("WARNING: {warning}"));
    }
    check_deployment_cancellation(&cancellation)?;

    if let Some(storage) = storage.backend() {
        logs.enter_stage(DeploymentStage::StoragePreparation);
        log::info!(
            "[AUDIT] [Container: {}] [Step: Storage] Creating {} storage...",
            name,
            storage.get_type().label()
        );
        logs.message(format!(
            "Creating storage (type: {:?})...",
            storage.get_type()
        ));
        let local_storage = DeploymentResource::LocalStorage(target.clone());
        persist_applying(
            job,
            DeploymentStage::StoragePreparation,
            vec![local_storage],
            report,
        )
        .await?;
        storage
            .create(&name)
            .await
            .host_context("create deployment storage")?;
        report.record_created(AppliedResource::LocalStorage);
        persist_committed(job, DeploymentStage::StoragePreparation, report).await?;
        check_deployment_cancellation(&cancellation)?;
    }

    let rootfs = if let Some(storage) = storage.backend() {
        log::info!(
            "[AUDIT] [Container: {}] [Step: Storage] Mounting storage tree...",
            name
        );
        logs.message("Mounting storage...".to_string());
        let storage_mount = DeploymentResource::StorageMount(target.clone());
        persist_applying(
            job,
            DeploymentStage::StoragePreparation,
            vec![storage_mount.clone()],
            report,
        )
        .await?;
        *storage_mount_attempted = true;
        let rootfs = storage
            .mount(&name)
            .await
            .host_context("mount deployment storage")?;
        report.record_typed(storage_mount, ResourceDisposition::Created);
        persist_committed(job, DeploymentStage::StoragePreparation, report).await?;
        rootfs
    } else {
        // For externally managed storage (clone/pull), the machine is already in /var/lib/machines.
        crate::paths::machine_root(&name)
    };

    logs.enter_stage(DeploymentStage::SourceDeployment);
    log::info!(
        "[AUDIT] [Container: {}] [Step: Deploy] Initiating base rootfs transfer...",
        name
    );
    *external_provider_started = matches!(storage, StoragePreparation::ProviderImage);
    let source_resources = source.source_stage_resources(&target);
    persist_applying(
        job,
        DeploymentStage::SourceDeployment,
        source_resources,
        report,
    )
    .await?;
    source
        .deploy(&name, &rootfs, logs, &cancellation, report)
        .await?;
    persist_committed(job, DeploymentStage::SourceDeployment, report).await?;
    check_deployment_cancellation(&cancellation)?;

    let Some(SystemConfiguration {
        machine: cfg,
        users,
        wayland: wayland_intents,
        integration,
    }) = configuration
    else {
        check_deployment_cancellation(&cancellation)?;
        return Ok(());
    };
    let guest_hostname =
        crate::domain::machine::GuestHostname::resolve(&cfg.guest_hostname, &target)
            .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?;
    let nvidia_profile = integration.nvidia_profile;
    let nvidia_cdi_source = integration.nvidia_cdi_source;

    logs.enter_stage(DeploymentStage::RootfsMutation);
    let mut actual_rootfs_target = RootfsTarget::from_provisioned_path(&name, &rootfs)
        .host_context("resolve deployment rootfs")?;
    let rootfs_exists = host.guest.has_os_release(&actual_rootfs_target).await?;
    if !rootfs_exists && actual_rootfs_target.supports_raw_fallback() {
        logs.message("Mounting raw image for configuration...".to_string());
        let mount_resource = DeploymentResource::RawConfigurationMount(target.clone());
        persist_applying(
            job,
            DeploymentStage::RootfsMutation,
            vec![mount_resource.clone()],
            report,
        )
        .await?;
        match RawRootfsMount::mount(&target, host.commands.as_ref()).await {
            Ok(Some(mount)) => {
                actual_rootfs_target = mount.target();
                *raw_mount = Some(mount);
                report.record_typed(mount_resource, ResourceDisposition::Created);
            }
            Ok(None) => {}
            Err(error) => {
                return Err(OperationFailure::host(
                    "mount raw image for configuration",
                    error,
                ))
            }
        }
        persist_committed(job, DeploymentStage::RootfsMutation, report).await?;
        check_deployment_cancellation(&cancellation)?;
    }

    let has_os_layout = host.guest.has_os_release(&actual_rootfs_target).await?;
    let supports_offline_commands = has_os_layout
        && host
            .guest
            .supports_nspawn_commands(&actual_rootfs_target)
            .await?;

    let has_account_changes = secrets.has_account_changes();
    if has_account_changes && !supports_offline_commands {
        return Err(OperationFailure::InvalidInput(
            "Requested account initialization requires a bootstrapped rootfs with an OS layout and support for systemd-nspawn commands".into(),
        ));
    }

    if has_os_layout {
        logs.message(format!(
            "Setting guest hostname to {}...",
            guest_hostname.as_str()
        ));
        let rootfs_hostname = DeploymentResource::RootfsHostname(target.clone());
        persist_applying(
            job,
            DeploymentStage::RootfsMutation,
            vec![rootfs_hostname.clone()],
            report,
        )
        .await?;
        host.guest
            .configure_hostname(&actual_rootfs_target, &guest_hostname)
            .await?;
        report.record_typed(rootfs_hostname, ResourceDisposition::Committed);
        persist_committed(job, DeploymentStage::RootfsMutation, report).await?;
        check_deployment_cancellation(&cancellation)?;
    }

    if supports_offline_commands {
        if has_account_changes {
            persist_applying(
                job,
                DeploymentStage::RootfsMutation,
                vec![DeploymentResource::RootfsAccounts(target.clone())],
                report,
            )
            .await?;
        }
        if let Some(password) = secrets.take_root_password() {
            logs.message("Setting root password...".to_string());
            host.guest
                .set_root_password(&actual_rootfs_target, password)
                .await?;
            check_deployment_cancellation(&cancellation)?;
        }

        let mut ordered_users = users.iter().collect::<Vec<_>>();
        ordered_users.sort_by_key(|user| user.uid.is_none());
        for user in ordered_users {
            logs.message(format!("Creating user {}...", user.username));
            let password = secrets
                .take_user_password(&user.username)
                .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?;
            host.guest
                .create_user(&actual_rootfs_target, user, password)
                .await?;
            check_deployment_cancellation(&cancellation)?;
        }
    } else if !has_os_layout {
        logs.message(
            "WARNING: Could not verify the rootfs OS layout; the guest hostname was not changed."
                .to_string(),
        );
    }

    check_deployment_cancellation(&cancellation)?;
    let accounts = local_accounts(&actual_rootfs_target).await;
    match &accounts {
        LocalAccountObservation::Read {
            accounts,
            rejected_lines,
        } => {
            logs.message(format!(
                "Observed {} local account(s) in /etc/passwd.",
                accounts.len()
            ));
            if !rejected_lines.is_empty() {
                logs.message(format!(
                    "WARNING: {} malformed passwd record(s) were skipped.",
                    rejected_lines.len()
                ));
            }
        }
        _ => logs.message(
            "WARNING: Local accounts could not be read; see the deployment result for details.",
        ),
    }
    output.local_accounts = Some(accounts);
    check_deployment_cancellation(&cancellation)?;

    let mut resolved_wayland = Vec::with_capacity(wayland_intents.len());
    for intent in wayland_intents {
        if !supports_offline_commands {
            return Err(OperationFailure::InvalidInput(
                "Wayland grant requires a rootfs that supports user identity lookup".into(),
            ));
        }
        let user = users
            .iter()
            .find(|user| user.username == intent.target_username())
            .ok_or_else(|| {
                OperationFailure::InvalidInput(
                    "Wayland target is not part of this deployment".into(),
                )
            })?;
        logs.message(format!(
            "Resolving Wayland target identity for {}...",
            user.username
        ));
        let identity = host
            .guest
            .resolve_user_identity(&actual_rootfs_target, &user.username)
            .await?;
        let grant = crate::domain::provisioning::wayland::resolve_wayland_grant(
            intent,
            identity,
            cfg.private_users,
        )
        .map_err(|error| OperationFailure::InvalidInput(error.to_string()))?;
        logs.message(format!(
            "Prepared {} startup Wayland projection(s) for {}...",
            grant.sockets().len(),
            user.username,
        ));
        check_deployment_cancellation(&cancellation)?;
        resolved_wayland.push(grant);
    }
    if supports_offline_commands && has_account_changes {
        report.record_typed(
            DeploymentResource::RootfsAccounts(target.clone()),
            ResourceDisposition::Committed,
        );
        persist_committed(job, DeploymentStage::RootfsMutation, report).await?;
    }

    if supports_offline_commands {
        if let Some(mode) = &cfg.network {
            if mode.uses_default_guest_network_stack() {
                logs.message(
                    "Enabling container network and DNS services (systemd-networkd, systemd-resolved)..."
                        .to_string()
                );
                let rootfs_network = DeploymentResource::RootfsNetwork(target.clone());
                persist_applying(
                    job,
                    DeploymentStage::RootfsMutation,
                    vec![rootfs_network.clone()],
                    report,
                )
                .await?;
                for warning in host.guest.configure_network(&actual_rootfs_target).await? {
                    logs.message(warning);
                }
                report.record_typed(rootfs_network, ResourceDisposition::Committed);
                persist_committed(job, DeploymentStage::RootfsMutation, report).await?;
                check_deployment_cancellation(&cancellation)?;
            }
        }
    }

    let mut initial_nvidia_state = None;

    if cfg.nvidia_gpu {
        logs.message("Assembling initial NVIDIA GPU configuration...".to_string());

        // Run initial CDI discovery to seed the .nspawn config and state.
        // Remapping is applied inside get_nvidia_state after CDI + ldconfig collection.
        let state = crate::adapters::platform::nvidia::get_nvidia_state_from(
            nvidia_profile.as_ref(),
            &nvidia_cdi_source,
        )
        .await
        .host_context("discover initial NVIDIA CDI configuration")?;
        check_deployment_cancellation(&cancellation)?;

        // Write ld.so.conf.d and env vars into rootfs (one-time setup)
        if supports_offline_commands {
            let rootfs_nvidia = DeploymentResource::RootfsNvidia(target.clone());
            persist_applying(
                job,
                DeploymentStage::RootfsMutation,
                vec![rootfs_nvidia.clone()],
                report,
            )
            .await?;
            for warning in crate::adapters::platform::nvidia::lifecycle::inject_env_once(
                &actual_rootfs_target,
                &state,
            )
            .await
            .host_context("initialize guest NVIDIA environment")?
            {
                log::warn!("{}", warning);
                logs.message(warning);
            }
            report.record_typed(rootfs_nvidia, ResourceDisposition::Committed);
            persist_committed(job, DeploymentStage::RootfsMutation, report).await?;
        } else if has_os_layout {
            logs.message("WARNING: Skipping NVIDIA env/ldconfig injection because this rootfs cannot run systemd-nspawn offline commands.".to_string());
        } else {
            logs.message("WARNING: Skipping NVIDIA env/ldconfig injection because the rootfs OS layout could not be verified.".to_string());
        }
        // Persist the validated snapshot and its profile for lifecycle diffing.
        logs.enter_stage(DeploymentStage::HostConfiguration);
        persist_applying(
            job,
            DeploymentStage::HostConfiguration,
            vec![DeploymentResource::NvidiaState(target.clone())],
            report,
        )
        .await?;
        let state_apply = host
            .nvidia_state
            .write_initial(&name, &state)
            .await
            .host_context("write initial NVIDIA state")?;
        report.record_apply(AppliedResource::NvidiaState, state_apply)?;
        persist_committed(job, DeploymentStage::HostConfiguration, report).await?;
        match state_apply {
            ResourceApplyStatus::ReplacedOwned => {
                let warning = "Replaced existing Lasper-owned NVIDIA state for this deployment.";
                log::warn!("[AUDIT] [Container: {}] [Step: NVIDIA] {}", name, warning);
                logs.message(format!("WARNING: {warning}"));
            }
            ResourceApplyStatus::ConflictUnknownOwner => {
                let warning = "Preserved existing NVIDIA state because Lasper could not prove ownership; automatic NVIDIA lifecycle updates may use stale state.";
                log::warn!("[AUDIT] [Container: {}] [Step: NVIDIA] {}", name, warning);
                logs.message(format!("WARNING: {warning}"));
            }
            ResourceApplyStatus::Created | ResourceApplyStatus::Unchanged => {}
        }
        check_deployment_cancellation(&cancellation)?;

        initial_nvidia_state = Some(state);
    }

    if cfg.private_users == Some(PrivateUsersMode::No) {
        log::warn!(
            "[AUDIT] [Container: {}] [Security] PrivateUsers=no, user namespacing disabled.",
            name
        );
        logs.message("WARNING: PrivateUsers=no, user namespacing disabled.".to_string());
    }

    if cfg.privileged {
        log::warn!("[AUDIT] [Container: {}] [Security: Dangerous] Privileged mode enabled. Capability=all granted.", name);
        logs.message("DANGER: Privileged mode enabled (Capability=all).".to_string());
    }

    logs.enter_stage(DeploymentStage::RuntimeCommit);
    logs.message("Writing .nspawn config...".to_string());
    persist_applying(
        job,
        DeploymentStage::RuntimeCommit,
        vec![DeploymentResource::NspawnConfig(target.clone())],
        report,
    )
    .await?;
    let nspawn_apply = host
        .nspawn
        .write_generated(&cfg, &resolved_wayland, initial_nvidia_state.as_ref())
        .await
        .host_context("write deployment nspawn configuration")?;
    report.record_apply(AppliedResource::NspawnConfig, nspawn_apply)?;
    persist_committed(job, DeploymentStage::RuntimeCommit, report).await?;
    check_deployment_cancellation(&cancellation)?;

    if !cfg.device_binds.is_empty() || cfg.gpu_passthrough_all {
        log::info!(
            "[AUDIT] [Container: {}] [Step: Config] Writing systemd service override...",
            name
        );
        logs.message("Writing systemd service override...".to_string());
        persist_applying(
            job,
            DeploymentStage::RuntimeCommit,
            vec![DeploymentResource::SystemdOverride(target.clone())],
            report,
        )
        .await?;
        let override_apply = host
            .systemd_unit
            .write_override(&name, &cfg.device_binds, cfg.gpu_passthrough_all)
            .await
            .host_context("write deployment service override")?;
        report.record_apply(AppliedResource::SystemdOverride, override_apply)?;
        persist_committed(job, DeploymentStage::RuntimeCommit, report).await?;
        match override_apply {
            ResourceApplyStatus::ReplacedOwned => {
                let warning = "Replaced an existing Lasper-owned systemd service drop-in for this deployment.";
                log::warn!("[AUDIT] [Container: {}] [Step: Config] {}", name, warning);
                logs.message(format!("WARNING: {warning}"));
            }
            ResourceApplyStatus::ConflictUnknownOwner => {
                let warning = "Preserved the existing systemd service drop-in because Lasper could not prove ownership; requested device allowances were not written there.";
                log::warn!("[AUDIT] [Container: {}] [Step: Config] {}", name, warning);
                logs.message(format!("WARNING: {warning}"));
            }
            ResourceApplyStatus::Created | ResourceApplyStatus::Unchanged => {}
        }

        system_operations.reload_daemon().await?;
        check_deployment_cancellation(&cancellation)?;
    }

    check_deployment_cancellation(&cancellation)?;
    Ok(())
}
