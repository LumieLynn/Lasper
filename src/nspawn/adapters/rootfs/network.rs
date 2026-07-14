use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::sys::{log_output, CommandRunner, ElevatedIo};
use std::path::{Path, PathBuf};
use std::process::Output;

/// Enable systemd-networkd and systemd-resolved inside the container.
pub async fn enable_container_networkd(
    rootfs: &Path,
    cmd_runner: &dyn CommandRunner,
    io: &ElevatedIo,
) -> Result<()> {
    // Check systemctl exists before attempting anything
    if io
        .read_to_string(&rootfs.join("usr/bin/systemctl"))
        .await?
        .is_none()
    {
        return Ok(());
    }

    let rootfs_s = rootfs.to_string_lossy().to_string();

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

    // Replace resolv.conf symlink only when systemd-resolved is available.
    let link = cmd_runner.run(
        "systemd-nspawn",
        vec![
            "-D".into(),
            rootfs_s,
            "--quiet".into(),
            "sh".into(),
            "-c".into(),
            "rm -f /etc/resolv.conf && ln -sf ../run/systemd/resolve/stub-resolv.conf /etc/resolv.conf".into(),
        ],
    ).await;
    match link {
        Ok(output) => log_output("resolv.conf", &output),
        Err(error) => log::warn!(
            "Failed to update container resolv.conf inside {}: {}",
            rootfs.display(),
            error
        ),
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::ops::PermissionLevel;
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
        let io = ElevatedIo::new(PermissionLevel::Root);
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

        enable_container_networkd(rootfs.path(), &runner, &io)
            .await
            .unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].iter().any(|arg| arg == "systemd-networkd"));
        assert!(calls[1].iter().any(|arg| arg == "systemd-resolved"));
    }

    #[tokio::test]
    async fn networkd_failure_still_fails_network_enable() {
        let rootfs = rootfs_with_systemctl();
        let io = ElevatedIo::new(PermissionLevel::Root);
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut runner = MockCommandRunner::new();
        {
            let calls = calls.clone();
            runner.expect_run().returning(move |program, args| {
                assert_eq!(program, "systemd-nspawn");
                calls.lock().unwrap().push(args.clone());
                Ok(mock_output(false, "networkd failed"))
            });
        }

        let result = enable_container_networkd(rootfs.path(), &runner, &io).await;

        assert!(result.is_err());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].iter().any(|arg| arg == "systemd-networkd"));
    }
}
