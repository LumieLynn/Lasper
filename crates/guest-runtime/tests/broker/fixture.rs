use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct Broker {
    pub child: Child,
    pub directory: PathBuf,
    stopped: bool,
}
impl Broker {
    pub fn spawn(privileged: bool) -> Self {
        Self::spawn_with(privileged, false, None)
    }
    pub fn with_guest(privileged: bool) -> Self {
        Self::spawn_with(privileged, true, None)
    }
    pub fn with_umask(privileged: bool, mask: libc::mode_t) -> Self {
        Self::spawn_with(privileged, false, Some(mask))
    }
    fn spawn_with(privileged: bool, guest: bool, mask: Option<libc::mode_t>) -> Self {
        let directory = temporary();
        let mut command = command(&directory, privileged);
        if guest {
            command.arg("--guest");
        }
        if let Some(mask) = mask {
            command.args(["--umask", &format!("{mask:o}")]);
        }
        let child = command.spawn().unwrap();
        let mut broker = Self {
            child,
            directory,
            stopped: false,
        };
        broker.wait_listener();
        broker
    }
    pub fn binary(binary: &str) -> Self {
        let directory = temporary();
        let binary = fs::canonicalize(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(binary),
        )
        .unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .arg("--binary-driver")
            .arg(binary)
            .arg(&directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut broker = Self {
            child,
            directory,
            stopped: false,
        };
        broker.wait_listener();
        broker
    }
    fn wait_listener(&mut self) {
        wait_until(|| {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "broker exited before listening"
            );
            self.directory.join("guest-exec.sock").exists()
        });
    }
    pub fn fault(privileged: bool, name: &str) -> Self {
        let directory = temporary();
        let child = command(&directory, privileged)
            .args(["--fault", name])
            .spawn()
            .unwrap();
        let mut broker = Self {
            child,
            directory,
            stopped: false,
        };
        broker.wait_listener();
        broker
    }
    pub fn connect(&self) -> UnixStream {
        let mut stream = None;
        wait_until(
            || match UnixStream::connect(self.directory.join("guest-exec.sock")) {
                Ok(connection) => {
                    stream = Some(connection);
                    true
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    false
                }
                Err(error) => panic!("connect broker: {error}"),
            },
        );
        let stream = stream.unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        stream
    }
    pub fn restart(&mut self, privileged: bool) {
        self.restart_with_umask(privileged, None);
    }
    pub fn restart_with_umask(&mut self, privileged: bool, mask: Option<libc::mode_t>) {
        let mut command = command(&self.directory, privileged);
        if let Some(mask) = mask {
            command.args(["--umask", &format!("{mask:o}")]);
        }
        self.child = command.spawn().unwrap();
        self.stopped = false;
        self.wait_listener();
    }
    pub fn children(&self) -> Vec<i32> {
        let path = format!(
            "/proc/{}/task/{}/children",
            self.child.id(),
            self.child.id()
        );
        fs::read_to_string(path)
            .unwrap()
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect()
    }
    pub fn wait_children(&self, count: usize) {
        wait_until(|| self.children().len() == count);
    }
    pub fn stop(&mut self) -> ExitStatus {
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) },
            0
        );
        let status = self.wait();
        assert!(!self.directory.join("guest-exec.sock").exists());
        status
    }
    pub fn wait(&mut self) -> ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.stopped = true;
                return status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "broker did not reap its children"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Broker {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = fs::remove_dir_all(&self.directory);
        let _ = fs::remove_dir_all(
            std::env::temp_dir().join(format!("lasper-account-test-{}", self.child.id())),
        );
    }
}

pub fn command(directory: &Path, privileged: bool) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--broker-child")
        .arg(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if privileged {
        command.arg("--privileged");
    }
    command
}
fn temporary() -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "lasper-broker-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

pub fn enter_user_namespace() {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    assert_eq!(
        unsafe { libc::unshare(libc::CLONE_NEWUSER) },
        0,
        "{}",
        io::Error::last_os_error()
    );
    fs::write("/proc/self/setgroups", "deny\n").unwrap();
    fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")).unwrap();
    fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")).unwrap();
}

pub fn wait_until(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "broker condition did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn initialize_guest(privileged: bool) {
    initialize_guest_inner(privileged, None);
}

pub fn initialize_guest_binary(binary: &str) {
    initialize_guest_inner(false, Some(binary));
}

fn initialize_guest_inner(privileged: bool, binary: Option<&str>) {
    let guard = super::accounts::initialize(privileged);
    if let Some(binary) = binary {
        super::accounts::get().install_program(binary, "bin/guest-exec");
    }
    let root = super::accounts::get();
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
    for path in ["proc", "dev", "tmp"] {
        fs::create_dir(root.path(path)).unwrap();
        let target =
            std::ffi::CString::new(root.path(path).as_os_str().as_encoded_bytes()).unwrap();
        let source = std::ffi::CString::new(format!("/{path}")).unwrap();
        assert_eq!(
            unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REC,
                    std::ptr::null(),
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
    }
    root.enter_root().unwrap();
    std::env::set_current_dir("/").unwrap();
    std::mem::forget(guard);
}
