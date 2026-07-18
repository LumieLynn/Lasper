use crate::nspawn::adapters::rootfs::process::{nspawn_io_path, RootfsProcessRunner};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{validate_chpasswd_secret, CreateUser};
use crate::nspawn::sys::log_output;
use std::path::Path;

pub(crate) async fn create_user_in_container(
    rootfs: &Path,
    user: &CreateUser,
    runner: &dyn RootfsProcessRunner,
) -> Result<Vec<String>> {
    user.validate()?;
    let shell = user.login_shell();

    let output = runner
        .run(
            rootfs,
            vec![
                "useradd".into(),
                "-m".into(),
                "-s".into(),
                shell.into(),
                user.username.clone(),
            ],
            None,
        )
        .await
        .map_err(|error| NspawnError::Io(nspawn_io_path(), error))?;
    log_output("useradd", &output);
    if !output.status.success() {
        return Err(NspawnError::cmd_failed(
            "useradd in container",
            format!("systemd-nspawn -D {:?} -- useradd ...", rootfs),
            &output,
        ));
    }

    if user.sudoer {
        configure_sudoer(rootfs, &user.username, runner).await?;
    }

    let mut warnings = Vec::new();
    if !user.password.is_empty() {
        let output = run_chpasswd(rootfs, &user.username, &user.password, runner).await?;
        if !output.status.success() {
            warnings.push(format!(
                "WARNING: chpasswd failed for user '{}': {}",
                user.username,
                command_error(&output)
            ));
        }
    }

    Ok(warnings)
}

pub(crate) async fn set_root_password(
    rootfs: &Path,
    password: &str,
    runner: &dyn RootfsProcessRunner,
) -> Result<Vec<String>> {
    if password.is_empty() {
        return Ok(Vec::new());
    }
    let output = run_chpasswd(rootfs, "root", password, runner).await?;
    if output.status.success() {
        Ok(Vec::new())
    } else {
        Ok(vec![format!(
            "WARNING: chpasswd for root failed: {}",
            command_error(&output)
        )])
    }
}

async fn run_chpasswd(
    rootfs: &Path,
    username: &str,
    password: &str,
    runner: &dyn RootfsProcessRunner,
) -> Result<std::process::Output> {
    validate_chpasswd_secret("password", password)?;
    let input = format!("{username}:{password}\n").into_bytes();
    let output = runner
        .run(rootfs, vec!["chpasswd".into()], Some(input))
        .await
        .map_err(|error| NspawnError::Io(nspawn_io_path(), error))?;
    log_output("chpasswd", &output);
    Ok(output)
}

async fn configure_sudoer(
    rootfs: &Path,
    username: &str,
    runner: &dyn RootfsProcessRunner,
) -> Result<()> {
    for group in ["sudo", "wheel"] {
        let output = runner
            .run(
                rootfs,
                vec![
                    "usermod".into(),
                    "-aG".into(),
                    group.into(),
                    username.into(),
                ],
                None,
            )
            .await;
        if let Ok(output) = &output {
            log_output("usermod", output);
        }
        if !output.is_ok_and(|output| output.status.success()) {
            continue;
        }

        let sudoers = format!("%{group} ALL=(ALL:ALL) ALL\n").into_bytes();
        let output = runner
            .run(
                rootfs,
                vec![
                    "sh".into(),
                    "-eu".into(),
                    "-c".into(),
                    concat!(
                        "target=/etc/sudoers.d/$1; ",
                        "install -d -m 0750 /etc/sudoers.d; ",
                        "[ ! -L \"$target\" ]; ",
                        "cat > \"$target\"; chmod 0440 \"$target\""
                    )
                    .into(),
                    "_".into(),
                    group.into(),
                ],
                Some(sudoers),
            )
            .await
            .map_err(|error| NspawnError::Io(nspawn_io_path(), error))?;
        log_output("sudoers", &output);
        if !output.status.success() {
            return Err(NspawnError::cmd_failed(
                "write sudoers policy in container",
                format!("systemd-nspawn -D {:?} -- write sudoers", rootfs),
                &output,
            ));
        }
        break;
    }
    Ok(())
}

fn command_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    if message.is_empty() {
        format!("process exited with {}", output.status)
    } else {
        message.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::adapters::rootfs::process::MockRootfsProcessRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;

    fn success_output() -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    #[tokio::test]
    async fn create_user_uses_default_shell_after_validation() {
        let mut runner = MockRootfsProcessRunner::new();
        runner.expect_run().once().returning(|_, command, stdin| {
            assert!(stdin.is_none());
            assert!(command.windows(2).any(|pair| pair == ["-s", "/bin/bash"]));
            assert!(command.iter().any(|arg| arg == "alice"));
            Ok(success_output())
        });
        let user = CreateUser {
            username: "alice".into(),
            shell: String::new(),
            ..Default::default()
        };

        create_user_in_container(Path::new("/tmp/rootfs"), &user, &runner)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_user_rejects_chpasswd_record_injection_before_running() {
        let mut runner = MockRootfsProcessRunner::new();
        runner.expect_run().never();
        let user = CreateUser {
            username: "alice".into(),
            password: "safe\nroot:pwned".into(),
            shell: "/bin/bash".into(),
            sudoer: false,
        };

        let result = create_user_in_container(Path::new("/tmp/rootfs"), &user, &runner).await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[tokio::test]
    async fn password_is_sent_over_stdin_and_not_argv() {
        let mut runner = MockRootfsProcessRunner::new();
        runner.expect_run().once().returning(|_, command, stdin| {
            assert_eq!(command, vec!["chpasswd"]);
            assert_eq!(stdin.as_deref(), Some(b"root:secret-value\n".as_slice()));
            assert!(!command.iter().any(|arg| arg.contains("secret-value")));
            Ok(success_output())
        });

        set_root_password(Path::new("/tmp/rootfs"), "secret-value", &runner)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn set_root_password_rejects_chpasswd_record_injection_before_running() {
        let mut runner = MockRootfsProcessRunner::new();
        runner.expect_run().never();

        let result =
            set_root_password(Path::new("/tmp/rootfs"), "safe\nalice:pwned", &runner).await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }
}
