use super::*;
use std::collections::VecDeque;
use std::num::NonZeroU64;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Mutex;

struct Source {
    registrations: Mutex<VecDeque<MachineRegistration>>,
    groups: Mutex<VecDeque<PathBuf>>,
    exit_on_group_query: Mutex<Option<Child>>,
}

impl Source {
    fn new(
        registrations: impl IntoIterator<Item = MachineRegistration>,
        groups: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        Self {
            registrations: Mutex::new(registrations.into_iter().collect()),
            groups: Mutex::new(groups.into_iter().collect()),
            exit_on_group_query: Mutex::new(None),
        }
    }
}

impl MachineTargetSource for Source {
    async fn registration(&self, _: &str) -> io::Result<MachineRegistration> {
        Ok(self
            .registrations
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected registration query"))
    }

    async fn control_group(&self, unit: &str) -> io::Result<PathBuf> {
        assert_eq!(unit, "custom.scope");
        drop(self.exit_on_group_query.lock().unwrap().take());
        Ok(self
            .groups
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected unit query"))
    }
}

fn pidfd(pid: u32) -> OwnedFd {
    // SAFETY: pidfd_open has no pointer arguments and returns a new owned FD.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(fd >= 0, "{}", io::Error::last_os_error());
    // SAFETY: The successful syscall returned a descriptor without another owner.
    unsafe { OwnedFd::from_raw_fd(fd as i32) }
}

fn start_time(pid: u32) -> NonZeroU64 {
    let bytes = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
    let end = bytes.iter().rposition(|byte| *byte == b')').unwrap();
    let value = std::str::from_utf8(&bytes[end + 1..])
        .unwrap()
        .split_ascii_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    NonZeroU64::new(value).unwrap()
}

fn registration_for(pid: u32, name: &str, unit: &str) -> MachineRegistration {
    let observed = start_time(pid);
    MachineRegistration::new(name.into(), unit.into(), pid, observed, pidfd(pid)).unwrap()
}

fn registration() -> MachineRegistration {
    registration_for(std::process::id(), "guest", "custom.scope")
}

fn current_group() -> PathBuf {
    let records = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let legacy = records.lines().find_map(|line| {
        let (_, rest) = line.split_once(':')?;
        let (controllers, path) = rest.split_once(':')?;
        controllers
            .split(',')
            .any(|name| name == "name=systemd")
            .then_some(path)
    });
    legacy
        .or_else(|| records.lines().find_map(|line| line.strip_prefix("0::")))
        .unwrap()
        .into()
}

fn rejected<T>(result: io::Result<T>) -> io::Error {
    match result {
        Ok(_) => panic!("accepted invalid instance evidence"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn acquisition_retains_the_sources_process_across_both_queries() {
    let source = Source::new(
        [registration(), registration()],
        [current_group(), current_group()],
    );
    let target = MachineTarget::resolve(&source, "guest").await.unwrap();
    assert_eq!(target.leader_pid(), std::process::id() as i32);
    target.recheck().unwrap();
    assert!(source.registrations.lock().unwrap().is_empty());
    assert!(source.groups.lock().unwrap().is_empty());
}

#[test]
fn a_pidfd_with_the_same_numeric_pid_must_match_the_registered_start_time() {
    let pid = std::process::id();
    let observed = start_time(pid);
    let different =
        NonZeroU64::new(observed.get().saturating_sub(1)).unwrap_or(NonZeroU64::new(2).unwrap());
    let error = rejected(MachineRegistration::new(
        "guest".into(),
        "custom.scope".into(),
        pid,
        different,
        pidfd(pid),
    ));
    assert!(
        error.to_string().contains("registered leader start time"),
        "{error}"
    );
}

#[test]
fn a_pidfd_for_another_process_cannot_supply_the_registered_leader() {
    let pid = std::process::id();
    let error = rejected(MachineRegistration::new(
        "guest".into(),
        "custom.scope".into(),
        pid + 1,
        start_time(pid),
        pidfd(pid),
    ));
    assert!(
        error.to_string().contains("registered leader PID"),
        "{error}"
    );
}

#[tokio::test]
async fn rejects_unit_or_leader_replacement_and_cgroup_changes_during_acquisition() {
    let other = Child::spawn();
    for change in ["leader", "unit", "cgroup"] {
        let after = registration_for(
            if change == "leader" {
                other.pid
            } else {
                std::process::id()
            },
            "guest",
            if change == "unit" {
                "replacement.scope"
            } else {
                "custom.scope"
            },
        );
        let source = Source::new(
            [registration(), after],
            [
                current_group(),
                if change == "cgroup" {
                    "/different".into()
                } else {
                    current_group()
                },
            ],
        );
        let error = rejected(MachineTarget::resolve(&source, "guest").await);
        assert!(
            error
                .to_string()
                .contains("changed during target acquisition"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn a_source_cannot_return_a_previously_valid_registration_after_its_leader_exits() {
    let child = Child::spawn();
    let captured = registration_for(child.pid, "guest", "custom.scope");
    drop(child);
    let source = Source::new([captured], []);
    assert_eq!(
        rejected(MachineTarget::resolve(&source, "guest").await).kind(),
        io::ErrorKind::NotFound
    );
}

#[tokio::test]
async fn matching_registration_fields_do_not_hide_leader_exit_during_unit_query() {
    let child = Child::spawn();
    let source = Source::new(
        [
            registration_for(child.pid, "guest", "custom.scope"),
            registration_for(child.pid, "guest", "custom.scope"),
        ],
        [current_group()],
    );
    *source.exit_on_group_query.lock().unwrap() = Some(child);
    assert_eq!(
        rejected(MachineTarget::resolve(&source, "guest").await).kind(),
        io::ErrorKind::NotFound
    );
}

#[tokio::test]
async fn rejects_another_machines_registration_before_querying_its_unit() {
    let source = Source::new(
        [registration_for(
            std::process::id(),
            "other",
            "custom.scope",
        )],
        [],
    );
    let error = rejected(MachineTarget::resolve(&source, "guest").await);
    assert!(error.to_string().contains("different machine"), "{error}");
}

#[test]
fn invalid_registration_cannot_bypass_the_checked_constructor() {
    for (name, pid, unit) in [
        ("", std::process::id(), "custom.scope"),
        ("guest", 0, "custom.scope"),
        ("guest", std::process::id(), "machine.slice"),
        ("guest", std::process::id(), "/custom.scope"),
    ] {
        let error = rejected(MachineRegistration::new(
            name.into(),
            unit.into(),
            pid,
            start_time(std::process::id()),
            pidfd(std::process::id()),
        ));
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}

struct Child {
    pid: u32,
}

impl Child {
    fn spawn() -> Self {
        // SAFETY: The child uses only close_range/pause/_exit and never returns
        // to the allocator or inherited test-runner locks after fork.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "{}", io::Error::last_os_error());
        if pid == 0 {
            unsafe {
                // Do not retain other tests' sockets while this fixture waits.
                if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0) < 0 {
                    libc::_exit(1);
                }
                loop {
                    libc::pause();
                }
            }
        }
        Self { pid: pid as u32 }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // SAFETY: This fixture exclusively reaps the child, so its numeric PID
        // cannot be recycled before the signal and wait below.
        unsafe {
            libc::kill(self.pid as i32, libc::SIGKILL);
        }
        loop {
            // SAFETY: No other owner waits for this child; no output is needed.
            if unsafe { libc::waitpid(self.pid as i32, std::ptr::null_mut(), 0) } >= 0 {
                break;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
    }
}
