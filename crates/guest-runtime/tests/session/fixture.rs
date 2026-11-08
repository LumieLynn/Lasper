use lasper_guest_protocol::{PrepareReadyFrame, ProjectionObservations};
use lasper_guest_runtime::session::{SessionGuardian, WorkerBootstrap, WorkerFailure, WorkerStage};
use std::ffi::CString;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

unsafe extern "C" {
    fn getuid() -> u32;
    fn getgid() -> u32;
    fn getgroups(count: i32, groups: *mut u32) -> i32;
    fn unshare(flags: i32) -> i32;
    fn chroot(path: *const std::ffi::c_char) -> i32;
    fn chown(path: *const std::ffi::c_char, uid: u32, gid: u32) -> i32;
}

pub(super) struct Fixture {
    root: PathBuf,
    root_c: CString,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();

pub(super) struct ReceivedReady {
    pub frame: PrepareReadyFrame,
    pub descriptors: Vec<OwnedFd>,
}

pub(super) fn receive_ready(worker: &mut SessionGuardian<'_>, now_ms: u64) -> ReceivedReady {
    let ready = worker.take_ready(now_ms).unwrap();
    ready
        .deliver(|frame, descriptors| {
            let descriptors = descriptors
                .as_raw_fds()
                .iter()
                .map(|&fd| {
                    // SAFETY: Ready keeps every FD alive until delivery completes.
                    unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()
                })
                .collect::<std::io::Result<Vec<_>>>()?;
            Ok::<_, std::io::Error>(ReceivedReady { frame, descriptors })
        })
        .unwrap()
}

pub(super) fn get() -> &'static Fixture {
    FIXTURE.get().unwrap()
}

pub(super) fn initialize(privileged: bool) -> FixtureGuard {
    if privileged {
        assert_eq!(unsafe { getuid() }, 0, "--privileged needs root");
    } else {
        let uid = unsafe { getuid() };
        let gid = unsafe { getgid() };
        assert_eq!(
            unsafe { unshare(0x10000000) },
            0,
            "unshare user namespace: {}",
            std::io::Error::last_os_error()
        );
        std::fs::write("/proc/self/setgroups", "deny\n").unwrap();
        std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n")).unwrap();
        std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n")).unwrap();
    }
    let (uid, gid, groups) = if privileged {
        (1000, 100, vec![18, 44, 100])
    } else {
        let count = unsafe { getgroups(0, std::ptr::null_mut()) };
        assert!(count >= 0);
        let mut groups = vec![0; count as usize];
        assert_eq!(unsafe { getgroups(count, groups.as_mut_ptr()) }, count);
        groups.push(0);
        groups.sort_unstable();
        groups.dedup();
        (0, 0, groups)
    };
    let root = std::env::temp_dir().join(format!("lasper-account-test-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    for directory in ["etc", "bin", "home/alice"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    let fixture = Fixture {
        root_c: CString::new(root.as_os_str().as_encoded_bytes()).unwrap(),
        root,
        uid,
        gid,
        groups,
    };
    fixture.write_account("/home/alice", "/bin/account-shell", uid, gid);
    let mut group = String::new();
    for gid in &fixture.groups {
        group.push_str(&format!("g{gid}:x:{gid}:alice\n"));
    }
    std::fs::write(fixture.path("etc/group"), group).unwrap();
    fixture.install_program("/bin/sh", "bin/account-shell");
    fixture.install_tools();
    if privileged {
        let home = CString::new(fixture.path("home/alice").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { chown(home.as_ptr(), uid, gid) }, 0);
    }
    FIXTURE.set(fixture).ok().unwrap();
    FixtureGuard
}

pub(super) struct FixtureGuard;

impl Drop for FixtureGuard {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&get().root) {
            eprintln!("remove test guest root {}: {error}", get().root.display());
        }
    }
}

impl Fixture {
    pub fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }

    pub fn write_account(&self, home: &str, shell: &str, uid: u32, gid: u32) {
        std::fs::write(
            self.path("etc/passwd"),
            format!("root:x:0:0::/:/bin/account-shell\nalice:x:{uid}:{gid}::{home}:{shell}\n"),
        )
        .unwrap();
    }

    pub fn install_program(&self, source: impl AsRef<Path>, relative: impl AsRef<Path>) {
        let target = self.path(relative);
        if target.exists() {
            return;
        }
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(source, &target).unwrap();
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn shell_script(&self, script: &str) {
        std::fs::write(self.path("home/alice/.profile"), script).unwrap();
    }

    fn install_tools(&self) {
        for source in [
            "/bin/sh",
            "/bin/true",
            "/bin/sleep",
            "/usr/bin/setsid",
            "/usr/bin/stty",
        ] {
            let path = Path::new(source);
            self.install_program(path, path.strip_prefix("/").unwrap());
            let output = std::process::Command::new("ldd")
                .arg(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "ldd {source}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            for token in String::from_utf8(output.stdout).unwrap().split_whitespace() {
                if token.starts_with('/') {
                    let library = Path::new(token);
                    self.install_program(library, library.strip_prefix("/").unwrap());
                }
            }
        }
    }
}

pub(super) fn entry(bootstrap: WorkerBootstrap<'_>) -> ! {
    let root = &get().root_c;
    if unsafe { chroot(root.as_ptr()) } != 0 {
        bootstrap.fail(WorkerFailure::new(WorkerStage::Account, 13));
    }
    bootstrap.run(|_| Ok(ProjectionObservations::default()))
}
