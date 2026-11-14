use clap::{
    builder::NonEmptyStringValueParser, error::ErrorKind, CommandFactory, Parser, Subcommand,
};
use std::ffi::OsString;

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
        let (target, tty, command) = match Cli::try_parse_from(arguments)?.command {
            Action::Exec(options) => (options.target, options.tty, options.command),
            Action::Shell(target) => (target, true, Vec::new()),
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
            command,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(
            std::iter::once("lasper-guest-test-client").chain(args.iter().copied()),
        )
    }
    #[test]
    fn command_arguments_are_not_reparsed() {
        let args = parse(&[
            "exec",
            "--machine",
            "demo",
            "--",
            "/bin/bash",
            "-c",
            "printf '%s' \"$1\"",
            "--user",
        ])
        .unwrap();
        assert_eq!(
            args.command,
            ["/bin/bash", "-c", "printf '%s' \"$1\"", "--user"]
        );
        assert_eq!(args.user, "root");
        assert!(!args.tty);
    }
    #[test]
    fn account_shell_is_interactive_without_a_command() {
        let args = parse(&["shell", "--machine", "demo", "--user", "alice"]).unwrap();
        assert!(args.tty && args.command.is_empty());
        assert_eq!(args.user, "alice");
    }
    #[test]
    fn rejects_incomplete_or_ambiguous_invocations() {
        for args in [
            vec!["exec", "--machine", "demo"],
            vec!["exec", "--machine", "demo", "--", "bash"],
            vec!["shell", "--machine", "demo", "--", "/bin/sh"],
            vec!["shell", "--machine", "demo", "--machine", "other"],
            vec!["shell", "--machine"],
            vec!["shell", "--machine", ""],
            vec!["shell", "--machine", "demo", "--user", ""],
            vec!["shell", "--machine", "demo", "--socket", "relative.sock"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn help_is_available_for_root_and_subcommands() {
        for args in [
            &["--help"][..],
            &["-h"][..],
            &["exec", "--help"][..],
            &["shell", "-h"][..],
            &["help", "exec"][..],
        ] {
            let error = parse(args).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::DisplayHelp);
            assert!(error.to_string().contains("Usage:"));
            assert!(!error.use_stderr());
        }
    }

    #[test]
    fn guest_help_arguments_are_not_client_help() {
        let args = parse(&[
            "exec",
            "--machine",
            "demo",
            "--",
            "/bin/sh",
            "--help",
            "--user",
            "other",
        ])
        .unwrap();
        assert_eq!(args.user, "root");
        assert_eq!(args.command, ["/bin/sh", "--help", "--user", "other"]);
    }
}
