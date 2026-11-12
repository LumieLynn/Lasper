use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Broker {
    child: Child,
    directory: tempfile::TempDir,
    launch: Option<ChildStdin>,
}

impl Broker {
    pub fn spawn() -> Self {
        Self::spawn_with(None)
    }

    pub fn failed_start() -> Self {
        Self::spawn_with(Some("--fail-start"))
    }

    pub fn with_accounts() -> Self {
        Self::spawn_with(Some("--accounts"))
    }

    fn spawn_with(option: Option<&str>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--broker-child")
            .arg(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if let Some(option) = option {
            command.arg(option);
        }
        let mut child = command.spawn().unwrap();
        let launch = child.stdin.take();
        let mut initialized = child.stdout.take().unwrap();
        let broker = Self {
            child,
            directory,
            launch,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut event = libc::pollfd {
            fd: initialized.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "broker fixture did not initialize");
            // SAFETY: poll borrows this owned pipe and writes its single event record.
            let result = unsafe { libc::poll(&mut event, 1, remaining.as_millis() as i32) };
            if result > 0 {
                break;
            }
            if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            panic!("broker fixture initialization timed out or failed");
        }

        let mut ready = [0];
        initialized.read_exact(&mut ready).unwrap();
        assert_eq!(ready, [1]);
        // The child has entered its namespace but cannot bind before start().
        broker
    }

    pub fn start(&mut self) {
        self.launch.take().unwrap().write_all(&[1]).unwrap();
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn socket(&self) -> std::path::PathBuf {
        self.directory.path().join("guest-exec.sock")
    }

    pub async fn stop(&mut self) {
        // SAFETY: the fixture owns this unreaped child and requests its normal stop.
        assert_eq!(unsafe { libc::kill(self.pid() as i32, libc::SIGTERM) }, 0);
        assert!(self.wait().await.success());
    }

    pub async fn expect_failed_start(&mut self) {
        assert_eq!(self.wait().await.code(), Some(70));
    }

    async fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "broker did not stop and reap its guardians"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn enter_user_namespace() {
    // SAFETY: the fixture child is single-threaded and has not started the broker.
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    assert_eq!(
        unsafe { libc::unshare(libc::CLONE_NEWUSER) },
        0,
        "{}",
        io::Error::last_os_error()
    );
    std::fs::write("/proc/self/setgroups", "deny\n").unwrap();
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")).unwrap();
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")).unwrap();
}

pub fn install_accounts(directory: &std::path::Path) {
    // Replace the account files in a private mount namespace so these tests
    // can still use the real /proc and devpts for process checks and PTYs.
    assert_eq!(unsafe { libc::unshare(libc::CLONE_NEWNS) }, 0);
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
        0
    );
    let passwd = directory.join("passwd");
    std::fs::write(
        &passwd,
        format!("root:x:0:0::{}:/bin/sh\n", directory.display()),
    )
    .unwrap();
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    assert!(count >= 0);
    let mut groups = vec![0; count as usize];
    assert_eq!(
        unsafe { libc::getgroups(count, groups.as_mut_ptr()) },
        count
    );
    groups.push(0);
    groups.sort_unstable();
    groups.dedup();
    let group = directory.join("group");
    let content: String = groups
        .iter()
        .map(|gid| format!("g{gid}:x:{gid}:root\n"))
        .collect();
    std::fs::write(&group, content).unwrap();
    for (source, destination) in [(passwd, c"/etc/passwd"), (group, c"/etc/group")] {
        let source = std::ffi::CString::new(source.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(
            unsafe {
                libc::mount(
                    source.as_ptr(),
                    destination.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                )
            },
            0
        );
    }
}
