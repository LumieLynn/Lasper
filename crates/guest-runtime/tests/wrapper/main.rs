#[allow(dead_code)]
#[path = "../support/accounts.rs"]
mod accounts;
#[allow(dead_code)]
#[path = "../broker/fixture.rs"]
mod fixture;

use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{symlink, MetadataExt, PermissionsExt},
        process::CommandExt,
    },
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--driver") {
        driver(
            &args[2],
            &args[3],
            args.iter().any(|arg| arg == "--privileged"),
        );
    }
    if args.iter().any(|arg| arg == "--sudo") {
        let status = Command::new("sudo")
            .arg("--")
            .arg(std::env::current_exe().unwrap())
            .args(args.iter().skip(1).filter(|arg| arg.as_str() != "--sudo"))
            .arg("--privileged")
            .status()
            .unwrap();
        std::process::exit(status.code().unwrap_or(1));
    }
    let Some(binary) = args
        .windows(2)
        .find(|args| args[0] == "--binary")
        .map(|args| {
            fs::canonicalize(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join(&args[1]),
            )
            .unwrap()
        })
    else {
        println!("guest wrapper: run --test wrapper -- --binary PATH (requires unprivileged user/mount namespaces)");
        return;
    };
    let mut cases = vec![
        "payload",
        "existing-parents",
        "existing-directory",
        "parent-symlink",
        "parent-file",
        "directory-symlink",
        "exec-failure",
        "unsafe-directory",
        "recursive-payload",
    ];
    if args.iter().any(|arg| arg == "--pid1") {
        cases.push("init");
    }
    let privileged = args.iter().any(|arg| arg == "--privileged");
    for case in &cases {
        println!("guest wrapper: {case}");
        scenario(binary.to_str().unwrap(), case, privileged);
    }
    println!(
        "guest wrapper: {} scenarios passed (PID 1 scenario requires --pid1)",
        cases.len()
    );
}

fn driver(binary: &str, case: &str, privileged: bool) -> ! {
    fixture::initialize_guest_binary_as(binary, privileged);
    let directory = std::path::Path::new(
        lasper_guest_runtime::paths::RUNTIME_DIRECTORY
            .to_str()
            .unwrap(),
    );
    if case == "init" {
        assert_eq!(
            unsafe { libc::unshare(libc::CLONE_NEWPID) },
            0,
            "create test PID namespace: {}",
            std::io::Error::last_os_error()
        );
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid != 0 {
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            std::process::exit(libc::WEXITSTATUS(status));
        }
        assert_eq!(
            unsafe {
                libc::mount(
                    c"proc".as_ptr(),
                    c"/proc".as_ptr(),
                    c"proc".as_ptr(),
                    0,
                    std::ptr::null(),
                )
            },
            0,
            "mount PID namespace procfs: {}",
            std::io::Error::last_os_error()
        );
    }
    match case {
        "payload" => assert!(!std::path::Path::new("/var").exists()),
        "existing-parents" => {
            fs::create_dir_all(directory.parent().unwrap()).unwrap();
            fs::set_permissions("/var", fs::Permissions::from_mode(0o751)).unwrap();
            fs::set_permissions("/var/opt", fs::Permissions::from_mode(0o711)).unwrap();
        }
        "existing-directory" => {
            fs::create_dir_all(directory).unwrap();
            fs::set_permissions("/var", fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions("/var/opt", fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(directory.join("marker"), b"unchanged").unwrap();
        }
        "parent-symlink" => {
            fs::create_dir("/var").unwrap();
            fs::set_permissions("/var", fs::Permissions::from_mode(0o755)).unwrap();
            fs::create_dir("/broker-parent").unwrap();
            fs::set_permissions("/broker-parent", fs::Permissions::from_mode(0o711)).unwrap();
            symlink("/broker-parent", "/var/opt").unwrap();
        }
        "parent-file" => {
            fs::create_dir("/var").unwrap();
            fs::write("/var/opt", b"not a directory").unwrap();
        }
        "directory-symlink" => {
            fs::create_dir_all(directory.parent().unwrap()).unwrap();
            fs::create_dir("/broker-target").unwrap();
            fs::set_permissions("/broker-target", fs::Permissions::from_mode(0o700)).unwrap();
            symlink("/broker-target", directory).unwrap();
        }
        "unsafe-directory" => {
            fs::create_dir_all(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o777)).unwrap();
        }
        "exec-failure" => {
            fs::write("/bin/bad-payload", b"invalid ELF\n").unwrap();
            fs::set_permissions("/bin/bad-payload", fs::Permissions::from_mode(0o755)).unwrap();
        }
        "init" => {
            fs::create_dir_all("/lib/systemd").unwrap();
            fs::create_dir_all("/sbin").unwrap();
            fs::hard_link("/bin/guest-exec", "/lib/systemd/systemd").unwrap();
            // Use the fixture shell as an init interpreter. --help must reach
            // this script rather than be consumed by the runtime CLI parser.
            fs::write(
                "/sbin/init",
                "#!/bin/sh\nprintf 'INIT:%s:%s:%s\\n' \"$$\" \"$0\" \"$1\"\nread line\n",
            )
            .unwrap();
            fs::set_permissions("/sbin/init", fs::Permissions::from_mode(0o755)).unwrap();
            let error = Command::new("/lib/systemd/systemd").arg("--help").exec();
            panic!("exec init wrapper: {error}");
        }
        _ => {}
    }
    let script = "if test -e /proc/self/fd/9; then while read key value rest; do if test \"$key\" = SigBlk:; then printf 'FD:%s' \"$value\" >&9; break; fi; done < /proc/self/status; exec 9>&-; fi; printf 'PAYLOAD:%s:%s:%s:%s\\n' \"$$\" \"$LASPER_WRAPPER_TEST\" \"$PWD\" \"$(umask)\"; read line";
    let mut command = Command::new("/bin/guest-exec");
    command.args(["wrap", "--"]);
    match case {
        "exec-failure" => {
            command.arg("/bin/bad-payload");
        }
        "recursive-payload" => {
            command.arg("/bin/guest-exec");
        }
        _ => {
            command.args(["/bin/sh", "-c", script]);
        }
    }
    command.env("LASPER_WRAPPER_TEST", "unchanged");
    std::env::set_current_dir("/home/alice").unwrap();
    unsafe { libc::umask(0o027) };
    let error = command.exec();
    panic!("exec explicit wrapper: {error}");
}

fn scenario(binary: &str, case: &str, privileged: bool) {
    use std::os::fd::AsRawFd;
    let (mut inherited_reader, inherited_writer) = std::os::unix::net::UnixStream::pair().unwrap();
    inherited_reader
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--driver", binary, case]);
    if privileged {
        command.arg("--privileged");
    }
    if case == "payload" {
        let fd = inherited_writer.as_raw_fd();
        // SAFETY: only async-signal-safe operations run in this fork child.
        // FD 9 models a payload activation descriptor that must survive both
        // execs in the parent and be pruned from the detached broker child.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 9) < 0 || libc::fcntl(9, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut mask: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut mask);
                libc::sigaddset(&mut mask, libc::SIGUSR1);
                if libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(inherited_writer);
    let driver_pid = child.id();
    let root = PathBuf::from(format!("/tmp/lasper-account-test-{driver_pid}"));
    let succeeds = matches!(
        case,
        "payload" | "existing-parents" | "existing-directory" | "parent-symlink" | "init"
    );
    if succeeds {
        let stdout = child.stdout.as_mut().unwrap();
        let mut ready = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(stdout),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(
            unsafe { libc::poll(&mut ready, 1, 8000) } > 0,
            "payload did not start"
        );
        let mut line = String::new();
        let mut byte = [0];
        while stdout.read(&mut byte).unwrap() == 1 {
            line.push(byte[0] as char);
            if byte[0] == b'\n' {
                break;
            }
        }
        if case != "init" {
            assert_eq!(
                line,
                format!("PAYLOAD:{driver_pid}:unchanged:/home/alice:0027\n")
            );
            let directory = if case == "parent-symlink" {
                root.join("broker-parent/lasper")
            } else {
                root.join("var/opt/lasper")
            };
            assert!(directory.join("guest-exec.sock").exists());
            let metadata = fs::metadata(&directory).unwrap();
            assert_eq!(metadata.mode() & 0o7777, 0o700);
            assert_eq!(metadata.uid(), fs::metadata(&root).unwrap().uid());
            let parent_modes = match case {
                "existing-parents" => [("var", 0o751), ("var/opt", 0o711)],
                "parent-symlink" => [("var", 0o755), ("broker-parent", 0o711)],
                _ => [("var", 0o755), ("var/opt", 0o755)],
            };
            for (parent, mode) in parent_modes {
                assert_eq!(
                    fs::metadata(root.join(parent)).unwrap().mode() & 0o7777,
                    mode
                );
            }
            if case == "existing-directory" {
                assert_eq!(fs::read(directory.join("marker")).unwrap(), b"unchanged");
            }
        } else {
            assert_eq!(line, "INIT:1:/sbin/init:--help\n");
        }
        if case == "payload" {
            let mut inherited = String::new();
            inherited_reader.read_to_string(&mut inherited).unwrap();
            assert_eq!(inherited, "FD:0000000000000200");
        }
        child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "wrapper did not finish");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(status.code(), Some(if succeeds { 0 } else { 70 }));
    match case {
        "unsafe-directory" => assert_eq!(
            fs::metadata(root.join("var/opt/lasper")).unwrap().mode() & 0o7777,
            0o777
        ),
        "parent-file" => {
            assert_eq!(fs::read(root.join("var/opt")).unwrap(), b"not a directory");
        }
        "directory-symlink" => {
            assert!(fs::symlink_metadata(root.join("var/opt/lasper"))
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                fs::metadata(root.join("broker-target")).unwrap().mode() & 0o7777,
                0o700
            );
            assert!(fs::read_dir(root.join("broker-target"))
                .unwrap()
                .next()
                .is_none());
        }
        _ => {}
    }
    if case != "init" {
        // In a non-PID1 wrapper the broker watches the pinned payload, exits on
        // its death, and unlinks its own socket. Wait for that independent exit.
        let socket = if case == "parent-symlink" {
            root.join("broker-parent/lasper/guest-exec.sock")
        } else {
            root.join("var/opt/lasper/guest-exec.sock")
        };
        while socket.exists() {
            assert!(Instant::now() < deadline, "broker outlived payload");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    if root.exists() {
        fs::remove_dir_all(root).unwrap();
    }
}
