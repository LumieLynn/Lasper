use super::rollback::{inspect_deployment_sidecars, rollback_apply_report};
use super::{
    capture_uncommitted_effects, finish_manifest, persist_applying, persist_cleanup_pending,
    persist_committed, send_deploy_log, AppliedResource, ApplyReport, Deployer,
    DirectProvisioningCapabilities,
};
use crate::adapters::storage::StorageBackend;
use crate::application::provisioning::{
    DeploymentEvent as DeployLogEvent, DeploymentJobContext, DeploymentResource, DeploymentSecrets,
    DeploymentStage, ResourceDisposition,
};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ApplyStatus, ContainerConfig};

/// Runs one deployment using application-owned job state and event transport.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_deployment(
    deployer: Box<dyn Deployer>,
    storage: Box<dyn StorageBackend>,
    name: String,
    cfg: ContainerConfig,
    nvidia_profile: Option<crate::domain::nvidia::NvidiaPassthroughProfile>,
    wayland_intents: Vec<crate::domain::wayland::WaylandGrantIntent>,
    host: DirectProvisioningCapabilities,
    secrets: DeploymentSecrets,
    job: DeploymentJobContext,
) -> Result<()> {
    let logs = job.event_sender();
    let result = run_deploy_internal(
        deployer,
        storage,
        name.clone(),
        cfg,
        nvidia_profile,
        wayland_intents,
        host,
        secrets,
        job,
    )
    .await;
    if let Err(error) = &result {
        let err_msg = format!("FATAL ERROR: {error}");
        match logs.try_send(DeployLogEvent::Line(err_msg.clone())) {
            Ok(_) => {}
            Err(_) => {
                // If channel is full, we log to stdout as fallback
                log::error!(
                    "[DEPLOY] [Container: {}] Channel full, cannot send log: {}",
                    name,
                    err_msg
                );
            }
        }
    }
    result
}

pub(crate) fn is_cancelled_outcome(error: &NspawnError) -> bool {
    matches!(
        error,
        NspawnError::DeploymentCancelled | NspawnError::DeploymentCancellationRollbackIncomplete(_)
    )
}

#[allow(clippy::too_many_arguments)]
async fn run_deploy_internal(
    deployer: Box<dyn Deployer>,
    storage: Box<dyn StorageBackend>,
    name: String,
    cfg: ContainerConfig,
    nvidia_profile: Option<crate::domain::nvidia::NvidiaPassthroughProfile>,
    wayland_intents: Vec<crate::domain::wayland::WaylandGrantIntent>,
    host: DirectProvisioningCapabilities,
    mut secrets: DeploymentSecrets,
    job: DeploymentJobContext,
) -> Result<()> {
    let logs = job.event_sender();
    let cancellation = job.cancellation();
    let nspawn_spec = crate::nspawn::models::NspawnConfigSpec::try_from(&cfg)?;
    let target = nspawn_spec.machine;
    let guest_hostname = nspawn_spec.guest_hostname;
    let system_operations = host.system_operations.clone();

    macro_rules! push_log {
        ($msg:expr) => {
            send_deploy_log(&logs, $msg).await;
        };
    }

    push_log!(format!("=== Deploying '{}' ===", name));

    let is_ext = deployer.is_external_storage_managed();
    let mut report = ApplyReport::new(target.clone());
    let mut raw_mount_target: Option<crate::adapters::rootfs::RootfsTarget> = None;
    let mut storage_mount_attempted = false;
    let mut external_provider_started = false;

    let result = async {
        cancellation.checkpoint()?;
        for warning in inspect_deployment_sidecars(&name, &host).await? {
            log::warn!("[AUDIT] [Container: {}] [Step: Preflight] {}", name, warning);
            push_log!(format!("WARNING: {warning}"));
        }
        cancellation.checkpoint()?;

        if !is_ext {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Storage] Creating {} storage...",
                name,
                storage.get_type().label()
            );
            push_log!(format!(
                "Creating storage (type: {:?})...",
                storage.get_type()
            ));
            let local_storage = DeploymentResource::LocalStorage(target.clone());
            persist_applying(
                &job,
                DeploymentStage::StoragePreparation,
                vec![local_storage],
                &report,
            )
            .await?;
            storage.create(&name).await?;
            report.record_created(AppliedResource::LocalStorage);
            persist_committed(&job, DeploymentStage::StoragePreparation, &report).await?;
            cancellation.checkpoint()?;
        }

        let rootfs = if !is_ext {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Storage] Mounting storage tree...",
                name
            );
            push_log!("Mounting storage...".to_string());
            let storage_mount = DeploymentResource::StorageMount(target.clone());
            persist_applying(
                &job,
                DeploymentStage::StoragePreparation,
                vec![storage_mount.clone()],
                &report,
            )
            .await?;
            storage_mount_attempted = true;
            let rootfs = storage.mount(&name).await?;
            report.record_typed(storage_mount, ResourceDisposition::Created);
            persist_committed(&job, DeploymentStage::StoragePreparation, &report).await?;
            rootfs
        } else {
            // For externally managed storage (clone/pull), the machine is already in /var/lib/machines.
            crate::paths::machine_root(&name)
        };

        // 3. Perform base deployment
        log::info!(
            "[AUDIT] [Container: {}] [Step: Deploy] Initiating base rootfs transfer...",
            name
        );
        external_provider_started = is_ext;
        let source_resources = deployer.source_stage_resources(&target);
        persist_applying(
            &job,
            DeploymentStage::SourceDeployment,
            source_resources,
            &report,
        )
        .await?;
        deployer
            .deploy(
                &name,
                &cfg,
                &rootfs,
                logs.clone(),
                &cancellation,
                &mut report,
            )
            .await?;
        persist_committed(&job, DeploymentStage::SourceDeployment, &report).await?;
        cancellation.checkpoint()?;

        // 4. Post-deployment configuration
        if !deployer.requires_post_config() {
            log::info!("[AUDIT] [Container: {}] [Step: Config] Skipping post-config for pre-configured clones.", name);
            cancellation.checkpoint()?;
            return Ok(());
        }

        let mut actual_rootfs_target =
            crate::adapters::rootfs::RootfsTarget::from_provisioned_path(&name, &rootfs)?;
        let rootfs_exists = host
            .rootfs
            .has_os_release(&actual_rootfs_target)
            .await?;
        if !rootfs_exists && actual_rootfs_target.supports_raw_fallback() {
            push_log!("Mounting raw image for configuration...".to_string());
            let raw_mount = DeploymentResource::RawConfigurationMount(target.clone());
            persist_applying(
                &job,
                DeploymentStage::RootfsMutation,
                vec![raw_mount.clone()],
                &report,
            )
            .await?;
            match host.rootfs.mount_managed_raw(&name).await {
                Ok(Some(target)) => {
                    actual_rootfs_target = target.clone();
                    raw_mount_target = Some(target);
                    report.record_typed(raw_mount, ResourceDisposition::Created);
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }
            persist_committed(&job, DeploymentStage::RootfsMutation, &report).await?;
            cancellation.checkpoint()?;
        }

        let has_os_layout = host
            .rootfs
            .has_os_release(&actual_rootfs_target)
            .await?;
        let supports_offline_commands = has_os_layout
            && host
                .rootfs
                .supports_nspawn_commands(&actual_rootfs_target)
                .await?;

        if has_os_layout {
            push_log!(format!(
                "Setting guest hostname to {}...",
                guest_hostname.as_str()
            ));
            let rootfs_hostname = DeploymentResource::RootfsHostname(target.clone());
            persist_applying(
                &job,
                DeploymentStage::RootfsMutation,
                vec![rootfs_hostname.clone()],
                &report,
            )
            .await?;
            host.rootfs
                .configure_hostname(&actual_rootfs_target, &guest_hostname)
                .await?;
            report.record_typed(rootfs_hostname, ResourceDisposition::Committed);
            persist_committed(&job, DeploymentStage::RootfsMutation, &report).await?;
            cancellation.checkpoint()?;
        }

        let has_account_changes = secrets.has_account_changes();
        if supports_offline_commands {
            if has_account_changes {
                persist_applying(
                    &job,
                    DeploymentStage::RootfsMutation,
                    vec![DeploymentResource::RootfsAccounts(target.clone())],
                    &report,
                )
                .await?;
            }
            if let Some(password) = secrets.take_root_password() {
                push_log!("Setting root password...".to_string());
                for warning in host
                    .rootfs
                    .set_root_password(&actual_rootfs_target, password)
                    .await?
                {
                    log::warn!("{}", warning);
                    push_log!(warning);
                }
                cancellation.checkpoint()?;
            }

            let mut users = cfg.users.iter().collect::<Vec<_>>();
            users.sort_by_key(|user| user.uid.is_none());
            for user in users {
                push_log!(format!("Creating user {}...", user.username));
                let password = secrets
                    .take_user_password(&user.username)
                    .map_err(|error| NspawnError::Validation(error.to_string()))?;
                for warning in host
                    .rootfs
                    .create_user(&actual_rootfs_target, user, password)
                    .await?
                {
                    log::warn!("{}", warning);
                    push_log!(warning);
                }
                cancellation.checkpoint()?;
            }
        } else if !has_os_layout {
            log::warn!("[AUDIT] [Container: {}] rootfs OS layout could not be verified. Skipping internal modifications.", name);
            push_log!("WARNING: Could not verify the rootfs OS layout. Skipping guest hostname, passwords, and user creation.".to_string());
        } else if has_account_changes {
            log::warn!("[AUDIT] [Container: {}] rootfs has no /usr tree required by systemd-nspawn offline commands. Skipping account modifications.", name);
            push_log!("WARNING: This rootfs has no /usr tree required by systemd-nspawn; skipping password and user creation.".to_string());
        }

        let mut resolved_wayland = Vec::with_capacity(wayland_intents.len());
        for intent in wayland_intents {
            if !supports_offline_commands {
                return Err(NspawnError::Validation(
                    "Wayland grant requires a rootfs that supports user identity lookup".into(),
                ));
            }
            let user = cfg
                .users
                .iter()
                .find(|user| user.username == intent.target_username())
                .ok_or_else(|| {
                    NspawnError::Validation(
                        "Wayland target is not part of this deployment".into(),
                    )
                })?;
            push_log!(format!(
                "Resolving Wayland target identity for {}...",
                user.username
            ));
            let identity = host
                .rootfs
                .resolve_user_identity(&actual_rootfs_target, &user.username)
                .await?;
            let grant = crate::application::provisioning::resolve_wayland_grant(
                intent,
                identity,
                cfg.private_users,
            )
            .map_err(|error| NspawnError::Validation(error.to_string()))?;
            host.nspawn.validate_wayland(&cfg, &grant).await?;
            push_log!(format!(
                "Setting up {} Wayland display(s) for {}...",
                grant.sockets().len(),
                user.username,
            ));
            host
                .rootfs
                .configure_wayland(
                    &actual_rootfs_target,
                    grant.target(),
                    &user.shell,
                    grant.default_display(),
                )
                .await?;
            cancellation.checkpoint()?;
            resolved_wayland.push(grant);
        }
        if supports_offline_commands && has_account_changes {
            report.record_typed(
                DeploymentResource::RootfsAccounts(target.clone()),
                ResourceDisposition::Committed,
            );
            persist_committed(&job, DeploymentStage::RootfsMutation, &report).await?;
        }

        let mut initial_nvidia_state = None;

        if cfg.nvidia_gpu {
            push_log!("Assembling initial NVIDIA GPU configuration...".to_string());

            // Run initial CDI discovery to seed the .nspawn config and state.
            // Remapping is applied inside get_nvidia_state after CDI + ldconfig collection.
            let state = crate::adapters::platform::nvidia::get_nvidia_state(
                nvidia_profile.as_ref(),
            )
            .await
            .map_err(|error| {
                NspawnError::Runtime(format!("NVIDIA CDI discovery failed: {error}"))
            })?;
            cancellation.checkpoint()?;

            // Persist the validated snapshot and its profile for lifecycle diffing.
            persist_applying(
                &job,
                DeploymentStage::HostConfiguration,
                vec![DeploymentResource::NvidiaState(target.clone())],
                &report,
            )
            .await?;
            let state_apply = host.nvidia_state.write_initial(&name, &state).await?;
            report.record_apply(AppliedResource::NvidiaState, state_apply)?;
            persist_committed(&job, DeploymentStage::HostConfiguration, &report).await?;
            match state_apply {
                ApplyStatus::ReplacedOwned => {
                    let warning = "Replaced existing Lasper-owned NVIDIA state for this deployment.";
                    log::warn!("[AUDIT] [Container: {}] [Step: NVIDIA] {}", name, warning);
                    push_log!(format!("WARNING: {warning}"));
                }
                ApplyStatus::ConflictUnknownOwner => {
                    let warning = "Preserved existing NVIDIA state because Lasper could not prove ownership; automatic NVIDIA lifecycle updates may use stale state.";
                    log::warn!("[AUDIT] [Container: {}] [Step: NVIDIA] {}", name, warning);
                    push_log!(format!("WARNING: {warning}"));
                }
                ApplyStatus::Created | ApplyStatus::Unchanged => {}
            }
            cancellation.checkpoint()?;

            // Write ld.so.conf.d and env vars into rootfs (one-time setup)
            if supports_offline_commands {
                let rootfs_nvidia = DeploymentResource::RootfsNvidia(target.clone());
                persist_applying(
                    &job,
                    DeploymentStage::RootfsMutation,
                    vec![rootfs_nvidia.clone()],
                    &report,
                )
                .await?;
                for warning in crate::adapters::platform::nvidia::lifecycle::inject_env_once(
                    &actual_rootfs_target,
                    &state,
                    &host.rootfs,
                )
                .await?
                {
                    log::warn!("{}", warning);
                    push_log!(warning);
                }
                report.record_typed(rootfs_nvidia, ResourceDisposition::Committed);
                persist_committed(&job, DeploymentStage::RootfsMutation, &report).await?;
            } else if has_os_layout {
                push_log!("WARNING: Skipping NVIDIA env/ldconfig injection because this rootfs cannot run systemd-nspawn offline commands.".to_string());
            } else {
                push_log!("WARNING: Skipping NVIDIA env/ldconfig injection because the rootfs OS layout could not be verified.".to_string());
            }
            initial_nvidia_state = Some(state);
            cancellation.checkpoint()?;
        }

        if cfg.private_users == Some(crate::nspawn::models::PrivateUsersMode::No) {
            log::warn!("[AUDIT] [Container: {}] [Security] PrivateUsers=no, user namespacing disabled.", name);
            push_log!("WARNING: PrivateUsers=no, user namespacing disabled.".to_string());
        }

        if cfg.privileged {
            log::warn!("[AUDIT] [Container: {}] [Security: Dangerous] Privileged mode enabled. Capability=all granted.", name);
            push_log!("DANGER: Privileged mode enabled (Capability=all).".to_string());
        }

        push_log!("Writing .nspawn config...".to_string());
        persist_applying(
            &job,
            DeploymentStage::RuntimeCommit,
            vec![DeploymentResource::NspawnConfig(target.clone())],
            &report,
        )
        .await?;
        let nspawn_apply = host
            .nspawn
            .write_generated(
                &cfg,
                &resolved_wayland,
                initial_nvidia_state.as_ref(),
            )
            .await?;
        report.record_apply(AppliedResource::NspawnConfig, nspawn_apply)?;
        persist_committed(&job, DeploymentStage::RuntimeCommit, &report).await?;
        cancellation.checkpoint()?;

        if !cfg.device_binds.is_empty() || cfg.gpu_passthrough_all {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Config] Writing systemd service override...",
                name
            );
            push_log!("Writing systemd service override...".to_string());
            persist_applying(
                &job,
                DeploymentStage::RuntimeCommit,
                vec![DeploymentResource::SystemdOverride(target.clone())],
                &report,
            )
            .await?;
            let override_apply = host
                .systemd_unit
                .write_override(
                    &name,
                    &cfg.device_binds,
                    cfg.gpu_passthrough_all,
                )
                .await?;
            report.record_apply(AppliedResource::SystemdOverride, override_apply)?;
            persist_committed(&job, DeploymentStage::RuntimeCommit, &report).await?;
            match override_apply {
                ApplyStatus::ReplacedOwned => {
                    let warning = "Replaced an existing Lasper-owned systemd service drop-in for this deployment.";
                    log::warn!("[AUDIT] [Container: {}] [Step: Config] {}", name, warning);
                    push_log!(format!("WARNING: {warning}"));
                }
                ApplyStatus::ConflictUnknownOwner => {
                    let warning = "Preserved the existing systemd service drop-in because Lasper could not prove ownership; requested device allowances were not written there.";
                    log::warn!("[AUDIT] [Container: {}] [Step: Config] {}", name, warning);
                    push_log!(format!("WARNING: {warning}"));
                }
                ApplyStatus::Created | ApplyStatus::Unchanged => {}
            }

            system_operations.reload_daemon().await?;
            cancellation.checkpoint()?;
        }

        if supports_offline_commands {
            if let Some(mode) = &cfg.network {
                if mode.uses_default_guest_network_stack() {
                    push_log!(
                        "Enabling container network and DNS services (systemd-networkd, systemd-resolved)..."
                            .to_string()
                    );
                    let rootfs_network = DeploymentResource::RootfsNetwork(target.clone());
                    persist_applying(
                        &job,
                        DeploymentStage::RootfsMutation,
                        vec![rootfs_network.clone()],
                        &report,
                    )
                    .await?;
                    let configured = match host.rootfs.configure_network(&actual_rootfs_target).await {
                        Ok(warnings) => {
                            for warning in warnings {
                                log::warn!("{}", warning);
                                push_log!(warning);
                            }
                            true
                        }
                        Err(error) => return Err(error),
                    };
                    if configured {
                        report.record_typed(rootfs_network, ResourceDisposition::Committed);
                    }
                    persist_committed(&job, DeploymentStage::RootfsMutation, &report).await?;
                    cancellation.checkpoint()?;
                }
            }
        }
        cancellation.checkpoint()?;
        Ok::<(), NspawnError>(())
    }
    .await;

    if result.is_err() {
        capture_uncommitted_effects(&job, &mut report).await;
    }

    if let Err(NspawnError::DeploymentProcessStateUnknown(message)) = &result {
        let warning = format!(
            "could not safely clean up deployment {name:?}: {message}; mounts and resources were preserved for manual inspection"
        );
        log::error!("[DEPLOY] {warning}");
        push_log!(format!("FATAL: {warning}"));
        if let Err(state_error) = persist_cleanup_pending(&job, &report).await {
            log::error!("[DEPLOY] could not persist cleanup-pending state: {state_error}");
        }
        return Err(NspawnError::DeploymentProcessStateUnknown(message.clone()));
    }

    let mut cleanup_errors = Vec::new();
    let mut durable_cleanup_failed = false;
    if let Some(target) = raw_mount_target {
        push_log!("Unmounting raw image...".to_string());
        let resource = DeploymentResource::RawConfigurationMount(report.target.clone());
        match persist_applying(
            &job,
            DeploymentStage::Cleanup,
            vec![resource.clone()],
            &report,
        )
        .await
        {
            Ok(()) => match host.rootfs.unmount_managed_raw(&target).await {
                Ok(()) => {
                    report.remove_typed(&resource);
                    if let Err(error) =
                        persist_committed(&job, DeploymentStage::Cleanup, &report).await
                    {
                        let message = format!("raw mount cleanup state: {error}");
                        durable_cleanup_failed = true;
                        report.record_typed(resource, ResourceDisposition::CleanupPending);
                        report.block_storage_removal(message.clone());
                        cleanup_errors.push(message);
                    }
                }
                Err(error) => {
                    let message = format!("raw image configuration mount: {error}");
                    log::warn!("Failed to clean up {message}");
                    report.record_typed(resource, ResourceDisposition::CleanupPending);
                    report.block_storage_removal(message.clone());
                    cleanup_errors.push(message);
                }
            },
            Err(error) => {
                let message = format!("raw mount cleanup was not attempted: {error}");
                durable_cleanup_failed = true;
                report.record_typed(resource, ResourceDisposition::CleanupPending);
                report.block_storage_removal(message.clone());
                cleanup_errors.push(message);
            }
        }
    }

    if storage_mount_attempted {
        push_log!("Cleaning up storage mount...".to_string());
        let resource = DeploymentResource::StorageMount(report.target.clone());
        if durable_cleanup_failed {
            let message =
                "storage cleanup was not attempted after durable cleanup state failed".to_string();
            report.record_typed(resource, ResourceDisposition::CleanupPending);
            report.block_storage_removal(message.clone());
            cleanup_errors.push(message);
        } else {
            match persist_applying(
                &job,
                DeploymentStage::Cleanup,
                vec![resource.clone()],
                &report,
            )
            .await
            {
                Ok(()) => match storage.unmount(&name).await {
                    Ok(()) => {
                        report.remove_typed(&resource);
                        if let Err(error) =
                            persist_committed(&job, DeploymentStage::Cleanup, &report).await
                        {
                            let message = format!("storage cleanup state: {error}");
                            durable_cleanup_failed = true;
                            report.record_typed(resource, ResourceDisposition::CleanupPending);
                            report.block_storage_removal(message.clone());
                            cleanup_errors.push(message);
                        }
                    }
                    Err(error) => {
                        let message = format!("storage unmount: {error}");
                        log::warn!("Failed to clean up {message}");
                        report.record_typed(resource, ResourceDisposition::CleanupPending);
                        report.block_storage_removal(message.clone());
                        cleanup_errors.push(message);
                    }
                },
                Err(error) => {
                    let message = format!("storage cleanup was not attempted: {error}");
                    durable_cleanup_failed = true;
                    report.record_typed(resource, ResourceDisposition::CleanupPending);
                    report.block_storage_removal(message.clone());
                    cleanup_errors.push(message);
                }
            }
        }
    }

    let result = if result.is_ok() && cancellation.is_requested() {
        Err(NspawnError::DeploymentCancelled)
    } else if result.is_ok() && !cleanup_errors.is_empty() {
        Err(NspawnError::Runtime(
            "Deployment cleanup failed before completion".into(),
        ))
    } else {
        result
    };

    if let Err(error) = result {
        push_log!(format!("Deployment stopped: {}", error));
        job.set_rolling_back(true);
        push_log!("Rolling back resources created by this deployment...".to_string());

        let external_ownership_confirmed = report.owns(AppliedResource::ExternalImage);
        let mut rollback_errors = cleanup_errors;
        if durable_cleanup_failed {
            rollback_errors
                .push("rollback was not attempted after durable cleanup state failed".into());
        } else {
            rollback_errors.extend(
                rollback_apply_report(&name, &mut report, storage.as_ref(), &host, &logs, &job)
                    .await,
            );
        }

        if external_provider_started && !external_ownership_confirmed {
            let warning = format!(
                "external provider did not confirm ownership of image {name:?}; any partial provider output was preserved for manual inspection"
            );
            log::warn!("{warning}");
            push_log!(format!("WARNING: {warning}"));
        }

        for resource in report.outcome_unknown_resources() {
            rollback_errors.push(format!(
                "{} outcome requires authoritative reconciliation",
                resource.label()
            ));
        }

        job.set_rolling_back(false);
        if rollback_errors.is_empty() {
            push_log!("Rollback complete.".to_string());
            if let Err(manifest_error) = finish_manifest(&job).await {
                let message = format!("deployment crash manifest: {manifest_error}");
                push_log!(format!("ROLLBACK ERROR: {message}"));
                return if matches!(error, NspawnError::DeploymentCancelled) {
                    Err(NspawnError::DeploymentCancellationRollbackIncomplete(
                        message,
                    ))
                } else {
                    Err(NspawnError::DeploymentRollbackIncomplete(format!(
                        "{error}; rollback completed but durable state cleanup failed: {message}"
                    )))
                };
            }
            return Err(error);
        }

        if let Err(state_error) = persist_cleanup_pending(&job, &report).await {
            rollback_errors.push(format!("deployment cleanup state: {state_error}"));
        }

        for rollback_error in &rollback_errors {
            push_log!(format!("ROLLBACK ERROR: {rollback_error}"));
        }
        let rollback_errors = rollback_errors.join("; ");
        return if matches!(error, NspawnError::DeploymentCancelled) {
            Err(NspawnError::DeploymentCancellationRollbackIncomplete(
                rollback_errors,
            ))
        } else {
            Err(NspawnError::DeploymentRollbackIncomplete(format!(
                "{error}; rollback incomplete: {rollback_errors}"
            )))
        };
    }

    push_log!("");
    push_log!("=== Deployment Complete ===".to_string());
    Ok(())
}
