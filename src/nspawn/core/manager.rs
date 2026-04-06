use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, MachineProperties};
use crate::nspawn::core::provider::{cli::CliProvider, dbus::DbusProvider};
use async_trait::async_trait;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

#[async_trait]
pub trait NspawnManager: Send + Sync + 'static {
    async fn list_all(&self) -> Result<Vec<ContainerEntry>>;
    async fn start(&self, name: &str) -> Result<()>;
    async fn terminate(&self, name: &str) -> Result<()>;
    async fn get_logs(&self, name: &str, lines: usize) -> Result<Vec<String>>;
    async fn get_properties(&self, name: &str) -> Result<MachineProperties>;
    async fn enable(&self, name: &str) -> Result<()>;
    async fn disable(&self, name: &str) -> Result<()>;
    async fn poweroff(&self, name: &str) -> Result<()>;
    async fn reboot(&self, name: &str) -> Result<()>;
    async fn kill(&self, name: &str, signal: &str) -> Result<()>;
    async fn is_dbus_available(&self) -> bool;
    fn did_fallback(&self) -> bool;
    async fn watch(&self, tx: tokio::sync::mpsc::Sender<()>);
    fn get_watch_paths(&self) -> Vec<PathBuf>;
}

pub struct DefaultManager {
    is_root: bool,
    dbus: DbusProvider,
    cli: CliProvider,
    last_fallback: AtomicBool,
    watch_paths: Vec<PathBuf>,
}

impl DefaultManager {
    pub fn new(is_root: bool) -> Self {
        Self {
            is_root,
            dbus: DbusProvider::new(),
            cli: CliProvider::new(is_root),
            last_fallback: AtomicBool::new(false),
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

    fn mark_fallback(&self) {
        self.last_fallback.store(true, Ordering::Relaxed);
    }

    async fn _ensure_gpu_passthrough(&self, name: &str) -> Result<()> {
        crate::nspawn::hw::nvidia::ensure_gpu_passthrough(name, &self.dbus).await
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
                    self.mark_fallback();
                }
            }
        } else {
            log::debug!("DBus not available for list_all, using CLI");
            self.mark_fallback();
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
                }
            }
        } else {
            log::warn!("DBus not available for start, falling back to CLI");
        }
        self.mark_fallback();
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
                }
            }
        } else {
            log::warn!("DBus not available for terminate, falling back to CLI");
        }
        self.mark_fallback();
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
                }
            }
        } else {
            log::warn!("DBus not available for poweroff, falling back to CLI");
        }
        self.mark_fallback();
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
                }
            }
        } else {
            log::warn!("DBus not available for reboot, falling back to CLI");
        }
        self.mark_fallback();
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
                }
            }
        } else {
            log::warn!("DBus not available for kill, falling back to CLI");
        }
        self.mark_fallback();
        self.cli.kill(name, signal).await.map_err(|e| {
            log::error!("CLI kill failed for {} (signal {}): {}", name, signal, e);
            e
        })
    }

    async fn get_logs(&self, name: &str, lines: usize) -> Result<Vec<String>> {
        self.cli.get_logs(name, lines).await.map_err(|e| {
            log::error!("CLI get_logs failed for {}: {}", name, e);
            e
        })
    }

    async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        if self.dbus.is_available().await {
            match self.dbus.get_properties(name).await {
                Ok(p) => return Ok(p),
                Err(e) => {
                    log::warn!("DBus get_properties failed, falling back to CLI: {}", e);
                }
            }
        } else {
            log::debug!("DBus not available for get_properties, using CLI");
        }
        self.mark_fallback();
        self.cli.get_properties(name).await.map_err(|e| {
            log::error!("CLI get_properties failed for {}: {}", name, e);
            e
        })
    }

    async fn is_dbus_available(&self) -> bool {
        self.dbus.is_available().await
    }

    fn did_fallback(&self) -> bool {
        self.last_fallback.swap(false, Ordering::Relaxed)
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
                    while let Ok(_) = notify_rx.try_recv() {}
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
