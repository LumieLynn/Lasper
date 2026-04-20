use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::CreateUser;
use crate::nspawn::sys::command::{log_output, new_command, CommandLogged};
use std::path::{Path, PathBuf};

/// Create a user inside the container rootfs via `systemd-nspawn --directory … useradd`.
pub async fn create_user_in_container(rootfs: &Path, user: &CreateUser) -> Result<()> {
    let shell = if user.shell.is_empty() {
        "/bin/bash"
    } else {
        user.shell.as_str()
    };

    let out = new_command("useradd")
        .args([
            "--root",
            &rootfs.to_string_lossy(),
            "-m",
            "-s",
            shell,
            &user.username,
        ])
        .logged_output("useradd")
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("useradd"), e))?;
    if !out.status.success() {
        return Err(NspawnError::cmd_failed(
            "useradd in container",
            format!("useradd --root {:?} ...", rootfs),
            &out,
        ));
    }

    if user.sudoer {
        for group in ["sudo", "wheel"] {
            let r = new_command("usermod")
                .args([
                    "--root",
                    &rootfs.to_string_lossy(),
                    "-aG",
                    group,
                    &user.username,
                ])
                .logged_output("usermod")
                .await;
            if r.map(|o| o.status.success()).unwrap_or(false) {
                let sudoers_dir = rootfs.join("etc/sudoers.d");
                let sudoers_file = sudoers_dir.join(group);
                let content = format!("%{} ALL=(ALL:ALL) ALL\n", group);
                crate::nspawn::sys::io::AsyncLockedWriter::write_atomic(&sudoers_file, &content)
                    .await?;

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = tokio::fs::metadata(&sudoers_file).await {
                        let mut perms = meta.permissions();
                        perms.set_mode(0o440);
                        let _ = tokio::fs::set_permissions(&sudoers_file, perms).await;
                    }
                }
                break;
            }
        }
    }

    if !user.password.is_empty() {
        let mut cmd = new_command("chpasswd");
        cmd.args(["--root", &rootfs.to_string_lossy()]);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| NspawnError::Io(PathBuf::from("chpasswd"), e))?;

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let cred = format!("{}:{}\n", user.username, user.password);
            let _ = stdin.write_all(cred.as_bytes()).await;
        }

        let res = child
            .wait_with_output()
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("chpasswd"), e))?;
        log_output("chpasswd", &res);
        if !res.status.success() {
            log::warn!(
                "chpasswd in container failed, proceeding anyway: {}",
                String::from_utf8_lossy(&res.stderr)
            );
        }
    }

    Ok(())
}

/// Set the root password via `chpasswd` inside the container.
pub async fn set_root_password(rootfs: &Path, password: &str) -> Result<()> {
    if password.is_empty() {
        return Ok(());
    }
    let mut cmd = new_command("chpasswd");
    cmd.args(["--root", &rootfs.to_string_lossy()]);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| NspawnError::Io(PathBuf::from("chpasswd"), e))?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let cred = format!("root:{}\n", password);
        let _ = stdin.write_all(cred.as_bytes()).await;
    }

    let res = child
        .wait_with_output()
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("chpasswd"), e))?;
    log_output("chpasswd", &res);
    if !res.status.success() {
        log::warn!(
            "chpasswd for root in container failed, proceeding anyway: {}",
            String::from_utf8_lossy(&res.stderr)
        );
    }
    Ok(())
}
