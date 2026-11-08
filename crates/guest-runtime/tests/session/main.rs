mod accounts;
mod fixture;
#[path = "../support/guardian.rs"]
pub mod guardian_fixture;
mod lifecycle;
mod memory;

use lasper_guest_runtime::session::GuardianContext;
use std::os::unix::process::CommandExt;
use std::process::Command;

type ScenarioFn = fn(&mut GuardianContext);

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
        guardian_fixture::enter(run_scenario);
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
        let mut guardian = guardian_fixture::GuardianProcess::spawn(command).unwrap();
        guardian.release();
        guardian.hello();
        let status = guardian.wait();
        assert!(status.success(), "session scenario {name}: {status}");
        passed += 1;
    }
    println!("guest session: {passed} scenarios passed");
}

fn run_scenario(mut context: GuardianContext) -> i32 {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let name = &arguments[1];
    let (_, run, _) = scenarios()
        .find(|(candidate, _, _)| candidate == name)
        .unwrap();
    let _fixture = fixture::initialize(arguments.iter().any(|argument| argument == "--privileged"));
    run(&mut context);
    0
}

fn scenarios() -> impl Iterator<Item = (&'static str, ScenarioFn, Mode)> {
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
