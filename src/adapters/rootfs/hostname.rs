use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::{log_output, CommandRunner};
use crate::domain::machine::GuestHostname;
use std::path::{Path, PathBuf};

/// Persist the guest's static hostname using systemd's own offline writer.
pub(crate) async fn configure_hostname_at(
    rootfs: &Path,
    hostname: &GuestHostname,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let args = vec![
        format!("--root={}", rootfs.display()),
        format!("--hostname={}", hostname.as_str()),
        "--force".into(),
    ];
    let output = runner
        .run("systemd-firstboot", args)
        .await
        .map_err(|error| NspawnError::Io(PathBuf::from("systemd-firstboot"), error))?;
    log_output("systemd-firstboot hostname", &output);
    if !output.status.success() {
        return Err(NspawnError::cmd_failed(
            "configure guest hostname",
            "systemd-firstboot",
            &output,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;

    fn output(success: bool, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn hostname_is_written_with_the_systemd_offline_writer() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run().once().returning(|program, args| {
            assert_eq!(program, "systemd-firstboot");
            assert_eq!(
                args,
                [
                    "--root=/var/lib/machines/test",
                    "--hostname=guest.example",
                    "--force",
                ]
            );
            Ok(output(true, ""))
        });

        configure_hostname_at(
            Path::new("/var/lib/machines/test"),
            &GuestHostname::new("guest.example").unwrap(),
            &runner,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn firstboot_failure_is_not_reported_as_a_successful_mutation() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .once()
            .returning(|_, _| Ok(output(false, "read-only rootfs")));

        let error = configure_hostname_at(
            Path::new("/var/lib/machines/test"),
            &GuestHostname::new("guest").unwrap(),
            &runner,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            NspawnError::CommandFailed(_, _, output) if output.contains("read-only rootfs")
        ));
    }
}
