use crate::nspawn::errors::Result;
use crate::nspawn::models::{validate_login_shell, validate_login_username, CreateUser};
use crate::nspawn::sys::ElevatedIo;
use std::path::Path;

const WAYLAND_RC_MARKER: &str = "# Added by Lasper: Wayland passthrough";
const WAYLAND_RC_SOURCE: &str = "[ -f ~/.wayland-env ] && source ~/.wayland-env";

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
    let existing = io.read_to_string(&rc_full_path).await?;
    if let Some(updated) = append_wayland_source(existing.as_deref()) {
        io.write(&rc_full_path, &updated).await?;
    }

    Ok(())
}

fn append_wayland_source(existing: Option<&str>) -> Option<String> {
    let existing = existing.unwrap_or_default();
    if existing.contains("source ~/.wayland-env") {
        return None;
    }

    let mut updated = existing.to_string();
    if !updated.is_empty() {
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push('\n');
    }
    updated.push_str(WAYLAND_RC_MARKER);
    updated.push('\n');
    updated.push_str(WAYLAND_RC_SOURCE);
    updated.push('\n');
    Some(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::ops::PermissionLevel;

    fn zsh_user() -> CreateUser {
        CreateUser {
            username: "alice".into(),
            shell: "/usr/bin/zsh".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn missing_zshrc_is_created_with_a_managed_source_block() {
        let rootfs = tempfile::tempdir().unwrap();
        let io = ElevatedIo::new(PermissionLevel::Root);

        setup_wayland_shell_env(rootfs.path(), &zsh_user(), &io)
            .await
            .unwrap();

        let zshrc = tokio::fs::read_to_string(rootfs.path().join("home/alice/.zshrc"))
            .await
            .unwrap();
        assert_eq!(
            zshrc,
            format!("{}\n{}\n", WAYLAND_RC_MARKER, WAYLAND_RC_SOURCE)
        );
    }

    #[tokio::test]
    async fn existing_rc_content_is_preserved_and_source_is_not_duplicated() {
        let rootfs = tempfile::tempdir().unwrap();
        let home = rootfs.path().join("home/alice");
        tokio::fs::create_dir_all(&home).await.unwrap();
        tokio::fs::write(home.join(".zshrc"), "export EDITOR=vim")
            .await
            .unwrap();
        let io = ElevatedIo::new(PermissionLevel::Root);

        setup_wayland_shell_env(rootfs.path(), &zsh_user(), &io)
            .await
            .unwrap();
        setup_wayland_shell_env(rootfs.path(), &zsh_user(), &io)
            .await
            .unwrap();

        let zshrc = tokio::fs::read_to_string(home.join(".zshrc"))
            .await
            .unwrap();
        assert!(zshrc.starts_with("export EDITOR=vim\n\n"));
        assert_eq!(zshrc.matches(WAYLAND_RC_MARKER).count(), 1);
        assert_eq!(zshrc.matches(WAYLAND_RC_SOURCE).count(), 1);
    }

    #[test]
    fn legacy_unmarked_source_line_is_left_unchanged() {
        let existing = format!("{}\n", WAYLAND_RC_SOURCE);
        assert_eq!(append_wayland_source(Some(&existing)), None);
    }
}
