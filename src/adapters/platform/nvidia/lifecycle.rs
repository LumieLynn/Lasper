use super::discovery::get_nvidia_state;
use super::state::{calculate_death_list, calculate_removed_binds, NvidiaState};
use crate::adapters::config::nspawn_file::NspawnConfig;
use crate::adapters::error::Result;
use crate::domain::nvidia::NvidiaPassthroughMode;

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
                crate::adapters::config::nspawn_file::parse_nspawn_bind_paths(val)
            {
                file_binds.insert((host, container, true));
            }
        } else if let Some(val) = trimmed.strip_prefix("Bind=") {
            if let Some((host, container)) =
                crate::adapters::config::nspawn_file::parse_nspawn_bind_paths(val)
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

async fn inject_persistent_device_allow(
    name: &str,
    state: &NvidiaState,
    systemd_unit: &crate::adapters::config::SystemdUnitStore,
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

/// Assemble the NVIDIA-specific rootfs data and submit one typed mutation.
pub(crate) async fn inject_env_once(
    target: &crate::adapters::rootfs::RootfsTarget,
    state: &NvidiaState,
    rootfs: &crate::adapters::rootfs::RootfsStore,
) -> Result<Vec<String>> {
    let (ld_cache_folders, environment, write_environment) = nvidia_rootfs_config(state);
    rootfs
        .configure_nvidia(target, ld_cache_folders, environment, write_environment)
        .await
}

fn nvidia_rootfs_config(state: &NvidiaState) -> (Vec<String>, Vec<(String, String)>, bool) {
    let mut folders = state.ldcache_folders.clone();
    if let Some(profile) = &state.profile {
        if profile.mode == NvidiaPassthroughMode::Categorized {
            use crate::domain::nvidia::NvidiaFileCategory;
            for category in [NvidiaFileCategory::Lib64, NvidiaFileCategory::Lib32] {
                if let Some(destination) = profile.category_destinations.get(&category) {
                    folders.push(destination.clone());
                }
            }
        }
    }
    let write_environment = state
        .profile
        .as_ref()
        .is_some_and(|profile| profile.inject_env);
    let environment = if write_environment {
        state.env_vars.clone()
    } else {
        Vec::new()
    };
    (folders, environment, write_environment)
}

pub async fn ensure_gpu_passthrough(
    name: &str,
    nspawn: &crate::adapters::config::NspawnConfigStore,
    systemd_unit: &crate::adapters::config::SystemdUnitStore,
    state_store: &crate::adapters::platform::nvidia::NvidiaStateStore,
    rootfs: &crate::adapters::rootfs::RootfsStore,
) -> Result<()> {
    // 1. Check if GPU passthrough is enabled in .nspawn config
    let config = match nspawn.read(name).await? {
        Some(c) => c,
        None => return Ok(()),
    };
    if !config.is_gpu_enabled()? {
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

    // Discovery and snapshot validation are fail-closed. No rootfs, config,
    // unit, or state mutation may move above this boundary.
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
    let removed_binds = calculate_removed_binds(&old_state, &host_state);
    if !death_list.is_empty() {
        log_step!(
            name,
            "Surgery",
            "Marked {} files for removal/update.",
            death_list.len()
        );
    }

    // Detect marker-external administrator bind conflicts before touching the
    // rootfs. The locked write below repeats this check against the latest file.
    NspawnConfig::apply_gpu_passthrough_to_content(
        config.content.clone(),
        &host_state,
        &removed_binds,
    )?;

    // 4. Cleanup stale files in rootfs
    for warning in rootfs.cleanup_nvidia(name, &death_list).await? {
        log::warn!("{}", warning);
    }

    // 5. Update .nspawn config (symlinks are now synthesized as Bind entries here)
    log_step!(name, "Surgery", "Mutating .nspawn configuration AST...");
    nspawn.update_gpu(name, &host_state, &removed_binds).await?;

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

    #[test]
    fn marker_match_preserves_writable_bind_destination() {
        let state = NvidiaState {
            binds: vec![crate::adapters::platform::nvidia::state::PassthroughBind {
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
            binds: vec![crate::adapters::platform::nvidia::state::PassthroughBind {
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

    #[test]
    fn nvidia_rootfs_config_includes_categorized_library_paths_and_opt_in_env() {
        use crate::domain::nvidia::NvidiaFileCategory;
        use crate::domain::nvidia::NvidiaPassthroughProfile;

        let mut profile = NvidiaPassthroughProfile {
            mode: NvidiaPassthroughMode::Categorized,
            inject_env: true,
            ..Default::default()
        };
        profile
            .category_destinations
            .insert(NvidiaFileCategory::Lib64, "/opt/nvidia/lib64".into());
        let state = NvidiaState {
            ldcache_folders: vec!["/usr/lib/wsl/lib".into()],
            env_vars: vec![("NVIDIA_VISIBLE_DEVICES".into(), "void".into())],
            profile: Some(profile),
            ..Default::default()
        };

        let (folders, environment, write_environment) = nvidia_rootfs_config(&state);

        assert_eq!(folders, vec!["/usr/lib/wsl/lib", "/opt/nvidia/lib64"]);
        assert_eq!(
            environment,
            vec![("NVIDIA_VISIBLE_DEVICES".into(), "void".into())]
        );
        assert!(write_environment);
    }
}
