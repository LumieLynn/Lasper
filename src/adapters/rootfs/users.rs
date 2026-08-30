use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::log_output;
use crate::adapters::rootfs::process::{nspawn_io_path, RootfsProcessRunner};
use crate::domain::provisioning::{validate_login_username, CreateUser};
use crate::domain::secret::{validate_chpasswd_secret, SecretBytes};
use crate::domain::wayland::ContainerUserIdentity;
use std::path::Path;

pub(crate) async fn create_user_in_container(
    rootfs: &Path,
    user: &CreateUser,
    password: Option<&str>,
    runner: &dyn RootfsProcessRunner,
) -> Result<Vec<String>> {
    user.validate()
        .map_err(|error| NspawnError::Validation(error.to_string()))?;
    if let Some(password) = password {
        validate_chpasswd_secret(password)
            .map_err(|error| NspawnError::Validation(error.message("user password")))?;
    }
    let shell = user.login_shell();

    let mut command = vec!["useradd".into(), "-m".into()];
    if let Some(uid) = user.uid {
        command.extend(["--uid".into(), uid.to_string()]);
    }
    command.extend(["-s".into(), shell.into(), user.username.clone()]);

    let output = runner
        .run(rootfs, command, None)
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

pub(crate) async fn resolve_user_identity(
    rootfs: &Path,
    username: &str,
    runner: &dyn RootfsProcessRunner,
) -> Result<ContainerUserIdentity> {
    validate_login_username(username)
        .map_err(|error| NspawnError::Validation(error.to_string()))?;
    let uid = query_numeric_identity(rootfs, username, "-u", "uid", runner).await?;
    let gid = query_numeric_identity(rootfs, username, "-g", "gid", runner).await?;
    Ok(ContainerUserIdentity {
        username: username.to_string(),
        uid,
        gid,
    })
}

async fn query_numeric_identity(
    rootfs: &Path,
    username: &str,
    flag: &str,
    label: &str,
    runner: &dyn RootfsProcessRunner,
) -> Result<u32> {
    let output = runner
        .run(
            rootfs,
            vec!["id".into(), flag.into(), username.into()],
            None,
        )
        .await
        .map_err(|error| NspawnError::Io(nspawn_io_path(), error))?;
    log_output("id", &output);
    if !output.status.success() {
        return Err(NspawnError::cmd_failed(
            "resolve container user identity",
            format!("systemd-nspawn -D {:?} -- id {flag} {username}", rootfs),
            &output,
        ));
    }
    let value = std::str::from_utf8(&output.stdout)
        .map_err(|_| NspawnError::Runtime(format!("container {label} is not valid UTF-8")))?
        .trim();
    value.parse::<u32>().map_err(|_| {
        NspawnError::Runtime(format!(
            "container {label} lookup returned an invalid value: {value:?}"
        ))
    })
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
    validate_chpasswd_secret(password)
        .map_err(|error| NspawnError::Validation(error.message("password")))?;
    let mut input = Vec::with_capacity(username.len() + password.len() + 2);
    input.extend_from_slice(username.as_bytes());
    input.push(b':');
    input.extend_from_slice(password.as_bytes());
    input.push(b'\n');
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
    use crate::adapters::rootfs::process::MockRootfsProcessRunner;
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

    fn success_output_with_stdout(stdout: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[tokio::test]
    async fn user_identity_is_resolved_from_the_provisioned_rootfs() {
        let mut sequence = mockall::Sequence::new();
        let mut runner = MockRootfsProcessRunner::new();
        runner
            .expect_run()
            .once()
            .in_sequence(&mut sequence)
            .withf(|_, command, stdin| {
                command.iter().map(String::as_str).eq(["id", "-u", "alice"]) && stdin.is_none()
            })
            .returning(|_, _, _| Ok(success_output_with_stdout("1001\n")));
        runner
            .expect_run()
            .once()
            .in_sequence(&mut sequence)
            .withf(|_, command, stdin| {
                command.iter().map(String::as_str).eq(["id", "-g", "alice"]) && stdin.is_none()
            })
            .returning(|_, _, _| Ok(success_output_with_stdout("1002\n")));

        let identity = resolve_user_identity(Path::new("/tmp/rootfs"), "alice", &runner)
            .await
            .unwrap();

        assert_eq!(identity.username, "alice");
        assert_eq!(identity.uid, 1001);
        assert_eq!(identity.gid, 1002);
    }

    #[tokio::test]
    async fn malformed_numeric_identity_is_rejected() {
        let mut runner = MockRootfsProcessRunner::new();
        runner
            .expect_run()
            .once()
            .returning(|_, _, _| Ok(success_output_with_stdout("not-a-uid\n")));

        let error = resolve_user_identity(Path::new("/tmp/rootfs"), "alice", &runner)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("invalid value"));
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
    async fn create_user_places_an_explicit_uid_before_the_account_name() {
        let mut runner = MockRootfsProcessRunner::new();
        runner.expect_run().once().returning(|_, command, stdin| {
            assert!(stdin.is_none());
            assert_eq!(
                command,
                vec!["useradd", "-m", "--uid", "1001", "-s", "/bin/bash", "alice"]
            );
            Ok(success_output())
        });
        let user = CreateUser {
            username: "alice".into(),
            uid: Some(1001),
            shell: "/bin/bash".into(),
            sudoer: false,
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
            uid: None,
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
