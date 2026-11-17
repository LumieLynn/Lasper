mod args;
mod outcome;
mod output;
mod paths;
mod session;
mod target;
mod terminal;

use anyhow::{bail, Context, Result};
use lasper_guest_host_transport::{
    ClientOptions, GuestClient, MachineTarget, ProcessSpec, Program, SessionIo, StartOptions,
    StartOutcome, Stdio,
};
use lasper_guest_protocol::FeatureSet;
use std::{collections::BTreeMap, sync::Arc};
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
    let mut stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = if args.tty {
        None
    } else {
        Some(tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::interrupt(),
        )?)
    };
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
    let client = GuestClient::new(
        target,
        ClientOptions {
            endpoint: args.socket,
            required_features: if args.execution_timeout.is_some() {
                FeatureSet::EXECUTION_TIMEOUT
            } else {
                FeatureSet::from_bits(0)
            },
            ..ClientOptions::default()
        },
    )?;
    let mut environment = BTreeMap::new();
    if args.tty {
        for name in ["TERM", "COLORTERM", "NO_COLOR"] {
            if let Ok(value) = std::env::var(name) {
                environment.insert(name.into(), value);
            }
        }
    }
    let prepared = client
        .prepare(
            ProcessSpec {
                user: args.user,
                program: if args.command.is_empty() {
                    Program::LoginShell
                } else {
                    Program::Argv(args.command)
                },
                stdio: match size {
                    Some(size) => Stdio::Terminal { size },
                    None => Stdio::Piped,
                },
                environment,
            },
            Instant::now() + Duration::from_secs(5),
        )
        .await?;
    let mut session = prepared.session;
    let start = session
        .start(
            StartOptions {
                terminal_size: size,
                execution_timeout: args.execution_timeout,
            },
            Instant::now() + Duration::from_secs(5),
        )
        .await;
    // Start releases its mutable borrow before the data task is constructed.
    // Every branch drains concurrently with cleanup, including rejected starts.
    let accepted = matches!(start, Ok(StartOutcome::Accepted));
    let drain = async {
        match prepared.io {
            SessionIo::Terminal(pty) if accepted => {
                terminal
                    .as_ref()
                    .context("missing host terminal")?
                    .relay(pty.into_owned_fd())
                    .await
            }
            SessionIo::Terminal(mut pty) => {
                output::copy(&mut pty, &mut tokio::io::stdout()).await?;
                Ok(())
            }
            SessionIo::Pipes {
                mut stdout,
                mut stderr,
            } => {
                let mut host_stdout = tokio::io::stdout();
                let mut host_stderr = tokio::io::stderr();
                output::pipes(
                    &mut stdout,
                    &mut stderr,
                    &mut host_stdout,
                    &mut host_stderr,
                    || async {
                        let _ = session.close().await;
                    },
                )
                .await?;
                Ok(())
            }
        }
    };
    let stopping = async {
        tokio::select! {
            _ = stop.recv() => {},
            _ = async {
                match interrupt.as_mut() {
                    Some(signal) => { signal.recv().await; },
                    None => std::future::pending::<()>().await,
                }
            } => {},
        }
    };
    let result =
        session::observe(session.wait(), session.close(), stopping, drain, !accepted).await?;
    match start {
        Err(error) => return Err(error.into()),
        Ok(StartOutcome::Unknown { cause }) => {
            bail!("Start outcome is unknown; the command may have run: {cause}")
        }
        _ => {}
    }
    if let Some(error) = result.error {
        bail!("session observation failed: {error}");
    }
    outcome::exit_status(
        result
            .report
            .context("session ended without a final exit report")?,
    )
}
