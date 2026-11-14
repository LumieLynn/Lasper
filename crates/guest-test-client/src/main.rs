mod args;
mod paths;
mod target;
mod terminal;

use anyhow::{bail, Context, Result};
use lasper_guest_host_transport::{
    BrokerConnection, BrokerSession, MachineTarget, SessionExit, SessionPhase, SessionStartOptions,
};
use lasper_guest_protocol::{
    CleanupStatus, CommandArgv, ControlLimits, ExecutionKind, ExecutionStatus, ExitStatus,
    GuestProcess, PrepareRequest, ProjectionRequests, TerminalEnvironment,
};
use std::{
    io::{self, Write},
    sync::Arc,
};
use tokio::time::{Duration, Instant};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match run(args::Args::parse()).await {
        Ok(status) => std::process::ExitCode::from(status),
        Err(error) => {
            eprintln!("guest test client: {error:#}");
            std::process::ExitCode::from(125)
        }
    }
}

async fn run(args: args::Args) -> Result<u8> {
    let terminal = args.tty.then(terminal::Terminal::open).transpose()?;
    let size = terminal
        .as_ref()
        .map(terminal::Terminal::size)
        .transpose()?;
    let source = target::HostMachines::connect()
        .await
        .context("connect to host system bus")?;
    let target = Arc::new(
        MachineTarget::resolve(&source, &args.machine)
            .await
            .context("resolve registered machine")?,
    );
    let connection = BrokerConnection::connect(
        target,
        &args.socket,
        ControlLimits::CURRENT,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let argv: Vec<&str> = args.command.iter().map(String::as_str).collect();
    let process = if argv.is_empty() {
        GuestProcess::LoginShell
    } else {
        GuestProcess::Command(
            CommandArgv::new(&argv).map_err(|e| anyhow::anyhow!("invalid guest command: {e:?}"))?,
        )
    };
    let environment = if args.tty {
        Some(
            TerminalEnvironment::new(
                &std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
                std::env::var("COLORTERM").ok().as_deref(),
                std::env::var("NO_COLOR").ok().as_deref(),
            )
            .map_err(|e| anyhow::anyhow!("invalid terminal environment: {e:?}"))?,
        )
    } else {
        None
    };
    let request = PrepareRequest::new(
        &args.user,
        if args.tty {
            ExecutionKind::Interactive
        } else {
            ExecutionKind::Bounded
        },
        process,
        environment,
        size,
        ProjectionRequests::default(),
    )
    .map_err(|e| anyhow::anyhow!("invalid preparation: {e:?}"))?;
    let mut session =
        BrokerSession::prepare(connection, request, Instant::now() + Duration::from_secs(5))
            .await?;
    let master = session
        .start(SessionStartOptions {
            terminal_size: size,
            ..Default::default()
        })
        .await?;
    let mut stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let execution = async {
        if let Some(terminal) = terminal {
            let relay = terminal.relay(master.context("interactive start did not return a PTY")?);
            tokio::pin!(relay);
            tokio::select! {
                result = &mut relay => { result?; finish(&mut session).await },
                result = finish(&mut session) => {
                    // The session Exit message can arrive before the PTY's final output.
                    if let Ok(result) = tokio::time::timeout(Duration::from_secs(1), &mut relay).await { result?; }
                    result
                }
            }
        } else {
            tokio::select! {
                result = finish(&mut session) => result,
                _ = tokio::signal::ctrl_c() => bail!("interrupted"),
            }
        }
    };
    let result = tokio::select! {
        result = execution => result,
        _ = stop.recv() => Err(anyhow::anyhow!("terminated")),
    };
    // Cleanup is attempted before the client runtime exits. Errors from closing
    // or waiting do not replace the original frontend error.
    let exit = match result {
        Ok(exit) => exit,
        Err(error) => {
            let _ = session.close().await;
            let _ = finish(&mut session).await;
            return Err(error);
        }
    };
    if let Some(output) = exit.output() {
        io::stdout().write_all(output.stdout().bytes())?;
        io::stderr().write_all(output.stderr().bytes())?;
        for (name, stream) in [("stdout", output.stdout()), ("stderr", output.stderr())] {
            if stream.truncated() || !stream.complete() {
                eprintln!(
                    "guest {name}: truncated={}, complete={}",
                    stream.truncated(),
                    stream.complete()
                );
            }
        }
    }
    let report = exit.report();
    if report.execution() != ExecutionStatus::Started || report.cleanup() != CleanupStatus::Complete
    {
        bail!("guest execution did not complete normally: {report:?}");
    }
    match report.exit() {
        Some(ExitStatus::Exited(code)) => Ok(code),
        Some(ExitStatus::Signaled(signal)) => Ok(128u8.saturating_add(signal)),
        None => bail!("guest exit status missing: {report:?}"),
    }
}

async fn finish(session: &mut BrokerSession) -> Result<Arc<SessionExit>> {
    loop {
        let snapshot = session.snapshot();
        if matches!(
            snapshot.phase(),
            SessionPhase::Closed | SessionPhase::Failed
        ) {
            if let Some(error) = snapshot.error() {
                bail!("session failed: {error}");
            }
            return snapshot
                .into_exit()
                .context("session ended without a final exit report");
        }
        session.changed().await?;
    }
}
