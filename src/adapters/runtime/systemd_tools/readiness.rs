//! Query guest manager readiness without starting a guest process or bus service.

use crate::adapters::process::{command_diagnostic, CommandRunner};
use crate::application::sessions::execution::NativeReadiness;
use crate::domain::machine::MachineName;
use std::io;
use tokio::time::Instant;

const BUS: &str = "org.freedesktop.DBus";
const MANAGER: &str = "org.freedesktop.systemd1";

pub(crate) async fn observe(
    machine: &MachineName,
    runner: &dyn CommandRunner,
    deadline: Instant,
) -> NativeReadiness {
    let owner = match query(
        machine,
        runner,
        deadline,
        &[
            BUS,
            "/org/freedesktop/DBus",
            BUS,
            "GetNameOwner",
            "s",
            MANAGER,
        ],
    )
    .await
    {
        Ok(output) => match string_reply(&output, "s ") {
            Some(owner) if valid_unique_name(&owner) => owner,
            _ => return NativeReadiness::unknown(format!("invalid bus owner reply: {output}")),
        },
        Err(observation) => return observation,
    };
    match query(
        machine,
        runner,
        deadline,
        &[
            &owner,
            "/org/freedesktop/systemd1",
            "org.freedesktop.DBus.Properties",
            "Get",
            "ss",
            "org.freedesktop.systemd1.Manager",
            "Version",
        ],
    )
    .await
    {
        Ok(output) => match string_reply(&output, "v s ") {
            Some(version) => NativeReadiness::Ready { version },
            None => NativeReadiness::unknown(format!("invalid manager version reply: {output}")),
        },
        Err(observation) => observation,
    }
}

async fn query(
    machine: &MachineName,
    runner: &dyn CommandRunner,
    deadline: Instant,
    call: &[&str],
) -> Result<String, NativeReadiness> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timeout());
    }
    let mut args = vec![
        format!("--machine={}", machine.as_str()),
        "--auto-start=no".into(),
        "--allow-interactive-authorization=no".into(),
        "--timeout=1s".into(),
        "--no-pager".into(),
        "call".into(),
    ];
    args.extend(call.iter().map(|value| (*value).to_owned()));
    match runner.run_bounded("busctl", args, remaining).await {
        Ok(output) if output.status.success() => {
            String::from_utf8(output.stdout).map_err(NativeReadiness::unknown)
        }
        // A command's text/exit code cannot reliably distinguish missing bus,
        // missing owner, and policy denial across supported systemd versions.
        Ok(output) => Err(NativeReadiness::unknown(format!(
            "busctl exited with {}: {}",
            output.status,
            command_diagnostic(&output)
        ))),
        Err(error) if error.kind() == io::ErrorKind::TimedOut => Err(timeout()),
        Err(error) => Err(NativeReadiness::unknown(error)),
    }
}

fn string_reply(output: &str, signature: &str) -> Option<String> {
    serde_json::from_str(output.trim().strip_prefix(signature)?).ok()
}

fn valid_unique_name(name: &str) -> bool {
    name.starts_with(':')
        && name.contains('.')
        && name.len() <= 255
        && name[1..].split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

fn timeout() -> NativeReadiness {
    NativeReadiness::Unresponsive {
        reason: "guest manager readiness query exceeded its deadline".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};
    use std::time::Duration;

    #[tokio::test]
    async fn reads_owner_then_version_on_the_unique_destination_without_activation() {
        let mut runner = MockCommandRunner::new();
        let mut sequence = mockall::Sequence::new();
        for (destination, member, stdout) in [
            (BUS, "GetNameOwner", r#"s ":1.42""#),
            (":1.42", "Get", r#"v s "261.2""#),
        ] {
            runner
                .expect_run_bounded()
                .withf(move |program, args, timeout| {
                    program == "busctl"
                        && args[0] == "--machine=demo"
                        && args[1] == "--auto-start=no"
                        && args[2] == "--allow-interactive-authorization=no"
                        && args[6] == destination
                        && args[9] == member
                        && *timeout <= Duration::from_secs(1)
                })
                .times(1)
                .in_sequence(&mut sequence)
                .returning(move |_, _, _| {
                    Ok(Output {
                        status: ExitStatus::from_raw(0),
                        stdout: stdout.as_bytes().to_vec(),
                        stderr: Vec::new(),
                    })
                });
        }
        let result = observe(
            &MachineName::new("demo").unwrap(),
            &runner,
            Instant::now() + Duration::from_secs(1),
        )
        .await;
        assert_eq!(
            result,
            NativeReadiness::Ready {
                version: "261.2".into()
            }
        );
    }

    #[tokio::test]
    async fn preserves_command_failure_without_guessing_the_guest_init() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run_bounded().times(1).returning(|_, _, _| {
            Ok(Output {
                status: ExitStatus::from_raw(1 << 8),
                stdout: Vec::new(),
                stderr: b"There is no system bus in container demo".to_vec(),
            })
        });
        let result = observe(
            &MachineName::new("demo").unwrap(),
            &runner,
            Instant::now() + Duration::from_secs(1),
        )
        .await;
        assert!(
            matches!(result, NativeReadiness::Unknown { reason } if reason.contains("no system bus") && reason.contains("exit status: 1"))
        );
    }
}
