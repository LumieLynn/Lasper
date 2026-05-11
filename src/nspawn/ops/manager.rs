use crate::nspawn::adapters::comm::backend::ContainerBackend;
use crate::nspawn::adapters::comm::cli::CliBackend;
use crate::nspawn::adapters::comm::dbus::DbusBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, MachineProperties};
use async_trait::async_trait;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
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
        tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    ) -> tokio::task::JoinHandle<()>;
    async fn get_properties(
        &self,
        name: &str,
        entry: &ContainerEntry,
    ) -> Result<MachineProperties>;
    async fn enable(&self, name: &str) -> Result<()>;
    async fn disable(&self, name: &str) -> Result<()>;
    async fn poweroff(&self, name: &str) -> Result<()>;
    async fn reboot(&self, name: &str) -> Result<()>;
    async fn kill(&self, name: &str, signal: &str) -> Result<()>;
    async fn remove(&self, name: &str) -> Result<()>;
    async fn is_dbus_available(&self) -> bool;
    fn did_fallback(&self) -> Option<String>;
    async fn watch(&self, tx: tokio::sync::mpsc::Sender<()>);
    fn get_watch_paths(&self) -> Vec<PathBuf>;
}

pub struct DefaultManager {
    is_root: bool,
    cli_mode: bool,
    dbus: std::sync::Arc<dyn ContainerBackend>,
    cli: std::sync::Arc<dyn ContainerBackend>,
    last_fallback_reason: parking_lot::Mutex<Option<String>>,
    watch_paths: Vec<PathBuf>,
    nudge_tx: tokio::sync::watch::Sender<()>,
}

impl DefaultManager {
    pub fn new(is_root: bool, cli_mode: bool) -> Self {
        if cli_mode {
            log::info!("CLI mode active — DBus backend disabled");
        }
        let cli_backend = CliBackend::new(is_root);
        let (nudge_tx, nudge_rx) = tokio::sync::watch::channel(());
        cli_backend.set_nudge(nudge_rx);
        Self {
            is_root,
            cli_mode,
            dbus: std::sync::Arc::new(DbusBackend::new()),
            cli: std::sync::Arc::new(cli_backend),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![crate::paths::machines_dir()],
            nudge_tx,
        }
    }

    fn require_root(&self) -> Result<()> {
        if !self.is_root {
            Err(NspawnError::PermissionDenied)
        } else {
            Ok(())
        }
    }

    fn mark_fallback(&self, reason: &str) {
        if self.cli_mode {
            return;
        }
        *self.last_fallback_reason.lock() = Some(reason.to_string());
    }

    fn nudge(&self) {
        let _ = self.nudge_tx.send(());
    }

    async fn _ensure_gpu_passthrough(&self, name: &str) -> Result<()> {
        crate::nspawn::platform::nvidia::ensure_gpu_passthrough(name).await?;
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
                    log::warn!("DBus reload_daemon failed, falling back to CLI: {}", e);
                    self.mark_fallback(&format!("{}", e));
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
macro_rules! fallback_to_cli {
    ($self:ident, $method:ident, $name:expr $(, $arg:expr)*) => {{
        let result = if !$self.cli_mode && $self.dbus.is_available().await {
            match $self.dbus.$method($name, $($arg),*).await {
                Ok(v) => Ok(v),
                Err(e) => {
                    log::warn!("DBus {} failed, falling back to CLI: {}", stringify!($method), e);
                    $self.mark_fallback(&format!("{}", e));
                    $self.cli.$method($name, $($arg),*).await
                }
            }
        } else {
            log::warn!("DBus not available for {}, falling back to CLI", stringify!($method));
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
        if !self.is_root {
            return self.cli.list_all().await;
        }
        if !self.cli_mode && self.dbus.is_available().await {
            match self.dbus.list_all().await {
                Ok(entries) => return Ok(entries),
                Err(e) => {
                    log::warn!("DBus list_all failed, falling back to CLI: {}", e);
                    self.mark_fallback(&format!("{}", e));
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
        self.require_root()?;
        self._ensure_gpu_passthrough(name).await?;
        fallback_to_cli!(self, start, name)
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        self.require_root()?;
        fallback_to_cli!(self, terminate, name)
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        self.require_root()?;
        fallback_to_cli!(self, poweroff, name)
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.require_root()?;
        fallback_to_cli!(self, reboot, name)
    }

    async fn enable(&self, name: &str) -> Result<()> {
        self.require_root()?;
        fallback_to_cli!(self, enable, name)
    }

    async fn disable(&self, name: &str) -> Result<()> {
        self.require_root()?;
        fallback_to_cli!(self, disable, name)
    }

    async fn kill(&self, name: &str, signal: &str) -> Result<()> {
        self.require_root()?;
        fallback_to_cli!(self, kill, name, signal)
    }

    async fn remove(&self, name: &str) -> Result<()> {
        self.require_root()?;

        let result = if !self.cli_mode && self.dbus.is_available().await {
            match self.dbus.remove(name).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    log::warn!("DBus remove failed, falling back to CLI: {}", e);
                    self.mark_fallback(&format!("{}", e));
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

        // systemd may or may not clean these up — an extra unlink is harmless
        let _ = tokio::fs::remove_file(
            crate::nspawn::adapters::config::nspawn_file::NspawnConfig::default_path(name),
        )
        .await;
        let _ = tokio::fs::remove_dir_all(format!(
            "/etc/systemd/system/systemd-nspawn@{}.service.d",
            name
        ))
        .await;
        let _ = tokio::fs::remove_file(
            crate::paths::state_file(name),
        )
        .await;

        if result.is_ok() {
            self.nudge();
        }
        result
    }

    fn spawn_log_stream(
        &self,
        name: &str,
        tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    ) -> tokio::task::JoinHandle<()> {
        let name = name.to_string();
        tokio::spawn(async move {
            let res: std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
                let mut child = tokio::process::Command::new("journalctl")
                    .args(["-M", &name, "-n", "1000", "-f", "--no-pager", "--output=short"])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;

                let mut lines =
                    tokio::io::BufReader::new(child.stdout.take().unwrap())
                        .lines();

                loop {
                    tokio::select! {
                        line_res = lines.next_line() => {
                            if let Ok(Some(line)) = line_res {
                                tx.send(crate::events::AppEvent::LogLine(line))
                                    .await
                                    .map_err(|_| "Channel closed")?;
                            } else {
                                break;
                            }
                        }
                        _ = child.wait() => break,
                    }
                }
                Ok(())
            }
            .await;

            if let Err(e) = res {
                tx.send(crate::events::AppEvent::LogLine(format!(
                    "Log stream stopped: {e}"
                )))
                .await
                .ok();
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
                    log::warn!("DBus get_properties failed, falling back to CLI: {}", e);
                    self.mark_fallback(&format!("{}", e));
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
        let dbus_available = self.is_root && self.dbus.is_available().await;

        // 1. DBus Engine: Instant lifecycle updates
        if dbus_available {
            let dbus_clone = self.dbus.clone();
            let tx_dbus = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = dbus_clone.watch_events(tx_dbus).await {
                    log::error!("DBus watcher crashed: {}", e);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::adapters::comm::backend::MockContainerBackend;
    use crate::nspawn::errors::NspawnError;
    use crate::nspawn::models::ContainerState;

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

        let cli = MockContainerBackend::new(); // cli should never be called

        let mgr = DefaultManager {
            is_root: true,
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
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
            is_root: true,
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
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
            is_root: true,
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        let entries = mgr.list_all().await.unwrap();
        assert_eq!(entries[0].name, "cli-only");
        assert!(mgr.did_fallback().is_some());
    }

    #[tokio::test]
    async fn test_list_all_non_root_uses_cli_only() {
        let dbus = MockContainerBackend::new(); // never called for non-root

        let mut cli = MockContainerBackend::new();
        cli.expect_list_all()
            .returning(|| Ok(vec![dummy_entry("non-root")]));

        let mgr = DefaultManager {
            is_root: false,
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        let entries = mgr.list_all().await.unwrap();
        assert_eq!(entries[0].name, "non-root");
    }

    // permission guard

    #[tokio::test]
    async fn test_start_requires_root() {
        let dbus = mock_dbus_available();
        let cli = MockContainerBackend::new();

        let mgr = DefaultManager {
            is_root: false,
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        let err = mgr.start("test").await.unwrap_err();
        assert!(matches!(err, NspawnError::PermissionDenied));
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
            is_root: true,
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
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

        let cli = MockContainerBackend::new(); // should never be called

        let mgr = DefaultManager {
            is_root: true,
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
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
            is_root: true,
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
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
            is_root: true,
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        mgr.list_all().await.unwrap();
        assert!(mgr.did_fallback().is_some()); // first read returns reason
        assert!(mgr.did_fallback().is_none()); // second returns None (taken)
    }

    // remove

    #[tokio::test]
    async fn test_remove_requires_root() {
        let dbus = mock_dbus_available();
        let cli = MockContainerBackend::new();

        let mgr = DefaultManager {
            is_root: false,
            cli_mode: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: parking_lot::Mutex::new(None),
            watch_paths: vec![],
            nudge_tx: tokio::sync::watch::channel(()).0,
        };

        let err = mgr.remove("test").await.unwrap_err();
        assert!(matches!(err, NspawnError::PermissionDenied));
    }
}
