use lasper_guest_protocol::{
    BrokerId, CommandArgv, ExecutionKind, GuestIdentityReply, GuestProcess, PreparationId,
    PrepareRequest, PrepareRequestFrame, ProjectionRequests, StartFrame, StartRequest,
    TerminalEnvironment, TerminalSize, MAX_IDENTITY_MESSAGE_BYTES,
};
use lasper_guest_runtime::session::*;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::{fixture, Mode};

const PREPARATION: PreparationId = PreparationId::new([2; 16]);
const EAGAIN: i32 = 11;

unsafe extern "C" {
    fn fcntl(fd: i32, operation: i32, ...) -> i32;
    fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
    fn setgroups(count: usize, groups: *const u32) -> i32;
}

fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn request(process: GuestProcess<'_>, interactive: bool) -> PrepareRequestFrame<'_> {
    PrepareRequestFrame::new(
        10,
        BrokerId::new([1; 16]),
        PrepareRequest::new(
            "alice",
            if interactive {
                ExecutionKind::Interactive
            } else {
                ExecutionKind::Bounded
            },
            process,
            interactive.then(|| {
                TerminalEnvironment::new("xterm-256color", Some("truecolor"), Some("")).unwrap()
            }),
            interactive.then(|| TerminalSize::new(80, 24).unwrap()),
            ProjectionRequests::default(),
        )
        .unwrap(),
    )
}

fn spawn<'a, 'context>(
    context: &'context GuardianContext,
    request: &'a PrepareRequestFrame<'a>,
) -> SessionGuardian<'a, 'context> {
    context
        .spawn(request, PREPARATION, now_ms(), fixture::entry)
        .unwrap()
}

fn prepared(worker: &mut SessionGuardian<'_, '_>) {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let state = worker.poll(now_ms()).unwrap();
        assert!(
            state.worker.failure.is_none() && state.worker.error.is_none(),
            "{state:?}"
        );
        if state.phase == SessionPhase::Prepared {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not prepare: {state:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn assert_credentials(worker: &SessionGuardian<'_, '_>, expected_groups: &[u32]) {
    let fixture = fixture::get();
    let status = std::fs::read_to_string(format!("/proc/{}/status", worker.pid())).unwrap();
    for (label, expected) in [("Uid:", fixture.uid), ("Gid:", fixture.gid)] {
        let line = status.lines().find(|line| line.starts_with(label)).unwrap();
        let values: Vec<u32> = line
            .split_whitespace()
            .skip(1)
            .map(|value| value.parse().unwrap())
            .collect();
        assert_eq!(values, vec![expected; 4]);
    }
    let groups = status
        .lines()
        .find(|line| line.starts_with("Groups:"))
        .unwrap();
    let mut groups: Vec<u32> = groups
        .split_whitespace()
        .skip(1)
        .map(|value| value.parse().unwrap())
        .collect();
    groups.sort_unstable();
    groups.dedup();
    assert_eq!(groups, expected_groups);
}

fn assert_terminal_owner(worker: &SessionGuardian<'_, '_>) {
    let stat = std::fs::metadata(format!("/proc/{}/fd/0", worker.pid())).unwrap();
    let fixture = fixture::get();
    assert_eq!((stat.uid(), stat.gid()), (fixture.uid, fixture.gid));
    assert_eq!(stat.mode() & 0o777, 0o600);
}

fn verify_reply(ready: &fixture::ReceivedReady, request: &PrepareRequestFrame<'_>) {
    let index = usize::from(request.request().execution() == ExecutionKind::Interactive);
    let identity: UnixDatagram = ready.descriptors[index].try_clone().unwrap().into();
    identity
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let challenge = ready.frame.identity_challenge([7; 32]);
    let mut packet = vec![0; MAX_IDENTITY_MESSAGE_BYTES];
    let length = challenge.encode_packet(&mut packet).unwrap();
    identity.send(&packet[..length]).unwrap();
    let length = identity.recv(&mut packet).unwrap();
    let reply =
        GuestIdentityReply::decode_for(&packet[..length], challenge, request.request()).unwrap();
    assert_eq!(reply.guest_user(), "alice");
    assert_eq!(
        (reply.guest_uid(), reply.guest_gid()),
        (fixture::get().uid, fixture::get().gid)
    );
}

fn start(worker: &mut SessionGuardian<'_, '_>, request: &PrepareRequestFrame<'_>) {
    worker
        .start(
            now_ms(),
            StartFrame::new(
                11,
                StartRequest::new(
                    request,
                    PREPARATION,
                    None,
                    None,
                    request.request().initial_size(),
                )
                .unwrap(),
            ),
        )
        .unwrap();
}

fn terminal(ready: &fixture::ReceivedReady) -> File {
    let fd = ready.descriptors[0].try_clone().unwrap();
    let flags = unsafe { fcntl(fd.as_raw_fd(), 3) };
    assert!(flags >= 0);
    assert_eq!(unsafe { fcntl(fd.as_raw_fd(), 4, flags | 0o4000) }, 0);
    fd.into()
}

fn completed(
    worker: &mut SessionGuardian<'_, '_>,
    mut terminal: Option<File>,
) -> (WorkerObservation, String) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    loop {
        let observation = worker.poll(now_ms()).unwrap();
        loop {
            let mut bytes = [0; 1024];
            let result = if let Some(master) = terminal.as_mut() {
                match master.read(&mut bytes) {
                    Ok(length) => Ok(length),
                    Err(error) if error.raw_os_error() == Some(5) => Ok(0),
                    Err(error) => Err(error.raw_os_error().unwrap()),
                }
            } else {
                worker.read_stdout(&mut bytes).map_err(|error| match error {
                    GuardianError::System { errno, .. } => errno,
                    other => panic!("unexpected output error: {other:?}"),
                })
            };
            match result {
                Ok(0) | Err(EAGAIN) => break,
                Ok(length) => output.extend_from_slice(&bytes[..length]),
                Err(errno) => panic!("read output: {errno}"),
            }
        }
        if observation.phase == SessionPhase::Closed {
            return (
                observation.worker,
                String::from_utf8(output).unwrap().replace("\r\n", "\n"),
            );
        }
        assert!(
            Instant::now() < deadline,
            "worker did not complete: {observation:?}; {}",
            String::from_utf8_lossy(&output)
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn login_snapshot(context: &mut GuardianContext) {
    let fixture = fixture::get();
    fixture.shell_script("printf 'argv0=%s\\nhome=%s\\nuser=%s\\nlogname=%s\\nshell=%s\\nterm=%s\\ncolor=%s\\nno_color=<%s>\\ncwd=' \"$0\" \"$HOME\" \"$USER\" \"$LOGNAME\" \"$SHELL\" \"$TERM\" \"$COLORTERM\" \"$NO_COLOR\"; pwd; printf 'sudo=%s\\nrun0=%s\\nld=%s\\n' \"${SUDO_USER-unset}\" \"${SYSTEMD_EXEC_PID-unset}\" \"${LD_PRELOAD-unset}\"; exit 0\n");
    std::env::set_var("SUDO_USER", "must-not-leak");
    std::env::set_var("SYSTEMD_EXEC_PID", "123");
    std::env::set_var("LD_PRELOAD", "/must-not-leak.so");
    let request = request(GuestProcess::LoginShell, true);
    let mut worker = spawn(context, &request);
    prepared(&mut worker);
    assert_credentials(&worker, &fixture.groups);
    assert_terminal_owner(&worker);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    verify_reply(&ready, &request);
    let master = terminal(&ready);
    fixture.write_account("/different-home", "/different-shell", 999, 999);
    std::fs::write(fixture.path("etc/group"), "broken\n").unwrap();
    start(&mut worker, &request);
    let (result, output) = completed(&mut worker, Some(master));
    assert_eq!(result.failure, None, "{result:?}");
    assert_eq!(result.exit, Some(WorkerExit::Exited(0)), "{output}");
    for expected in [
        "argv0=-account-shell\n",
        "home=/home/alice\n",
        "user=alice\n",
        "logname=alice\n",
        "shell=/bin/account-shell\n",
        "term=xterm-256color\n",
        "color=truecolor\n",
        "no_color=<>\n",
        "cwd=/home/alice\n",
        "sudo=unset\n",
        "run0=unset\n",
        "ld=unset\n",
    ] {
        assert!(
            output.contains(expected),
            "missing {expected:?}: {output:?}"
        );
    }
    drop(worker);
}

fn command_arguments(context: &mut GuardianContext) {
    let fixture = fixture::get();
    fixture.write_account("/home/alice", "/usr/sbin/nologin", fixture.uid, fixture.gid);
    let command = CommandArgv::new(&[
        "/bin/account-shell",
        "-c",
        "printf '<%s>\\n' \"$0\" \"$1\" \"$2\" \"$3\"; pwd; printf '%s' \"$SHELL\"",
        "chosen argv0",
        "with spaces",
        "",
        "'quoted'\\tail",
    ])
    .unwrap();
    let request = request(GuestProcess::Command(command), false);
    let mut worker = spawn(context, &request);
    prepared(&mut worker);
    assert_credentials(&worker, &fixture.groups);
    let _ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    start(&mut worker, &request);
    let (result, output) = completed(&mut worker, None);
    assert_eq!(result.failure, None);
    assert_eq!(result.exit, Some(WorkerExit::Exited(0)));
    assert_eq!(
        output,
        "<chosen argv0>\n<with spaces>\n<>\n<'quoted'\\tail>\n/home/alice\n/usr/sbin/nologin"
    );
    drop(worker);
}

fn failure(context: &mut GuardianContext, stage: WorkerStage, errno: i32) {
    let interactive = !matches!(
        stage,
        WorkerStage::UserIdentity | WorkerStage::GroupIdentity | WorkerStage::SupplementaryGroups
    );
    let process = if interactive {
        GuestProcess::LoginShell
    } else {
        GuestProcess::Command(CommandArgv::new(&["/bin/account-shell", "-c", "exit 0"]).unwrap())
    };
    let request = request(process, interactive);
    let mut worker = spawn(context, &request);
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let observation = worker.poll(now_ms()).unwrap();
        assert!(!observation.worker.prepared, "failed account became ready");
        if observation.phase == SessionPhase::Closed {
            assert_eq!(
                observation.worker.failure,
                Some(WorkerFailure::new(stage, errno)),
                "{observation:?}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "failed account did not finish: {observation:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(worker);
}

fn denied_groups(context: &mut GuardianContext) {
    std::fs::write(fixture::get().path("etc/group"), "extra:x:1:alice\n").unwrap();
    failure(context, WorkerStage::SupplementaryGroups, 1);
}

fn denied_home(context: &mut GuardianContext) {
    std::fs::set_permissions(
        fixture::get().path("home/alice"),
        std::fs::Permissions::from_mode(0o0),
    )
    .unwrap();
    failure(context, WorkerStage::HomeDirectory, 13);
}

fn unmapped_uid(context: &mut GuardianContext) {
    let fixture = fixture::get();
    fixture.write_account("/home/alice", "/bin/account-shell", 1, fixture.gid);
    failure(context, WorkerStage::UserIdentity, 22);
}

fn unmapped_gid(context: &mut GuardianContext) {
    let fixture = fixture::get();
    fixture.write_account("/home/alice", "/bin/account-shell", fixture.uid, 1);
    failure(context, WorkerStage::SupplementaryGroups, 1);
}

fn missing_home(context: &mut GuardianContext) {
    let fixture = fixture::get();
    fixture.write_account(
        "/does-not-exist",
        "/bin/account-shell",
        fixture.uid,
        fixture.gid,
    );
    failure(context, WorkerStage::HomeDirectory, 2);
}

fn missing_shell(context: &mut GuardianContext) {
    let fixture = fixture::get();
    fixture.write_account("/home/alice", "/missing-shell", fixture.uid, fixture.gid);
    failure(context, WorkerStage::LoginShell, 2);
}

fn nologin(context: &mut GuardianContext) {
    let fixture = fixture::get();
    fixture.write_account("/home/alice", "/usr/sbin/nologin", fixture.uid, fixture.gid);
    failure(context, WorkerStage::LoginDisabled, 13);
}

fn non_executable_shell(context: &mut GuardianContext) {
    std::fs::set_permissions(
        fixture::get().path("bin/account-shell"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    failure(context, WorkerStage::LoginShell, 13);
}

fn malformed_groups(context: &mut GuardianContext) {
    std::fs::write(fixture::get().path("etc/group"), "broken\n").unwrap();
    failure(context, WorkerStage::GroupDatabase, 22);
}

fn missing_groups(context: &mut GuardianContext) {
    let fixture = fixture::get();
    assert_eq!(
        unsafe { setgroups(fixture.groups.len(), fixture.groups.as_ptr()) },
        0
    );
    std::fs::remove_file(fixture.path("etc/group")).unwrap();
    let request = request(
        GuestProcess::Command(
            CommandArgv::new(&["/bin/account-shell", "-c", "printf minimal"]).unwrap(),
        ),
        false,
    );
    let mut worker = spawn(context, &request);
    prepared(&mut worker);
    assert_credentials(&worker, &[fixture.gid]);
    let ready = fixture::receive_ready(&mut worker, context.connection(), now_ms());
    verify_reply(&ready, &request);
    start(&mut worker, &request);
    let (result, output) = completed(&mut worker, None);
    assert_eq!(result.failure, None);
    assert_eq!(result.exit, Some(WorkerExit::Exited(0)));
    assert_eq!(output, "minimal");
}

fn invalid_group_file_type(context: &mut GuardianContext) {
    let path = fixture::get().path("etc/group");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(path).unwrap();
    failure(context, WorkerStage::GroupDatabase, 13);
}

fn oversized_passwd(context: &mut GuardianContext) {
    std::fs::write(fixture::get().path("etc/passwd"), vec![b'#'; 65537]).unwrap();
    failure(context, WorkerStage::Account, 27);
}

fn fifo_database(context: &mut GuardianContext) {
    let path = fixture::get().path("etc/passwd");
    std::fs::remove_file(&path).unwrap();
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { mkfifo(path.as_ptr(), 0o600) }, 0);
    failure(context, WorkerStage::Account, 13);
}

pub(super) const SCENARIOS: &[(&str, super::ScenarioFn, Mode)] = &[
    (
        "login shell uses prepared account snapshot and clean environment",
        login_snapshot,
        Mode::Both,
    ),
    (
        "explicit argv does not invoke account shell",
        command_arguments,
        Mode::Both,
    ),
    (
        "setgroups denial is not ignored",
        denied_groups,
        Mode::Namespace,
    ),
    ("unmapped uid is rejected", unmapped_uid, Mode::Namespace),
    (
        "unmapped gid cannot enter the target group set",
        unmapped_gid,
        Mode::Namespace,
    ),
    ("missing home is not replaced", missing_home, Mode::Both),
    (
        "home must be accessible after dropping root",
        denied_home,
        Mode::Privileged,
    ),
    ("missing shell is not replaced", missing_shell, Mode::Both),
    (
        "nologin account cannot start login shell",
        nologin,
        Mode::Both,
    ),
    (
        "shell must be executable by the selected account",
        non_executable_shell,
        Mode::Both,
    ),
    (
        "partial group database is rejected",
        malformed_groups,
        Mode::Both,
    ),
    (
        "missing group database removes inherited supplementary groups",
        missing_groups,
        Mode::Privileged,
    ),
    (
        "invalid group file type is not treated as missing",
        invalid_group_file_type,
        Mode::Both,
    ),
    (
        "account file size stays bounded",
        oversized_passwd,
        Mode::Both,
    ),
    (
        "fifo account file cannot block preparation",
        fifo_database,
        Mode::Both,
    ),
];
