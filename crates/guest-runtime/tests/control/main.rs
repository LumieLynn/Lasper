#[allow(dead_code)]
#[path = "../support/accounts.rs"]
mod accounts;
#[path = "../support/guardian.rs"]
mod bootstrap;
mod client;
mod failures;
mod lifecycle;
mod scenarios;
#[path = "../support/control.rs"]
mod wire;

use lasper_guest_runtime::session::GuardianContext;
use std::ffi::CString;
use std::os::unix::process::CommandExt;
use std::process::Command;

fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("--hold-output") {
        lifecycle::export_output(arguments.get(2).is_some_and(|arg| arg == "large"));
        return;
    }
    if arguments.get(1).map(String::as_str) == Some("--sudo") {
        let status = Command::new("sudo")
            .args(["--"])
            .arg(std::env::current_exe().unwrap())
            .arg("--privileged")
            .status()
            .unwrap();
        std::process::exit(status.code().unwrap_or(1));
    }
    if arguments.get(1).map(String::as_str) == Some("--child") {
        bootstrap::enter(run_child);
    }
    let privileged = arguments.iter().any(|arg| arg == "--privileged");
    if !privileged && !arguments.iter().any(|arg| arg == "--ignored") {
        println!("guest control: run --test control -- --ignored (user namespace) or --sudo (account switching)");
        return;
    }
    let filter = arguments
        .windows(2)
        .find(|pair| pair[0] == "--filter")
        .map(|pair| pair[1].as_str());
    let mut passed = 0;
    for &(name, run) in scenarios::SCENARIOS
        .iter()
        .chain(lifecycle::SCENARIOS)
        .chain(failures::SCENARIOS)
    {
        if filter.is_some_and(|filter| !name.contains(filter)) {
            continue;
        }
        println!("guest control: {name}");
        run(privileged);
        passed += 1;
    }
    println!("guest control: {passed} scenarios passed");
}

fn run_child(context: GuardianContext) -> i32 {
    let arguments: Vec<_> = std::env::args().collect();
    let privileged = arguments.iter().any(|arg| arg == "--privileged");
    let _accounts = accounts::initialize(privileged);
    if arguments[2] == "account-failure" {
        std::fs::remove_file(accounts::get().path("etc/passwd")).unwrap();
    }
    if arguments[2] == "held-output" {
        accounts::get().install_elf(std::env::current_exe().unwrap(), "bin/output-helper");
    }
    let proc_path = accounts::get().path("proc");
    std::fs::create_dir(&proc_path).unwrap();
    let proc_path = CString::new(proc_path.as_os_str().as_encoded_bytes()).unwrap();
    // Only this test child enters the fixture. Keep its proc mount so the real
    // guardian can inspect/reap children after entering the guest root.
    assert_eq!(
        unsafe { libc::unshare(libc::CLONE_NEWNS) },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        unsafe {
            libc::mount(
                c"/proc".as_ptr(),
                proc_path.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    let dev_path = accounts::get().path("dev");
    std::fs::create_dir(&dev_path).unwrap();
    let dev_path = CString::new(dev_path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(
        unsafe {
            libc::mount(
                c"/dev".as_ptr(),
                dev_path.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    accounts::get().enter_root().unwrap();
    std::env::set_current_dir("/").unwrap();
    std::mem::forget(_accounts);
    failures::install(&arguments[2]);
    let result = context.run_session();
    if let Err(error) = result {
        eprintln!("guest control completed cleanup: {error:?}");
    }
    if failures::expected(&arguments[2], result) {
        0
    } else {
        71
    }
}

pub type Scenario = (&'static str, fn(bool));

pub fn command(name: &str, privileged: bool) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--child", name]);
    if privileged {
        command.arg("--privileged");
    }
    // The production worker must also fit the budget from a full control loop.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: lasper_guest_runtime::session::WORKER_STACK_BUDGET_BYTES as _,
                rlim_max: libc::RLIM_INFINITY,
            };
            if libc::setrlimit(libc::RLIMIT_STACK, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    command
}
