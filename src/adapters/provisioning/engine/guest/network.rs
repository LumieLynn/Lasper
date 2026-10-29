use crate::adapters::process::log_output;
use crate::adapters::process::{nspawn_io_path, RootfsProcessRunner};
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use std::path::Path;
use std::process::Output;

/// Enable the guest services used by Lasper's veth and bridge network setup.
pub(super) async fn configure_network_at(
    rootfs: &Path,
    runner: &dyn RootfsProcessRunner,
) -> OperationResult<Vec<String>> {
    let systemctl_probe = runner
        .run(
            rootfs,
            vec!["test".into(), "-e".into(), "/usr/bin/systemctl".into()],
            None,
        )
        .await
        .map_err(|error| OperationFailure::Io(nspawn_io_path(), error))?;
    log_output("systemctl probe", &systemctl_probe);
    if !systemctl_probe.status.success() {
        return Ok(vec![
            "WARNING: Guest network services were not enabled because systemctl could not be used inside the container."
                .into(),
        ]);
    }

    let mut warnings = Vec::new();
    for (unit, consequence) in [
        (
            "systemd-networkd.service",
            "Veth/bridge addressing may not work",
        ),
        (
            "systemd-resolved.service",
            "Veth/bridge DNS resolution may not work",
        ),
    ] {
        let output = enable_systemd_unit(rootfs, unit, runner).await?;
        if !output.status.success() {
            warnings.push(format!(
                "WARNING: Could not enable {unit} inside the container; the unit may be missing. {consequence}."
            ));
        }
    }

    Ok(warnings)
}

async fn enable_systemd_unit(
    rootfs: &Path,
    unit: &str,
    runner: &dyn RootfsProcessRunner,
) -> OperationResult<Output> {
    let out = runner
        .run(
            rootfs,
            vec!["systemctl".into(), "enable".into(), unit.into()],
            None,
        )
        .await
        .map_err(|error| OperationFailure::Io(nspawn_io_path(), error))?;
    log_output(&format!("systemctl enable {unit}"), &out);
    if !out.status.success() {
        log::warn!("Failed to enable {} inside {}", unit, rootfs.display());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::MockRootfsProcessRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Arc, Mutex};

    fn mock_output(status: bool, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(if status { 0 } else { 256 }),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn network_services_are_enabled_without_rewriting_resolver_configuration() {
        let rootfs = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut runner = MockRootfsProcessRunner::new();
        {
            let calls = calls.clone();
            runner.expect_run().returning(move |_, args, stdin| {
                assert!(stdin.is_none());
                calls.lock().unwrap().push(args.clone());
                Ok(mock_output(true, ""))
            });
        }

        let warnings = configure_network_at(rootfs.path(), &runner).await.unwrap();

        let calls = calls.lock().unwrap();
        assert!(warnings.is_empty());
        assert_eq!(calls.len(), 3);
        assert!(calls[1].iter().any(|arg| arg == "systemd-networkd.service"));
        assert!(calls
            .iter()
            .any(|args| args.iter().any(|arg| arg == "systemd-resolved.service")));
        assert!(calls.iter().all(|args| !args
            .iter()
            .any(|arg| arg == "/etc/resolv.conf" || arg.contains("stub-resolv.conf"))));
    }

    #[tokio::test]
    async fn network_unit_failures_are_reported_independently() {
        let rootfs = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut runner = MockRootfsProcessRunner::new();
        {
            let calls = calls.clone();
            runner.expect_run().returning(move |_, args, stdin| {
                assert!(stdin.is_none());
                calls.lock().unwrap().push(args.clone());
                if args.iter().any(|arg| arg == "test") {
                    Ok(mock_output(true, ""))
                } else {
                    Ok(mock_output(false, "networkd failed"))
                }
            });
        }

        let warnings = configure_network_at(rootfs.path(), &runner).await.unwrap();

        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().any(|warning| warning.contains("networkd")));
        assert!(warnings.iter().any(|warning| warning.contains("resolved")));
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls
            .iter()
            .any(|args| args.iter().any(|arg| arg == "systemd-networkd.service")));
        assert!(calls
            .iter()
            .any(|args| args.iter().any(|arg| arg == "systemd-resolved.service")));
    }

    #[tokio::test]
    async fn missing_systemctl_skips_network_configuration() {
        let rootfs = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let mut runner = MockRootfsProcessRunner::new();
        {
            let calls = calls.clone();
            runner.expect_run().returning(move |_, args, stdin| {
                assert!(stdin.is_none());
                calls.lock().unwrap().push(args);
                Ok(mock_output(false, "missing"))
            });
        }

        let warnings = configure_network_at(rootfs.path(), &runner).await.unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("systemctl could not be used"));
        assert_eq!(calls.len(), 1);
        assert!(calls[0].iter().any(|arg| arg == "/usr/bin/systemctl"));
    }
}
