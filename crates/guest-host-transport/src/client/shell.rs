//! Login-shell convenience entry that preserves diagnostics on failed Start.

use super::{
    GuestClient, PrepareError, Prepared, ProcessSpec, Program, Session, StartOptions, Stdio,
};
use crate::{SessionIo, StartError, StartOutcome, TerminalSize};
use std::collections::BTreeMap;
use tokio::time::Instant;

/// A failed shell start retains its control owner and terminal diagnostics.
/// Do not write to `pty`; drain it concurrently with `session.close()`.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // The error transfers both owners to its caller.
pub enum ShellOpenError {
    Prepare(PrepareError),
    Start {
        outcome: Result<StartOutcome, StartError>,
        session: Session,
        pty: crate::PtyMaster,
    },
}

#[allow(clippy::result_large_err)] // Preserve the Session and PTY for diagnostic draining.
pub async fn open_shell(
    client: &GuestClient,
    user: String,
    size: TerminalSize,
    environment: BTreeMap<String, String>,
) -> Result<(Session, crate::PtyMaster), ShellOpenError> {
    let Prepared { mut session, io } = client
        .prepare(
            ProcessSpec {
                user,
                program: Program::LoginShell,
                stdio: Stdio::Terminal { size },
                environment,
            },
            Instant::now() + client.options.timeouts.prepare,
        )
        .await
        .map_err(ShellOpenError::Prepare)?;
    let SessionIo::Terminal(pty) = io else {
        unreachable!("Terminal Ready supplies a PTY");
    };
    let outcome = session
        .start(
            StartOptions::default(),
            Instant::now() + client.options.timeouts.operation,
        )
        .await;
    match outcome {
        Ok(StartOutcome::Accepted) => Ok((session, pty)),
        outcome => Err(ShellOpenError::Start {
            outcome,
            session,
            pty,
        }),
    }
}
