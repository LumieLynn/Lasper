use super::*;
use crate::adapters::process::MockRootfsProcessRunner;
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

    assert!(matches!(result, Err(OperationFailure::InvalidInput(_))));
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
async fn failed_root_password_change_fails_initialization_without_exposing_the_secret() {
    let mut runner = MockRootfsProcessRunner::new();
    runner.expect_run().once().returning(|_, command, _| {
        assert_eq!(command, vec!["chpasswd"]);
        Ok(failure_output("password update rejected"))
    });
    let error = set_root_password(Path::new("/tmp/rootfs"), "root-sentinel", &runner)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("set password for root"));
    assert!(error.to_string().contains("password update rejected"));
    assert!(!error.to_string().contains("root-sentinel"));
}

#[tokio::test]
async fn failed_user_password_change_does_not_report_a_successful_account_initialization() {
    let mut runner = MockRootfsProcessRunner::new();
    let mut sequence = mockall::Sequence::new();
    runner
        .expect_run()
        .once()
        .in_sequence(&mut sequence)
        .returning(|_, command, _| {
            assert_eq!(command[0], "useradd");
            Ok(success_output())
        });
    runner
        .expect_run()
        .once()
        .in_sequence(&mut sequence)
        .returning(|_, command, _| {
            assert_eq!(command, vec!["chpasswd"]);
            Ok(failure_output("password update rejected"))
        });
    let user = CreateUser {
        username: "alice".into(),
        ..Default::default()
    };
    let error = create_user_in_container(
        Path::new("/tmp/rootfs"),
        &user,
        Some("user-sentinel"),
        &runner,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("set password for alice"));
    assert!(!error.to_string().contains("user-sentinel"));
}

#[tokio::test]
async fn set_root_password_rejects_chpasswd_record_injection_before_running() {
    let mut runner = MockRootfsProcessRunner::new();
    runner.expect_run().never();

    let result = set_root_password(Path::new("/tmp/rootfs"), "safe\nalice:pwned", &runner).await;

    assert!(matches!(result, Err(OperationFailure::InvalidInput(_))));
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
