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
use crate::adapters::process::CommandRunner;
use crate::adapters::rootfs::RootfsStore;
use crate::adapters::storage::StorageBackend;
use crate::adapters::system_operation::SystemOperationStore;
use crate::adapters::trusted_state::TrustedStateRoot;
pub(crate) use crate::application::provisioning::{
    DeploymentCancellation, DeploymentEvent as DeployLogEvent, DeploymentProgress as DeployProgress,
};
use crate::application::provisioning::{DeploymentJobContext, DeploymentSecrets};
use crate::application::provisioning::{
    DeploymentResource, DeploymentStage, ResourceDisposition, ResourceLedger,
};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ApplyStatus, ContainerConfig};
use tokio::sync::mpsc::Sender;

/// Direct host capabilities used by the provisioning implementation.
///
/// The elevated route submits the complete deployment to the daemon, whose
/// worker constructs this same direct bundle. Keeping this type route-specific
/// prevents a deployment stage from selecting a daemon transport internally.
#[derive(Clone)]
pub(crate) struct DirectProvisioningCapabilities {
    system_operations: SystemOperationStore,
    nspawn: NspawnConfigStore,
    systemd_unit: SystemdUnitStore,
    rootfs: RootfsStore,
    nvidia_state: NvidiaStateStore,
}

impl DirectProvisioningCapabilities {
    pub(crate) fn from_direct(
        command_runner: std::sync::Arc<dyn CommandRunner>,
        state_root: TrustedStateRoot,
    ) -> Self {
        Self {
            system_operations: SystemOperationStore::new(command_runner, None),
            nspawn: NspawnConfigStore::new(None),
            systemd_unit: SystemdUnitStore::new(None),
            rootfs: RootfsStore::new(None),
            nvidia_state: NvidiaStateStore::new(None, state_root),
        }
    }

    pub(crate) fn system_operations(&self) -> &SystemOperationStore {
        &self.system_operations
    }

    pub(crate) fn nspawn(&self) -> &NspawnConfigStore {
        &self.nspawn
    }

    pub(crate) fn systemd_unit(&self) -> &SystemdUnitStore {
        &self.systemd_unit
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

#[derive(Debug)]
pub(crate) struct ApplyReport {
    target: crate::domain::machine::MachineName,
    ledger: ResourceLedger,
    external_image_blockers: Vec<String>,
    storage_removal_blockers: Vec<String>,
}

impl ApplyReport {
    fn new(target: crate::domain::machine::MachineName) -> Self {
        Self {
            target,
            ledger: ResourceLedger::default(),
            external_image_blockers: Vec::new(),
            storage_removal_blockers: Vec::new(),
        }
    }

    fn typed(&self, resource: AppliedResource) -> DeploymentResource {
        match resource {
            AppliedResource::LocalStorage => DeploymentResource::LocalStorage(self.target.clone()),
            AppliedResource::ExternalImage => {
                DeploymentResource::ExternalImage(self.target.clone())
            }
            AppliedResource::NvidiaState => DeploymentResource::NvidiaState(self.target.clone()),
            AppliedResource::NspawnConfig => DeploymentResource::NspawnConfig(self.target.clone()),
            AppliedResource::SystemdOverride => {
                DeploymentResource::SystemdOverride(self.target.clone())
            }
        }
    }

    pub(crate) fn record_created(&mut self, resource: AppliedResource) {
        self.ledger
            .record(self.typed(resource), ResourceDisposition::Created);
    }

    pub(crate) fn record_apply(
        &mut self,
        resource: AppliedResource,
        status: ApplyStatus,
    ) -> Result<()> {
        match status {
            ApplyStatus::Created => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::Created);
                Ok(())
            }
            ApplyStatus::Unchanged => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::PreExisting);
                if resource == AppliedResource::NspawnConfig {
                    self.external_image_blockers
                        .push("an unchanged .nspawn configuration predates this deployment".into());
                }
                Ok(())
            }
            ApplyStatus::ReplacedOwned => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::Adopted);
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
                Ok(())
            }
            ApplyStatus::ConflictUnknownOwner => {
                self.ledger
                    .record(self.typed(resource), ResourceDisposition::PreExisting);
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
        self.ledger.owns(&self.typed(resource))
    }

    fn record_typed(&mut self, resource: DeploymentResource, disposition: ResourceDisposition) {
        self.ledger.record(resource, disposition);
    }

    fn record_outcome_unknown_if_unclassified(&mut self, resource: DeploymentResource) {
        if self.ledger.disposition(&resource).is_none() {
            self.ledger
                .record(resource, ResourceDisposition::OutcomeUnknown);
        }
    }

    fn remove_typed(&mut self, resource: &DeploymentResource) {
        self.ledger.remove(resource);
    }

    fn typed_owned_in_reverse(&self) -> Vec<DeploymentResource> {
        self.ledger
            .owned_in_reverse()
            .into_iter()
            .filter(|resource| {
                matches!(
                    resource,
                    DeploymentResource::LocalStorage(_)
                        | DeploymentResource::ExternalImage(_)
                        | DeploymentResource::NvidiaState(_)
                        | DeploymentResource::NspawnConfig(_)
                        | DeploymentResource::SystemdOverride(_)
                )
            })
            .collect()
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

    fn application_ledger(&self) -> &ResourceLedger {
        &self.ledger
    }

    fn remove_rootfs_dependents(&mut self) {
        for resource in [
            DeploymentResource::StorageMount(self.target.clone()),
            DeploymentResource::RawConfigurationMount(self.target.clone()),
            DeploymentResource::RootfsAccounts(self.target.clone()),
            DeploymentResource::RootfsNvidia(self.target.clone()),
            DeploymentResource::RootfsNetwork(self.target.clone()),
        ] {
            self.ledger.remove(&resource);
        }
    }

    fn remove_external_image_dependents(&mut self) {
        self.remove_rootfs_dependents();
        self.ledger
            .remove(&DeploymentResource::NspawnConfig(self.target.clone()));
    }

    fn outcome_unknown_resources(&self) -> Vec<DeploymentResource> {
        self.ledger
            .snapshot()
            .entries()
            .iter()
            .filter(|entry| entry.disposition == ResourceDisposition::OutcomeUnknown)
            .map(|entry| entry.resource.clone())
            .collect()
    }
}

async fn persist_applying(
    job: &DeploymentJobContext,
    stage: DeploymentStage,
    intended_resources: Vec<DeploymentResource>,
    report: &ApplyReport,
) -> Result<()> {
    if let Some(state) = job.state_session() {
        state
            .applying(stage, intended_resources, report.application_ledger())
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))?;
    }
    Ok(())
}

async fn persist_committed(
    job: &DeploymentJobContext,
    stage: DeploymentStage,
    report: &ApplyReport,
) -> Result<()> {
    if let Some(state) = job.state_session() {
        state
            .committed(stage, report.application_ledger())
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))?;
    }
    Ok(())
}

async fn persist_cleanup_pending(job: &DeploymentJobContext, report: &ApplyReport) -> Result<()> {
    if let Some(state) = job.state_session() {
        state
            .cleanup_pending(report.application_ledger())
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))?;
    }
    Ok(())
}

async fn finish_manifest(job: &DeploymentJobContext) -> Result<()> {
    if let Some(state) = job.state_session() {
        state
            .finish()
            .await
            .map_err(|error| NspawnError::Runtime(error.to_string()))?;
    }
    Ok(())
}

async fn capture_uncommitted_effects(job: &DeploymentJobContext, report: &mut ApplyReport) {
    let Some(state) = job.state_session() else {
        return;
    };
    for resource in state.current_applying_resources().await {
        if matches!(resource, DeploymentResource::RawConfigurationMount(_))
            && report.ledger.disposition(&resource).is_none()
        {
            report.block_storage_removal(
                "raw image configuration mount outcome requires reconciliation",
            );
        }
        report.record_outcome_unknown_if_unclassified(resource);
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

    /// Resources whose outcome may change while the source stage is running.
    ///
    /// This list is persisted before dispatch. It may conservatively include
    /// optional effects, but it must include every effect the deployer can
    /// create before it returns an authoritative result.
    fn source_stage_resources(
        &self,
        target: &crate::domain::machine::MachineName,
    ) -> Vec<DeploymentResource> {
        vec![if self.is_external_storage_managed() {
            DeploymentResource::ExternalImage(target.clone())
        } else {
            DeploymentResource::LocalStorage(target.clone())
        }]
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
    let target = crate::nspawn::models::NspawnConfigSpec::try_from(&cfg)?.machine;
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

async fn inspect_deployment_sidecars(
    name: &str,
    host: &DirectProvisioningCapabilities,
) -> Result<Vec<String>> {
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
    host: &DirectProvisioningCapabilities,
    logs: &Sender<DeployLogEvent>,
    job: &DeploymentJobContext,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut reload_daemon = false;

    for resource in report.typed_owned_in_reverse() {
        send_deploy_log(logs, format!("Rolling back {}...", resource.label())).await;
        if let Err(error) = persist_applying(
            job,
            DeploymentStage::Rollback,
            vec![resource.clone()],
            report,
        )
        .await
        {
            report.record_typed(resource, ResourceDisposition::CleanupPending);
            errors.push(format!("rollback was not attempted: {error}"));
            break;
        }
        let result = match &resource {
            DeploymentResource::SystemdOverride(_) => {
                reload_daemon = true;
                host.systemd_unit.remove_service_override(name).await
            }
            DeploymentResource::NspawnConfig(_) => host.nspawn.remove(name).await,
            DeploymentResource::NvidiaState(_) => host.nvidia_state.remove(name).await,
            DeploymentResource::ExternalImage(_) => {
                let blockers = report.removal_blockers(AppliedResource::ExternalImage);
                if blockers.is_empty() {
                    host.system_operations.remove_image(name).await
                } else {
                    Err(NspawnError::Runtime(format!(
                        "external image removal blocked: {}",
                        blockers.join("; ")
                    )))
                }
            }
            DeploymentResource::LocalStorage(_) => {
                let blockers = report.removal_blockers(AppliedResource::LocalStorage);
                if blockers.is_empty() {
                    storage.delete(name).await
                } else {
                    Err(NspawnError::Runtime(format!(
                        "local storage removal blocked: {}",
                        blockers.join("; ")
                    )))
                }
            }
            _ => continue,
        };
        match result {
            Ok(()) => {
                report.remove_typed(&resource);
                match resource {
                    DeploymentResource::ExternalImage(_) => {
                        report.remove_external_image_dependents();
                    }
                    DeploymentResource::LocalStorage(_) => report.remove_rootfs_dependents(),
                    _ => {}
                }
                if let Err(error) = persist_committed(job, DeploymentStage::Rollback, report).await
                {
                    report.record_typed(resource.clone(), ResourceDisposition::CleanupPending);
                    errors.push(format!(
                        "{} durable cleanup state: {error}",
                        resource.label()
                    ));
                    break;
                }
            }
            Err(error) => {
                report.record_typed(resource.clone(), ResourceDisposition::CleanupPending);
                errors.push(format!("{}: {error}", resource.label()));
            }
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
    use crate::application::provisioning::{
        deployment_job_channel, DeploymentId, DeploymentPlan, DeploymentRequest, DeploymentSource,
        DeploymentStatePort, DeploymentStateSession, DeploymentStorage, MemoryDeploymentStatePort,
    };
    use std::sync::Arc;

    fn apply_report() -> ApplyReport {
        ApplyReport::new(crate::domain::machine::MachineName::new("test").unwrap())
    }

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
        let mut report = apply_report();
        report
            .record_apply(AppliedResource::NspawnConfig, ApplyStatus::Created)
            .unwrap();
        report
            .record_apply(AppliedResource::NspawnConfig, ApplyStatus::Created)
            .unwrap();
        report
            .record_apply(AppliedResource::SystemdOverride, ApplyStatus::Unchanged)
            .unwrap();

        assert!(report.owns(AppliedResource::NspawnConfig));
        assert!(!report.owns(AppliedResource::SystemdOverride));
    }

    #[test]
    fn unknown_nspawn_owner_blocks_external_image_compensation() {
        let mut report = apply_report();
        let error = report
            .record_apply(
                AppliedResource::NspawnConfig,
                ApplyStatus::ConflictUnknownOwner,
            )
            .unwrap_err();

        assert!(error.to_string().contains("unknown ownership"));
        assert!(!report.owns(AppliedResource::NspawnConfig));
        assert_eq!(report.external_image_blockers.len(), 1);
    }

    #[test]
    fn sidecar_conflicts_are_preserved_and_owned_replacements_are_adopted() {
        let mut report = apply_report();
        report
            .record_apply(
                AppliedResource::NvidiaState,
                ApplyStatus::ConflictUnknownOwner,
            )
            .unwrap();
        report
            .record_apply(AppliedResource::SystemdOverride, ApplyStatus::ReplacedOwned)
            .unwrap();

        assert!(report.owns(AppliedResource::SystemdOverride));
        assert!(!report.owns(AppliedResource::NvidiaState));
        assert!(report.external_image_blockers.is_empty());
    }

    #[test]
    fn unknown_effects_are_not_rolled_back_as_owned_resources() {
        let mut report = apply_report();
        let unknown = DeploymentResource::NspawnConfig(report.target.clone());
        report.record_outcome_unknown_if_unclassified(unknown.clone());

        assert_eq!(report.outcome_unknown_resources(), vec![unknown]);
        assert!(!report.owns(AppliedResource::NspawnConfig));
        assert!(report.typed_owned_in_reverse().is_empty());
    }

    #[test]
    fn removing_owned_storage_resolves_unknown_rootfs_effects() {
        let mut report = apply_report();
        report.record_created(AppliedResource::LocalStorage);
        report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsAccounts(
            report.target.clone(),
        ));
        report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsNetwork(
            report.target.clone(),
        ));
        report.record_outcome_unknown_if_unclassified(DeploymentResource::RootfsNvidia(
            report.target.clone(),
        ));

        report.remove_rootfs_dependents();

        assert!(report.outcome_unknown_resources().is_empty());
        assert!(report.owns(AppliedResource::LocalStorage));
    }

    #[tokio::test]
    async fn interrupted_applying_effect_is_persisted_as_unknown_not_owned() {
        let plan = DeploymentPlan::build(DeploymentRequest {
            config: ContainerConfig {
                name: "test".into(),
                ..Default::default()
            },
            source: DeploymentSource::Copy {
                source_name: "base".into(),
            },
            storage: DeploymentStorage::Directory,
            nvidia_profile: None,
            wayland: Vec::new(),
            allow_unsafe_remote_tar: false,
        })
        .unwrap();
        let id = DeploymentId::from_u128(42);
        let state = Arc::new(MemoryDeploymentStatePort::default());
        let session = DeploymentStateSession::new(state.clone(), id, &plan);
        session.prepare().await.unwrap();
        let resource = DeploymentResource::RawConfigurationMount(plan.target().clone());
        session
            .applying(
                DeploymentStage::RootfsMutation,
                vec![resource.clone()],
                &ResourceLedger::default(),
            )
            .await
            .unwrap();
        let (_handle, job) = deployment_job_channel(id);
        let job = job.with_state_session(session);
        let mut report = ApplyReport::new(plan.target().clone());

        capture_uncommitted_effects(&job, &mut report).await;
        persist_cleanup_pending(&job, &report).await.unwrap();

        assert_eq!(
            report.ledger.disposition(&resource),
            Some(ResourceDisposition::OutcomeUnknown)
        );
        assert!(!report.ledger.owns(&resource));
        assert_eq!(report.storage_removal_blockers.len(), 1);
        let manifests = state.unfinished().await.unwrap();
        assert_eq!(manifests.len(), 1);
        assert!(matches!(
            manifests[0].state,
            crate::application::provisioning::DeploymentManifestState::CleanupPending
        ));
        assert_eq!(
            manifests[0].committed_ledger.entries()[0].disposition,
            ResourceDisposition::OutcomeUnknown
        );
    }

    #[test]
    fn failed_unmount_blocks_local_and_external_storage_compensation() {
        let mut report = apply_report();
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
