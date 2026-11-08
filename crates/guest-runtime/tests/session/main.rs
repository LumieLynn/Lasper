mod accounts;
mod fixture;
mod lifecycle;
mod memory;

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn getuid() -> u32;
}

fn main() {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.as_slice() == ["--sudo"] {
        let status = Command::new("sudo")
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .arg("--privileged")
            .status()
            .expect("run compiled session tests through sudo");
        std::process::exit(status.code().unwrap_or(1));
    }
    let privileged = arguments.iter().any(|argument| argument == "--privileged");
    if privileged {
        assert_eq!(
            unsafe { getuid() },
            0,
            "use --sudo to run the compiled tests as root"
        );
    }
    if arguments.first().map(String::as_str) == Some("--scenario") {
        let name = &arguments[1];
        let (_, run, _) = scenarios()
            .find(|(candidate, _, _)| candidate == name)
            .unwrap();
        let _fixture = fixture::initialize(privileged);
        run();
        return;
    }
    if !privileged && !arguments.iter().any(|argument| argument == "--ignored") {
        println!("guest session: namespace tests ignored; run --test session -- --ignored");
        println!("guest session: credential-switch tests ignored; run --test session -- --sudo");
        return;
    }
    let mut passed = 0;
    for (name, _, mode) in scenarios() {
        if !mode.accepts(privileged) {
            continue;
        }
        println!("guest session: {name}");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--scenario", name]);
        if privileged {
            command.arg("--privileged");
        }
        // Set the limit before exec, so the new process actually starts with
        // the budgeted stack rather than retaining an already-grown mapping.
        unsafe {
            command.pre_exec(memory::limit_stack);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "session scenario {name}: {status}");
                passed += 1;
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("session scenario {name} exceeded its deadline");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    println!("guest session: {passed} scenarios passed");
}

fn scenarios() -> impl Iterator<Item = (&'static str, fn(), Mode)> {
    accounts::SCENARIOS.iter().copied().chain(
        lifecycle::SCENARIOS
            .iter()
            .chain(memory::SCENARIOS)
            .map(|&(name, run)| (name, run, Mode::Both)),
    )
}

#[derive(Clone, Copy)]
enum Mode {
    Both,
    Namespace,
    Privileged,
}

impl Mode {
    fn accepts(self, privileged: bool) -> bool {
        matches!(self, Self::Both)
            || matches!(
                (self, privileged),
                (Self::Namespace, false) | (Self::Privileged, true)
            )
    }
}
