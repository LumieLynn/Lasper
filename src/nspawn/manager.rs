use super::errors::{NspawnError, Result};
use super::models::{ContainerEntry, MachineProperties};
use super::provider::{cli::CliProvider, dbus::DbusProvider};
use async_trait::async_trait;
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
}

pub struct DefaultManager {
    is_root: bool,
    dbus: DbusProvider,
    cli: CliProvider,
    last_fallback: AtomicBool,
}

impl DefaultManager {
    pub fn new(is_root: bool) -> Self {
        Self {
            is_root,
            dbus: DbusProvider::new(),
            cli: CliProvider::new(is_root),
            last_fallback: AtomicBool::new(false),
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
        super::nvidia::ensure_gpu_passthrough(name, &self.dbus).await
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
        self.cli.list_all().await
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
        self.cli.start(name).await
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
        self.cli.terminate(name).await
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
        self.cli.poweroff(name).await
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
        self.cli.reboot(name).await
    }

    async fn enable(&self, name: &str) -> Result<()> {
        self.require_root()?;
        self.cli.enable(name).await
    }

    async fn disable(&self, name: &str) -> Result<()> {
        self.require_root()?;
        self.cli.disable(name).await
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
        self.cli.kill(name, signal).await
    }

    async fn get_logs(&self, name: &str, lines: usize) -> Result<Vec<String>> {
        self.cli.get_logs(name, lines).await
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
        self.cli.get_properties(name).await
    }

    async fn is_dbus_available(&self) -> bool {
        self.dbus.is_available().await
    }

    fn did_fallback(&self) -> bool {
        self.last_fallback.swap(false, Ordering::Relaxed)
    }
}
