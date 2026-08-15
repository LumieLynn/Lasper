//! Deployment trait and orchestrator.

pub(crate) mod bootstrap_operation;
pub mod builders;
pub(crate) mod image_operation;
pub(crate) mod oci_operation;

pub use bootstrap_operation::BootstrapStore;
pub use image_operation::ImageImportStore;
pub use oci_operation::OciPullStore;

use crate::events::AppEvent;
use crate::nspawn::adapters::storage::StorageBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ApplyStatus, ContainerConfig, NetworkMode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployProgress {
    pub label: String,
    pub permille: u16,
}

impl DeployProgress {
    pub fn new(label: impl Into<String>, permille: u16) -> Self {
        Self {
            label: label.into(),
            permille: permille.min(1000),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeployLogEvent {
    Line(String),
    Progress(DeployProgress),
}

#[derive(Clone, Debug, Default)]
pub struct DeploymentCancellation {
    requested: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl DeploymentCancellation {
    pub fn request(&self) {
        if !self.requested.swap(true, Ordering::SeqCst) {
            self.notify.notify_waiters();
        }
    }

    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    pub fn checkpoint(&self) -> Result<()> {
        if self.is_requested() {
            Err(NspawnError::DeploymentCancelled)
        } else {
            Ok(())
        }
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_requested() {
                return;
            }
            notified.await;
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
                }
                Err(NspawnError::Runtime(format!(
                    "Deployment cannot roll back replaced {} without its previous content",
                    resource.label()
                )))
            }
            ApplyStatus::ConflictUnknownOwner => {
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("an existing .nspawn configuration has unknown ownership".into());
                }
                Err(NspawnError::Validation(format!(
                    "Refusing to replace existing {} with unknown ownership",
                    resource.label()
                )))
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

/// RAII guard to ensure the 'done' flag is always set, even on panic or early return.
struct DoneGuard {
    done: Arc<AtomicBool>,
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
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
    mut spawned: crate::nspawn::sys::command::SpawnedProcess,
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

/// Orchestrates the asynchronous deployment of a new container.
#[allow(clippy::too_many_arguments)]
pub async fn run_deploy_task(
    deployer: Box<dyn Deployer>,
    storage: Box<dyn StorageBackend>,
    name: String,
    cfg: ContainerConfig,
    nvidia_profile: Option<crate::nspawn::platform::nvidia::profile::NvidiaPassthroughProfile>,
    exec_ctx: std::sync::Arc<crate::nspawn::sys::ExecutionContext>,
    logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
    done: Arc<AtomicBool>,
    success: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    rolling_back: Arc<AtomicBool>,
    cancellation: DeploymentCancellation,
    tx: tokio::sync::mpsc::Sender<AppEvent>,
) {
    // 1. Initialize the guard. When this is dropped (at end of function or on panic),
    // it will unconditionally set done = true, unblocking the UI spinner.
    let _guard = DoneGuard { done };

    // 2. Perform deployment
    if let Err(e) = run_deploy_internal(
        deployer,
        storage,
        name.clone(),
        cfg,
        nvidia_profile,
        exec_ctx,
        logs.clone(),
        cancellation.clone(),
        rolling_back,
    )
    .await
    {
        let was_cancelled = is_cancelled_outcome(&e);
        // Attempt to log the error. We use a non-blocking approach to prevent deadlocks
        // if the log channel happens to be full.
        let err_msg = format!("FATAL ERROR: {}", e);
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
        success.store(false, Ordering::SeqCst);
        cancelled.store(was_cancelled, Ordering::SeqCst);
        let response = if was_cancelled {
            crate::nspawn::ops::BackendResponse::DeployCancelled(e.to_string())
        } else {
            crate::nspawn::ops::BackendResponse::DeployFailed(e.to_string())
        };
        let _ = tx.send(AppEvent::BackendResult(response)).await;
    } else {
        success.store(true, Ordering::SeqCst);
    }
}

fn is_cancelled_outcome(error: &NspawnError) -> bool {
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
    nvidia_profile: Option<crate::nspawn::platform::nvidia::profile::NvidiaPassthroughProfile>,
    exec_ctx: std::sync::Arc<crate::nspawn::sys::ExecutionContext>,
    logs: tokio::sync::mpsc::Sender<DeployLogEvent>,
    cancellation: DeploymentCancellation,
    rolling_back: Arc<AtomicBool>,
) -> Result<()> {
    crate::nspawn::models::NspawnConfigSpec::try_from(&cfg)?;
    let system_operations = exec_ctx.system_operations.clone();

    macro_rules! push_log {
        ($msg:expr) => {
            send_deploy_log(&logs, $msg).await;
        };
    }

    push_log!(format!("=== Deploying '{}' ===", name));

    let is_ext = deployer.is_external_storage_managed();
    let mut report = ApplyReport::default();
    let mut raw_mount_target: Option<crate::nspawn::adapters::rootfs::RootfsTarget> = None;
    let mut storage_mount_attempted = false;
    let mut external_provider_started = false;

    let result = async {
        cancellation.checkpoint()?;
        validate_deployment_sidecars(&name, &exec_ctx).await?;
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
            crate::nspawn::adapters::rootfs::RootfsTarget::from_provisioned_path(&name, &rootfs)?;
        let rootfs_exists = exec_ctx
            .rootfs
            .has_os_release(&actual_rootfs_target)
            .await?;
        if !rootfs_exists && actual_rootfs_target.supports_raw_fallback() {
            push_log!("Mounting raw image for configuration...".to_string());
            match exec_ctx.rootfs.mount_managed_raw(&name).await {
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

        let has_os_layout = exec_ctx
            .rootfs
            .has_os_release(&actual_rootfs_target)
            .await?;
        let supports_offline_commands = has_os_layout
            && exec_ctx
                .rootfs
                .supports_nspawn_commands(&actual_rootfs_target)
                .await?;

        if supports_offline_commands {
            if let Some(pwd) = &cfg.root_password {
                push_log!("Setting root password...".to_string());
                for warning in exec_ctx
                    .rootfs
                    .set_root_password(&actual_rootfs_target, pwd)
                    .await?
                {
                    log::warn!("{}", warning);
                    push_log!(warning);
                }
                cancellation.checkpoint()?;
            }

            for user in &cfg.users {
                push_log!(format!("Creating user {}...", user.username));
                for warning in exec_ctx
                    .rootfs
                    .create_user(&actual_rootfs_target, user)
                    .await?
                {
                    log::warn!("{}", warning);
                    push_log!(warning);
                }
                cancellation.checkpoint()?;

                if cfg.wayland_socket.is_some() {
                    push_log!(format!("Setting up wayland env for {}...", user.username));
                    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
                    exec_ctx
                        .rootfs
                        .configure_wayland(&actual_rootfs_target, user, &display)
                        .await?;
                    cancellation.checkpoint()?;
                }
            }
        } else if !has_os_layout {
            log::warn!("[AUDIT] [Container: {}] rootfs OS layout could not be verified. Skipping internal modifications.", name);
            push_log!("WARNING: Could not verify the rootfs OS layout. Skipping passwords and user creation.".to_string());
        } else if cfg.root_password.is_some() || !cfg.users.is_empty() {
            log::warn!("[AUDIT] [Container: {}] rootfs has no /usr tree required by systemd-nspawn offline commands. Skipping account modifications.", name);
            push_log!("WARNING: This rootfs has no /usr tree required by systemd-nspawn; skipping password and user creation.".to_string());
        }

        let xdg_runtime = crate::nspawn::platform::capabilities::get_xdg_runtime()
            .await
            .ok();
        let mut initial_nvidia_state = None;

        if cfg.nvidia_gpu {
            push_log!("Assembling initial NVIDIA GPU configuration...".to_string());

            // Run initial CDI discovery to seed the .nspawn config and state.
            // Remapping is applied inside get_nvidia_state after CDI + ldconfig collection.
            let state = crate::nspawn::platform::nvidia::get_nvidia_state(
                nvidia_profile.as_ref(),
            )
            .await
            .map_err(|error| {
                NspawnError::Runtime(format!("NVIDIA CDI discovery failed: {error}"))
            })?;
            cancellation.checkpoint()?;

            // Persist the validated snapshot and its profile for lifecycle diffing.
            let state_apply = exec_ctx.nvidia_state.write_initial(&name, &state).await?;
            report.record_apply(AppliedResource::NvidiaState, state_apply)?;
            cancellation.checkpoint()?;

            // Write ld.so.conf.d and env vars into rootfs (one-time setup)
            if supports_offline_commands {
                match crate::nspawn::platform::nvidia::lifecycle::inject_env_once(
                    &actual_rootfs_target,
                    &state,
                    &exec_ctx.rootfs,
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
        let nspawn_apply = exec_ctx
            .nspawn
            .write_generated(
                &cfg,
                xdg_runtime.as_deref(),
                initial_nvidia_state.as_ref(),
            )
            .await?;
        report.record_apply(AppliedResource::NspawnConfig, nspawn_apply)?;
        cancellation.checkpoint()?;

        if !cfg.device_binds.is_empty() || cfg.nvidia_gpu || cfg.wayland_socket.is_some() || cfg.graphics_acceleration {
            log::info!(
                "[AUDIT] [Container: {}] [Step: Config] Writing systemd service override...",
                name
            );
            push_log!("Writing systemd service override...".to_string());
            let override_apply = exec_ctx
                .systemd_unit
                .write_override(
                    &name,
                    &cfg.device_binds,
                    cfg.nvidia_gpu,
                    cfg.graphics_acceleration,
                    cfg.wayland_socket.is_some(),
                )
                .await?;
            report.record_apply(AppliedResource::SystemdOverride, override_apply)?;

            system_operations.reload_daemon().await?;
            cancellation.checkpoint()?;
        }

        if supports_offline_commands {
            if let Some(mode) = &cfg.network {
                if matches!(
                    mode,
                    NetworkMode::None | NetworkMode::Veth | NetworkMode::Bridge(_)
                ) {
                    push_log!("Enabling container network (systemd-networkd)...".to_string());
                    if let Err(e) = exec_ctx.rootfs.configure_network(&actual_rootfs_target).await {
                        push_log!(format!("WARNING: {} (might not be a systemd container)", e));
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
        if let Err(error) = exec_ctx.rootfs.unmount_managed_raw(&target).await {
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
        rolling_back.store(true, Ordering::SeqCst);
        push_log!("Rolling back resources created by this deployment...".to_string());

        let external_ownership_confirmed = report.owns(AppliedResource::ExternalImage);
        let mut rollback_errors = cleanup_errors;
        rollback_errors.extend(
            rollback_apply_report(&name, &mut report, storage.as_ref(), &exec_ctx, &logs).await,
        );

        if external_provider_started && !external_ownership_confirmed {
            let warning = format!(
                "external provider did not confirm ownership of image {name:?}; any partial provider output was preserved for manual inspection"
            );
            log::warn!("{warning}");
            push_log!(format!("WARNING: {warning}"));
        }

        rolling_back.store(false, Ordering::SeqCst);
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

async fn validate_deployment_sidecars(
    name: &str,
    exec_ctx: &crate::nspawn::sys::ExecutionContext,
) -> Result<()> {
    if let Some(config) = exec_ctx.nspawn.inspect(name).await? {
        return Err(NspawnError::Validation(format!(
            "Deployment target has existing .nspawn configuration: {}",
            config.path.display()
        )));
    }
    if exec_ctx.nvidia_state.read(name).await?.is_some() {
        return Err(NspawnError::Validation(format!(
            "Deployment target {name:?} has existing NVIDIA state"
        )));
    }

    let unit = exec_ctx.systemd_unit.read(name).await?;
    if let Some(drop_in) = unit.drop_ins.iter().find(|drop_in| {
        std::path::Path::new(&drop_in.path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "override.conf" | "10-lasper-nvidia.conf"))
    }) {
        return Err(NspawnError::Validation(format!(
            "Deployment target has existing Lasper-managed unit drop-in: {}",
            drop_in.path
        )));
    }
    Ok(())
}

async fn rollback_apply_report(
    name: &str,
    report: &mut ApplyReport,
    storage: &dyn StorageBackend,
    exec_ctx: &crate::nspawn::sys::ExecutionContext,
    logs: &Sender<DeployLogEvent>,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut reload_daemon = false;

    while let Some(resource) = report.resources.pop() {
        send_deploy_log(logs, format!("Rolling back {}...", resource.label())).await;
        let result = match resource {
            AppliedResource::SystemdOverride => {
                reload_daemon = true;
                exec_ctx.systemd_unit.remove_service_override(name).await
            }
            AppliedResource::NspawnConfig => exec_ctx.nspawn.remove(name).await,
            AppliedResource::NvidiaState => exec_ctx.nvidia_state.remove(name).await,
            AppliedResource::ExternalImage => {
                let blockers = report.removal_blockers(resource);
                if blockers.is_empty() {
                    exec_ctx.system_operations.remove_image(name).await
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
        if let Err(error) = exec_ctx.system_operations.reload_daemon().await {
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
        assert!(matches!(
            cancellation.checkpoint(),
            Err(NspawnError::DeploymentCancelled)
        ));
    }

    #[tokio::test]
    async fn failed_process_wait_is_not_treated_as_a_rollback_safe_failure() {
        let spawned = crate::nspawn::sys::command::SpawnedProcess::new_cancellable(
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

        let spawned = crate::nspawn::sys::command::SpawnedProcess::new_cancellable(
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
