use crate::nspawn::adapters::rootfs::process::{nspawn_io_path, RootfsProcessRunner};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{validate_login_shell, validate_login_username};
use crate::nspawn::sys::log_output;
use std::path::Path;

const WAYLAND_RC_MARKER: &str = "# Added by Lasper: Wayland passthrough";
const WAYLAND_RC_SOURCE: &str = "[ -f ~/.wayland-env ] && source ~/.wayland-env";

pub(crate) async fn setup_wayland_shell_env(
    rootfs: &Path,
    username: &str,
    shell: &str,
    runner: &dyn RootfsProcessRunner,
) -> Result<()> {
    validate_wayland_config(username, shell)?;

    let home = if username == "root" {
        "/root".to_string()
    } else {
        format!("/home/{username}")
    };
    let env_path = format!("{home}/.wayland-env");
    let script = r#"
export WAYLAND_DISPLAY=/mnt/wayland-socket
"#
    .as_bytes()
    .to_vec();
    write_user_file(rootfs, username, &env_path, script, runner).await?;

    if shell.ends_with("fish") {
        let fish_path = format!("{home}/.config/fish/conf.d/wayland-env.fish");
        let fish_script = r#"
set -gx WAYLAND_DISPLAY /mnt/wayland-socket
"#
        .as_bytes()
        .to_vec();
        write_user_file(rootfs, username, &fish_path, fish_script, runner).await?;
        return Ok(());
    }

    let rc_file = if shell.ends_with("zsh") {
        ".zshrc"
    } else {
        ".bashrc"
    };
    append_wayland_source(rootfs, username, &format!("{home}/{rc_file}"), runner).await
}

pub(crate) fn validate_wayland_config(username: &str, shell: &str) -> Result<()> {
    validate_login_username(username)?;
    validate_login_shell(shell)
}

async fn write_user_file(
    rootfs: &Path,
    username: &str,
    target: &str,
    content: Vec<u8>,
    runner: &dyn RootfsProcessRunner,
) -> Result<()> {
    let output = runner
        .run(
            rootfs,
            vec![
                "sh".into(),
                "-eu".into(),
                "-c".into(),
                concat!(
                    "target=$1; owner=$2; parent=${target%/*}; ",
                    "group=$(id -gn \"$owner\"); ",
                    "install -d -m 0755 -o \"$owner\" -g \"$group\" \"$parent\"; ",
                    "[ ! -L \"$target\" ]; ",
                    "cat > \"$target\"; chmod 0644 \"$target\"; ",
                    "chown \"$owner:$group\" \"$target\""
                )
                .into(),
                "_".into(),
                target.into(),
                username.into(),
            ],
            Some(content),
        )
        .await
        .map_err(|error| NspawnError::Io(nspawn_io_path(), error))?;
    log_output("wayland environment", &output);
    if output.status.success() {
        Ok(())
    } else {
        Err(NspawnError::cmd_failed(
            "write Wayland environment in container",
            format!("systemd-nspawn -D {:?} -- write {target}", rootfs),
            &output,
        ))
    }
}

async fn append_wayland_source(
    rootfs: &Path,
    username: &str,
    target: &str,
    runner: &dyn RootfsProcessRunner,
) -> Result<()> {
    let output = runner
        .run(
            rootfs,
            vec![
                "sh".into(),
                "-eu".into(),
                "-c".into(),
                format!(
                    concat!(
                        "target=$1; owner=$2; parent=${{target%/*}}; ",
                        "group=$(id -gn \"$owner\"); ",
                        "install -d -m 0755 -o \"$owner\" -g \"$group\" \"$parent\"; ",
                        "[ ! -L \"$target\" ]; ",
                        "touch \"$target\"; ",
                        "if ! grep -Fq {source} \"$target\"; then ",
                        "if [ -s \"$target\" ]; then ",
                        "last=$(tail -c 1 \"$target\"); [ -z \"$last\" ] || printf '\\n' >> \"$target\"; ",
                        "printf '\\n' >> \"$target\"; fi; ",
                        "printf '%s\\n%s\\n' {marker} {source} >> \"$target\"; fi; ",
                        "chmod 0644 \"$target\"; chown \"$owner:$group\" \"$target\""
                    ),
                    marker = shell_single_quote(WAYLAND_RC_MARKER),
                    source = shell_single_quote(WAYLAND_RC_SOURCE),
                ),
                "_".into(),
                target.into(),
                username.into(),
            ],
            None,
        )
        .await
        .map_err(|error| NspawnError::Io(nspawn_io_path(), error))?;
    log_output("wayland shell rc", &output);
    if output.status.success() {
        Ok(())
    } else {
        Err(NspawnError::cmd_failed(
            "update Wayland shell rc in container",
            format!("systemd-nspawn -D {:?} -- update {target}", rootfs),
            &output,
        ))
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::adapters::rootfs::process::MockRootfsProcessRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;
    use std::sync::{Arc, Mutex};

    fn success_output() -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    #[tokio::test]
    async fn missing_zshrc_is_created_with_a_managed_source_block() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut runner = MockRootfsProcessRunner::new();
        let captured = calls.clone();
        runner
            .expect_run()
            .times(2)
            .returning(move |_, command, stdin| {
                captured.lock().unwrap().push((command, stdin));
                Ok(success_output())
            });

        setup_wayland_shell_env(Path::new("/tmp/rootfs"), "alice", "/usr/bin/zsh", &runner)
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert!(calls[0]
            .0
            .iter()
            .any(|arg| arg == "/home/alice/.wayland-env"));
        let env = String::from_utf8(calls[0].1.clone().unwrap()).unwrap();
        assert!(env.contains("WAYLAND_DISPLAY=/mnt/wayland-socket"));
        assert!(!env.lines().any(|line| line.starts_with("export DISPLAY=")));
        assert!(!env.contains("host-x11"));
        assert!(!env.contains("XDG_RUNTIME_DIR"));
        assert!(!env.contains("mkdir -p"));
        assert!(!env.contains("ln -sf /mnt/wayland-socket"));
        assert!(calls[1].0.iter().any(|arg| arg == "/home/alice/.zshrc"));
        assert!(calls[1].0.iter().any(|arg| arg.contains(WAYLAND_RC_MARKER)));
        assert!(calls[1].1.is_none());
    }

    #[tokio::test]
    async fn fish_env_uses_bound_wayland_socket_without_runtime_directory_mutation() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut runner = MockRootfsProcessRunner::new();
        let captured = calls.clone();
        runner
            .expect_run()
            .times(2)
            .returning(move |_, command, stdin| {
                captured.lock().unwrap().push((command, stdin));
                Ok(success_output())
            });

        setup_wayland_shell_env(Path::new("/tmp/rootfs"), "alice", "/usr/bin/fish", &runner)
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        let fish_env = String::from_utf8(calls[1].1.clone().unwrap()).unwrap();
        assert!(fish_env.contains("WAYLAND_DISPLAY /mnt/wayland-socket"));
        assert!(!fish_env
            .lines()
            .any(|line| line.starts_with("set -gx DISPLAY ")));
        assert!(!fish_env.contains("host-x11"));
        assert!(!fish_env.contains("XDG_RUNTIME_DIR"));
        assert!(!fish_env.contains("mkdir -p"));
        assert!(!fish_env.contains("ln -sf /mnt/wayland-socket"));
    }
}
