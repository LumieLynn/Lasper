use clap::{
    builder::NonEmptyStringValueParser, error::ErrorKind, CommandFactory, Parser, Subcommand,
};
use lasper_guest_protocol::MAX_EXECUTION_TIMEOUT_MS;
use std::ffi::OsString;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "lasper-guest-test-client",
    about = "Run commands and interactive shells through a guest runtime",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Run a command in the guest
    Exec(ExecOptions),
    /// Open the selected guest account's login shell
    Shell(TargetOptions),
}

#[derive(Debug, clap::Args)]
struct TargetOptions {
    /// Registered machine name on the host
    #[arg(long, value_name = "NAME", value_parser = NonEmptyStringValueParser::new())]
    machine: String,
    /// Existing guest account to run as
    #[arg(long, value_name = "USER", default_value = "root", value_parser = NonEmptyStringValueParser::new())]
    user: String,
    /// Broker socket path inside the guest root
    #[arg(long, value_name = "PATH", default_value = crate::paths::BROKER_SOCKET, value_parser = absolute_guest_path)]
    socket: String,
}

#[derive(Debug, clap::Args)]
struct ExecOptions {
    #[command(flatten)]
    target: TargetOptions,
    /// Use an interactive PTY; requires a host terminal
    #[arg(long)]
    tty: bool,
    /// Limit execution to this many seconds (default: unlimited)
    #[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..=MAX_EXECUTION_TIMEOUT_MS / 1000))]
    timeout: Option<u64>,
    /// Absolute guest program path followed by its arguments
    #[arg(last = true, required = true, num_args = 1.., value_name = "PROGRAM")]
    command: Vec<String>,
}

fn absolute_guest_path(value: &str) -> Result<String, &'static str> {
    if value.starts_with('/') {
        Ok(value.into())
    } else {
        Err("expected an absolute path inside the guest")
    }
}

#[derive(Debug)]
pub struct Args {
    pub machine: String,
    pub user: String,
    pub socket: String,
    pub tty: bool,
    pub execution_timeout: Option<Duration>,
    pub command: Vec<String>,
}

impl Args {
    pub fn parse() -> Self {
        Self::try_parse_from(std::env::args_os()).unwrap_or_else(|error| error.exit())
    }

    fn try_parse_from<I, T>(arguments: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let (target, tty, command, execution_timeout) =
            match Cli::try_parse_from(arguments)?.command {
                Action::Exec(options) => (
                    options.target,
                    options.tty,
                    options.command,
                    options.timeout.map(Duration::from_secs),
                ),
                Action::Shell(target) => (target, true, Vec::new(), None),
            };
        if command.first().is_some_and(|path| !path.starts_with('/')) {
            let mut cli = Cli::command();
            cli.build();
            return Err(cli.find_subcommand_mut("exec").unwrap().error(
                ErrorKind::ValueValidation,
                "the guest executable must be an absolute path",
            ));
        }
        Ok(Self {
            machine: target.machine,
            user: target.user,
            socket: target.socket,
            tty,
            execution_timeout,
            command,
        })
    }
}

#[cfg(test)]
#[path = "../tests/args.rs"]
mod tests;
