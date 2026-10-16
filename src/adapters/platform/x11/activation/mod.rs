//! User-manager wake-ups for system-machine X11 grants. No X11 traffic passes
//! through these units, and the reconcile service exits after each run.

mod dbus;
mod legacy;
mod systemd_tools;

use std::path::PathBuf;
use std::time::Duration;

use super::state::{claims_need_activation, X11RuntimeState};

const PATH_UNIT: &str = "lasper-x11-lifecycle.path";
const SERVICE_UNIT: &str = "lasper-x11-lifecycle.service";
const PATH_DESCRIPTION: &str = "Lasper X11 system-machine lifecycle watcher";
const SERVICE_DESCRIPTION: &str = "Lasper X11 system-machine reconcile: ";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivationBackend {
    Dbus,
    SystemdTools,
}

/// The catalog decision and the manager changes share the ACL lock. A stop
/// based on an old catalog must not undo another process's new authorization.
pub(super) async fn synchronize_system_machine_path_activation(
    backend: ActivationBackend,
) -> Result<(), String> {
    let (needed, acl_lock, legacy) = tokio::task::spawn_blocking(|| {
        let state = X11RuntimeState::open_existing()?;
        let lock = state.as_ref().map(X11RuntimeState::lock).transpose()?;
        let needed = match &state {
            Some(state) => claims_need_activation(&state.load_claims(), &state.load_records())?,
            None => false,
        };
        Ok::<_, String>((needed, lock, legacy::LegacyUnits::find()?))
    })
    .await
    .map_err(|error| format!("prepare X11 lifecycle activation: {error}"))??;

    // No state or legacy units means there is nothing for the user manager to
    // do. In particular, an unused X11 integration must not activate services.
    if acl_lock.is_none() && legacy.is_none() {
        return Ok(());
    }

    let manager: Box<dyn UnitManager> = match backend {
        ActivationBackend::Dbus => Box::new(dbus::UserManager::connect().await?),
        ActivationBackend::SystemdTools => Box::new(systemd_tools::UserManager::new()),
    };
    if needed {
        let spec = ActivationSpec::current()?;
        ensure_activation(manager.as_ref(), &spec).await?;
    } else {
        stop_activation(manager.as_ref()).await?;
    }
    // Install the new watcher first so migration never leaves live grants
    // without a wake-up. Never stop a reconcile service: it may be this process.
    if let Some(legacy) = legacy {
        legacy.remove(manager.as_ref()).await?;
    }
    Ok(())
}

struct ActivationSpec {
    executable: String,
    watched_directory: PathBuf,
}

impl ActivationSpec {
    fn current() -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("resolve current Lasper executable: {error}"))?;
        let executable = executable
            .into_os_string()
            .into_string()
            .map_err(|_| "the Lasper executable path is not valid UTF-8".to_owned())?;
        Ok(Self {
            executable,
            watched_directory: crate::paths::runtime_machines_dir(),
        })
    }

    fn service_description(&self) -> String {
        format!("{SERVICE_DESCRIPTION}{}", self.executable)
    }
}

struct UnitState {
    transient: bool,
    description: String,
    active_state: String,
}

impl UnitState {
    fn require_owned(&self, unit: &str) -> Result<(), String> {
        let owned = self.transient
            && match unit {
                PATH_UNIT => self.description == PATH_DESCRIPTION,
                SERVICE_UNIT => self.description.starts_with(SERVICE_DESCRIPTION),
                _ => false,
            };
        if owned {
            Ok(())
        } else {
            Err(format!("refusing to change an external user unit: {unit}"))
        }
    }

    fn running(&self) -> bool {
        matches!(
            self.active_state.as_str(),
            "active" | "activating" | "deactivating" | "reloading"
        )
    }
}

/// Both implementations wait for the manager job to finish, rather than
/// treating a queued StartTransientUnit job as a successful registration.
#[async_trait::async_trait]
trait UnitManager: Send + Sync {
    async fn inspect(&self, unit: &str) -> Result<Option<UnitState>, String>;
    async fn create(&self, spec: &ActivationSpec, include_service: bool) -> Result<(), String>;
    async fn start(&self, unit: &str) -> Result<(), String>;
    async fn stop(&self, unit: &str) -> Result<(), String>;
    async fn reset_failed(&self, unit: &str) -> Result<(), String>;
    async fn reload(&self) -> Result<(), String>;
}

async fn ensure_activation(manager: &dyn UnitManager, spec: &ActivationSpec) -> Result<(), String> {
    let path = manager.inspect(PATH_UNIT).await?;
    let service = manager.inspect(SERVICE_UNIT).await?;
    if let Some(path) = &path {
        path.require_owned(PATH_UNIT)?;
    }
    if let Some(service) = &service {
        service.require_owned(SERVICE_UNIT)?;
    }

    if let Some(service) = &service {
        if service.description == spec.service_description() {
            if let Some(path) = &path {
                if service.active_state == "failed" {
                    manager.reset_failed(SERVICE_UNIT).await?;
                }
                if path.active_state != "active" {
                    manager.start(PATH_UNIT).await?;
                }
                return Ok(());
            }
            // The last reconcile may have stopped its own path just
            // before another process added a new claim. Reuse a live
            // worker, but it can exit and be collected after this snapshot.
            if service.running() {
                let created = manager.create(spec, false).await;
                if let Some(companion) = manager.inspect(SERVICE_UNIT).await? {
                    companion.require_owned(SERVICE_UNIT)?;
                    if companion.description != spec.service_description() {
                        return Err(format!(
                            "{SERVICE_UNIT} changed while rearming the X11 watcher"
                        ));
                    }
                    // A successfully started path now references the loaded
                    // service, preventing GC when the old worker exits.
                    return created;
                }
                // A path may start with a not-found trigger. Remove that
                // orphan before recreating both units in the same request.
                // A failed registration needs the same recovery if the
                // companion disappeared and no path remains loaded.
                let path = manager.inspect(PATH_UNIT).await?;
                return recreate_activation(manager, spec, path.as_ref(), None).await;
            }
        }
    }

    recreate_activation(manager, spec, path.as_ref(), service.as_ref()).await
}

async fn recreate_activation(
    manager: &dyn UnitManager,
    spec: &ActivationSpec,
    path: Option<&UnitState>,
    service: Option<&UnitState>,
) -> Result<(), String> {
    if let Some(path) = path {
        path.require_owned(PATH_UNIT)?;
    }
    // Transient units cannot have their ExecStart replaced while loaded. Do
    // not wait for or interrupt a running worker while holding its ACL lock.
    if service.is_some_and(UnitState::running) {
        return Err(format!(
            "cannot replace {SERVICE_UNIT} while reconciliation is running; retry after it exits"
        ));
    }
    if path.is_some() {
        manager.stop(PATH_UNIT).await?;
    }
    if service.is_some_and(|state| state.active_state == "failed") {
        manager.reset_failed(SERVICE_UNIT).await?;
    }
    // Stopping the path releases its reference to the companion service.
    // Wait only for inactive units to be collected, never for a running worker.
    if path.is_some() || service.is_some() {
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            loop {
                if manager.inspect(PATH_UNIT).await?.is_none()
                    && manager.inspect(SERVICE_UNIT).await?.is_none()
                {
                    return Ok::<_, String>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| "waiting for old X11 transient units to unload timed out".to_owned())??;
    }
    manager.create(spec, true).await
}

async fn stop_activation(manager: &dyn UnitManager) -> Result<(), String> {
    if let Some(path) = manager.inspect(PATH_UNIT).await? {
        path.require_owned(PATH_UNIT)?;
        manager.stop(PATH_UNIT).await?;
    }
    // The service is collected after its current invocation finishes. Stopping
    // it here could kill the very reconcile that released the last grant.
    Ok(())
}

#[cfg(test)]
#[path = "tests/support.rs"]
mod test_support;

#[cfg(test)]
#[path = "tests/lifecycle.rs"]
mod tests;
