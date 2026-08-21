use crate::domain::secret::SecretBytes;
use crate::nspawn::adapters::rootfs::process::{nspawn_io_path, RootfsProcessRunner};
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{validate_chpasswd_secret, CreateUser};
use crate::nspawn::sys::log_output;
use std::path::Path;

pub(crate) async fn create_user_in_container(
    rootfs: &Path,
    user: &CreateUser,
    password: Option<&str>,
    runner: &dyn RootfsProcessRunner,
) -> Result<Vec<String>> {
    user.validate()?;
    if let Some(password) = password {
        validate_chpasswd_secret("user password", password)?;
    }
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
    if let Some(password) = password.filter(|password| !password.is_empty()) {
        let output = run_chpasswd(rootfs, &user.username, password, runner).await?;
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
        .run(
            rootfs,
            vec!["chpasswd".into()],
            Some(SecretBytes::new(input)),
        )
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
    let mut group_failures = Vec::new();
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
        match output {
            Ok(output) if output.status.success() => {
                log_output("usermod", &output);
            }
            Ok(output) => {
                log_output("usermod", &output);
                group_failures.push(format!("{group}: {}", command_error(&output)));
                continue;
            }
            Err(error) => {
                group_failures.push(format!("{group}: {error}"));
                continue;
            }
        }

        let sudoers = format!(
            "# Managed by Lasper: sudo access for provisioned users\n%{group} ALL=(ALL:ALL) ALL\n"
        )
        .into_bytes();
        let output = runner
            .run(
                rootfs,
                vec![
                    "sh".into(),
                    "-eu".into(),
                    "-c".into(),
                    concat!(
                        "grep -Eq '^[[:space:]]*([@#]includedir)[[:space:]]+\"?(/etc/)?sudoers\\.d/?\"?([[:space:]]|$)' /etc/sudoers || ",
                        "{ echo '/etc/sudoers does not include /etc/sudoers.d' >&2; exit 1; }; ",
                        "command -v visudo >/dev/null || { echo 'visudo is unavailable' >&2; exit 1; }; ",
                        "LC_ALL=C visudo -cf /etc/sudoers >/dev/null; ",
                        "target=/etc/sudoers.d/90-lasper-$1; ",
                        "install -d -m 0750 /etc/sudoers.d; ",
                        "[ ! -L \"$target\" ]; ",
                        "if [ -e \"$target\" ]; then head -n 1 \"$target\" | ",
                        "grep -Fx '# Managed by Lasper: sudo access for provisioned users' >/dev/null || ",
                        "{ echo 'refusing to replace an unmanaged sudoers policy' >&2; exit 1; }; fi; ",
                        "tmp=$(mktemp /etc/sudoers.d/.lasper-sudoers.XXXXXX); ",
                        "trap 'rm -f \"$tmp\"' EXIT; cat > \"$tmp\"; chmod 0440 \"$tmp\"; ",
                        "LC_ALL=C visudo -cf \"$tmp\" >/dev/null; mv -f \"$tmp\" \"$target\"; ",
                        "trap - EXIT; LC_ALL=C visudo -cf /etc/sudoers >/dev/null"
                    )
                    .into(),
                    "_".into(),
                    group.into(),
                ],
                Some(SecretBytes::new(sudoers)),
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
        return Ok(());
    }
    Err(NspawnError::Runtime(format!(
        "Could not grant sudo access to {username:?}: neither sudo nor wheel group was usable ({})",
        group_failures.join("; ")
    )))
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

    fn failure_output(message: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: Vec::new(),
            stderr: message.as_bytes().to_vec(),
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

        create_user_in_container(Path::new("/tmp/rootfs"), &user, None, &runner)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_user_rejects_chpasswd_record_injection_before_running() {
        let mut runner = MockRootfsProcessRunner::new();
        runner.expect_run().never();
        let user = CreateUser {
            username: "alice".into(),
            shell: "/bin/bash".into(),
            sudoer: false,
        };

        let result = create_user_in_container(
            Path::new("/tmp/rootfs"),
            &user,
            Some("safe\nroot:pwned"),
            &runner,
        )
        .await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[tokio::test]
    async fn password_is_sent_over_stdin_and_not_argv() {
        let mut runner = MockRootfsProcessRunner::new();
        runner.expect_run().once().returning(|_, command, stdin| {
            assert_eq!(command, vec!["chpasswd"]);
            assert_eq!(
                stdin.as_ref().map(SecretBytes::as_slice),
                Some(b"root:secret-value\n".as_slice())
            );
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

    #[tokio::test]
    async fn sudoer_configuration_fails_when_neither_group_is_usable() {
        let mut runner = MockRootfsProcessRunner::new();
        runner
            .expect_run()
            .times(2)
            .withf(|_, command, stdin| {
                command.first().is_some_and(|arg| arg == "usermod") && stdin.is_none()
            })
            .returning(|_, command, _| {
                let group = command.get(2).map(String::as_str).unwrap_or("unknown");
                Ok(failure_output(&format!("group {group} does not exist")))
            });

        let error = configure_sudoer(Path::new("/tmp/rootfs"), "alice", &runner)
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("neither sudo nor wheel group was usable"));
        assert!(message.contains("sudo: group sudo does not exist"));
        assert!(message.contains("wheel: group wheel does not exist"));
    }

    #[tokio::test]
    async fn sudoer_configuration_falls_back_to_wheel_and_writes_managed_policy() {
        let mut sequence = mockall::Sequence::new();
        let mut runner = MockRootfsProcessRunner::new();
        runner
            .expect_run()
            .once()
            .in_sequence(&mut sequence)
            .withf(|_, command, stdin| {
                command
                    .iter()
                    .map(String::as_str)
                    .eq(["usermod", "-aG", "sudo", "alice"])
                    && stdin.is_none()
            })
            .returning(|_, _, _| Ok(failure_output("group sudo does not exist")));
        runner
            .expect_run()
            .once()
            .in_sequence(&mut sequence)
            .withf(|_, command, stdin| {
                command
                    .iter()
                    .map(String::as_str)
                    .eq(["usermod", "-aG", "wheel", "alice"])
                    && stdin.is_none()
            })
            .returning(|_, _, _| Ok(success_output()));
        runner
            .expect_run()
            .once()
            .in_sequence(&mut sequence)
            .withf(|_, command, stdin| {
                let script = command.get(3).map(String::as_str).unwrap_or_default();
                command.first().is_some_and(|arg| arg == "sh")
                    && command.last().is_some_and(|arg| arg == "wheel")
                    && script.contains("/etc/sudoers does not include /etc/sudoers.d")
                    && script.contains("90-lasper-$1")
                    && script.contains("visudo -cf")
                    && stdin
                        .as_ref()
                        .map(SecretBytes::as_slice)
                        .is_some_and(|policy| {
                            policy.starts_with(b"# Managed by Lasper:")
                                && policy.ends_with(b"%wheel ALL=(ALL:ALL) ALL\n")
                        })
            })
            .returning(|_, _, _| Ok(success_output()));

        configure_sudoer(Path::new("/tmp/rootfs"), "alice", &runner)
            .await
            .unwrap();
    }
}
