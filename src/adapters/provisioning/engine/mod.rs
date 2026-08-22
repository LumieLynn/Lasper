//! Deployment trait and orchestrator.

pub(crate) mod bootstrap_operation;
pub mod builders;
pub(crate) mod image_operation;
pub(crate) mod oci_operation;
mod tar_limits;

pub use bootstrap_operation::BootstrapStore;
pub use image_operation::ImageImportStore;
pub use oci_operation::OciPullStore;

use crate::adapters::config::{NspawnConfigStore, SystemdUnitStore};
use crate::adapters::platform::nvidia::NvidiaStateStore;
use crate::adapters::rootfs::RootfsStore;
use crate::adapters::storage::StorageBackend;
use crate::adapters::system_operation::SystemOperationStore;
pub(crate) use crate::application::provisioning::{
    DeploymentCancellation, DeploymentEvent as DeployLogEvent, DeploymentProgress as DeployProgress,
};
use crate::application::provisioning::{DeploymentJobContext, DeploymentSecrets};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ApplyStatus, ContainerConfig};
use tokio::sync::mpsc::Sender;

/// Concrete host capabilities still used by the legacy deployment pipeline.
///
/// Keeping this workflow-specific bundle here prevents the pipeline from
/// reaching through the process-wide `ExecutionContext`. Each field will be
/// replaced by its consumer-owned port as the corresponding stage migrates.
#[derive(Clone)]
pub(crate) struct DeploymentHost {
    pub(crate) system_operations: SystemOperationStore,
    pub(crate) nspawn: NspawnConfigStore,
    pub(crate) systemd_unit: SystemdUnitStore,
    pub(crate) rootfs: RootfsStore,
    pub(crate) nvidia_state: NvidiaStateStore,
}

impl DeploymentHost {
    pub(crate) fn new(
        system_operations: SystemOperationStore,
        nspawn: NspawnConfigStore,
        systemd_unit: SystemdUnitStore,
        rootfs: RootfsStore,
        nvidia_state: NvidiaStateStore,
    ) -> Self {
        Self {
            system_operations,
            nspawn,
            systemd_unit,
            rootfs,
            nvidia_state,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppliedResource {
    LocalStorage,
    ExternalImage,
    NvidiaState,
    NspawnConfig,
    SystemdOverride,
}

impl AppliedResource {
    fn label(self) -> &'static str {
        match self {
            Self::LocalStorage => "local storage",
            Self::ExternalImage => "external image",
            Self::NvidiaState => "NVIDIA state",
            Self::NspawnConfig => ".nspawn configuration",
            Self::SystemdOverride => "systemd service override",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ApplyReport {
    resources: Vec<AppliedResource>,
    external_image_blockers: Vec<String>,
    storage_removal_blockers: Vec<String>,
}

impl ApplyReport {
    pub(crate) fn record_created(&mut self, resource: AppliedResource) {
        if !self.resources.contains(&resource) {
            self.resources.push(resource);
        }
    }

    pub(crate) fn record_apply(
        &mut self,
        resource: AppliedResource,
        status: ApplyStatus,
    ) -> Result<()> {
        match status {
            ApplyStatus::Created => {
                self.record_created(resource);
                Ok(())
            }
            ApplyStatus::Unchanged => {
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("an unchanged .nspawn configuration predates this deployment".into());
                }
                Ok(())
            }
            ApplyStatus::ReplacedOwned => {
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("a replaced .nspawn configuration cannot be restored".into());
                    return Err(NspawnError::Runtime(format!(
                        "Deployment cannot roll back replaced {} without its previous content",
                        resource.label()
                    )));
                }
                // A create intent may adopt and replace a stale, proven-owned
                // sidecar. Rollback removes the replacement instead of
                // restoring stale state for a target that was not deployed.
                self.record_created(resource);
                Ok(())
            }
            ApplyStatus::ConflictUnknownOwner => {
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("an existing .nspawn configuration has unknown ownership".into());
                    return Err(NspawnError::Validation(format!(
                        "Refusing to replace existing {} with unknown ownership",
                        resource.label()
                    )));
                }
                // Auxiliary state is optional. Preserve the unknown file and
                // let the caller surface the degraded result as a warning.
                Ok(())
            }
        }
    }

    fn owns(&self, resource: AppliedResource) -> bool {
        self.resources.contains(&resource)
    }

    fn block_storage_removal(&mut self, reason: impl Into<String>) {
        self.storage_removal_blockers.push(reason.into());
    }

    pub(crate) fn block_external_image_removal(&mut self, reason: impl Into<String>) {
        self.external_image_blockers.push(reason.into());
    }

    fn removal_blockers(&self, resource: AppliedResource) -> Vec<&str> {
        let external = if resource == AppliedResource::ExternalImage {
            self.external_image_blockers.as_slice()
        } else {
            &[]
        };
        external
            .iter()
            .chain(&self.storage_removal_blockers)
            .map(String::as_str)
            .collect()
    }
}

#[async_trait::async_trait]
pub trait Deployer: Send + Sync {
    /// Performs the actual deployment (bootstrapping / cloning) of the container.
    async fn deploy(
        &self,
        name: &str,
        cfg: &ContainerConfig,
        rootfs: &std::path::Path,
        logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
        cancellation: &DeploymentCancellation,
        report: &mut ApplyReport,
    ) -> Result<()>;

    /// Returns true if this deployer manages its own storage (e.g. machinectl clone).
    fn is_external_storage_managed(&self) -> bool {
        false
    }

    /// Returns true if this deployer requires post-deployment configuration (passwords, etc).
    /// Default is true. Clones might set this to false if they are already configured.
    fn requires_post_config(&self) -> bool {
        true
    }
}

pub(crate) async fn send_deploy_log(logs: &Sender<DeployLogEvent>, message: impl Into<String>) {
    let message = message.into();
    log::info!("[DEPLOY] {}", message);
    let _ = logs.send(DeployLogEvent::Line(message)).await;
}

pub(crate) async fn send_deploy_stream_log(
    logs: &Sender<DeployLogEvent>,
    message: impl Into<String>,
) {
    let message = message.into();
    if is_high_signal_deploy_stream(&message) {
        log::warn!("[DEPLOY stream] {}", message);
    } else {
        // Deployment output must survive the wizard and be available in the
        // normal per-run log without requiring RUST_LOG=debug.
        log::info!("[DEPLOY stream] {}", message);
    }
    let _ = logs.send(DeployLogEvent::Line(message)).await;
}

fn is_high_signal_deploy_stream(message: &str) -> bool {
    let message = message.trim_start().to_ascii_lowercase();
    ["w:", "e:", "warning:", "error:", "fatal:"]
        .iter()
        .any(|prefix| message.starts_with(prefix))
        || [
            "permission denied",
            "operation not permitted",
            "failed",
            "failure",
        ]
        .iter()
        .any(|marker| message.contains(marker))
}

pub(crate) async fn send_deploy_progress(
    logs: &Sender<DeployLogEvent>,
    label: impl Into<String>,
    permille: u16,
) {
    let progress = DeployProgress::new(label, permille);
    log::trace!(
        "[DEPLOY progress] {}: {}.{:01}%",
        progress.label,
        progress.permille / 10,
        progress.permille % 10
    );
    let _ = logs.send(DeployLogEvent::Progress(progress)).await;
}

pub(crate) async fn stream_deploy_command(
    mut spawned: crate::adapters::process::SpawnedProcess,
    logs: &Sender<DeployLogEvent>,
    cancellation: &DeploymentCancellation,
    label: &str,
) -> Result<std::process::ExitStatus> {
    use tokio::io::AsyncBufReadExt;

    let mut cancelled = false;
    let mut stream_error = None;
    {
        let mut lines = tokio::io::BufReader::new(&mut spawned.stdout).lines();
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    cancelled = true;
                    break;
                }
                line = lines.next_line() => {
                    match line {
                        Ok(Some(line)) => send_deploy_stream_log(logs, line).await,
                        Ok(None) => break,
                        Err(error) => {
                            stream_error = Some(error);
                            break;
                        }
                    }
                }
            }
        }
    }

    if cancelled || cancellation.is_requested() {
        send_deploy_log(logs, format!("Stopping {label}...")).await;
        let completion_wins = spawned.completion_wins_cancellation();
        let status = spawned
            .terminate_and_wait()
            .await
            .map_err(|error| process_state_unknown(label, error))?;
        if completion_wins && status.success() {
            return Ok(status);
        }
        return Err(NspawnError::DeploymentCancelled);
    }

    if let Some(error) = stream_error {
        send_deploy_log(
            logs,
            format!("Stopping {label} after its output stream failed..."),
        )
        .await;
        spawned
            .terminate_and_wait()
            .await
            .map_err(|wait_error| process_state_unknown(label, wait_error))?;
        return Err(NspawnError::Io(std::path::PathBuf::from(label), error));
    }

    spawned
        .wait()
        .await
        .map_err(|error| process_state_unknown(label, error))
}

pub(crate) fn process_state_unknown(label: &str, error: std::io::Error) -> NspawnError {
    NspawnError::DeploymentProcessStateUnknown(format!(
        "could not confirm that {label} exited: {error}"
    ))
}

/// Runs one deployment using application-owned job state and event transport.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_deployment(
    deployer: Box<dyn Deployer>,
    storage: Box<dyn StorageBackend>,
    name: String,
    cfg: ContainerConfig,
    nvidia_profile: Option<crate::domain::nvidia::NvidiaPassthroughProfile>,
    wayland_intents: Vec<crate::domain::wayland::WaylandGrantIntent>,
    host: DeploymentHost,
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
    host: DeploymentHost,
    mut secrets: DeploymentSecrets,
    job: DeploymentJobContext,
) -> Result<()> {
    let logs = job.event_sender();
    let cancellation = job.cancellation();
    crate::nspawn::models::NspawnConfigSpec::try_from(&cfg)?;
    let system_operations = host.system_operations.clone();

    macro_rules! push_log {
        ($msg:expr) => {
            send_deploy_log(&logs, $msg).await;
        };
    }

    push_log!(format!("=== Deploying '{}' ===", name));

    let is_ext = deployer.is_external_storage_managed();
    let mut report = ApplyReport::default();
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
            storage.create(&name).await?;
            report.record_created(AppliedResource::LocalStorage);
            cancellation.checkpoint()?;
        }

        let rootfs = if !is_ext {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Storage] Mounting storage tree...",
                name
            );
            push_log!("Mounting storage...".to_string());
            storage_mount_attempted = true;
            let rootfs = storage.mount(&name).await?;
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
            match host.rootfs.mount_managed_raw(&name).await {
                Ok(Some(target)) => {
                    actual_rootfs_target = target.clone();
                    raw_mount_target = Some(target);
                }
                Ok(None) => {}
                Err(error) => {
                    push_log!(format!(
                        "WARNING: Failed to mount raw image with systemd-dissect: {}",
                        error
                    ));
                }
            }
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

        let has_account_changes = secrets.has_account_changes();
        if supports_offline_commands {
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
            push_log!("WARNING: Could not verify the rootfs OS layout. Skipping passwords and user creation.".to_string());
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
            let state_apply = host.nvidia_state.write_initial(&name, &state).await?;
            report.record_apply(AppliedResource::NvidiaState, state_apply)?;
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
                match crate::adapters::platform::nvidia::lifecycle::inject_env_once(
                    &actual_rootfs_target,
                    &state,
                    &host.rootfs,
                )
                .await
                {
                    Ok(warnings) => {
                        for warning in warnings {
                            log::warn!("{}", warning);
                            push_log!(warning);
                        }
                    }
                    Err(error) => {
                        push_log!(format!(
                            "WARNING: Failed to inject NVIDIA env/ldconfig: {}",
                            error
                        ));
                    }
                }
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
        let nspawn_apply = host
            .nspawn
            .write_generated(
                &cfg,
                &resolved_wayland,
                initial_nvidia_state.as_ref(),
            )
            .await?;
        report.record_apply(AppliedResource::NspawnConfig, nspawn_apply)?;
        cancellation.checkpoint()?;

        if !cfg.device_binds.is_empty() || cfg.gpu_passthrough_all {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Config] Writing systemd service override...",
                name
            );
            push_log!("Writing systemd service override...".to_string());
            let override_apply = host
                .systemd_unit
                .write_override(
                    &name,
                    &cfg.device_binds,
                    cfg.gpu_passthrough_all,
                )
                .await?;
            report.record_apply(AppliedResource::SystemdOverride, override_apply)?;
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
                    match host.rootfs.configure_network(&actual_rootfs_target).await {
                        Ok(warnings) => {
                            for warning in warnings {
                                log::warn!("{}", warning);
                                push_log!(warning);
                            }
                        }
                        Err(error) => {
                            push_log!(format!(
                                "WARNING: {} (might not be a systemd container)",
                                error
                            ));
                        }
                    }
                    cancellation.checkpoint()?;
                }
            }
        }
        cancellation.checkpoint()?;
        Ok::<(), NspawnError>(())
    }
    .await;

    if let Err(NspawnError::DeploymentProcessStateUnknown(message)) = &result {
        let warning = format!(
            "could not safely clean up deployment {name:?}: {message}; mounts and resources were preserved for manual inspection"
        );
        log::error!("[DEPLOY] {warning}");
        push_log!(format!("FATAL: {warning}"));
        return Err(NspawnError::DeploymentProcessStateUnknown(message.clone()));
    }

    let mut cleanup_errors = Vec::new();
    if let Some(target) = raw_mount_target {
        push_log!("Unmounting raw image...".to_string());
        if let Err(error) = host.rootfs.unmount_managed_raw(&target).await {
            let message = format!("raw image configuration mount: {error}");
            log::warn!("Failed to clean up {message}");
            report.block_storage_removal(message.clone());
            cleanup_errors.push(message);
        }
    }

    if storage_mount_attempted {
        push_log!("Cleaning up storage mount...".to_string());
        if let Err(error) = storage.unmount(&name).await {
            let message = format!("storage unmount: {error}");
            log::warn!("Failed to clean up {message}");
            report.block_storage_removal(message.clone());
            cleanup_errors.push(message);
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
        rollback_errors.extend(
            rollback_apply_report(&name, &mut report, storage.as_ref(), &host, &logs).await,
        );

        if external_provider_started && !external_ownership_confirmed {
            let warning = format!(
                "external provider did not confirm ownership of image {name:?}; any partial provider output was preserved for manual inspection"
            );
            log::warn!("{warning}");
            push_log!(format!("WARNING: {warning}"));
        }

        job.set_rolling_back(false);
        if rollback_errors.is_empty() {
            push_log!("Rollback complete.".to_string());
            return Err(error);
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
            Err(NspawnError::DeployError(format!(
                "{error}; rollback incomplete: {rollback_errors}"
            )))
        };
    }

    push_log!("");
    push_log!("=== Deployment Complete ===".to_string());
    Ok(())
}

async fn inspect_deployment_sidecars(name: &str, host: &DeploymentHost) -> Result<Vec<String>> {
    if let Some(config) = host.nspawn.inspect(name).await? {
        return Err(NspawnError::Validation(format!(
            "Deployment target has existing .nspawn configuration: {}",
            config.path.display()
        )));
    }
    let mut warnings = Vec::new();
    match host.nvidia_state.read(name).await {
        Ok(Some(_)) => warnings.push(format!(
            "Deployment target {name:?} has existing NVIDIA state; it will be replaced only when current Lasper ownership can be proven."
        )),
        Ok(None) => {}
        Err(error) => warnings.push(format!(
            "Existing NVIDIA state could not be safely inspected and will be preserved: {error}"
        )),
    }

    match host.systemd_unit.read(name).await {
        Ok(unit) => {
            for drop_in in unit.drop_ins {
                let file_name = std::path::Path::new(&drop_in.path)
                    .file_name()
                    .and_then(|name| name.to_str());
                if file_name == Some("90-lasper.conf") {
                    warnings.push(format!(
                        "Existing systemd service drop-in {} will be replaced only when current Lasper ownership can be proven.",
                        drop_in.path
                    ));
                } else {
                    warnings.push(format!(
                        "Existing systemd service drop-in {} will be preserved.",
                        drop_in.path
                    ));
                }
            }
        }
        Err(error) => warnings.push(format!(
            "Existing systemd service drop-ins could not be safely inspected and will be preserved unless a validated Lasper-owned target is updated: {error}"
        )),
    }
    Ok(warnings)
}

async fn rollback_apply_report(
    name: &str,
    report: &mut ApplyReport,
    storage: &dyn StorageBackend,
    host: &DeploymentHost,
    logs: &Sender<DeployLogEvent>,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut reload_daemon = false;

    while let Some(resource) = report.resources.pop() {
        send_deploy_log(logs, format!("Rolling back {}...", resource.label())).await;
        let result = match resource {
            AppliedResource::SystemdOverride => {
                reload_daemon = true;
                host.systemd_unit.remove_service_override(name).await
            }
            AppliedResource::NspawnConfig => host.nspawn.remove(name).await,
            AppliedResource::NvidiaState => host.nvidia_state.remove(name).await,
            AppliedResource::ExternalImage => {
                let blockers = report.removal_blockers(resource);
                if blockers.is_empty() {
                    host.system_operations.remove_image(name).await
                } else {
                    Err(NspawnError::Runtime(format!(
                        "external image removal blocked: {}",
                        blockers.join("; ")
                    )))
                }
            }
            AppliedResource::LocalStorage => {
                let blockers = report.removal_blockers(resource);
                if blockers.is_empty() {
                    storage.delete(name).await
                } else {
                    Err(NspawnError::Runtime(format!(
                        "local storage removal blocked: {}",
                        blockers.join("; ")
                    )))
                }
            }
        };
        if let Err(error) = result {
            errors.push(format!("{}: {error}", resource.label()));
        }
    }

    if reload_daemon {
        if let Err(error) = host.system_operations.reload_daemon().await {
            errors.push(format!("systemd daemon reload: {error}"));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_stream_classifies_recoverable_bootstrap_diagnostics() {
        for message in [
            "W: Failure trying to run: test-dev-null",
            "test-dev-null: Permission denied",
            "E: bootstrap failed",
            "operation not permitted while probing target",
        ] {
            assert!(
                is_high_signal_deploy_stream(message),
                "did not classify {message:?}"
            );
        }
    }

    #[test]
    fn deploy_stream_distinguishes_normal_output_from_warnings() {
        for message in [
            "I: Retrieving base-files",
            "I: Extracting base-passwd",
            "Download complete",
        ] {
            assert!(
                !is_high_signal_deploy_stream(message),
                "misclassified {message:?}"
            );
        }
    }

    #[test]
    fn apply_report_records_only_resources_created_by_this_attempt() {
        let mut report = ApplyReport::default();
        report
            .record_apply(AppliedResource::NspawnConfig, ApplyStatus::Created)
            .unwrap();
        report
            .record_apply(AppliedResource::NspawnConfig, ApplyStatus::Created)
            .unwrap();
        report
            .record_apply(AppliedResource::SystemdOverride, ApplyStatus::Unchanged)
            .unwrap();

        assert_eq!(report.resources, vec![AppliedResource::NspawnConfig]);
    }

    #[test]
    fn unknown_nspawn_owner_blocks_external_image_compensation() {
        let mut report = ApplyReport::default();
        let error = report
            .record_apply(
                AppliedResource::NspawnConfig,
                ApplyStatus::ConflictUnknownOwner,
            )
            .unwrap_err();

        assert!(error.to_string().contains("unknown ownership"));
        assert!(report.resources.is_empty());
        assert_eq!(report.external_image_blockers.len(), 1);
    }

    #[test]
    fn sidecar_conflicts_are_preserved_and_owned_replacements_are_adopted() {
        let mut report = ApplyReport::default();
        report
            .record_apply(
                AppliedResource::NvidiaState,
                ApplyStatus::ConflictUnknownOwner,
            )
            .unwrap();
        report
            .record_apply(AppliedResource::SystemdOverride, ApplyStatus::ReplacedOwned)
            .unwrap();

        assert_eq!(report.resources, vec![AppliedResource::SystemdOverride]);
        assert!(report.external_image_blockers.is_empty());
    }

    #[test]
    fn failed_unmount_blocks_local_and_external_storage_compensation() {
        let mut report = ApplyReport::default();
        report
            .external_image_blockers
            .push("unknown .nspawn owner".into());
        report.block_storage_removal("storage is still mounted");

        assert_eq!(
            report.removal_blockers(AppliedResource::LocalStorage),
            vec!["storage is still mounted"]
        );
        assert_eq!(
            report.removal_blockers(AppliedResource::ExternalImage),
            vec!["unknown .nspawn owner", "storage is still mounted"]
        );
    }

    #[tokio::test]
    async fn deployment_cancellation_notifies_waiters_and_fails_checkpoints() {
        let cancellation = DeploymentCancellation::default();
        let waiter = cancellation.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });

        cancellation.request();
        task.await.unwrap();
        let result: Result<()> = cancellation.checkpoint().map_err(Into::into);
        assert!(matches!(result, Err(NspawnError::DeploymentCancelled)));
    }

    #[tokio::test]
    async fn failed_process_wait_is_not_treated_as_a_rollback_safe_failure() {
        let spawned = crate::adapters::process::SpawnedProcess::new_cancellable(
            Box::new(tokio::io::empty()),
            async { Err(std::io::Error::other("wait channel closed")) },
            |_| Box::pin(async { Ok(()) }),
        );
        let (logs, _receiver) = tokio::sync::mpsc::channel(4);

        let error = stream_deploy_command(
            spawned,
            &logs,
            &DeploymentCancellation::default(),
            "test deployer",
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            NspawnError::DeploymentProcessStateUnknown(message)
                if message.contains("test deployer") && message.contains("wait channel closed")
        ));
    }

    #[tokio::test]
    async fn authoritative_completion_wins_a_racing_cancellation() {
        use std::os::unix::process::ExitStatusExt;

        let spawned = crate::adapters::process::SpawnedProcess::new_cancellable(
            Box::new(tokio::io::empty()),
            async { Ok(std::process::ExitStatus::from_raw(0)) },
            |_| Box::pin(async { Ok(()) }),
        )
        .with_completion_wins_cancellation();
        let cancellation = DeploymentCancellation::default();
        cancellation.request();
        let (logs, _receiver) = tokio::sync::mpsc::channel(4);

        let status = stream_deploy_command(spawned, &logs, &cancellation, "authoritative transfer")
            .await
            .unwrap();

        assert!(status.success());
    }

    #[test]
    fn only_confirmed_cancellation_outcomes_are_reported_as_cancelled() {
        assert!(is_cancelled_outcome(&NspawnError::DeploymentCancelled));
        assert!(is_cancelled_outcome(
            &NspawnError::DeploymentCancellationRollbackIncomplete("cleanup failed".into())
        ));
        assert!(!is_cancelled_outcome(
            &NspawnError::DeploymentProcessStateUnknown("still running".into())
        ));
    }
}
