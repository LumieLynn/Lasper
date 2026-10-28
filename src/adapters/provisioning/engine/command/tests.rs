use super::*;
use crate::adapters::provisioning::engine::execution::check_deployment_cancellation;
use crate::application::provisioning::{DeploymentEvent, DeploymentId};
use crate::domain::machine::MachineName;
use std::os::unix::process::ExitStatusExt;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn command_output_bounds_long_lines_and_preserves_following_diagnostics() {
    let mut input = vec![b'a'; MAX_COMMAND_LINE_BYTES + 200];
    input.extend_from_slice(b"\ninvalid: \xff\nlast diagnostic");
    let (events, mut receiver) = tokio::sync::mpsc::channel(4);
    let diagnostics = DeploymentDiagnostics::new(
        DeploymentId::from_u128(1),
        MachineName::new("test").unwrap(),
        events,
    );

    stream_command_output(&mut input.as_slice(), &diagnostics)
        .await
        .unwrap();

    let DeploymentEvent::Line(first) = receiver.try_recv().unwrap() else {
        panic!("expected output");
    };
    assert!(first.starts_with(&"a".repeat(MAX_COMMAND_LINE_BYTES)));
    assert!(first.ends_with("[command output truncated]"));
    assert!(first.len() < MAX_COMMAND_LINE_BYTES + 100);
    let DeploymentEvent::Line(second) = receiver.try_recv().unwrap() else {
        panic!("expected output");
    };
    assert_eq!(second, "invalid: \u{fffd}");
    assert!(
        matches!(receiver.try_recv().unwrap(), DeploymentEvent::Line(last) if last == "last diagnostic")
    );
}

#[tokio::test]
async fn a_full_ui_queue_cannot_block_process_cancellation() {
    let (reader, mut writer) = tokio::io::duplex(1024);
    let process = SpawnedProcess::new_cancellable(
        Box::new(reader),
        async { Ok(std::process::ExitStatus::from_raw(0)) },
        |_| Box::pin(async { Ok(()) }),
    );
    let (events, _receiver) = tokio::sync::mpsc::channel(1);
    events
        .try_send(DeploymentEvent::Line("buffer already full".into()))
        .unwrap();
    let diagnostics = DeploymentDiagnostics::new(
        DeploymentId::from_u128(1),
        MachineName::new("test").unwrap(),
        events,
    );
    let cancellation = DeploymentCancellation::default();

    let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(
            wait_for_deployment_command(process, &diagnostics, &cancellation, "test command"),
            async {
                writer.write_all(b"command output\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
                cancellation.request();
                drop(writer);
            },
        )
    })
    .await
    .expect("cancellation was blocked by UI delivery");
    assert!(matches!(result, Err(OperationFailure::Cancelled)));
}

#[tokio::test]
async fn deployment_cancellation_notifies_waiters_and_fails_checkpoints() {
    let cancellation = DeploymentCancellation::default();
    let waiter = cancellation.clone();
    let task = tokio::spawn(async move { waiter.cancelled().await });

    cancellation.request();
    task.await.unwrap();
    let result = check_deployment_cancellation(&cancellation);
    assert!(matches!(result, Err(OperationFailure::Cancelled)));
}

#[tokio::test]
async fn failed_process_wait_is_not_treated_as_a_rollback_safe_failure() {
    let spawned = crate::adapters::process::SpawnedProcess::new_cancellable(
        Box::new(tokio::io::empty()),
        async { Err(std::io::Error::other("wait channel closed")) },
        |_| Box::pin(async { Ok(()) }),
    );
    let (logs, _receiver) = tokio::sync::mpsc::channel(4);
    let logs = DeploymentDiagnostics::new(
        DeploymentId::from_u128(1),
        MachineName::new("test").unwrap(),
        logs,
    );

    let error = wait_for_deployment_command(
        spawned,
        &logs,
        &DeploymentCancellation::default(),
        "test deployer",
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        OperationFailure::ProcessExitUnconfirmed { operation, source }
            if operation == "test deployer" && source.to_string().contains("wait channel closed")
    ));
}

#[tokio::test]
async fn authoritative_completion_wins_a_racing_cancellation() {
    use std::os::unix::process::ExitStatusExt;

    let spawned = crate::adapters::process::SpawnedProcess::new_cancellable(
        Box::new(tokio::io::empty()),
        async { Ok(std::process::ExitStatus::from_raw(0)) },
        |_| Box::pin(async { Ok(()) }),
    )
    .with_completion_wins_cancellation();
    let cancellation = DeploymentCancellation::default();
    cancellation.request();
    let (logs, _receiver) = tokio::sync::mpsc::channel(4);
    let logs = DeploymentDiagnostics::new(
        DeploymentId::from_u128(1),
        MachineName::new("test").unwrap(),
        logs,
    );

    let status =
        wait_for_deployment_command(spawned, &logs, &cancellation, "authoritative transfer")
            .await
            .unwrap();

    assert!(status.success());
}
