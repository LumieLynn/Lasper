use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{log_output, CommandRunner};
use std::path::{Path, PathBuf};
use std::process::Output;

/// Enable systemd-networkd and systemd-resolved inside the container.
pub(crate) async fn configure_network_at(
    rootfs: &Path,
    cmd_runner: &dyn CommandRunner,
) -> Result<()> {
    let rootfs_s = rootfs.to_string_lossy().to_string();
    let systemctl_probe = cmd_runner
        .run(
            "systemd-nspawn",
            vec![
                "-D".into(),
                rootfs_s.clone(),
                "--quiet".into(),
                "--settings=no".into(),
                "test".into(),
                "-e".into(),
                "/usr/bin/systemctl".into(),
            ],
        )
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("systemd-nspawn"), error))?;
    log_output("systemctl probe", &systemctl_probe);
    if !systemctl_probe.status.success() {
        return Ok(());
    }

    let networkd = enable_systemd_unit(rootfs, &rootfs_s, "systemd-networkd", cmd_runner).await?;
    if !networkd.status.success() {
        return Err(NspawnError::cmd_failed(
            "systemctl enable systemd-networkd in container",
            format!(
                "systemd-nspawn -D {:?} systemctl enable systemd-networkd",
                rootfs
            ),
            &networkd,
        ));
    }

    let resolved = enable_systemd_unit(rootfs, &rootfs_s, "systemd-resolved", cmd_runner).await?;
    if !resolved.status.success() {
        log::warn!(
            "systemd-resolved could not be enabled inside {}. Leaving /etc/resolv.conf unchanged.",
            rootfs.display()
        );
        return Ok(());
    }

    update_resolv_conf(rootfs, &rootfs_s, cmd_runner).await;

    Ok(())
}

async fn enable_systemd_unit(
    rootfs: &Path,
    rootfs_s: &str,
    unit: &str,
    cmd_runner: &dyn CommandRunner,
) -> Result<Output> {
    let out = cmd_runner
        .run(
            "systemd-nspawn",
            vec![
                "-D".into(),
                rootfs_s.to_string(),
                "--quiet".into(),
                "--settings=no".into(),
                "systemctl".into(),
                "enable".into(),
                unit.into(),
            ],
        )
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("systemd-nspawn"), e))?;
    log_output(&format!("systemctl enable {unit}"), &out);
    if !out.status.success() {
        log::warn!("Failed to enable {} inside {}", unit, rootfs.display());
    }
    Ok(out)
}

async fn update_resolv_conf(rootfs: &Path, rootfs_s: &str, cmd_runner: &dyn CommandRunner) {
    let rm = cmd_runner
        .run(
            "systemd-nspawn",
            vec![
                "-D".into(),
                rootfs_s.to_string(),
                "--quiet".into(),
                "--settings=no".into(),
                "rm".into(),
                "-f".into(),
                "/etc/resolv.conf".into(),
            ],
        )
        .await;
    match rm {
        Ok(output) => {
            log_output("resolv.conf rm", &output);
            if !output.status.success() {
                log::warn!(
                    "Failed to remove container resolv.conf inside {}.",
                    rootfs.display()
                );
                return;
            }
        }
        Err(error) => {
            log::warn!(
                "Failed to remove container resolv.conf inside {}: {}",
                rootfs.display(),
                error
            );
            return;
        }
    }

    let link = cmd_runner
        .run(
            "systemd-nspawn",
            vec![
                "-D".into(),
                rootfs_s.to_string(),
                "--quiet".into(),
                "--settings=no".into(),
                "ln".into(),
                "-sf".into(),
                "../run/systemd/resolve/stub-resolv.conf".into(),
                "/etc/resolv.conf".into(),
            ],
        )
        .await;
    match link {
        Ok(output) => log_output("resolv.conf link", &output),
        Err(error) => log::warn!(
            "Failed to update container resolv.conf inside {}: {}",
            rootfs.display(),
            error
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::sys::command::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Arc, Mutex};

    fn mock_output(status: bool, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(if status { 0 } else { 256 }),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn rootfs_with_systemctl() -> tempfile::TempDir {
        let rootfs = tempfile::tempdir().unwrap();
        let bin = rootfs.path().join("usr/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("systemctl"), "systemctl").unwrap();
        rootfs
    }

    #[tokio::test]
    async fn resolved_failure_does_not_fail_networkd_enable() {
        let rootfs = rootfs_with_systemctl();
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut runner = MockCommandRunner::new();
        {
            let calls = calls.clone();
            runner.expect_run().returning(move |program, args| {
                assert_eq!(program, "systemd-nspawn");
                calls.lock().unwrap().push(args.clone());
                if args.iter().any(|arg| arg == "systemd-resolved") {
                    Ok(mock_output(
                        false,
                        "Unit systemd-resolved.service does not exist",
                    ))
                } else {
                    Ok(mock_output(true, ""))
                }
            });
        }

        configure_network_at(rootfs.path(), &runner).await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls[1].iter().any(|arg| arg == "systemd-networkd"));
        assert!(calls[2].iter().any(|arg| arg == "systemd-resolved"));
    }

    #[tokio::test]
    async fn resolved_success_updates_resolv_conf_without_shell() {
        let rootfs = rootfs_with_systemctl();
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut runner = MockCommandRunner::new();
        {
            let calls = calls.clone();
            runner.expect_run().returning(move |program, args| {
                assert_eq!(program, "systemd-nspawn");
                calls.lock().unwrap().push(args.clone());
                Ok(mock_output(true, ""))
            });
        }

        configure_network_at(rootfs.path(), &runner).await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        assert!(calls[1].iter().any(|arg| arg == "systemd-networkd"));
        assert!(calls[2].iter().any(|arg| arg == "systemd-resolved"));
        assert!(calls[3].windows(2).any(|pair| pair == ["rm", "-f"]));
        assert!(calls[3].iter().any(|arg| arg == "/etc/resolv.conf"));
        assert!(calls[4].windows(2).any(|pair| pair == ["ln", "-sf"]));
        assert!(calls[4]
            .iter()
            .any(|arg| arg == "../run/systemd/resolve/stub-resolv.conf"));
        assert!(calls[4].iter().any(|arg| arg == "/etc/resolv.conf"));
        assert!(calls
            .iter()
            .all(|args| !args.iter().any(|arg| arg == "sh" || arg == "-c")));
        assert!(calls
            .iter()
            .all(|args| args.iter().any(|arg| arg == "--settings=no")));
    }

    #[tokio::test]
    async fn networkd_failure_still_fails_network_enable() {
        let rootfs = rootfs_with_systemctl();
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut runner = MockCommandRunner::new();
        {
            let calls = calls.clone();
            runner.expect_run().returning(move |program, args| {
                assert_eq!(program, "systemd-nspawn");
                calls.lock().unwrap().push(args.clone());
                if args.iter().any(|arg| arg == "test") {
                    Ok(mock_output(true, ""))
                } else {
                    Ok(mock_output(false, "networkd failed"))
                }
            });
        }

        let result = configure_network_at(rootfs.path(), &runner).await;

        assert!(result.is_err());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].iter().any(|arg| arg == "systemd-networkd"));
    }

    #[tokio::test]
    async fn missing_systemctl_skips_network_configuration() {
        let rootfs = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut runner = MockCommandRunner::new();
        {
            let calls = calls.clone();
            runner.expect_run().returning(move |program, args| {
                assert_eq!(program, "systemd-nspawn");
                calls.lock().unwrap().push(args);
                Ok(mock_output(false, "missing"))
            });
        }

        configure_network_at(rootfs.path(), &runner).await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].iter().any(|arg| arg == "/usr/bin/systemctl"));
    }
}
