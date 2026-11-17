use super::*;
fn parse(args: &[&str]) -> Result<Args, clap::Error> {
    Args::try_parse_from(std::iter::once("lasper-guest-test-client").chain(args.iter().copied()))
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
    assert_eq!(args.execution_timeout, None);
}
#[test]
fn account_shell_is_interactive_without_a_command() {
    let args = parse(&["shell", "--machine", "demo", "--user", "alice"]).unwrap();
    assert!(args.tty && args.command.is_empty());
    assert_eq!(args.user, "alice");
    assert_eq!(args.execution_timeout, None);
}

#[test]
fn exec_accepts_a_positive_timeout_with_or_without_a_pty() {
    for tty in [false, true] {
        let mut arguments = vec!["exec", "--machine", "demo", "--timeout", "180"];
        if tty {
            arguments.push("--tty");
        }
        arguments.extend(["--", "/bin/sleep", "120"]);
        let args = parse(&arguments).unwrap();
        assert_eq!(args.execution_timeout, Some(Duration::from_secs(180)));
        assert_eq!(args.tty, tty);
    }
}

#[test]
fn exec_rejects_invalid_or_excessive_timeouts() {
    let above_maximum = (MAX_EXECUTION_TIMEOUT_MS / 1000 + 1).to_string();
    for value in [
        "0",
        "-1",
        "1.5",
        above_maximum.as_str(),
        "18446744073709551",
        "18446744073709552",
        "18446744073709551616",
    ] {
        assert!(
            parse(&[
                "exec",
                "--machine",
                "demo",
                "--timeout",
                value,
                "--",
                "/bin/true"
            ])
            .is_err(),
            "{value}"
        );
    }
}

#[test]
fn exec_accepts_the_protocols_largest_whole_second_timeout() {
    let maximum = MAX_EXECUTION_TIMEOUT_MS / 1000;
    let args = parse(&[
        "exec",
        "--machine",
        "demo",
        "--timeout",
        &maximum.to_string(),
        "--",
        "/bin/true",
    ])
    .unwrap();
    assert_eq!(args.execution_timeout, Some(Duration::from_secs(maximum)));
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
        "--timeout",
        "0",
    ])
    .unwrap();
    assert_eq!(args.user, "root");
    assert_eq!(
        args.command,
        ["/bin/sh", "--help", "--user", "other", "--timeout", "0"]
    );
    assert_eq!(args.execution_timeout, None);
}
