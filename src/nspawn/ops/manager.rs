use crate::nspawn::adapters::comm::backend::ContainerBackend;
use crate::nspawn::adapters::comm::cli::CliBackend;
use crate::nspawn::adapters::comm::daemon_backend::DaemonBackend;
use crate::nspawn::adapters::comm::dbus::DbusBackend;
use crate::nspawn::errors::Result;
use crate::nspawn::models::{AllowedSignal, ContainerEntry, MachineProperties};
use crate::nspawn::ops::{PermissionLevel, PermissionManager};
use crate::nspawn::sys::ExecutionContext;
use async_trait::async_trait;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use tokio::io::AsyncBufReadExt;

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait NspawnManager: Send + Sync + 'static {
    async fn list_all(&self) -> Result<Vec<ContainerEntry>>;
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
    async fn remove(&self, name: &str) -> Result<()>;
    async fn is_dbus_available(&self) -> bool;
    fn did_fallback(&self) -> Option<String>;
    async fn watch(&self, tx: tokio::sync::mpsc::Sender<()>);
    fn get_watch_paths(&self) -> Vec<PathBuf>;
}

pub struct DefaultManager {
    #[allow(dead_code)] // used in constructor and tests via struct literals
    pm: std::sync::Arc<dyn PermissionManager>,
    cli_mode: bool,
    dbus: std::sync::Arc<dyn ContainerBackend>,
    cli: std::sync::Arc<dyn ContainerBackend>,
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
            log::info!("CLI mode active — DBus backend disabled");
        }

        let dbus_backend: std::sync::Arc<dyn ContainerBackend> = match pm.level() {
            PermissionLevel::Elevated => {
                log::info!("Elevated mode — DBus operations proxied through daemon");
                std::sync::Arc::new(DaemonBackend::new(
                    exec_ctx
                        .daemon_ref()
                        .cloned()
                        .expect("daemon required for Elevated mode"),
                ))
            }
            _ => std::sync::Arc::new(DbusBackend::new()),
        };

        let cli_backend = CliBackend::new(exec_ctx.cmd.clone());
        let (nudge_tx, nudge_rx) = tokio::sync::watch::channel(());
        cli_backend.set_nudge(nudge_rx);
        Self {
            pm,
            cli_mode,
            dbus: dbus_backend,
            cli: std::sync::Arc::new(cli_backend),
            exec_ctx,
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![crate::paths::machines_dir()],
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

    fn elevated_io(&self) -> crate::nspawn::sys::ElevatedIo {
        self.exec_ctx.io.clone()
    }

    async fn _ensure_gpu_passthrough(&self, name: &str) -> Result<()> {
        let io = self.elevated_io();
        crate::nspawn::platform::nvidia::ensure_gpu_passthrough(
            name,
            &io,
            &self.exec_ctx.nspawn,
            self.exec_ctx.cmd.as_ref(),
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
            match self.dbus.reload_daemon().await {
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
        self.cli.reload_daemon().await.map_err(|e| {
            log::error!("CLI reload_daemon failed: {}", e);
            e
        })
    }
}

/// Try DBus first, then fall back to CLI with consistent logging and error reporting.
/// When Elevated, the CLI backend runs via `sudo`, making elevated DBus calls through
/// `machinectl`.
macro_rules! fallback_to_cli {
    ($self:ident, $method:ident, $name:expr $(, $arg:expr)*) => {{
        let result = if !$self.cli_mode && $self.dbus.is_available().await {
            match $self.dbus.$method($name, $($arg),*).await {
                Ok(v) => Ok(v),
                Err(e) => {
                    let reason = Self::classify_fallback(&e);
                    log::warn!(
                        "DBus {} failed ({}), falling back to CLI",
                        stringify!($method),
                        reason
                    );
                    $self.mark_fallback(&reason);
                    $self.cli.$method($name, $($arg),*).await
                }
            }
        } else {
            log::debug!("DBus not available for {}, using CLI", stringify!($method));
            $self.mark_fallback("DBus not available");
            $self.cli.$method($name, $($arg),*).await
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
    async fn list_all(&self) -> Result<Vec<ContainerEntry>> {
        if !self.cli_mode && self.dbus.is_available().await {
            match self.dbus.list_all().await {
                Ok(entries) => return Ok(entries),
                Err(e) => {
                    let reason = Self::classify_fallback(&e);
                    log::warn!("DBus list_all failed ({}), falling back to CLI", reason);
                    self.mark_fallback(&reason);
                }
            }
        } else {
            log::debug!("DBus not available for list_all, using CLI");
            self.mark_fallback("DBus not available");
        }
        self.cli.list_all().await.map_err(|e| {
            log::error!("CLI list_all failed: {}", e);
            e
        })
    }

    async fn start(&self, name: &str) -> Result<()> {
        self._ensure_gpu_passthrough(name).await?;
        fallback_to_cli!(self, start, name)
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
        let result = if !self.cli_mode && self.dbus.is_available().await {
            match self.dbus.remove(name).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let reason = Self::classify_fallback(&e);
                    log::warn!("DBus remove failed ({}), falling back to CLI", reason);
                    self.mark_fallback(&reason);
                    self.cli.remove(name).await.map_err(|e| {
                        log::error!("CLI remove failed for {}: {}", name, e);
                        e
                    })
                }
            }
        } else {
            self.cli.remove(name).await.map_err(|e| {
                log::error!("CLI remove failed for {}: {}", name, e);
                e
            })
        };

        result?;

        // systemd may or may not clean these up — an extra unlink is harmless
        let io = self.elevated_io();
        let _ = self.exec_ctx.nspawn.remove(name).await;
        let _ = self.exec_ctx.systemd_unit.remove_overrides(name).await;
        let _ = io.remove_file(&crate::paths::state_file(name)).await;

        self.nudge();
        Ok(())
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
        let mut props = if !self.cli_mode && self.dbus.is_available().await {
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
            .get_group_mut(crate::nspawn::models::GROUP_SYSTEMD_UNIT)
            .get("UnitFileState")
        {
            let ufs = ufs.clone();
            props.insert(
                crate::nspawn::models::GROUP_SYSTEMD_UNIT,
                "Enabled".into(),
                ufs,
            );
        }
        if let Some(image_type) = &entry.image_type {
            if let Some(machine_type) = props
                .get_group_mut(crate::nspawn::models::GROUP_MACHINE)
                .remove("Type")
            {
                props.insert(
                    crate::nspawn::models::GROUP_MACHINE,
                    "Class".into(),
                    machine_type,
                );
            }
            props.insert(
                crate::nspawn::models::GROUP_MACHINE,
                "Type".into(),
                image_type.clone(),
            );
        }
        if !entry.state.is_running() {
            props.insert(
                crate::nspawn::models::GROUP_MACHINE,
                "ReadOnly".into(),
                entry.readonly.to_string(),
            );
            if let Some(u) = &entry.usage {
                props.insert(
                    crate::nspawn::models::GROUP_MACHINE,
                    "Usage".into(),
                    u.clone(),
                );
            }
            props.insert(
                crate::nspawn::models::GROUP_MACHINE,
                "State".into(),
                entry.state.label().into(),
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

    async fn watch(&self, tx: tokio::sync::mpsc::Sender<()>) {
        let dbus_available = self.dbus.is_available().await;

        // 1. DBus Engine: Instant lifecycle updates via signals.
        //    Falls back to CLI polling when watch_events is unsupported
        //    (e.g. DaemonBackend without streaming RPC) or DBus is down.
        if dbus_available {
            let dbus_clone = self.dbus.clone();
            let cli_clone = self.cli.clone();
            let tx_dbus = tx.clone();
            let tx_cli = tx.clone();
            tokio::spawn(async move {
                match dbus_clone.watch_events(tx_dbus).await {
                    Ok(()) => {}
                    Err(e) => {
                        log::warn!(
                            "DBus watcher unavailable ({}), falling back to CLI polling",
                            e
                        );
                        if let Err(e2) = cli_clone.watch_events(tx_cli).await {
                            log::error!("CLI watcher crashed: {}", e2);
                        }
                    }
                }
            });
        } else {
            // CLI watcher: poll-based lifecycle detection with action nudging
            let cli_clone = self.cli.clone();
            let tx_cli = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = cli_clone.watch_events(tx_cli).await {
                    log::error!("CLI watcher crashed: {}", e);
                }
            });
        }

        // 2. FS Engine: Inotify for images/storage changes
        let tx_fs = tx.clone();
        let paths = self.get_watch_paths();
        tokio::spawn(async move {
            let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();

            let mut watcher = match RecommendedWatcher::new(
                move |res: std::result::Result<Event, notify::Error>| {
                    if let Ok(event) = &res {
                        // Only react to files appearing or disappearing;
                        // ignore Modify events from container disk writes.
                        if matches!(
                            event.kind,
                            notify::EventKind::Create(_) | notify::EventKind::Remove(_)
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
                        log::error!("Failed to watch path {}: {}", path.display(), e);
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
                    let _ = tx_fs.send(()).await;
                }
            }
        });

        // 3. Heartbeat Engine: Safety net (15s)
        let tx_hb = tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(15));
            loop {
                interval.tick().await;
                log::debug!("Heartbeat nudge");
                let _ = tx_hb.send(()).await;
            }
        });
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
    use crate::nspawn::adapters::comm::backend::MockContainerBackend;
    use crate::nspawn::errors::NspawnError;
    use crate::nspawn::models::ContainerState;
    use crate::nspawn::ops::DefaultPermissionManager;
    fn dummy_entry(name: &str) -> ContainerEntry {
        ContainerEntry {
            name: name.to_string(),
            state: ContainerState::Off,
            image_type: None,
            readonly: false,
            usage: None,
            address: None,
            all_addresses: vec![],
        }
    }

    fn test_pm() -> std::sync::Arc<dyn crate::nspawn::ops::PermissionManager> {
        std::sync::Arc::new(DefaultPermissionManager::new())
    }

    fn test_exec_ctx() -> std::sync::Arc<crate::nspawn::sys::ExecutionContext> {
        std::sync::Arc::new(crate::nspawn::sys::ExecutionContext::new(
            crate::nspawn::ops::PermissionLevel::User,
            None,
        ))
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

    // list_all

    #[tokio::test]
    async fn test_list_all_uses_dbus_when_available() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_all()
            .returning(|| Ok(vec![dummy_entry("dbus-entry")]));

        let cli = MockContainerBackend::new();

        let mgr = DefaultManager {
            pm: test_pm(),
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            exec_ctx: test_exec_ctx(),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        let entries = mgr.list_all().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "dbus-entry");
        assert!(mgr.did_fallback().is_none());
    }

    #[tokio::test]
    async fn test_list_all_falls_back_to_cli_when_dbus_fails() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_all()
            .returning(|| Err(NspawnError::Dbus(zbus::Error::Failure("dbus down".into()))));

        let mut cli = MockContainerBackend::new();
        cli.expect_list_all()
            .returning(|| Ok(vec![dummy_entry("cli-entry")]));

        let mgr = DefaultManager {
            pm: test_pm(),
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            exec_ctx: test_exec_ctx(),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        let entries = mgr.list_all().await.unwrap();
        assert_eq!(entries[0].name, "cli-entry");
        assert!(mgr.did_fallback().is_some());
    }

    #[tokio::test]
    async fn test_list_all_uses_cli_when_dbus_unavailable() {
        let dbus = mock_dbus_unavailable();

        let mut cli = MockContainerBackend::new();
        cli.expect_list_all()
            .returning(|| Ok(vec![dummy_entry("cli-only")]));

        let mgr = DefaultManager {
            pm: test_pm(),
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            exec_ctx: test_exec_ctx(),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        let entries = mgr.list_all().await.unwrap();
        assert_eq!(entries[0].name, "cli-only");
        assert!(mgr.did_fallback().is_some());
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

        let mgr = DefaultManager {
            pm: test_pm(),
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            exec_ctx: test_exec_ctx(),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        mgr.start("test").await.unwrap();
        assert!(mgr.did_fallback().is_some());
    }

    // enable/disable now use DBus-first fallback

    #[tokio::test]
    async fn test_enable_calls_dbus_first() {
        let mut dbus = mock_dbus_available();
        dbus.expect_enable()
            .with(mockall::predicate::eq("test-ctr"))
            .returning(|_| Ok(()));

        let cli = MockContainerBackend::new();

        let mgr = DefaultManager {
            pm: test_pm(),
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            exec_ctx: test_exec_ctx(),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

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

        let mgr = DefaultManager {
            pm: test_pm(),
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            exec_ctx: test_exec_ctx(),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        mgr.disable("test-ctr").await.unwrap();
        assert!(mgr.did_fallback().is_some());
    }

    // did_fallback state

    #[tokio::test]
    async fn test_did_fallback_clears_after_read() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_all()
            .returning(|| Err(NspawnError::Dbus(zbus::Error::Failure("fail".into()))));

        let mut cli = MockContainerBackend::new();
        cli.expect_list_all()
            .returning(|| Ok(vec![dummy_entry("test")]));

        let mgr = DefaultManager {
            pm: test_pm(),
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            exec_ctx: test_exec_ctx(),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        mgr.list_all().await.unwrap();
        assert!(mgr.did_fallback().is_some());
        assert!(mgr.did_fallback().is_none());
    }
}
