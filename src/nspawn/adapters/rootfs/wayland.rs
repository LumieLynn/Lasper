use crate::nspawn::errors::Result;
use crate::nspawn::models::{validate_login_shell, validate_login_username, CreateUser};
use crate::nspawn::sys::ElevatedIo;
use std::path::Path;

/// Sets up the target user's shell environments with exported Wayland variables.
pub async fn setup_wayland_shell_env(
    rootfs: &Path,
    user: &CreateUser,
    io: &ElevatedIo,
) -> Result<()> {
    validate_login_username(&user.username)?;
    validate_login_shell(&user.shell)?;

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
if [ -d /mnt/host-x11 ] && [ -d /tmp/.X11-unix ]; then
    for sock in /mnt/host-x11/*; do
        if [ -S "$sock" ]; then
            ln -sf "$sock" "/tmp/.X11-unix/$(basename "$sock")" 2>/dev/null
        fi
    done
fi
"#,
        host_display
    );

    let full_path = rootfs.join(env_script_path.trim_start_matches('/'));
    if let Some(parent) = full_path.parent() {
        io.create_dir_all(parent).await?;
    }
    io.write(&full_path, &script_content).await?;

    let shell = user.shell.as_str();
    if shell.ends_with("fish") {
        let fish_dir = rootfs.join(format!(
            "{}/.config/fish/conf.d",
            home_dir.trim_start_matches('/')
        ));
        io.create_dir_all(&fish_dir).await?;
        let host_display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
        let fish_script = format!(
            r#"
set -gx XDG_RUNTIME_DIR /run/user/(id -u)
set -gx WAYLAND_DISPLAY wayland-socket
set -gx DISPLAY {}
mkdir -p "$XDG_RUNTIME_DIR"
ln -sf /mnt/wayland-socket "$XDG_RUNTIME_DIR/wayland-socket"
if test -d /mnt/host-x11; and test -d /tmp/.X11-unix
    for sock in /mnt/host-x11/*
        if test -S "$sock"
            ln -sf "$sock" "/tmp/.X11-unix/"(basename "$sock") 2>/dev/null
        end
    end
end
"#,
            host_display
        );
        let script_path = fish_dir.join("wayland-env.fish");
        io.write(&script_path, &fish_script).await?;
        return Ok(());
    }

    let rc_file = if shell.ends_with("zsh") {
        ".zshrc"
    } else {
        ".bashrc"
    };
    let rc_full_path = rootfs.join(format!("{}/{}", home_dir.trim_start_matches('/'), rc_file));
    let existing = io.read_to_string(&rc_full_path).await?.unwrap_or_default();
    if !existing.contains("source ~/.wayland-env") {
        let appended = format!(
            "{}\n[ -f ~/.wayland-env ] && source ~/.wayland-env\n",
            existing
        );
        io.write(&rc_full_path, &appended).await?;
    }

    Ok(())
}
