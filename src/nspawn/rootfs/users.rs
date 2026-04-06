use std::path::{Path, PathBuf};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::CreateUser;
use crate::nspawn::utils::command::new_command;

/// Create a user inside the container rootfs via `systemd-nspawn --directory … useradd`.
pub async fn create_user_in_container(rootfs: &Path, user: &CreateUser) -> Result<()> {
    let shell = if user.shell.is_empty() {
        "/bin/bash"
    } else {
        user.shell.as_str()
    };

    let out = new_command("systemd-nspawn")
        .args([
            "--directory",
            &rootfs.to_string_lossy(),
            "useradd",
            "-m",
            "-s",
            shell,
            &user.username,
        ])
        .output()
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "useradd in container",
            format!("systemd-nspawn --directory {:?} useradd ...", rootfs),
            &out,
        ));
    }

    if user.sudoer {
        for group in ["sudo", "wheel"] {
            let r = new_command("systemd-nspawn")
                .args([
                    "--directory",
                    &rootfs.to_string_lossy(),
                    "usermod",
                    "-aG",
                    group,
                    &user.username,
                ])
                .output()
                .await;
            if r.map(|o| o.status.success()).unwrap_or(false) {
                let sudoers_dir = rootfs.join("etc/sudoers.d");
                std::fs::create_dir_all(&sudoers_dir)
                    .map_err(|e| NspawnError::Io(sudoers_dir.clone(), e))?;
                let sudoers_file = sudoers_dir.join(group);
                let content = format!("%{} ALL=(ALL:ALL) ALL\n", group);
                std::fs::write(&sudoers_file, content)
                    .map_err(|e| NspawnError::Io(sudoers_file.clone(), e))?;

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(mut perms) = std::fs::metadata(&sudoers_file).map(|m| m.permissions())
                    {
                        perms.set_mode(0o440);
                        let _ = std::fs::set_permissions(&sudoers_file, perms);
                    }
                }
                break;
            }
        }
    }

    if !user.password.is_empty() {
        let script = format!("echo '{}:{}' | chpasswd", user.username, user.password);
        let mut cmd = new_command("systemd-nspawn");
        cmd.args([
            "-q",
            "--directory",
            &rootfs.to_string_lossy(),
            "sh",
            "-c",
            &script,
        ]);

        let res = cmd
            .output()
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
        if !res.status.success() {
            return Err(NspawnError::cmd_failed(
                "chpasswd in container",
                format!("systemd-nspawn --directory {:?} sh -c ...", rootfs),
                &res,
            ));
        }
    }

    Ok(())
}

/// Set the root password via `chpasswd` inside the container.
pub async fn set_root_password(rootfs: &Path, password: &str) -> Result<()> {
    if password.is_empty() {
        return Ok(());
    }
    let script = format!("echo 'root:{}' | chpasswd", password);
    let mut cmd = new_command("systemd-nspawn");
    cmd.args([
        "-q",
        "--directory",
        &rootfs.to_string_lossy(),
        "sh",
        "-c",
        &script,
    ]);

    let res = cmd
        .output()
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
    if !res.status.success() {
        return Err(NspawnError::cmd_failed(
            "chpasswd for root in container",
            format!("systemd-nspawn --directory {:?} sh -c ...", rootfs),
            &res,
        ));
    }
    Ok(())
}
