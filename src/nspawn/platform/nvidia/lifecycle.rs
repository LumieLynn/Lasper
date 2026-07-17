use super::discovery::get_nvidia_state;
use super::profile::NvidiaPassthroughMode;
use super::staging::NvidiaInjectionFileKind;
use super::state::{calculate_death_list, NvidiaState};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::MachineName;
use crate::nspawn::sys::log_output;
use std::path::Path;

macro_rules! log_step {
    ($name:expr, $step:expr, $msg:expr) => {
        log::info!("[AUDIT] [Container: {}] [Step: {}] {}", $name, $step, $msg);
    };
    ($name:expr, $step:expr, $fmt:expr, $($arg:tt)*) => {
        log::info!(
            "[AUDIT] [Container: {}] [Step: {}] {}",
            $name,
            $step,
            format!($fmt, $($arg)*)
        );
    };
}

/// Check whether every bind in `state.binds` is reflected in the managed
/// block between `X-Lasper-Nvidia-Begin` and `X-Lasper-Nvidia-End`, and
/// vice versa.  A mismatch (empty block, manual edit, partial write) means
/// the `.nspawn` is stale and must be regenerated.
fn marker_binds_match_state(content: &str, state: &NvidiaState) -> bool {
    use std::collections::HashSet;

    let mut inside = false;
    let mut file_binds: HashSet<(String, String, bool)> = HashSet::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("X-Lasper-Nvidia-Begin=") {
            inside = true;
            continue;
        }
        if trimmed.starts_with("X-Lasper-Nvidia-End=") {
            break;
        }
        if !inside {
            continue;
        }
        if let Some(val) = trimmed.strip_prefix("BindReadOnly=") {
            if let Some((host, container)) =
                crate::nspawn::adapters::config::nspawn_file::parse_nspawn_bind_paths(val)
            {
                file_binds.insert((host, container, true));
            }
        } else if let Some(val) = trimmed.strip_prefix("Bind=") {
            if let Some((host, container)) =
                crate::nspawn::adapters::config::nspawn_file::parse_nspawn_bind_paths(val)
            {
                file_binds.insert((host, container, false));
            }
        }
    }

    let state_binds: HashSet<(String, String, bool)> = state
        .binds
        .iter()
        .map(|b| (b.host_path.clone(), b.container_path.clone(), b.readonly))
        .collect();

    file_binds == state_binds
}

pub async fn cleanup_container_garbage(
    name: &str,
    death_list: &[String],
    cmd_runner: &dyn crate::nspawn::sys::CommandRunner,
) -> Result<()> {
    if death_list.is_empty() {
        return Ok(());
    }
    let machine =
        MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))?;
    for path in death_list {
        validate_cleanup_path(path)?;
    }

    log_step!(
        name,
        "Cleanup",
        "Inspecting and removing leftover driver files..."
    );

    let rootfs = crate::paths::machine_root(machine.as_str());
    let mut args = vec![
        "-D".to_string(),
        rootfs.to_string_lossy().to_string(),
        "--settings=no".to_string(),
        "-q".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        "for path do [ -f \"$path\" ] && [ ! -s \"$path\" ] && rm -f -- \"$path\"; done"
            .to_string(),
        "_".to_string(),
    ];
    args.extend(death_list.iter().cloned());

    cmd_runner.run("systemd-nspawn", args).await?;

    Ok(())
}

fn validate_cleanup_path(path: &str) -> Result<()> {
    if path.is_empty() || path.chars().any(char::is_control) || !Path::new(path).is_absolute() {
        return Err(NspawnError::Validation(format!(
            "Invalid NVIDIA cleanup path: {path:?}"
        )));
    }
    Ok(())
}

async fn inject_persistent_device_allow(
    name: &str,
    state: &NvidiaState,
    systemd_unit: &crate::nspawn::adapters::config::SystemdUnitStore,
) -> Result<()> {
    let device_paths = state
        .binds
        .iter()
        .filter(|bind| !bind.readonly)
        .map(|bind| bind.host_path.clone())
        .collect::<Vec<_>>();
    systemd_unit
        .write_nvidia_device_allow(name, &device_paths)
        .await
}

/// Write ld.so.conf.d entry and /etc/environment vars into the container
/// rootfs via `systemd-nspawn -D <root> --bind <tmp>:<tmp> sh -c "cp ..."`.
///
/// Content is staged through a typed NVIDIA staging store so Elevated mode
/// does not expose arbitrary host temp-file paths. Uses the supplied
/// [`CommandRunner`] to execute nspawn (which goes through the daemon when
/// elevated).
///
/// Done at creation time (not every startup) — called from provisioning.
pub(crate) async fn inject_env_once(
    name: &str,
    target: &crate::nspawn::adapters::rootfs::RootfsTarget,
    state: &NvidiaState,
    staging: &crate::nspawn::platform::nvidia::NvidiaStagingStore,
    cmd_runner: &dyn crate::nspawn::sys::CommandRunner,
) -> Result<()> {
    let rootfs = target.path()?;
    let rootfs_str = rootfs.to_string_lossy();

    // ── ld.so.conf.d ──
    let mut ld_content = String::new();
    for folder in &state.ldcache_folders {
        ld_content.push_str(folder);
        ld_content.push('\n');
    }
    if let Some(ref prof) = state.profile {
        if prof.mode == NvidiaPassthroughMode::Categorized {
            use crate::nspawn::platform::nvidia::classify::NvidiaFileCategory;
            for cat in [NvidiaFileCategory::Lib64, NvidiaFileCategory::Lib32] {
                if let Some(dest) = prof.category_destinations.get(&cat) {
                    ld_content.push_str(dest);
                    ld_content.push('\n');
                }
            }
        }
    }

    if !ld_content.is_empty() {
        let file = staging
            .create_injection_file(name, NvidiaInjectionFileKind::LdConfig, &ld_content)
            .await?;
        let tmp = file.path.clone();
        let copy = cmd_runner
            .run(
                "systemd-nspawn",
                vec![
                    "-D".to_string(),
                    rootfs_str.to_string(),
                    "--settings=no".to_string(),
                    "--bind".to_string(),
                    format!("{}:{}", tmp, tmp),
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "mkdir -p /etc/ld.so.conf.d && cp {} /etc/ld.so.conf.d/lasper-nvidia.conf",
                        tmp
                    ),
                ],
            )
            .await;
        match copy {
            Ok(output) => {
                log_output("nvidia ld.so.conf", &output);
                if output.status.success() {
                    refresh_container_ld_cache(name, &rootfs_str, cmd_runner).await;
                } else {
                    log::warn!("Failed to inject NVIDIA ld.so.conf into container {}", name);
                }
            }
            Err(error) => {
                log::warn!(
                    "Failed to run NVIDIA ld.so.conf injection for container {}: {}",
                    name,
                    error
                );
            }
        }
        let _ = staging.remove_injection_file(name, &file).await;
    }

    // ── /etc/environment ──
    if state.profile.as_ref().is_some_and(|p| p.inject_env) {
        // Read existing content from the container
        let old_env = cmd_runner
            .run(
                "systemd-nspawn",
                vec![
                    "-D".to_string(),
                    rootfs_str.to_string(),
                    "--settings=no".to_string(),
                    "cat".to_string(),
                    "/etc/environment".to_string(),
                ],
            )
            .await
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
            .unwrap_or_default();

        let mut lines: Vec<String> = old_env.lines().map(|s| s.to_string()).collect();
        for (key, val) in &state.env_vars {
            let prefix = format!("{}=", key);
            lines.retain(|l| !l.starts_with(&prefix));
            lines.push(format!("{}={}", key, val));
        }
        let new_env = lines.join("\n") + "\n";

        let file = staging
            .create_injection_file(name, NvidiaInjectionFileKind::Environment, &new_env)
            .await?;
        let tmp = file.path.clone();
        let _ = cmd_runner
            .run(
                "systemd-nspawn",
                vec![
                    "-D".to_string(),
                    rootfs_str.to_string(),
                    "--settings=no".to_string(),
                    "--bind".to_string(),
                    format!("{}:{}", tmp, tmp),
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("cp {} /etc/environment", tmp),
                ],
            )
            .await;
        let _ = staging.remove_injection_file(name, &file).await;
    }

    Ok(())
}

async fn refresh_container_ld_cache(
    name: &str,
    rootfs: &str,
    cmd_runner: &dyn crate::nspawn::sys::CommandRunner,
) {
    let output = cmd_runner
        .run(
            "systemd-nspawn",
            vec![
                "-D".to_string(),
                rootfs.to_string(),
                "--quiet".to_string(),
                "--settings=no".to_string(),
                "ldconfig".to_string(),
            ],
        )
        .await;
    match output {
        Ok(output) => {
            log_output("ldconfig", &output);
            if !output.status.success() {
                log::warn!("ldconfig failed inside NVIDIA container {}", name);
            }
        }
        Err(error) => {
            log::warn!(
                "Failed to run ldconfig inside NVIDIA container {}: {}",
                name,
                error
            );
        }
    }
}

pub async fn ensure_gpu_passthrough(
    name: &str,
    nspawn: &crate::nspawn::adapters::config::NspawnConfigStore,
    systemd_unit: &crate::nspawn::adapters::config::SystemdUnitStore,
    state_store: &crate::nspawn::platform::nvidia::NvidiaStateStore,
    cmd_runner: &dyn crate::nspawn::sys::CommandRunner,
) -> Result<()> {
    // 1. Check if GPU passthrough is enabled in .nspawn config
    let config = match nspawn.read(name).await? {
        Some(c) => c,
        None => return Ok(()),
    };
    if !config.is_gpu_enabled() {
        return Ok(());
    }

    log_step!(
        name,
        "Lifecycle",
        "GPU Passthrough enabled, initiating state synchronization..."
    );

    // 2. Load old state and profile, then scan host
    log_step!(name, "Detection", "Scanning host for NVIDIA CDI devices...");

    let external_cache = state_store.read(name).await?.unwrap_or_default();
    let profile = external_cache.profile.clone().unwrap_or_default();

    // Remapping already happens inside get_nvidia_state
    let host_state = get_nvidia_state(Some(&profile)).await?;

    log_step!(
        name,
        "Detection",
        "Detected driver: {}, {} binds, {} ldconfig folders.",
        host_state.driver_version,
        host_state.binds.len(),
        host_state.ldcache_folders.len()
    );

    // 3. Compare old vs new state
    let old_state = external_cache.clone();

    if old_state == host_state && !old_state.driver_version.is_empty() {
        // State matches — verify .nspawn markers exist and have content.
        if config
            .content
            .contains("X-Lasper-Nvidia-Begin=managed-by-lasper")
            && marker_binds_match_state(&config.content, &host_state)
        {
            log::debug!(
                "GPU state identity match for {}, skipping re-assembly.",
                name
            );
            inject_persistent_device_allow(name, &host_state, systemd_unit).await?;
            return Ok(());
        }
        log::info!(
            "GPU state matches but managed .nspawn markers are missing or stale for {}; rebuilding configuration.",
            name
        );
    } else if old_state.driver_version.is_empty() {
        log::info!(
            "No previous NVIDIA state found for {}; assembling configuration.",
            name
        );
    } else if old_state.driver_version != host_state.driver_version {
        log::info!(
            "GPU driver change detected ({} -> {}), refreshing configuration...",
            old_state.driver_version,
            host_state.driver_version
        );
    } else {
        log::info!(
            "NVIDIA CDI state changed while driver version {} remained unchanged; refreshing configuration for {}.",
            host_state.driver_version,
            name
        );
    }

    let death_list = calculate_death_list(&old_state, &host_state);
    if !death_list.is_empty() {
        log_step!(
            name,
            "Surgery",
            "Marked {} files for removal/update.",
            death_list.len()
        );
    }

    // 4. Cleanup stale files in rootfs
    cleanup_container_garbage(name, &death_list, cmd_runner).await?;

    // 5. Update .nspawn config (symlinks are now synthesized as Bind entries here)
    log_step!(name, "Surgery", "Mutating .nspawn configuration AST...");
    nspawn.update_gpu(name, &host_state, &death_list).await?;

    // 6. Persist state and inject DeviceAllow rules
    log_step!(
        name,
        "Surgery",
        "Persisting state and injecting persistent DeviceAllow rules..."
    );
    state_store.write(name, &host_state).await?;
    inject_persistent_device_allow(name, &host_state, systemd_unit).await?;

    // 7. Reload daemon
    log_step!(
        name,
        "Lifecycle",
        "Reloading systemd daemon to commit changes."
    );
    log_step!(name, "Lifecycle", "GPU surgery successful.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::sys::command::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;
    use std::sync::{Arc, Mutex};

    fn mock_output(status: bool) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(if status { 0 } else { 256 }),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn marker_match_preserves_writable_bind_destination() {
        let state = NvidiaState {
            binds: vec![crate::nspawn::platform::nvidia::state::PassthroughBind {
                host_path: "/dev/dri/card0".into(),
                container_path: "/dev/dri/by-path/nvidia-card".into(),
                readonly: false,
            }],
            ..Default::default()
        };
        let content = "[Files]\nX-Lasper-Nvidia-Begin=managed-by-lasper\nBind=/dev/dri/card0:/dev/dri/by-path/nvidia-card\nX-Lasper-Nvidia-End=true\n";

        assert!(marker_binds_match_state(content, &state));
    }

    #[test]
    fn marker_match_decodes_escaped_pci_colons() {
        let state = NvidiaState {
            binds: vec![crate::nspawn::platform::nvidia::state::PassthroughBind {
                host_path: "/dev/dri/card0".into(),
                container_path: "/dev/dri/by-path/pci-0000:01:00.0-card".into(),
                readonly: false,
            }],
            ..Default::default()
        };
        let content = r"[Files]
X-Lasper-Nvidia-Begin=managed-by-lasper
Bind=/dev/dri/card0:/dev/dri/by-path/pci-0000\:01\:00.0-card
X-Lasper-Nvidia-End=true
";

        assert!(marker_binds_match_state(content, &state));
    }

    #[tokio::test]
    async fn cleanup_container_garbage_routes_paths_as_shell_args() {
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut runner = MockCommandRunner::new();
        {
            let calls = calls.clone();
            runner.expect_run().once().returning(move |program, args| {
                assert_eq!(program, "systemd-nspawn");
                calls.lock().unwrap().push(args.clone());
                Ok(mock_output(true))
            });
        }

        cleanup_container_garbage(
            "valid-machine",
            &["/usr/lib/odd'path.so".into(), "/usr/lib/zero.so".into()],
            &runner,
        )
        .await
        .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let args = &calls[0];
        let script_index = args
            .iter()
            .position(|arg| arg == "-c")
            .expect("shell script marker")
            + 1;
        let script = &args[script_index];
        assert!(script.contains("\"$path\""));
        assert!(!script.contains("odd'path"));
        assert!(args.iter().any(|arg| arg == "/usr/lib/odd'path.so"));
        assert!(args.iter().any(|arg| arg == "/usr/lib/zero.so"));
    }

    #[tokio::test]
    async fn cleanup_container_garbage_rejects_invalid_machine_name_before_nspawn() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run().never();

        let result =
            cleanup_container_garbage("../escape", &["/usr/lib/libcuda.so".into()], &runner).await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[tokio::test]
    async fn inject_env_refreshes_ld_cache_after_writing_ld_config() {
        let staging = crate::nspawn::platform::nvidia::NvidiaStagingStore::new(None);
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut runner = MockCommandRunner::new();
        {
            let calls = calls.clone();
            runner.expect_run().returning(move |program, args| {
                assert_eq!(program, "systemd-nspawn");
                calls.lock().unwrap().push(args.clone());
                Ok(mock_output(true))
            });
        }
        let state = NvidiaState {
            ldcache_folders: vec![
                "/usr/lib/wsl/drivers/nvam.inf_amd64_example".into(),
                "/usr/lib/wsl/lib".into(),
            ],
            ..Default::default()
        };
        let name = format!("ldconfig-test-{}", std::process::id());
        let target = crate::nspawn::adapters::rootfs::RootfsTarget::from_provisioned_path(
            &name,
            &crate::paths::machine_root(&name),
        )
        .unwrap();

        inject_env_once(&name, &target, &state, &staging, &runner)
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0]
            .iter()
            .any(|arg| arg.contains("/etc/ld.so.conf.d/lasper-nvidia.conf")));
        assert!(calls[1].iter().any(|arg| arg == "ldconfig"));
    }
}
