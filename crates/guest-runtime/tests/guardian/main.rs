mod bootstrap;
#[path = "../transport/support.rs"]
pub mod control_fixture;
#[path = "../support/guardian.rs"]
pub mod guardian_fixture;
mod hello;
mod prepare;
mod signal_setup;

use lasper_guest_runtime::session::GuardianContext;
use std::process::Command;

fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("--child") {
        // These owners deliberately remain on the caller's stack. The entry
        // must exit without returning here and dropping their old FD numbers.
        let _inherited = bootstrap::inherited_owners(&arguments[2]);
        hello::before_bootstrap(&arguments[2]);
        signal_setup::before_bootstrap(&arguments[2]);
        guardian_fixture::enter(run_child);
    }
    let mut passed = 0;
    for &(name, run) in bootstrap::SCENARIOS
        .iter()
        .chain(hello::SCENARIOS)
        .chain(signal_setup::SCENARIOS)
        .chain(prepare::SCENARIOS)
    {
        println!("guest guardian: {name}");
        run();
        passed += 1;
    }
    println!("guest guardian: {passed} scenarios passed");
}

fn run_child(mut context: GuardianContext) -> i32 {
    let name = std::env::args().nth(2).unwrap();
    if name.starts_with("hello-") {
        hello::child(&context, &name);
    } else if name.starts_with("prepare-") {
        prepare::child(&mut context, &name);
    } else {
        bootstrap::child(&mut context, &name);
    }
    0
}

fn command(name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--child", name]);
    command
}
