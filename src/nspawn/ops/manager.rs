use crate::nspawn::adapters::comm::backend::{
    ControlAdapter, MachineControl, RuntimeAdapter, RuntimeSource,
};
use crate::nspawn::adapters::comm::cli::CliBackend;
use crate::nspawn::adapters::comm::daemon_backend::DaemonBackend;
use crate::nspawn::adapters::comm::dbus::DbusBackend;
use crate::nspawn::errors::Result;
use crate::nspawn::models::{
    AllowedSignal, ContainerEntry, ImageEntry, MachineProperties, RuntimeSnapshot, StatusUpdate,
};
use crate::nspawn::ops::{PermissionLevel, PermissionManager};
use crate::nspawn::sys::ExecutionContext;
use async_trait::async_trait;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::AsyncBufReadExt;

const START_CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const START_CONFIRM_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Debug, PartialEq, Eq)]
enum StartObservation {
    Started,
    Pending(String),
    Failed(String),
}

fn systemd_property<'a>(properties: &'a MachineProperties, key: &str) -> Option<&'a str> {
    properties
        .groups
        .iter()
        .find(|group| group.name == crate::nspawn::models::GROUP_SYSTEMD_UNIT)
        .and_then(|group| group.properties.get(key))
        .map(String::as_str)
}

fn describe_start_properties(properties: &MachineProperties) -> String {
    [
        "ActiveState",
        "SubState",
        "Result",
        "ExecMainCode",
        "ExecMainStatus",
        "StatusText",
    ]
    .into_iter()
    .filter_map(|key| systemd_property(properties, key).map(|value| format!("{key}={value}")))
    .collect::<Vec<_>>()
    .join(", ")
}

fn observe_start(properties: &MachineProperties) -> StartObservation {
    let active_state = systemd_property(properties, "ActiveState").unwrap_or_default();
    let service_result = systemd_property(properties, "Result").unwrap_or_default();
    let details = describe_start_properties(properties);

    if active_state == "active" {
        return StartObservation::Started;
    }
    if active_state == "failed"
        || (!service_result.is_empty()
            && service_result != "success"
            && service_result != "[not set]")
    {
        return StartObservation::Failed(details);
    }
    StartObservation::Pending(details)
}

fn valid_invocation_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait NspawnManager: Send + Sync + 'static {
    async fn list_machines(&self) -> Result<Vec<ContainerEntry>>;
    async fn snapshot(&self) -> Result<RuntimeSnapshot>;
    async fn start(&self, name: &str) -> Result<()>;
    async fn terminate(&self, name: &str) -> Result<()>;
    fn spawn_log_stream(
        &self,
        name: &str,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
        fatal: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> tokio::task::JoinHandle<()>;
    async fn get_properties(&self, name: &str, entry: &ContainerEntry)
        -> Result<MachineProperties>;
    async fn enable(&self, name: &str) -> Result<()>;
    async fn disable(&self, name: &str) -> Result<()>;
    async fn poweroff(&self, name: &str) -> Result<()>;
    async fn reboot(&self, name: &str) -> Result<()>;
    async fn kill(&self, name: &str, signal: AllowedSignal) -> Result<()>;
    /// Remove the systemd-managed image and its settings through `RemoveImage`.
    async fn remove(&self, name: &str) -> Result<()>;
    /// Remove Lasper's NVIDIA state and known systemd unit drop-ins.
    async fn cleanup_image_artifacts(&self, name: &str) -> Result<()>;
    async fn is_dbus_available(&self) -> bool;
    fn did_fallback(&self) -> Option<String>;
    async fn watch(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>);
    fn get_watch_paths(&self) -> Vec<PathBuf>;
}

pub struct DefaultManager {
    #[allow(dead_code)] // used in constructor and tests via struct literals
    pm: std::sync::Arc<dyn PermissionManager>,
    cli_mode: bool,
    dbus: std::sync::Arc<dyn RuntimeSource>,
    cli: std::sync::Arc<dyn RuntimeSource>,
    dbus_control: std::sync::Arc<dyn MachineControl>,
    cli_control: std::sync::Arc<dyn MachineControl>,
    exec_ctx: std::sync::Arc<ExecutionContext>,
    last_fallback_reason: parking_lot::Mutex<Option<String>>,
    watch_paths: Vec<PathBuf>,
    nudge_tx: tokio::sync::watch::Sender<()>,
}

impl DefaultManager {
    pub fn new(
        pm: std::sync::Arc<dyn PermissionManager>,
        cli_mode: bool,
        exec_ctx: std::sync::Arc<ExecutionContext>,
    ) -> Self {
        if cli_mode {
            log::info!("CLI mode active — Lasper DBus backend disabled");
            if pm.level() == PermissionLevel::Elevated {
                log::info!("Elevated CLI inspection routed through the root daemon");
            }
        }

        let (dbus_backend, dbus_control): (
            std::sync::Arc<dyn RuntimeSource>,
            std::sync::Arc<dyn MachineControl>,
        ) = match pm.level() {
            PermissionLevel::Elevated => {
                if !cli_mode {
                    log::info!("Elevated mode — DBus operations proxied through daemon");
                }
                let backend = DaemonBackend::new(
                    exec_ctx
                        .daemon_ref()
                        .cloned()
                        .expect("daemon required for Elevated mode"),
                );
                (
                    std::sync::Arc::new(RuntimeAdapter::new(backend.clone())),
                    std::sync::Arc::new(ControlAdapter::new(backend)),
                )
            }
            _ => {
                let backend = DbusBackend::new();
                (
                    std::sync::Arc::new(RuntimeAdapter::new(backend.clone())),
                    std::sync::Arc::new(ControlAdapter::new(backend)),
                )
            }
        };

        let cli_backend = CliBackend::with_system_operations(
            exec_ctx.local_cmd.clone(),
            exec_ctx.system_operations.clone(),
        );
        let cli_control: std::sync::Arc<dyn MachineControl> =
            std::sync::Arc::new(exec_ctx.system_operations.clone());
        let (nudge_tx, nudge_rx) = tokio::sync::watch::channel(());
        cli_backend.set_nudge(nudge_rx);
        Self {
            pm,
            cli_mode,
            dbus: dbus_backend,
            cli: std::sync::Arc::new(RuntimeAdapter::new(cli_backend)),
            dbus_control,
            cli_control,
            exec_ctx,
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![
                crate::paths::machines_dir(),
                crate::paths::runtime_machines_dir(),
            ],
            nudge_tx,
        }
    }

    fn mark_fallback(&self, reason: &str) {
        if self.cli_mode {
            return;
        }
        *self.last_fallback_reason.lock() = Some(reason.to_string());
    }

    /// Classify a DBus error into a human-readable fallback reason.
    ///
    /// - Polkit rejection → tells the user *why* CLI was needed
    /// - System bus unavailable → generic "DBus not available"
    /// - Other errors → passes through the error message
    fn classify_fallback(err: &crate::nspawn::errors::NspawnError) -> String {
        if err.is_polkit_rejection() {
            "polkit denied access — try running with -e to elevate".into()
        } else {
            format!("{}", err)
        }
    }

    fn nudge(&self) {
        let _ = self.nudge_tx.send(());
    }

    async fn _ensure_gpu_passthrough(&self, name: &str) -> Result<()> {
        crate::nspawn::platform::nvidia::ensure_gpu_passthrough(
            name,
            &self.exec_ctx.nspawn,
            &self.exec_ctx.systemd_unit,
            &self.exec_ctx.nvidia_state,
            &self.exec_ctx.rootfs,
        )
        .await?;
        // Reload systemd after GPU surgery — always needed when device allows change.
        let _ = self.reload_daemon_fallback().await;
        self.nudge();
        Ok(())
    }

    /// DBus-first / CLI-fallback for `reload_daemon` (takes no name, doesn't fit the macro).
    async fn reload_daemon_fallback(&self) -> Result<()> {
        if !self.cli_mode && self.dbus.is_available().await {
            match self.dbus_control.reload_daemon().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let reason = Self::classify_fallback(&e);
                    log::warn!(
                        "DBus reload_daemon failed ({}), falling back to CLI",
                        reason
                    );
                    self.mark_fallback(&reason);
                }
            }
        } else {
            log::debug!("DBus not available for reload_daemon, using CLI");
            self.mark_fallback("DBus not available");
        }
        self.cli_control.reload_daemon().await.map_err(|e| {
            log::error!("CLI reload_daemon failed: {}", e);
            e
        })
    }

    async fn wait_for_start(&self, name: &str, backend: &dyn RuntimeSource) -> Result<()> {
        let started_at = tokio::time::Instant::now();
        let started_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut last_observation = "systemd unit properties unavailable".to_string();
        let mut last_invocation_id = None;

        loop {
            match backend.get_properties(name).await {
                Ok(properties) => {
                    let invocation_id = systemd_property(&properties, "InvocationID")
                        .filter(|value| valid_invocation_id(value))
                        .map(str::to_string);
                    if invocation_id.is_some() {
                        last_invocation_id = invocation_id.clone();
                    }
                    match observe_start(&properties) {
                        StartObservation::Started => return Ok(()),
                        StartObservation::Pending(details) => {
                            if !details.is_empty() {
                                last_observation = details;
                            }
                        }
                        StartObservation::Failed(details) => {
                            return Err(self
                                .start_failure(
                                    name,
                                    &details,
                                    invocation_id.as_deref(),
                                    started_epoch,
                                )
                                .await);
                        }
                    }
                }
                Err(error) => last_observation = error.to_string(),
            }

            if started_at.elapsed() >= START_CONFIRM_TIMEOUT {
                let details = format!(
                    "timed out after {}s; {}",
                    START_CONFIRM_TIMEOUT.as_secs(),
                    last_observation
                );
                return Err(self
                    .start_failure(name, &details, last_invocation_id.as_deref(), started_epoch)
                    .await);
            }
            tokio::time::sleep(START_CONFIRM_INTERVAL).await;
        }
    }

    async fn start_failure(
        &self,
        name: &str,
        details: &str,
        invocation_id: Option<&str>,
        started_epoch: u64,
    ) -> crate::nspawn::errors::NspawnError {
        let unit = crate::nspawn::models::MachineName::new(name)
            .map(|machine| machine.systemd_nspawn_unit())
            .unwrap_or_else(|_| format!("systemd-nspawn@{name}.service"));
        let (selector, selector_display) = if let Some(invocation_id) = invocation_id {
            (
                vec![format!("_SYSTEMD_INVOCATION_ID={invocation_id}")],
                format!("_SYSTEMD_INVOCATION_ID={invocation_id}"),
            )
        } else {
            (
                vec![
                    "-u".into(),
                    unit.clone(),
                    "--since".into(),
                    format!("@{started_epoch}"),
                ],
                format!("-u {unit} --since @{started_epoch}"),
            )
        };
        let journal_command = format!("journalctl {selector_display} --no-pager");

        // nspawn emits some fatal setup diagnostics below warning priority. A
        // priority filter can therefore retain an unrelated warning while
        // hiding the line that explains why the process exited.
        let mut journal_args = selector;
        journal_args.extend([
            "-n".into(),
            "40".into(),
            "--no-pager".into(),
            "--quiet".into(),
            "--output=short".into(),
        ]);
        let journal = self.read_journal(journal_args).await;

        if let Some(journal) = journal {
            log::error!(
                "Container start failed for {}: {}\nRecent {} journal:\n{}",
                name,
                details,
                unit,
                journal
            );
        } else {
            log::error!("Container start failed for {}: {}", name, details);
        }

        crate::nspawn::errors::NspawnError::Runtime(format!(
            "Container '{name}' failed to start ({details}). Inspect host logs with `{journal_command}`."
        ))
    }

    async fn read_journal(&self, args: Vec<String>) -> Option<String> {
        self.exec_ctx
            .local_cmd
            .run("journalctl", args)
            .await
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|output| !output.is_empty())
    }
}

/// Try DBus first, then fall back to CLI with consistent logging and error reporting.
/// When Elevated, the CLI backend runs via `sudo`, making elevated DBus calls through
/// `machinectl`.
macro_rules! fallback_to_cli {
    ($self:ident, $method:ident, $name:expr $(, $arg:expr)*) => {{
        let result = if !$self.cli_mode && $self.dbus.is_available().await {
            match $self.dbus_control.$method($name, $($arg),*).await {
                Ok(v) => Ok(v),
                Err(e) => {
                    let reason = Self::classify_fallback(&e);
                    log::warn!(
                        "DBus {} failed ({}), falling back to CLI",
                        stringify!($method),
                        reason
                    );
                    $self.mark_fallback(&reason);
                    $self.cli_control.$method($name, $($arg),*).await
                }
            }
        } else {
            log::debug!("DBus not available for {}, using CLI", stringify!($method));
            $self.mark_fallback("DBus not available");
            $self.cli_control.$method($name, $($arg),*).await
        };
        if result.is_ok() {
            $self.nudge();
        }
        result.map_err(|e| {
            log::error!("CLI {} failed for {}: {}", stringify!($method), $name, e);
            e
        })
    }};
}

#[async_trait]
impl NspawnManager for DefaultManager {
    async fn list_machines(&self) -> Result<Vec<ContainerEntry>> {
        if !self.cli_mode && self.dbus.is_available().await {
            match self.dbus.list_machines().await {
                Ok(entries) => return Ok(entries),
                Err(e) => {
                    let reason = Self::classify_fallback(&e);
                    log::warn!(
                        "DBus list_machines failed ({}), falling back to CLI",
                        reason
                    );
                    self.mark_fallback(&reason);
                }
            }
        } else {
            log::debug!("DBus not available for list_machines, using CLI");
            self.mark_fallback("DBus not available");
        }
        self.cli.list_machines().await.map_err(|e| {
            log::error!("CLI list_machines failed: {}", e);
            e
        })
    }

    async fn snapshot(&self) -> Result<RuntimeSnapshot> {
        if !self.cli_mode && self.dbus.is_available().await {
            match self.dbus.snapshot().await {
                Ok(snapshot) => return Ok(snapshot),
                Err(error) => {
                    let reason = Self::classify_fallback(&error);
                    log::warn!("DBus snapshot failed ({}), falling back to CLI", reason);
                    self.mark_fallback(&reason);
                }
            }
        } else if !self.cli_mode {
            self.mark_fallback("DBus not available");
        }

        self.cli.snapshot().await.map_err(|error| {
            log::error!("CLI snapshot failed: {}", error);
            error
        })
    }

    async fn start(&self, name: &str) -> Result<()> {
        self._ensure_gpu_passthrough(name).await?;
        let backend = if !self.cli_mode && self.dbus.is_available().await {
            match self.dbus_control.start(name).await {
                Ok(()) => self.dbus.clone(),
                Err(error) => {
                    let reason = Self::classify_fallback(&error);
                    log::warn!("DBus start failed ({}), falling back to CLI", reason);
                    self.mark_fallback(&reason);
                    self.cli_control.start(name).await?;
                    self.cli.clone()
                }
            }
        } else {
            log::debug!("DBus not available for start, using CLI");
            self.mark_fallback("DBus not available");
            self.cli_control.start(name).await?;
            self.cli.clone()
        };

        self.nudge();
        self.wait_for_start(name, backend.as_ref()).await
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        fallback_to_cli!(self, terminate, name)
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        fallback_to_cli!(self, poweroff, name)
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        fallback_to_cli!(self, reboot, name)
    }

    async fn enable(&self, name: &str) -> Result<()> {
        fallback_to_cli!(self, enable, name)
    }

    async fn disable(&self, name: &str) -> Result<()> {
        fallback_to_cli!(self, disable, name)
    }

    async fn kill(&self, name: &str, signal: AllowedSignal) -> Result<()> {
        fallback_to_cli!(self, kill, name, signal)
    }

    async fn remove(&self, name: &str) -> Result<()> {
        if ImageEntry::is_protected_name(name) {
            return Err(crate::nspawn::errors::NspawnError::Validation(
                "the .host image cannot be removed".into(),
            ));
        }
        if !ImageEntry::is_valid_name(name) {
            return Err(crate::nspawn::errors::NspawnError::Validation(format!(
                "invalid image name {:?}",
                name
            )));
        }
        if self
            .list_machines()
            .await?
            .iter()
            .any(|machine| machine.name == name && machine.state.is_running())
        {
            return Err(crate::nspawn::errors::NspawnError::ContainerAlreadyRunning(
                name.to_string(),
            ));
        }

        let hidden = ImageEntry::is_hidden_name(name);
        let managed_machine_name = crate::nspawn::models::MachineName::new(name).is_ok();
        // Keep a removed regular image from leaving an enabled nspawn unit
        // behind. Hidden images and regular images whose names cannot identify
        // an nspawn machine have no Lasper-managed unit sidecars.
        if !hidden && managed_machine_name {
            if let Err(error) = self.disable(name).await {
                log::warn!("Failed to disable {} before image removal: {}", name, error);
            }
        }

        // systemd's `RemoveImage` operation accepts image names rather than
        // machine names, including dot-prefixed hidden images. The CLI uses
        // the same D-Bus method, so both paths share the same validation and
        // deletion semantics.
        let result = if !self.cli_mode && self.dbus.is_available().await {
            match self.dbus_control.remove(name).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let reason = Self::classify_fallback(&e);
                    log::warn!("DBus remove failed ({}), falling back to CLI", reason);
                    self.mark_fallback(&reason);
                    self.cli_control.remove(name).await.map_err(|e| {
                        log::error!("CLI remove failed for {}: {}", name, e);
                        e
                    })
                }
            }
        } else {
            self.cli_control.remove(name).await.map_err(|e| {
                log::error!("CLI remove failed for {}: {}", name, e);
                e
            })
        };

        result?;
        self.nudge();
        Ok(())
    }

    async fn cleanup_image_artifacts(&self, name: &str) -> Result<()> {
        crate::nspawn::models::MachineName::new(name)
            .map_err(|error| crate::nspawn::errors::NspawnError::Validation(error.to_string()))?;

        let mut cleanup_errors = Vec::new();
        if let Err(error) = self.exec_ctx.systemd_unit.remove_overrides(name).await {
            cleanup_errors.push(format!("systemd unit drop-ins: {error}"));
        }
        if let Err(error) = self.exec_ctx.nvidia_state.remove(name).await {
            cleanup_errors.push(format!("NVIDIA state: {error}"));
        }
        if let Err(error) = self.reload_daemon_fallback().await {
            cleanup_errors.push(format!("systemd daemon reload: {error}"));
        }

        if cleanup_errors.is_empty() {
            Ok(())
        } else {
            Err(crate::nspawn::errors::NspawnError::Runtime(format!(
                "Lasper artifact cleanup was incomplete: {}",
                cleanup_errors.join("; ")
            )))
        }
    }

    fn spawn_log_stream(
        &self,
        name: &str,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
        fatal: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        let name = name.to_string();

        if let Some(daemon) = self.exec_ctx.daemon_ref().cloned() {
            let name = name.clone();
            let fatal_clone = fatal.clone();
            return tokio::spawn(async move {
                let stdout_fd = match daemon.spawn_journalctl(&name).await {
                    Ok(fd) => fd,
                    Err(e) => {
                        fatal_clone.store(true, Ordering::Relaxed);
                        let _ = tx.send(format!("Log stream error: {e}"));
                        return;
                    }
                };

                // Use tokio async pipe so the runtime can cancel this during shutdown.
                match pipe_reader(stdout_fd) {
                    Ok(receiver) => {
                        let mut lines = tokio::io::BufReader::new(receiver).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            if tx.send(line).is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(format!("Log stream error: {e}"));
                    }
                }
            });
        }

        tokio::spawn(async move {
            let mut child = match crate::nspawn::sys::new_command("journalctl")
                .args([
                    "-M",
                    &name,
                    "-n",
                    "1000",
                    "-f",
                    "--no-pager",
                    "--output=short",
                ])
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    fatal.store(true, Ordering::Relaxed);
                    let _ = tx.send(format!("Log stream error: {e}"));
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        let _ = tx.send(
                            "Hint: add yourself to the 'systemd-journal' \
                             group: sudo usermod -a -G systemd-journal $USER"
                                .into(),
                        );
                    }
                    return;
                }
            };

            let stdout = child.stdout.take().expect("journalctl stdout piped");
            let mut stderr_pipe = child.stderr.take().expect("journalctl stderr piped");
            let mut lines = tokio::io::BufReader::new(stdout).lines();

            let stream_result: std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> =
                async {
                    loop {
                        tokio::select! {
                            line_res = lines.next_line() => {
                                if let Ok(Some(line)) = line_res {
                                    if tx.send(line).is_err() {
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            }
                            _ = child.wait() => break,
                        }
                    }

                    // Drain stderr after the stream ends — journalctl writes
                    // permission hints (e.g. "add yourself to systemd-journal")
                    // to stderr.
                    use tokio::io::AsyncReadExt;
                    let mut buf = Vec::new();
                    if let Ok(n) = stderr_pipe.read_to_end(&mut buf).await {
                        if n > 0 {
                            fatal.store(true, Ordering::Relaxed);
                            let _ =
                                tx.send(format!("Log stream: {}", String::from_utf8_lossy(&buf)));
                        }
                    }
                    Ok(())
                }
                .await;

            if let Err(e) = stream_result {
                let _ = tx.send(format!("Log stream stopped: {e}"));
            }
        })
    }

    async fn get_properties(
        &self,
        name: &str,
        entry: &ContainerEntry,
    ) -> Result<MachineProperties> {
        let mut props = if self.cli_mode {
            self.exec_ctx
                .machine_inspection
                .inspect(name, entry)
                .await?
        } else if self.dbus.is_available().await {
            match self.dbus.get_properties(name).await {
                Ok(p) => p,
                Err(e) => {
                    let reason = Self::classify_fallback(&e);
                    log::warn!(
                        "DBus get_properties failed ({}), falling back to CLI",
                        reason
                    );
                    self.mark_fallback(&reason);
                    self.cli.get_properties(name).await?
                }
            }
        } else {
            log::debug!("DBus not available for get_properties, using CLI");
            self.mark_fallback("DBus not available");
            self.cli.get_properties(name).await?
        };

        // Enrich with entry-derived fields
        if !entry.all_addresses.is_empty() {
            props.insert(
                crate::nspawn::models::GROUP_MACHINE,
                "IPAddresses".into(),
                entry.all_addresses.join(", "),
            );
        }
        if let Some(ufs) = props
            .get_group(crate::nspawn::models::GROUP_SYSTEMD_UNIT)
            .and_then(|group| group.get("UnitFileState"))
        {
            let ufs = ufs.clone();
            props.insert(
                crate::nspawn::models::GROUP_SYSTEMD_UNIT,
                "Enabled".into(),
                ufs,
            );
        }
        Ok(props)
    }

    async fn is_dbus_available(&self) -> bool {
        if self.cli_mode {
            log::debug!("is_dbus_available → false (cli_mode active)");
            return false;
        }
        self.dbus.is_available().await
    }

    fn did_fallback(&self) -> Option<String> {
        self.last_fallback_reason.lock().take()
    }

    async fn watch(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) {
        let dbus_available = if self.cli_mode {
            false
        } else {
            self.dbus.is_available().await
        };
        let cli_observer_active = std::sync::Arc::new(AtomicBool::new(!dbus_available));

        // 1. DBus Engine: Instant lifecycle updates via signals.
        //    Falls back to CLI polling if the signal stream closes or DBus is
        //    unavailable.
        if dbus_available {
            let dbus_clone = self.dbus.clone();
            let cli_clone = self.cli.clone();
            let tx_dbus = tx.clone();
            let tx_cli = tx.clone();
            let cli_active = cli_observer_active.clone();
            tokio::spawn(async move {
                match dbus_clone.watch_events(tx_dbus).await {
                    Ok(()) => {}
                    Err(e) => {
                        log::warn!(
                            "DBus watcher unavailable ({}), falling back to CLI polling",
                            e
                        );
                        cli_active.store(true, Ordering::Release);
                        if let Err(e2) = cli_clone.watch_events(tx_cli).await {
                            log::error!("CLI watcher crashed: {}", e2);
                            cli_active.store(false, Ordering::Release);
                        }
                    }
                }
            });
        } else {
            // CLI watcher: poll-based lifecycle detection with action nudging
            let cli_clone = self.cli.clone();
            let tx_cli = tx.clone();
            let cli_active = cli_observer_active.clone();
            tokio::spawn(async move {
                if let Err(e) = cli_clone.watch_events(tx_cli).await {
                    log::error!("CLI watcher crashed: {}", e);
                    cli_active.store(false, Ordering::Release);
                }
            });
        }

        // 2. FS Engine: Inotify for images/storage changes
        let tx_fs = tx.clone();
        let paths = self.get_watch_paths();
        let cli_active = cli_observer_active.clone();
        let nudge_fs = self.nudge_tx.clone();
        tokio::spawn(async move {
            let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();

            let mut watcher = match RecommendedWatcher::new(
                move |res: std::result::Result<Event, notify::Error>| {
                    if let Ok(event) = &res {
                        // Creation/removal and atomic renames change image or
                        // runtime-registration membership. Ordinary content
                        // writes below machine roots remain ignored because
                        // these watches are non-recursive.
                        if matches!(
                            event.kind,
                            notify::EventKind::Create(_)
                                | notify::EventKind::Remove(_)
                                | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                        ) {
                            let _ = notify_tx.send(());
                        }
                    }
                },
                Config::default(),
            ) {
                Ok(w) => w,
                Err(e) => {
                    log::error!("Failed to create FS watcher: {}. Inotify-based refresh disabled; relying on heartbeat.", e);
                    return;
                }
            };

            for path in paths {
                if path.exists() {
                    if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                        log::warn!(
                            "Filesystem watcher unavailable for {} ({}); relying on DBus events and heartbeat refresh",
                            path.display(),
                            e
                        );
                    }
                } else {
                    log::warn!("Watch path does not exist: {}", path.display());
                }
            }

            loop {
                if notify_rx.recv().await.is_some() {
                    // Wait 200ms to consolidate burst events
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    while notify_rx.try_recv().is_ok() {}
                    if cli_active.load(Ordering::Acquire) {
                        let _ = nudge_fs.send(());
                    } else if tx_fs.send(StatusUpdate::Dirty).await.is_err() {
                        break;
                    }
                }
            }
        });

        // 3. Heartbeat Engine: DBus safety net. The CLI observer performs its
        // own five-second snapshot polling and needs no second timer.
        let tx_hb = tx.clone();
        let cli_active = cli_observer_active.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(15));
            interval.tick().await;
            loop {
                interval.tick().await;
                if !cli_active.load(Ordering::Acquire)
                    && tx_hb.send(StatusUpdate::Dirty).await.is_err()
                {
                    break;
                }
            }
        });

        if dbus_available {
            let _ = tx.send(StatusUpdate::Dirty).await;
        }
    }

    fn get_watch_paths(&self) -> Vec<PathBuf> {
        self.watch_paths.clone()
    }
}

/// Wrap a raw pipe fd in a tokio async pipe `Receiver` so the runtime can
/// cancel reads during shutdown (unlike `spawn_blocking`).
///
/// Uses the checked [`Receiver::from_owned_fd`] which verifies the fd is a
/// pipe, is readable, and sets O_NONBLOCK.
fn pipe_reader(fd: std::os::unix::io::RawFd) -> std::io::Result<tokio::net::unix::pipe::Receiver> {
    use std::os::fd::OwnedFd;
    use std::os::unix::io::FromRawFd;
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    tokio::net::unix::pipe::Receiver::from_owned_fd(owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::adapters::comm::backend::{
        ContainerBackend, MachineControl, MockContainerBackend, RuntimeSource,
    };
    use crate::nspawn::errors::NspawnError;
    use crate::nspawn::models::ContainerState;
    use crate::nspawn::ops::DefaultPermissionManager;
    fn dummy_entry(name: &str) -> ContainerEntry {
        ContainerEntry {
            name: name.to_string(),
            state: ContainerState::Running,
            address: None,
            all_addresses: vec![],
        }
    }

    fn dummy_snapshot(name: &str) -> RuntimeSnapshot {
        RuntimeSnapshot::new(vec![dummy_entry(name)], vec![])
    }

    fn systemd_properties(entries: &[(&str, &str)]) -> MachineProperties {
        let mut properties = MachineProperties::default();
        for (key, value) in entries {
            properties.insert(
                crate::nspawn::models::GROUP_SYSTEMD_UNIT,
                (*key).into(),
                (*value).into(),
            );
        }
        properties
    }

    fn test_pm() -> std::sync::Arc<dyn crate::nspawn::ops::PermissionManager> {
        std::sync::Arc::new(DefaultPermissionManager::new())
    }

    fn test_exec_ctx() -> std::sync::Arc<crate::nspawn::sys::ExecutionContext> {
        std::sync::Arc::new(
            crate::nspawn::sys::ExecutionContext::new(
                crate::nspawn::ops::PermissionLevel::User,
                None,
            )
            .unwrap(),
        )
    }

    fn mock_dbus_available() -> MockContainerBackend {
        let mut backend = MockContainerBackend::new();
        backend.expect_is_available().returning(|| true);
        backend
    }

    fn mock_dbus_unavailable() -> MockContainerBackend {
        let mut backend = MockContainerBackend::new();
        backend.expect_is_available().returning(|| false);
        backend
    }

    struct SharedMockBackend(std::sync::Arc<MockContainerBackend>);

    #[async_trait::async_trait]
    impl RuntimeSource for SharedMockBackend {
        async fn is_available(&self) -> bool {
            self.0.is_available().await
        }

        async fn list_machines(&self) -> Result<Vec<ContainerEntry>> {
            self.0.list_machines().await
        }

        async fn list_images(&self) -> Result<Vec<ImageEntry>> {
            self.0.list_images().await
        }

        async fn snapshot(&self) -> Result<RuntimeSnapshot> {
            self.0.snapshot().await
        }

        async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
            self.0.get_properties(name).await
        }

        async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()> {
            self.0.watch_events(tx).await
        }
    }

    #[async_trait::async_trait]
    impl MachineControl for SharedMockBackend {
        async fn start(&self, name: &str) -> Result<()> {
            self.0.start(name).await
        }

        async fn terminate(&self, name: &str) -> Result<()> {
            self.0.terminate(name).await
        }

        async fn poweroff(&self, name: &str) -> Result<()> {
            self.0.poweroff(name).await
        }

        async fn reboot(&self, name: &str) -> Result<()> {
            self.0.reboot(name).await
        }

        async fn enable(&self, name: &str) -> Result<()> {
            self.0.enable(name).await
        }

        async fn disable(&self, name: &str) -> Result<()> {
            self.0.disable(name).await
        }

        async fn kill(
            &self,
            name: &str,
            signal: crate::nspawn::models::AllowedSignal,
        ) -> Result<()> {
            self.0.kill(name, signal).await
        }

        async fn remove(&self, name: &str) -> Result<()> {
            self.0.remove(name).await
        }

        async fn reload_daemon(&self) -> Result<()> {
            self.0.reload_daemon().await
        }
    }

    fn test_manager(
        dbus: MockContainerBackend,
        cli: MockContainerBackend,
        cli_mode: bool,
    ) -> DefaultManager {
        let dbus = std::sync::Arc::new(dbus);
        let cli = std::sync::Arc::new(cli);
        let dbus_runtime: std::sync::Arc<dyn RuntimeSource> =
            std::sync::Arc::new(SharedMockBackend(dbus.clone()));
        let dbus_control: std::sync::Arc<dyn MachineControl> =
            std::sync::Arc::new(SharedMockBackend(dbus));
        let cli_runtime: std::sync::Arc<dyn RuntimeSource> =
            std::sync::Arc::new(SharedMockBackend(cli.clone()));
        let cli_control: std::sync::Arc<dyn MachineControl> =
            std::sync::Arc::new(SharedMockBackend(cli));
        DefaultManager {
            pm: test_pm(),
            cli_mode,
            dbus: dbus_runtime,
            cli: cli_runtime,
            dbus_control,
            cli_control,
            exec_ctx: test_exec_ctx(),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        }
    }

    #[tokio::test]
    async fn remove_allows_hidden_images_without_origin_classification() {
        let dbus = mock_dbus_unavailable();
        let mut cli = MockContainerBackend::new();
        cli.expect_list_machines().returning(|| Ok(vec![]));
        cli.expect_remove().returning(|_| Ok(()));
        let mgr = test_manager(dbus, cli, false);

        let result = mgr.remove(".download").await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn remove_allows_hidden_images_of_unknown_type() {
        let dbus = mock_dbus_unavailable();
        let mut cli = MockContainerBackend::new();
        cli.expect_list_machines().returning(|| Ok(vec![]));
        cli.expect_remove().returning(|_| Ok(()));
        let mgr = test_manager(dbus, cli, false);

        let result = mgr.remove(".unclassified-image").await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn remove_hidden_image_uses_dbus_image_name_validation() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_machines().returning(|| Ok(vec![]));
        dbus.expect_remove()
            .withf(|name| name == ".oci-sha256:abc")
            .returning(|_| Ok(()));

        let mgr = test_manager(dbus, MockContainerBackend::new(), false);

        mgr.remove(".oci-sha256:abc").await.unwrap();
        assert!(mgr.did_fallback().is_none());
    }

    #[tokio::test]
    async fn remove_hidden_image_falls_back_only_after_a_real_dbus_failure() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_machines().returning(|| Ok(vec![]));
        dbus.expect_remove().returning(|_| {
            Err(NspawnError::Dbus(zbus::Error::Failure(
                "machined unavailable".into(),
            )))
        });

        let mut cli = MockContainerBackend::new();
        cli.expect_remove()
            .withf(|name| name == ".oci-sha256:abc")
            .returning(|_| Ok(()));

        let mgr = test_manager(dbus, cli, false);

        mgr.remove(".oci-sha256:abc").await.unwrap();
        assert!(mgr.did_fallback().is_some());
    }

    #[tokio::test]
    async fn remove_rejects_protected_and_temporary_image_names_before_routing() {
        let mgr = test_manager(
            MockContainerBackend::new(),
            MockContainerBackend::new(),
            false,
        );

        assert!(matches!(
            mgr.remove(".host").await.unwrap_err(),
            NspawnError::Validation(_)
        ));
        assert!(matches!(
            mgr.remove(".#temporary").await.unwrap_err(),
            NspawnError::Validation(_)
        ));
    }

    #[tokio::test]
    async fn remove_rechecks_running_machine_state() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_machines().returning(|| {
            let mut entry = dummy_entry("active");
            entry.state = ContainerState::Running;
            Ok(vec![entry])
        });
        let mgr = test_manager(dbus, MockContainerBackend::new(), false);

        let error = mgr.remove("active").await.unwrap_err();

        assert!(matches!(error, NspawnError::ContainerAlreadyRunning(_)));
    }

    // list_machines

    #[tokio::test]
    async fn test_list_machines_uses_dbus_when_available() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_machines()
            .returning(|| Ok(vec![dummy_entry("dbus-entry")]));

        let cli = MockContainerBackend::new();

        let mgr = test_manager(dbus, cli, false);

        let entries = mgr.list_machines().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "dbus-entry");
        assert!(mgr.did_fallback().is_none());
    }

    #[tokio::test]
    async fn test_list_machines_falls_back_to_cli_when_dbus_fails() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_machines()
            .returning(|| Err(NspawnError::Dbus(zbus::Error::Failure("dbus down".into()))));

        let mut cli = MockContainerBackend::new();
        cli.expect_list_machines()
            .returning(|| Ok(vec![dummy_entry("cli-entry")]));

        let mgr = test_manager(dbus, cli, false);

        let entries = mgr.list_machines().await.unwrap();
        assert_eq!(entries[0].name, "cli-entry");
        assert!(mgr.did_fallback().is_some());
    }

    #[tokio::test]
    async fn test_list_machines_uses_cli_when_dbus_unavailable() {
        let dbus = mock_dbus_unavailable();

        let mut cli = MockContainerBackend::new();
        cli.expect_list_machines()
            .returning(|| Ok(vec![dummy_entry("cli-only")]));

        let mgr = test_manager(dbus, cli, false);

        let entries = mgr.list_machines().await.unwrap();
        assert_eq!(entries[0].name, "cli-only");
        assert!(mgr.did_fallback().is_some());
    }

    #[tokio::test]
    async fn snapshot_falls_back_as_one_backend_unit() {
        let mut dbus = mock_dbus_available();
        dbus.expect_snapshot()
            .returning(|| Err(NspawnError::Dbus(zbus::Error::Failure("dbus down".into()))));

        let mut cli = MockContainerBackend::new();
        cli.expect_snapshot()
            .returning(|| Ok(dummy_snapshot("cli-snapshot")));

        let mgr = test_manager(dbus, cli, false);

        let snapshot = mgr.snapshot().await.unwrap();
        assert_eq!(snapshot.machines[0].name, "cli-snapshot");
        assert!(mgr.did_fallback().is_some());
    }

    #[tokio::test]
    async fn cli_mode_snapshot_does_not_probe_dbus() {
        let dbus = MockContainerBackend::new();
        let mut cli = MockContainerBackend::new();
        cli.expect_snapshot()
            .returning(|| Ok(dummy_snapshot("cli-only")));

        let mgr = test_manager(dbus, cli, true);

        let snapshot = mgr.snapshot().await.unwrap();
        assert_eq!(snapshot.machines[0].name, "cli-only");
        assert!(mgr.did_fallback().is_none());
    }

    #[tokio::test]
    async fn cli_mode_inspect_does_not_call_either_transport_backend() {
        let mgr = test_manager(
            MockContainerBackend::new(),
            MockContainerBackend::new(),
            true,
        );
        let entry = dummy_entry("lasper-cli-inspect-test");

        let properties = mgr
            .get_properties("lasper-cli-inspect-test", &entry)
            .await
            .unwrap();

        assert_eq!(properties.get_summary()[0].0, "Name");
        assert_eq!(properties.get_summary()[0].1, "lasper-cli-inspect-test");
        assert!(mgr.did_fallback().is_none());
    }

    #[tokio::test]
    async fn cli_mode_watcher_does_not_probe_dbus() {
        let dbus = MockContainerBackend::new();
        let mut cli = MockContainerBackend::new();
        cli.expect_watch_events().returning(|tx| {
            tx.try_send(StatusUpdate::BackendFailure {
                message: "observer started".into(),
                consecutive_failures: 1,
            })
            .unwrap();
            Ok(())
        });

        let mgr = test_manager(dbus, cli, true);
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);

        mgr.watch(tx).await;

        let update = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(update, StatusUpdate::BackendFailure { .. }));
    }

    // fallback paths

    #[tokio::test]
    async fn test_start_falls_back_to_cli_when_dbus_fails() {
        let mut dbus = mock_dbus_available();
        dbus.expect_reload_daemon().returning(|| Ok(()));
        dbus.expect_start()
            .returning(|_| Err(NspawnError::Dbus(zbus::Error::Failure("fail".into()))));

        let mut cli = MockContainerBackend::new();
        cli.expect_start().returning(|_| Ok(()));
        cli.expect_get_properties().returning(|_| {
            Ok(systemd_properties(&[
                ("ActiveState", "active"),
                ("SubState", "running"),
                ("Result", "success"),
            ]))
        });

        let mgr = test_manager(dbus, cli, false);

        mgr.start("test").await.unwrap();
        assert!(mgr.did_fallback().is_some());
    }

    #[test]
    fn start_observation_requires_active_and_reports_service_failure() {
        let active = systemd_properties(&[
            ("ActiveState", "active"),
            ("SubState", "running"),
            ("Result", "success"),
        ]);
        assert_eq!(observe_start(&active), StartObservation::Started);

        let failed = systemd_properties(&[
            ("ActiveState", "failed"),
            ("SubState", "failed"),
            ("Result", "exit-code"),
            ("ExecMainStatus", "1"),
        ]);
        assert_eq!(
            observe_start(&failed),
            StartObservation::Failed(
                "ActiveState=failed, SubState=failed, Result=exit-code, ExecMainStatus=1".into()
            )
        );

        let pending = systemd_properties(&[
            ("ActiveState", "activating"),
            ("SubState", "start"),
            ("Result", "success"),
        ]);
        assert!(matches!(
            observe_start(&pending),
            StartObservation::Pending(_)
        ));

        assert!(valid_invocation_id("0123456789abcdef0123456789ABCDEF"));
        assert!(!valid_invocation_id("[not set]"));
        assert!(!valid_invocation_id("0123456789abcdef"));
    }

    // enable/disable now use DBus-first fallback

    #[tokio::test]
    async fn test_enable_calls_dbus_first() {
        let mut dbus = mock_dbus_available();
        dbus.expect_enable()
            .with(mockall::predicate::eq("test-ctr"))
            .returning(|_| Ok(()));

        let cli = MockContainerBackend::new();

        let mgr = test_manager(dbus, cli, false);

        mgr.enable("test-ctr").await.unwrap();
        assert!(mgr.did_fallback().is_none());
    }

    #[tokio::test]
    async fn test_disable_falls_back_to_cli_when_dbus_fails() {
        let mut dbus = mock_dbus_available();
        dbus.expect_disable()
            .returning(|_| Err(NspawnError::Dbus(zbus::Error::Failure("fail".into()))));

        let mut cli = MockContainerBackend::new();
        cli.expect_disable()
            .with(mockall::predicate::eq("test-ctr"))
            .returning(|_| Ok(()));

        let mgr = test_manager(dbus, cli, false);

        mgr.disable("test-ctr").await.unwrap();
        assert!(mgr.did_fallback().is_some());
    }

    // did_fallback state

    #[tokio::test]
    async fn test_did_fallback_clears_after_read() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_machines()
            .returning(|| Err(NspawnError::Dbus(zbus::Error::Failure("fail".into()))));

        let mut cli = MockContainerBackend::new();
        cli.expect_list_machines()
            .returning(|| Ok(vec![dummy_entry("test")]));

        let mgr = test_manager(dbus, cli, false);

        mgr.list_machines().await.unwrap();
        assert!(mgr.did_fallback().is_some());
        assert!(mgr.did_fallback().is_none());
    }
}
