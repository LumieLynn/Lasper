use crate::nspawn::adapters::comm::cli::{CliProvider, DefaultCliProvider};
use crate::nspawn::adapters::comm::dbus::{DbusProvider, DefaultDbusProvider};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, MachineProperties};
use async_trait::async_trait;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;

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
    async fn get_properties(&self, name: &str) -> Result<MachineProperties>;
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
    dbus: std::sync::Arc<dyn DbusProvider>,
    cli: std::sync::Arc<dyn CliProvider>,
    last_fallback_reason: std::sync::Mutex<Option<String>>,
    watch_paths: Vec<PathBuf>,
}

impl DefaultManager {
    pub fn new(is_root: bool) -> Self {
        Self {
            is_root,
            dbus: std::sync::Arc::new(DefaultDbusProvider::new()),
            cli: std::sync::Arc::new(DefaultCliProvider::new(is_root)),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![PathBuf::from("/var/lib/machines")],
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
        *self.last_fallback_reason.lock().unwrap() = Some(reason.to_string());
    }

    async fn _ensure_gpu_passthrough(&self, name: &str) -> Result<()> {
        crate::nspawn::platform::nvidia::ensure_gpu_passthrough(name, &*self.dbus).await
    }
}

#[async_trait]
impl NspawnManager for DefaultManager {
    async fn list_all(&self) -> Result<Vec<ContainerEntry>> {
        if !self.is_root {
            return self.cli.list_all().await;
        }
        if self.dbus.is_available().await {
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

        if self.dbus.is_available().await {
            match self.dbus.start(name).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    log::warn!("DBus start failed, falling back to CLI: {}", e);
                    self.mark_fallback(&format!("{}", e));
                }
            }
        } else {
            log::warn!("DBus not available for start, falling back to CLI");
            self.mark_fallback("DBus not available");
        }
        self.cli.start(name).await.map_err(|e| {
            log::error!("CLI start failed for {}: {}", name, e);
            e
        })
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        self.require_root()?;
        if self.dbus.is_available().await {
            match self.dbus.terminate(name).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    log::warn!("DBus terminate failed, falling back to CLI: {}", e);
                    self.mark_fallback(&format!("{}", e));
                }
            }
        } else {
            log::warn!("DBus not available for terminate, falling back to CLI");
            self.mark_fallback("DBus not available");
        }
        self.cli.terminate(name).await.map_err(|e| {
            log::error!("CLI terminate failed for {}: {}", name, e);
            e
        })
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        self.require_root()?;
        if self.dbus.is_available().await {
            match self.dbus.poweroff(name).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    log::warn!("DBus poweroff failed, falling back to CLI: {}", e);
                    self.mark_fallback(&format!("{}", e));
                }
            }
        } else {
            log::warn!("DBus not available for poweroff, falling back to CLI");
            self.mark_fallback("DBus not available");
        }
        self.cli.poweroff(name).await.map_err(|e| {
            log::error!("CLI poweroff failed for {}: {}", name, e);
            e
        })
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.require_root()?;
        if self.dbus.is_available().await {
            match self.dbus.reboot(name).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    log::warn!("DBus reboot failed, falling back to CLI: {}", e);
                    self.mark_fallback(&format!("{}", e));
                }
            }
        } else {
            log::warn!("DBus not available for reboot, falling back to CLI");
            self.mark_fallback("DBus not available");
        }
        self.cli.reboot(name).await.map_err(|e| {
            log::error!("CLI reboot failed for {}: {}", name, e);
            e
        })
    }

    async fn enable(&self, name: &str) -> Result<()> {
        self.require_root()?;
        self.cli.enable(name).await.map_err(|e| {
            log::error!("CLI enable failed for {}: {}", name, e);
            e
        })
    }

    async fn disable(&self, name: &str) -> Result<()> {
        self.require_root()?;
        self.cli.disable(name).await.map_err(|e| {
            log::error!("CLI disable failed for {}: {}", name, e);
            e
        })
    }

    async fn kill(&self, name: &str, signal: &str) -> Result<()> {
        self.require_root()?;
        if self.dbus.is_available().await {
            match self.dbus.kill(name, signal).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    log::warn!("DBus kill failed, falling back to CLI: {}", e);
                    self.mark_fallback(&format!("{}", e));
                }
            }
        } else {
            log::warn!("DBus not available for kill, falling back to CLI");
            self.mark_fallback("DBus not available");
        }
        self.cli.kill(name, signal).await.map_err(|e| {
            log::error!("CLI kill failed for {} (signal {}): {}", name, signal, e);
            e
        })
    }

    async fn remove(&self, name: &str) -> Result<()> {
        self.require_root()?;

        let result = if self.dbus.is_available().await {
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
        let _ =
            tokio::fs::remove_file(crate::nspawn::platform::nvidia::state::get_state_dir().join(
                format!("{}.json", name),
            ))
            .await;

        result
    }

    fn spawn_log_stream(
        &self,
        name: &str,
        tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    ) -> tokio::task::JoinHandle<()> {
        self.cli.spawn_log_stream(name, tx)
    }

    async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        if self.dbus.is_available().await {
            match self.dbus.get_properties(name).await {
                Ok(p) => return Ok(p),
                Err(e) => {
                    log::warn!("DBus get_properties failed, falling back to CLI: {}", e);
                    self.mark_fallback(&format!("{}", e));
                }
            }
        } else {
            log::debug!("DBus not available for get_properties, using CLI");
            self.mark_fallback("DBus not available");
        }
        self.cli.get_properties(name).await.map_err(|e| {
            log::error!("CLI get_properties failed for {}: {}", name, e);
            e
        })
    }

    async fn is_dbus_available(&self) -> bool {
        self.dbus.is_available().await
    }

    fn did_fallback(&self) -> Option<String> {
        self.last_fallback_reason.lock().unwrap().take()
    }

    async fn watch(&self, tx: tokio::sync::mpsc::Sender<()>) {
        // 1. DBus Engine: Instant lifecycle updates
        if self.is_root && self.dbus.is_available().await {
            let dbus_clone = self.dbus.clone();
            let tx_dbus = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = dbus_clone.watch_events(tx_dbus).await {
                    log::error!("DBus watcher crashed: {}", e);
                }
            });
        }

        // 2. FS Engine: Inotify for images/storage changes
        let tx_fs = tx.clone();
        let paths = self.get_watch_paths();
        tokio::spawn(async move {
            let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();

            let mut watcher = RecommendedWatcher::new(
                move |res: std::result::Result<Event, notify::Error>| {
                    if res.is_ok() {
                        let _ = notify_tx.send(());
                    }
                },
                Config::default(),
            )
            .expect("Failed to create FS watcher");

            for path in paths {
                if path.exists() {
                    if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                        log::error!("Failed to watch path {}: {}", path.display(), e);
                    }
                } else {
                    log::warn!("Watch path does not exist: {}", path.display());
                }
            }

            // Debouncer loop
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
    use crate::nspawn::adapters::comm::cli::MockCliProvider;
    use crate::nspawn::adapters::comm::dbus::MockDbusProvider;
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

    fn mock_dbus_available() -> MockDbusProvider {
        let mut dbus = MockDbusProvider::new();
        dbus.expect_is_available().returning(|| true);
        dbus
    }

    fn mock_dbus_unavailable() -> MockDbusProvider {
        let mut dbus = MockDbusProvider::new();
        dbus.expect_is_available().returning(|| false);
        dbus
    }

    // list_all

    #[tokio::test]
    async fn test_list_all_uses_dbus_when_available() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_all()
            .returning(|| Ok(vec![dummy_entry("dbus-entry")]));

        let cli = MockCliProvider::new(); // cli should never be called

        let mgr = DefaultManager {
            is_root: true,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![],
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

        let mut cli = MockCliProvider::new();
        cli.expect_list_all()
            .returning(|| Ok(vec![dummy_entry("cli-entry")]));

        let mgr = DefaultManager {
            is_root: true,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![],
        };

        let entries = mgr.list_all().await.unwrap();
        assert_eq!(entries[0].name, "cli-entry");
        assert!(mgr.did_fallback().is_some());
    }

    #[tokio::test]
    async fn test_list_all_uses_cli_when_dbus_unavailable() {
        let dbus = mock_dbus_unavailable();

        let mut cli = MockCliProvider::new();
        cli.expect_list_all()
            .returning(|| Ok(vec![dummy_entry("cli-only")]));

        let mgr = DefaultManager {
            is_root: true,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![],
        };

        let entries = mgr.list_all().await.unwrap();
        assert_eq!(entries[0].name, "cli-only");
        assert!(mgr.did_fallback().is_some());
    }

    #[tokio::test]
    async fn test_list_all_non_root_uses_cli_only() {
        let dbus = MockDbusProvider::new(); // never called for non-root

        let mut cli = MockCliProvider::new();
        cli.expect_list_all()
            .returning(|| Ok(vec![dummy_entry("non-root")]));

        let mgr = DefaultManager {
            is_root: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![],
        };

        let entries = mgr.list_all().await.unwrap();
        assert_eq!(entries[0].name, "non-root");
    }

    // permission guard

    #[tokio::test]
    async fn test_start_requires_root() {
        let dbus = mock_dbus_available();
        let cli = MockCliProvider::new();

        let mgr = DefaultManager {
            is_root: false, // not root
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![],
        };

        let err = mgr.start("test").await.unwrap_err();
        assert!(matches!(err, NspawnError::PermissionDenied));
    }

    // fallback paths

    #[tokio::test]
    async fn test_start_falls_back_to_cli_when_dbus_fails() {
        let mut dbus = mock_dbus_available();
        dbus.expect_start()
            .returning(|_| Err(NspawnError::Dbus(zbus::Error::Failure("fail".into()))));

        let mut cli = MockCliProvider::new();
        cli.expect_start().returning(|_| Ok(()));

        let mgr = DefaultManager {
            is_root: true,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![],
        };

        mgr.start("test").await.unwrap();
        assert!(mgr.did_fallback().is_some());
    }

    // enable/disable bypass DBus

    #[tokio::test]
    async fn test_enable_calls_cli_directly() {
        let dbus = MockDbusProvider::new(); // should never be called for enable

        let mut cli = MockCliProvider::new();
        cli.expect_enable()
            .with(mockall::predicate::eq("test-ctr"))
            .returning(|_| Ok(()));

        let mgr = DefaultManager {
            is_root: true,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![],
        };

        mgr.enable("test-ctr").await.unwrap();
    }

    #[tokio::test]
    async fn test_disable_calls_cli_directly() {
        let dbus = MockDbusProvider::new();

        let mut cli = MockCliProvider::new();
        cli.expect_disable()
            .with(mockall::predicate::eq("test-ctr"))
            .returning(|_| Ok(()));

        let mgr = DefaultManager {
            is_root: true,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![],
        };

        mgr.disable("test-ctr").await.unwrap();
    }

    // did_fallback state

    #[tokio::test]
    async fn test_did_fallback_clears_after_read() {
        let mut dbus = mock_dbus_available();
        dbus.expect_list_all()
            .returning(|| Err(NspawnError::Dbus(zbus::Error::Failure("fail".into()))));

        let mut cli = MockCliProvider::new();
        cli.expect_list_all()
            .returning(|| Ok(vec![dummy_entry("test")]));

        let mgr = DefaultManager {
            is_root: true,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![],
        };

        mgr.list_all().await.unwrap();
        assert!(mgr.did_fallback().is_some()); // first read returns reason
        assert!(mgr.did_fallback().is_none()); // second returns None (taken)
    }

    // remove

    #[tokio::test]
    async fn test_remove_requires_root() {
        let dbus = mock_dbus_available();
        let cli = MockCliProvider::new();

        let mgr = DefaultManager {
            is_root: false,
            dbus: std::sync::Arc::new(dbus),
            cli: std::sync::Arc::new(cli),
            last_fallback_reason: std::sync::Mutex::new(None),
            watch_paths: vec![],
        };

        let err = mgr.remove("test").await.unwrap_err();
        assert!(matches!(err, NspawnError::PermissionDenied));
    }
}
