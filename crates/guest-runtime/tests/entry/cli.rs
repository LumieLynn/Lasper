use std::{os::unix::process::CommandExt, process::Command};

pub const SCENARIOS: &[super::Scenario] = &[
    ("root help does not require a guest environment", root_help),
    ("wrapper has its own help", wrapper_help),
    ("invalid CLI reports errors on stderr", invalid_arguments),
    (
        "payload help stays behind the wrapper separator",
        payload_help,
    ),
    ("init arguments bypass runtime CLI parsing", init_arguments),
];

fn root_help(binary: &str) {
    let long = Command::new(binary).arg("--help").output().unwrap();
    let short = Command::new(binary).arg("-h").output().unwrap();
    assert_eq!(long.status.code(), Some(0));
    assert_eq!(short.status.code(), Some(0));
    assert_eq!(long.stdout, short.stdout);
    assert!(long.stderr.is_empty());
    assert!(short.stderr.is_empty());
    let help = String::from_utf8(long.stdout).unwrap();
    for expected in [
        "Usage:",
        "Commands:",
        "Options:",
        "--identify",
        "--broker",
        "wrap",
    ] {
        assert!(help.contains(expected), "missing {expected}: {help}");
    }
}

fn wrapper_help(binary: &str) {
    let long = Command::new(binary)
        .args(["wrap", "--help"])
        .output()
        .unwrap();
    let short = Command::new(binary).args(["wrap", "-h"]).output().unwrap();
    assert_eq!(long.status.code(), Some(0));
    assert_eq!(short.status.code(), Some(0));
    assert_eq!(long.stdout, short.stdout);
    assert!(long.stderr.is_empty());
    assert!(short.stderr.is_empty());
    let help = String::from_utf8(long.stdout).unwrap();
    assert!(
        help.contains("Usage: lasper-guest-runtime wrap -- <PROGRAM>..."),
        "{help}"
    );
}

fn invalid_arguments(binary: &str) {
    for args in [
        &[][..],
        &["--invalid"][..],
        &["--broker"][..],
        &["wrap"][..],
        &["wrap", "--"][..],
        &["wrap", "/bin/sh"][..],
    ] {
        let output = Command::new(binary).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(64), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.starts_with("error:"), "{args:?}: {error}");
        assert!(error.contains("Usage:"), "{args:?}: {error}");
    }
}

fn init_arguments(binary: &str) {
    for candidate in ["/lib/systemd/systemd", "/usr/lib/systemd/systemd"] {
        for args in [
            &["--help"][..],
            &["-h"][..],
            &["wrap", "--help"][..],
            &["--broker", "/var/opt/lasper"][..],
        ] {
            let output = Command::new(binary)
                .arg0(candidate)
                .args(args)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(64), "{candidate}: {output:?}");
            assert!(output.stdout.is_empty(), "{candidate}: {output:?}");
            let error = String::from_utf8(output.stderr).unwrap();
            assert!(
                error.contains("automatic init wrapper requires PID 1"),
                "{error}"
            );
        }
    }
}

fn payload_help(binary: &str) {
    let output = Command::new(binary)
        .args(["wrap", "--", "/proc/self/fd/-1", "--help"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(126), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("stat wrapper payload"), "{error}");
}
