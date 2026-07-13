use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::CreateUser;
use crate::nspawn::sys::{log_output, CommandRunner, ElevatedIo};
use std::path::{Path, PathBuf};

/// Create a user inside the container rootfs via `systemd-nspawn -D`.
pub async fn create_user_in_container(
    rootfs: &Path,
    user: &CreateUser,
    logs: &tokio::sync::mpsc::Sender<String>,
    cmd_runner: &dyn CommandRunner,
    io: &ElevatedIo,
) -> Result<()> {
    let rootfs_s = rootfs.to_string_lossy().to_string();
    let shell = if user.shell.is_empty() {
        "/bin/bash"
    } else {
        user.shell.as_str()
    };

    let out = cmd_runner
        .run(
            "systemd-nspawn",
            vec![
                "-D".into(),
                rootfs_s.clone(),
                "--quiet".into(),
                "useradd".into(),
                "-m".into(),
                "-s".into(),
                shell.into(),
                user.username.clone(),
            ],
        )
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
    log_output("useradd", &out);
    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "useradd in container",
            format!("systemd-nspawn -D {:?} -- useradd ...", rootfs),
            &out,
        ));
    }

    if user.sudoer {
        for group in ["sudo", "wheel"] {
            let r = cmd_runner
                .run(
                    "systemd-nspawn",
                    vec![
                        "-D".into(),
                        rootfs_s.clone(),
                        "--quiet".into(),
                        "usermod".into(),
                        "-aG".into(),
                        group.into(),
                        user.username.clone(),
                    ],
                )
                .await;
            if let Ok(ref o) = r {
                log_output("usermod", o);
            }
            if r.map(|o| o.status.success()).unwrap_or(false) {
                let sudoers_file = rootfs.join("etc/sudoers.d").join(group);
                let content = format!("%{} ALL=(ALL:ALL) ALL\n", group);
                io.write(&sudoers_file, &content).await?;

                #[cfg(unix)]
                {
                    // best-effort chmod via systemd-nspawn
                    let _ = cmd_runner
                        .run(
                            "systemd-nspawn",
                            vec![
                                "-D".into(),
                                rootfs_s.clone(),
                                "chmod".into(),
                                "440".into(),
                                format!("/etc/sudoers.d/{}", group),
                            ],
                        )
                        .await;
                }
                break;
            }
        }
    }

    if !user.password.is_empty() {
        // Pass credentials as a positional shell parameter ($1), never
        // interpolated into the script string.  $1 is data (from execve argv),
        // not code; "$1" prevents word-splitting and globbing.
        let cred = format!("{}:{}", user.username, user.password);
        let res = cmd_runner
            .run(
                "systemd-nspawn",
                vec![
                    "-D".into(),
                    rootfs_s.clone(),
                    "--quiet".into(),
                    "--pipe".into(),
                    "sh".into(),
                    "-c".into(),
                    "printf '%s\\n' \"$1\" | chpasswd".into(),
                    "_".into(), // $0 (unused)
                    cred,       // $1 — data, never interpreted as shell code
                ],
            )
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
        log_output("chpasswd", &res);
        if !res.status.success() {
            let msg = format!(
                "WARNING: chpasswd failed for user '{}': {}",
                user.username,
                String::from_utf8_lossy(&res.stderr).trim()
            );
            log::warn!("{}", msg);
            let _ = logs.send(msg).await;
        }
    }

    Ok(())
}

/// Set the root password via `chpasswd` inside the container.
pub async fn set_root_password(
    rootfs: &Path,
    password: &str,
    logs: &tokio::sync::mpsc::Sender<String>,
    cmd_runner: &dyn CommandRunner,
) -> Result<()> {
    if password.is_empty() {
        return Ok(());
    }
    // Pass credentials as a positional shell parameter ($1), never
    // interpolated into the script string.  $1 is data (from execve argv),
    // not code; "$1" prevents word-splitting and globbing.
    let cred = format!("root:{}", password);
    let res = cmd_runner
        .run(
            "systemd-nspawn",
            vec![
                "-D".into(),
                rootfs.to_string_lossy().to_string(),
                "--quiet".into(),
                "--pipe".into(),
                "sh".into(),
                "-c".into(),
                "printf '%s\\n' \"$1\" | chpasswd".into(),
                "_".into(), // $0 (unused)
                cred,       // $1 — data, never interpreted as shell code
            ],
        )
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
    log_output("chpasswd", &res);
    if !res.status.success() {
        let msg = format!(
            "WARNING: chpasswd for root failed: {}",
            String::from_utf8_lossy(&res.stderr).trim()
        );
        log::warn!("{}", msg);
        let _ = logs.send(msg).await;
    }
    Ok(())
}
