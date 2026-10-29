//! Validate images already published and owned by the deployment transaction.

use crate::adapters::process::{log_output, CommandRunner};
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use std::path::{Path, PathBuf};

const IMAGE_VALIDATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) async fn validate_raw_system_image(
    path: &Path,
    commands: &dyn CommandRunner,
) -> OperationResult<()> {
    let output = commands
        .run_bounded(
            "systemd-dissect",
            vec!["--validate".into(), path.to_string_lossy().into_owned()],
            IMAGE_VALIDATION_TIMEOUT,
        )
        .await
        .map_err(|error| OperationFailure::Io(PathBuf::from("systemd-dissect"), error))?;
    log_output("systemd-dissect --validate", &output);
    if !output.status.success() {
        return Err(OperationFailure::command_failed(
            "validate imported raw system image",
            format!("systemd-dissect --validate {}", path.display()),
            &output,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::{unconfirmed_process_exit, MockCommandRunner};
    use std::os::unix::process::ExitStatusExt;

    #[tokio::test]
    async fn validation_failure_leaves_the_image_for_transaction_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.raw");
        std::fs::write(&path, b"invalid image").unwrap();
        let expected_path = path.clone();
        let mut commands = MockCommandRunner::new();
        commands
            .expect_run_bounded()
            .withf(move |program, args, timeout| {
                program == "systemd-dissect"
                    && args == &["--validate", expected_path.to_str().unwrap()]
                    && *timeout == IMAGE_VALIDATION_TIMEOUT
            })
            .once()
            .returning(|_, _, _| {
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(1 << 8),
                    stdout: Vec::new(),
                    stderr: b"image has no root filesystem".to_vec(),
                })
            });

        let error = validate_raw_system_image(&path, &commands)
            .await
            .unwrap_err();

        assert!(
            matches!(error, OperationFailure::CommandFailed { diagnostic, .. }
            if diagnostic == "image has no root filesystem")
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid image");
    }

    #[tokio::test]
    async fn validation_retains_unconfirmed_process_exit_for_the_coordinator() {
        let mut commands = MockCommandRunner::new();
        commands.expect_run_bounded().once().returning(|_, _, _| {
            Err(unconfirmed_process_exit(
                "systemd-dissect",
                std::io::Error::other("exit was not observed"),
            ))
        });

        let error = validate_raw_system_image(Path::new("test.raw"), &commands)
            .await
            .unwrap_err();

        assert!(error.outcome_is_unconfirmed());
    }
}
