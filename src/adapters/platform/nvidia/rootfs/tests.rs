use super::*;
use crate::adapters::process::MockRootfsProcessRunner;
use std::os::unix::process::ExitStatusExt;
use std::process::Output;
use std::sync::{Arc, Mutex};

fn success_output(stdout: &[u8]) -> Output {
    Output {
        status: std::process::ExitStatus::from_raw(0),
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
    }
}

#[test]
fn mutation_validation_rejects_path_and_environment_injection() {
    assert!(validate_nvidia_config(&["../../host".into()], &[], false).is_err());
    assert!(
        validate_nvidia_config(&[], &[("LD_PRELOAD\nEVIL".into(), "/tmp/x".into())], true,)
            .is_err()
    );
    assert!(validate_cleanup_paths(&["/usr/lib/../etc/shadow".into()]).is_err());
}

#[tokio::test]
async fn environment_update_preserves_unmanaged_lines() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runner = MockRootfsProcessRunner::new();
    let captured = calls.clone();
    runner
        .expect_run()
        .times(2)
        .returning(move |_, command, stdin| {
            captured.lock().unwrap().push((command.clone(), stdin));
            if command.first().is_some_and(|arg| arg == "cat") {
                Ok(success_output(b"LANG=C\nNVIDIA_VISIBLE_DEVICES=old\n"))
            } else {
                Ok(success_output(&[]))
            }
        });

    configure_nvidia_rootfs(
        Path::new("/tmp/rootfs"),
        &[],
        &[("NVIDIA_VISIBLE_DEVICES".into(), "void".into())],
        true,
        &runner,
    )
    .await
    .unwrap();

    let calls = calls.lock().unwrap();
    let content = std::str::from_utf8(calls[1].1.as_ref().unwrap().as_slice()).unwrap();
    assert_eq!(content, "LANG=C\nNVIDIA_VISIBLE_DEVICES=void\n");
}

#[tokio::test]
async fn cleanup_paths_are_data_arguments_not_shell_source() {
    let mut runner = MockRootfsProcessRunner::new();
    runner.expect_run().once().returning(|_, command, stdin| {
        assert!(stdin.is_none());
        let script = command
            .iter()
            .find(|arg| arg.contains("for path do"))
            .unwrap();
        assert!(!script.contains("odd'path"));
        assert!(command.iter().any(|arg| arg == "/usr/lib/odd'path.so"));
        Ok(success_output(&[]))
    });

    cleanup_nvidia_files(
        Path::new("/tmp/rootfs"),
        &["/usr/lib/odd'path.so".into()],
        &runner,
    )
    .await
    .unwrap();
}
