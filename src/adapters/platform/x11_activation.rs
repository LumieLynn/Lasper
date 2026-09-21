//! Runtime user-manager activation for system-scope X11 machine claims.
//!
//! These units are a lifecycle wake-up mechanism only. They never transport
//! X11 traffic and never select or manage a user-scope nspawn machine.

use crate::adapters::process::{command_diagnostic, CommandRunner, DefaultCommandRunner};
use crate::adapters::trusted_state::TrustedDirectory;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PATH_UNIT: &str = "lasper-x11-machine.path";
const SERVICE_UNIT: &str = "lasper-x11-reconcile.service";
const UNIT_MARKER: &str = "# Managed by Lasper: X11 system-machine reconcile v1";
const MAX_UNIT_BYTES: usize = 16 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivationBackend {
    Dbus,
    SystemdTools,
}

/// Install and start the user-manager path watcher for the invoking desktop
/// user. The target machine scope remains system scope; the user manager is
/// only an unprivileged lifecycle trigger.
pub(crate) async fn ensure_system_machine_path_activation(
    backend: ActivationBackend,
) -> Result<(), String> {
    let units = tokio::task::spawn_blocking(prepare_units)
        .await
        .map_err(|error| format!("X11 activation preparation task failed: {error}"))??;

    match backend {
        ActivationBackend::Dbus => start_with_dbus().await,
        ActivationBackend::SystemdTools => start_with_systemd_tools().await,
    }
    .map_err(|error| {
        format!(
            "X11 activation manager could not start {}: {error}",
            units.path_name
        )
    })
}

/// Synchronize the watcher with the claim catalog. No active claim means
/// there is no reason to leave a path unit loaded in the user manager.
pub(crate) async fn synchronize_system_machine_path_activation(
    backend: ActivationBackend,
    active_claims: bool,
) -> Result<(), String> {
    if active_claims {
        ensure_system_machine_path_activation(backend).await
    } else {
        remove_system_machine_path_activation(backend).await
    }
}

struct PreparedUnits {
    path_name: &'static str,
}

fn prepare_units() -> Result<PreparedUnits, String> {
    let uid = uzers::get_effective_uid();
    let runtime = runtime_directory(uid)?;
    let runtime = TrustedDirectory::open_existing(&runtime, uid)
        .map_err(|error| format!("open XDG runtime directory: {error}"))?;
    let systemd = match runtime
        .open_existing_child("systemd")
        .map_err(|error| format!("open user systemd runtime directory: {error}"))?
    {
        Some(systemd) => systemd,
        None => runtime
            .open_or_create_child("systemd", 0o755)
            .map_err(|error| format!("create user systemd runtime directory: {error}"))?,
    };
    let units = match systemd
        .open_existing_child("user")
        .map_err(|error| format!("open user unit directory: {error}"))?
    {
        Some(units) => units,
        None => systemd
            .open_or_create_child("user", 0o700)
            .map_err(|error| format!("create user runtime unit directory: {error}"))?,
    };

    let executable = validated_executable(uid)?;
    let executable = systemd_exec_arg(&executable);
    let service = reconcile_service_contents(&executable);
    let path = format!(
        "{UNIT_MARKER}\n[Unit]\nDescription=Wake Lasper X11 reconcile after system machine changes\n\n[Path]\nPathChanged=/run/systemd/machines\nUnit={SERVICE_UNIT}\n\n[Install]\nWantedBy=default.target\n",
    );
    units
        .with_exclusive_lock("x11-activation", || {
            ensure_unit(&units, SERVICE_UNIT, service.as_bytes())?;
            ensure_unit(&units, PATH_UNIT, path.as_bytes())
        })
        .map_err(|error| format!("write X11 runtime units: {error}"))?;
    Ok(PreparedUnits {
        path_name: PATH_UNIT,
    })
}

fn reconcile_service_contents(executable: &str) -> String {
    format!(
        "{UNIT_MARKER}\n[Unit]\nDescription=Lasper X11 system-machine reconcile\n\n[Service]\nType=oneshot\nExecStart={executable} --internal-x11-reconcile\nUMask=0077\n"
    )
}

fn ensure_unit(
    units: &TrustedDirectory,
    name: &str,
    expected: &[u8],
) -> Result<(), crate::adapters::error::NspawnError> {
    if let Some(existing) = units.read_bounded(name, MAX_UNIT_BYTES)? {
        if existing.uid != units.expected_uid() || existing.mode & 0o077 != 0 {
            return Err(crate::adapters::error::NspawnError::Validation(format!(
                "Lasper runtime unit has unsafe ownership or mode: {}",
                name
            )));
        }
        if existing.bytes == expected {
            return Ok(());
        }
        if !existing.bytes.starts_with(UNIT_MARKER.as_bytes()) {
            return Err(crate::adapters::error::NspawnError::Validation(format!(
                "refusing to replace an external user runtime unit: {name}"
            )));
        }
    }
    units.write_atomic(name, expected, 0o600)
}

async fn remove_system_machine_path_activation(backend: ActivationBackend) -> Result<(), String> {
    let Some(units) = tokio::task::spawn_blocking(open_existing_unit_directory)
        .await
        .map_err(|error| format!("X11 activation cleanup task failed: {error}"))??
    else {
        return Ok(());
    };
    let validation_units = units.clone();
    tokio::task::spawn_blocking(move || validate_owned_units(&validation_units))
        .await
        .map_err(|error| format!("X11 activation ownership check failed: {error}"))??;

    stop_path_unit(backend).await?;
    tokio::task::spawn_blocking(move || {
        units
            .with_exclusive_lock("x11-activation", || {
                units.remove_unlocked(PATH_UNIT)?;
                units.remove_unlocked(SERVICE_UNIT)
            })
            .map_err(|error| format!("remove X11 runtime units: {error}"))
    })
    .await
    .map_err(|error| format!("X11 activation removal task failed: {error}"))??;
    reload_manager(backend).await
}

fn open_existing_unit_directory() -> Result<Option<TrustedDirectory>, String> {
    let uid = uzers::get_effective_uid();
    let runtime = runtime_directory(uid)?;
    if !runtime.exists() {
        return Ok(None);
    }
    let runtime = TrustedDirectory::open_existing(&runtime, uid)
        .map_err(|error| format!("open XDG runtime directory: {error}"))?;
    let Some(systemd) = runtime
        .open_existing_child("systemd")
        .map_err(|error| format!("open user systemd runtime directory: {error}"))?
    else {
        return Ok(None);
    };
    let units = systemd
        .open_existing_child("user")
        .map_err(|error| format!("open user unit directory: {error}"))?;
    Ok(units)
}

fn validate_owned_units(units: &TrustedDirectory) -> Result<(), String> {
    for name in [PATH_UNIT, SERVICE_UNIT] {
        let Some(file) = units
            .read_bounded(name, MAX_UNIT_BYTES)
            .map_err(|error| format!("read X11 runtime unit {name}: {error}"))?
        else {
            return Err(format!("X11 runtime unit {name} is missing"));
        };
        if file.uid != units.expected_uid() || file.mode & 0o077 != 0 {
            return Err(format!(
                "X11 runtime unit {name} has unsafe ownership or mode"
            ));
        }
        if !file.bytes.starts_with(UNIT_MARKER.as_bytes()) {
            return Err(format!(
                "refusing to remove an external user runtime unit: {name}"
            ));
        }
    }
    Ok(())
}

fn runtime_directory(uid: u32) -> Result<PathBuf, String> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
    if !path.is_absolute() {
        return Err(format!(
            "XDG_RUNTIME_DIR must be absolute: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn validated_executable(uid: u32) -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve current Lasper executable: {error}"))?;
    let metadata = std::fs::symlink_metadata(&executable)
        .map_err(|error| format!("inspect current Lasper executable: {error}"))?;
    if !executable_metadata_is_safe(
        metadata.file_type().is_file(),
        metadata.uid(),
        metadata.permissions().mode(),
        uid,
    ) {
        return Err(format!(
            "current Lasper executable is not a private regular file: {}",
            executable.display()
        ));
    }
    Ok(executable)
}

fn executable_metadata_is_safe(is_regular: bool, owner: u32, mode: u32, uid: u32) -> bool {
    is_regular && (owner == uid || owner == 0) && mode & 0o022 == 0 && mode & 0o111 != 0
}

fn systemd_exec_arg(path: &Path) -> String {
    let mut escaped = String::with_capacity(path.as_os_str().len() + 2);
    escaped.push('\'');
    for character in path.to_string_lossy().chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\'' => escaped.push_str("'\\''"),
            _ => escaped.push(character),
        }
    }
    escaped.push('\'');
    escaped
}

async fn start_with_systemd_tools() -> Result<(), String> {
    let runner = DefaultCommandRunner;
    for args in [
        vec![
            "--user".into(),
            "--no-ask-password".into(),
            "daemon-reload".into(),
        ],
        vec![
            "--user".into(),
            "--no-ask-password".into(),
            "start".into(),
            PATH_UNIT.into(),
        ],
    ] {
        let output = runner
            .run_bounded("systemctl", args, COMMAND_TIMEOUT)
            .await
            .map_err(|error| format!("run systemctl --user: {error}"))?;
        if !output.status.success() {
            return Err(command_diagnostic(&output));
        }
    }
    Ok(())
}

async fn stop_path_unit(backend: ActivationBackend) -> Result<(), String> {
    match backend {
        ActivationBackend::Dbus => {
            let connection = connect_user_bus().await?;
            let proxy = manager_proxy(&connection).await?;
            tokio::time::timeout(
                COMMAND_TIMEOUT,
                proxy.call::<_, _, zbus::zvariant::OwnedObjectPath>(
                    "StopUnit",
                    &(PATH_UNIT, "replace"),
                ),
            )
            .await
            .map_err(|_| "stop X11 path unit timed out".to_owned())?
            .map_err(|error| format!("stop X11 path unit: {error}"))?;
            Ok(())
        }
        ActivationBackend::SystemdTools => {
            let output = DefaultCommandRunner
                .run_bounded(
                    "systemctl",
                    vec![
                        "--user".into(),
                        "--no-ask-password".into(),
                        "stop".into(),
                        PATH_UNIT.into(),
                    ],
                    COMMAND_TIMEOUT,
                )
                .await
                .map_err(|error| format!("run systemctl --user stop: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(command_diagnostic(&output))
            }
        }
    }
}

async fn reload_manager(backend: ActivationBackend) -> Result<(), String> {
    match backend {
        ActivationBackend::Dbus => {
            let connection = connect_user_bus().await?;
            let proxy = manager_proxy(&connection).await?;
            tokio::time::timeout(COMMAND_TIMEOUT, proxy.call::<_, _, ()>("Reload", &()))
                .await
                .map_err(|_| "reload user manager timed out".to_owned())?
                .map_err(|error| format!("reload user manager: {error}"))?;
            Ok(())
        }
        ActivationBackend::SystemdTools => {
            let output = DefaultCommandRunner
                .run_bounded(
                    "systemctl",
                    vec![
                        "--user".into(),
                        "--no-ask-password".into(),
                        "daemon-reload".into(),
                    ],
                    COMMAND_TIMEOUT,
                )
                .await
                .map_err(|error| format!("run systemctl --user daemon-reload: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(command_diagnostic(&output))
            }
        }
    }
}

async fn connect_user_bus() -> Result<zbus::Connection, String> {
    tokio::time::timeout(COMMAND_TIMEOUT, zbus::Connection::session())
        .await
        .map_err(|_| "connect to the user D-Bus timed out".to_owned())?
        .map_err(|error| format!("connect to the user D-Bus: {error}"))
}

async fn manager_proxy(connection: &zbus::Connection) -> Result<zbus::Proxy<'_>, String> {
    tokio::time::timeout(
        COMMAND_TIMEOUT,
        zbus::Proxy::new(
            connection,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        ),
    )
    .await
    .map_err(|_| "create user systemd manager proxy timed out".to_owned())?
    .map_err(|error| format!("create user systemd manager proxy: {error}"))
}

async fn start_with_dbus() -> Result<(), String> {
    let connection = connect_user_bus().await?;
    let proxy = manager_proxy(&connection).await?;
    tokio::time::timeout(COMMAND_TIMEOUT, proxy.call::<_, _, ()>("Reload", &()))
        .await
        .map_err(|_| "reload user manager timed out".to_owned())?
        .map_err(|error| format!("reload user manager: {error}"))?;
    tokio::time::timeout(
        COMMAND_TIMEOUT,
        proxy.call::<_, _, zbus::zvariant::OwnedObjectPath>("StartUnit", &(PATH_UNIT, "fail")),
    )
    .await
    .map_err(|_| "start X11 path unit timed out".to_owned())?
    .map_err(|error| format!("start X11 path unit: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_path_uses_systemd_safe_single_quoting() {
        assert_eq!(
            systemd_exec_arg(Path::new("/tmp/Lasper\"x\\y")),
            "'/tmp/Lasper\"x\\\\y'"
        );
        assert_eq!(
            systemd_exec_arg(Path::new("/tmp/Lasper'x")),
            "'/tmp/Lasper'\\''x'"
        );
    }

    #[test]
    fn reconcile_service_uses_only_the_internal_entry() {
        let service = reconcile_service_contents("'/usr/local/bin/lasper'");
        assert!(service.contains("ExecStart='/usr/local/bin/lasper' --internal-x11-reconcile\n"));
        assert!(!service.contains("--systemd-tools"));
    }

    #[test]
    fn packaged_root_owned_executable_is_valid_for_user_activation() {
        assert!(executable_metadata_is_safe(true, 0, 0o755, 1000));
        assert!(executable_metadata_is_safe(true, 1000, 0o755, 1000));
        assert!(!executable_metadata_is_safe(true, 2000, 0o755, 1000));
        assert!(!executable_metadata_is_safe(true, 0, 0o775, 1000));
        assert!(!executable_metadata_is_safe(true, 0, 0o644, 1000));
    }
}
