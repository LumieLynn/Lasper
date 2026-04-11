use std::io::Write;
use std::path::Path;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::CreateUser;

/// Sets up the target user's shell environments with exported Wayland variables.
pub async fn setup_wayland_shell_env(rootfs: &Path, user: &CreateUser) -> Result<()> {
    let home_dir = if user.username == "root" {
        "/root".to_string()
    } else {
        format!("/home/{}", user.username)
    };
    let env_script_path = format!("{}/.wayland-env", home_dir);

    let host_display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());

    let script_content = format!(
        r#"
export XDG_RUNTIME_DIR=/run/user/$(id -u)
export WAYLAND_DISPLAY=wayland-socket
export DISPLAY={}
mkdir -p "$XDG_RUNTIME_DIR"
ln -sf /mnt/wayland-socket "$XDG_RUNTIME_DIR/wayland-socket"
"#,
        host_display
    );

    let full_path = rootfs.join(env_script_path.trim_start_matches('/'));
    if let Some(parent) = full_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| NspawnError::Io(parent.to_path_buf(), e))?;
    }
    tokio::fs::write(&full_path, script_content)
        .await
        .map_err(|e| NspawnError::Io(full_path, e))?;

    let shell = user.shell.as_str();
    let rc_file = if shell.ends_with("zsh") {
        ".zshrc"
    } else if shell.ends_with("fish") {
        let fish_dir = rootfs.join(format!(
            "{}/.config/fish/conf.d",
            home_dir.trim_start_matches('/')
        ));
        tokio::fs::create_dir_all(&fish_dir)
            .await
            .map_err(|e| NspawnError::Io(fish_dir.clone(), e))?;
        let host_display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        let fish_script = format!(
            r#"
set -gx XDG_RUNTIME_DIR /run/user/(id -u)
set -gx WAYLAND_DISPLAY wayland-socket
set -gx DISPLAY {}
mkdir -p $XDG_RUNTIME_DIR
ln -sf /mnt/wayland-socket $XDG_RUNTIME_DIR/wayland-socket
"#,
            host_display
        );
        let script_path = fish_dir.join("wayland-env.fish");
        tokio::fs::write(&script_path, fish_script)
            .await
            .map_err(|e| NspawnError::Io(script_path, e))?;
        return Ok(());
    } else {
        ".bashrc"
    };

    let rc_full_path = rootfs.join(format!("{}/{}", home_dir.trim_start_matches('/'), rc_file));
    if let Ok(mut f) = tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&rc_full_path)
        .await
    {
        use tokio::io::AsyncWriteExt;
        let _ = f
            .write_all(b"\n[ -f ~/.wayland-env ] && source ~/.wayland-env\n")
            .await;
    }

    Ok(())
}
