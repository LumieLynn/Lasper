#[allow(dead_code)]
#[path = "../support/accounts.rs"]
mod accounts;
mod client;
mod faults;
mod fixture;
mod scenarios;
#[path = "../support/control.rs"]
mod wire;

use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

pub type Scenario = (&'static str, fn(bool));

fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("--broker-child") {
        let privileged = arguments.iter().any(|arg| arg == "--privileged");
        if arguments.iter().any(|arg| arg == "--guest") {
            fixture::initialize_guest(privileged);
        } else if !privileged {
            fixture::enter_user_namespace();
        }
        if let Some(fault) = arguments.windows(2).find(|args| args[0] == "--fault") {
            faults::install(&fault[1]);
        }
        let mask = arguments
            .windows(2)
            .find(|args| args[0] == "--umask")
            .map(|args| libc::mode_t::from_str_radix(&args[1], 8).unwrap());
        if let Some(mask) = mask {
            unsafe { libc::umask(mask) };
        }
        let path = CString::new(arguments[2].as_bytes()).unwrap();
        let result = unsafe { lasper_guest_runtime::broker::run(&path) };
        if let Some(mask) = mask {
            assert_eq!(unsafe { libc::umask(mask) }, mask, "launch umask changed");
        }
        if let Err(error) = result {
            eprintln!("broker test child: {error:?}");
        }
        std::process::exit(if result.is_ok() { 0 } else { 70 });
    }
    if arguments.get(1).map(String::as_str) == Some("--sudo") {
        let status = Command::new("sudo")
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .arg("--privileged")
            .status()
            .unwrap();
        std::process::exit(status.code().unwrap_or(1));
    }
    if arguments.get(1).map(String::as_str) == Some("--binary-driver") {
        fixture::initialize_guest_binary(&arguments[2]);
        let binary = "/bin/guest-exec";
        let directory = &arguments[3];
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Keep the launch stack below the worker budget; the production ELF
        // must provide its own stack rather than inherit this test's limit.
        let limit = libc::rlimit {
            rlim_cur: 256 * 1024,
            rlim_max: 256 * 1024,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_STACK, &limit) }, 0);
        unsafe { libc::umask(0o777) };
        let error = std::os::unix::process::CommandExt::exec(
            Command::new(binary).args(["--broker", directory]),
        );
        panic!("exec standalone broker: {error}");
    }
    let privileged = arguments.iter().any(|arg| arg == "--privileged");
    if !privileged && !arguments.iter().any(|arg| arg == "--ignored") {
        println!("guest broker: run --test broker -- --ignored (user namespace) or --sudo");
        return;
    }
    let binary = arguments
        .windows(2)
        .find(|args| args[0] == "--binary")
        .map(|args| args[1].as_str());
    let mut passed = 0;
    for &(name, scenario) in scenarios::SCENARIOS {
        println!("guest broker: {name}");
        scenario(privileged);
        passed += 1;
    }
    if let Some(binary) = binary {
        println!("guest broker: standalone ELF executes a bounded command and reaps its guardian");
        scenarios::standalone(binary);
        passed += 1;
    }
    println!("guest broker: {passed} scenarios passed");
}
