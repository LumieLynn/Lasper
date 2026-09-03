//! Select a closed, typed terminal attachment command for a running machine.

use crate::adapters::runtime::machine1::{Machine1OpenRequest, Machine1ShellRequest};
use crate::adapters::runtime::state as runtime_state;
#[cfg(test)]
use crate::application::sessions::ValidatedGuestUserName;
use crate::application::sessions::{SessionError, SessionSendStatus, TerminalSessionHandle};
use crate::domain::machine::MachineName;
pub use crate::domain::session::TerminalAttachmentKind as TerminalAttachKind;
use crate::domain::session::{SessionLifecycle, SessionSize};
use portable_pty::CommandBuilder;
use std::io::{self, IsTerminal};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const NAMESPACE_NAMES: [&str; 6] = ["user", "mnt", "uts", "ipc", "net", "pid"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NamespaceSnapshot {
    leader: u32,
    process: PathBuf,
    start_time: u64,
    namespaces: [FileIdentity; NAMESPACE_NAMES.len()],
    root: FileIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AttachProcessEnvironment {
    Inherited,
    MachinectlShell(crate::application::sessions::InteractiveShellEnvironment),
    SanitizedNamespace,
    MachinectlProbe,
}

impl NamespaceSnapshot {
    fn capture(leader: u32, process: &Path) -> io::Result<Self> {
        let namespaces =
            NAMESPACE_NAMES.map(|namespace| file_identity(&process.join("ns").join(namespace)));
        let namespaces = namespaces.into_iter().collect::<io::Result<Vec<_>>>()?;
        let namespaces = namespaces.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid namespace snapshot")
        })?;
        Ok(Self {
            leader,
            process: process.to_path_buf(),
            start_time: process_start_time(process)?,
            namespaces,
            root: file_identity(&process.join("root"))?,
        })
    }

    fn verify(&self) -> io::Result<()> {
        let current_start_time = process_start_time(&self.process)?;
        if current_start_time != self.start_time {
            return Err(snapshot_changed(self.leader, "process start time"));
        }
        for (namespace, expected) in NAMESPACE_NAMES.iter().zip(self.namespaces) {
            let current = file_identity(&self.process.join("ns").join(namespace))?;
            if current != expected {
                return Err(snapshot_changed(self.leader, namespace));
            }
        }
        if file_identity(&self.process.join("root"))? != self.root {
            return Err(snapshot_changed(self.leader, "root filesystem"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalAttachCommand {
    kind: TerminalAttachKind,
    program: String,
    args: Vec<String>,
    namespace_snapshot: Option<NamespaceSnapshot>,
    process_environment: AttachProcessEnvironment,
}

impl TerminalAttachCommand {
    pub fn kind(&self) -> TerminalAttachKind {
        self.kind
    }

    #[cfg(test)]
    fn program(&self) -> &str {
        &self.program
    }

    #[cfg(test)]
    fn args(&self) -> &[String] {
        &self.args
    }

    pub fn into_pty_command(self) -> io::Result<CommandBuilder> {
        let Self {
            program,
            args,
            namespace_snapshot,
            process_environment,
            ..
        } = self;
        if let Some(snapshot) = namespace_snapshot {
            snapshot.verify()?;
        }
        let mut command = CommandBuilder::new(program);
        command.args(args);
        match process_environment {
            AttachProcessEnvironment::Inherited => {}
            AttachProcessEnvironment::MachinectlShell(environment) => {
                command.env("TERM", environment.term());
                match environment.colorterm() {
                    Some(value) => command.env("COLORTERM", value),
                    None => command.env_remove("COLORTERM"),
                }
                match environment.no_color() {
                    Some(value) => command.env("NO_COLOR", value),
                    None => command.env_remove("NO_COLOR"),
                }
            }
            AttachProcessEnvironment::SanitizedNamespace => {
                // Do not copy sudo/daemon environment variables into a root
                // shell inside a potentially untrusted container.
                command.env_clear();
                command.env(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                );
                command.env("HOME", "/root");
                command.env("USER", "root");
                command.env("LOGNAME", "root");
                command.env("TERM", "xterm-256color");
            }
            AttachProcessEnvironment::MachinectlProbe => {
                // machinectl emits OSC 3008 context sequences whenever its
                // own terminal is non-dumb, including after probe output.
                command.env("TERM", "dumb");
                command.env_remove("COLORTERM");
                command.env_remove("NO_COLOR");
            }
        }
        Ok(command)
    }
}

pub fn select(name: &MachineName) -> std::io::Result<TerminalAttachCommand> {
    let leader = match runtime_state::leader_pid(name) {
        Ok(leader) => leader,
        Err(error) => return login_after_inspection_failure(name, error),
    };
    select_registered(name, leader, Path::new("/proc"), system_bus_available)
}

#[cfg(test)]
fn select_at(
    name: &MachineName,
    state_path: &Path,
    proc_root: &Path,
    bus_available: bool,
) -> std::io::Result<TerminalAttachCommand> {
    let leader = match runtime_state::leader_pid_at(state_path, name.as_str()) {
        Ok(leader) => leader,
        Err(error) => return login_after_inspection_failure(name, error),
    };
    select_registered(name, leader, proc_root, |_| bus_available)
}

fn login_after_inspection_failure(
    name: &MachineName,
    error: std::io::Error,
) -> std::io::Result<TerminalAttachCommand> {
    log::warn!(
        "Cannot inspect leader for terminal attachment to {}: {}; falling back to machinectl login",
        name,
        error
    );
    Ok(login_command(name))
}

fn select_registered(
    name: &MachineName,
    leader: u32,
    proc_root: &Path,
    bus_probe: impl FnOnce(&Path) -> bool,
) -> std::io::Result<TerminalAttachCommand> {
    let process = proc_root.join(leader.to_string());
    if bus_probe(&process) {
        return Ok(login_command(name));
    }

    namespace_command(leader, &process)
}

fn system_bus_available(process: &Path) -> bool {
    let bus_socket = process.join("root/run/dbus/system_bus_socket");
    std::fs::metadata(bus_socket)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
}

fn login_command(name: &MachineName) -> TerminalAttachCommand {
    TerminalAttachCommand {
        kind: TerminalAttachKind::Login,
        program: "machinectl".to_string(),
        args: vec!["--".to_string(), "login".to_string(), name.to_string()],
        namespace_snapshot: None,
        process_environment: AttachProcessEnvironment::Inherited,
    }
}

pub(crate) fn login(name: &MachineName) -> TerminalAttachCommand {
    login_command(name)
}

/// Build the fixed selected-user route. The username remains one argv
/// element all the way to machinectl and is never parsed by a shell.
#[cfg(test)]
pub(crate) fn shell(name: &MachineName, user: &ValidatedGuestUserName) -> TerminalAttachCommand {
    TerminalAttachCommand {
        kind: TerminalAttachKind::Login,
        program: "machinectl".to_string(),
        args: vec![
            "--".to_string(),
            "shell".to_string(),
            format!("{}@{}", user, name),
        ],
        namespace_snapshot: None,
        process_environment: AttachProcessEnvironment::Inherited,
    }
}

fn selected_user_shell(request: Machine1ShellRequest) -> io::Result<TerminalAttachCommand> {
    let process_environment = AttachProcessEnvironment::MachinectlShell(
        request.environment().terminal_environment().clone(),
    );
    let assignments = request.environment().assignments();
    let mut args = assignments
        .into_iter()
        .map(|assignment| format!("--setenv={assignment}"))
        .collect::<Vec<_>>();
    args.extend([
        "--".to_string(),
        "shell".to_string(),
        format!("{}@{}", request.user(), request.machine()),
    ]);
    Ok(TerminalAttachCommand {
        kind: TerminalAttachKind::Login,
        program: "machinectl".to_string(),
        args,
        namespace_snapshot: None,
        process_environment,
    })
}

/// Convert the closed machine1 request set to its machinectl equivalent.
pub(crate) fn machine1(request: Machine1OpenRequest) -> io::Result<TerminalAttachCommand> {
    match request {
        Machine1OpenRequest::Shell(request) => selected_user_shell(request),
        Machine1OpenRequest::WaylandProbe(request) => {
            let mut args = vec![
                "--quiet".to_string(),
                "--".to_string(),
                "shell".to_string(),
                format!("{}@{}", request.user(), request.machine()),
            ];
            args.extend(request.args());
            Ok(TerminalAttachCommand {
                kind: TerminalAttachKind::Login,
                program: "machinectl".to_string(),
                args,
                namespace_snapshot: None,
                process_environment: AttachProcessEnvironment::MachinectlProbe,
            })
        }
    }
}

#[cfg(test)]
fn wayland_shell(
    name: &MachineName,
    user: &ValidatedGuestUserName,
    guest_socket: &Path,
) -> io::Result<TerminalAttachCommand> {
    selected_user_shell(
        crate::adapters::runtime::machine1::Machine1ShellRequest::new(
            name.clone(),
            user.clone(),
            crate::adapters::runtime::machine1::Machine1Environment::shell(
                crate::application::sessions::InteractiveShellEnvironment::default(),
                Some(guest_socket),
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
        ),
    )
}

pub(crate) fn inherited_terminal_size() -> Result<SessionSize, SessionError> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(SessionError::new(
            "lasper shell requires an interactive terminal on stdin and stdout",
        ));
    }
    let (cols, rows) = crossterm::terminal::size()
        .map_err(|error| SessionError::new(format!("read terminal size: {error}")))?;
    SessionSize::new(cols, rows)
        .map_err(|error| SessionError::new(format!("validate terminal size: {error}")))
}

/// Forward the caller's terminal byte-for-byte to an application session.
pub(crate) async fn run_inherited_terminal(
    mut handle: TerminalSessionHandle,
) -> Result<i32, SessionError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct RawModeGuard;
    impl RawModeGuard {
        fn enter() -> Result<Self, SessionError> {
            crossterm::terminal::enable_raw_mode()
                .map_err(|error| SessionError::new(format!("enable terminal raw mode: {error}")))?;
            Ok(Self)
        }
    }
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }

    let mut output = handle
        .take_output()
        .ok_or_else(|| SessionError::new("terminal session output is unavailable"))?;
    let input = handle.input();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut input_buffer = [0u8; 4096];
    let mut resize = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        .map_err(|error| SessionError::new(format!("watch terminal size: {error}")))?;
    let raw_mode = RawModeGuard::enter()?;
    let mut lifecycle = Box::pin(handle.wait());
    let mut completed = None;
    let state = loop {
        if let Some(state) = completed.take() {
            while let Some(chunk) = output.recv().await {
                stdout.write_all(&chunk).await.map_err(|error| {
                    SessionError::new(format!("write terminal output: {error}"))
                })?;
            }
            stdout
                .flush()
                .await
                .map_err(|error| SessionError::new(format!("flush terminal output: {error}")))?;
            break state;
        }
        tokio::select! {
            read = stdin.read(&mut input_buffer) => {
                let read = read.map_err(|error| SessionError::new(format!("read terminal input: {error}")))?;
                if read == 0 {
                    break SessionLifecycle::Closed;
                }
                if input.send_input(input_buffer[..read].to_vec()).await == SessionSendStatus::Closed {
                    break lifecycle.await;
                }
            }
            chunk = output.recv() => {
                match chunk {
                    Some(chunk) => {
                        stdout
                            .write_all(&chunk)
                            .await
                            .map_err(|error| SessionError::new(format!("write terminal output: {error}")))?;
                        stdout
                            .flush()
                            .await
                            .map_err(|error| SessionError::new(format!("flush terminal output: {error}")))?;
                    }
                    None => break lifecycle.await,
                }
            }
            _ = resize.recv() => {
                if let Ok(size) = inherited_terminal_size() {
                    let _ = input.try_resize(size);
                }
            }
            state = &mut lifecycle => completed = Some(state),
        }
    };
    drop(raw_mode);

    match state {
        SessionLifecycle::Exited { success, code } => {
            Ok(code.unwrap_or(if success { 0 } else { 1 }))
        }
        SessionLifecycle::Closed => Ok(0),
        SessionLifecycle::Failed(message) => Err(SessionError::new(message)),
        SessionLifecycle::Running => Err(SessionError::new(
            "terminal session returned without a terminal lifecycle state",
        )),
    }
}

fn namespace_command(leader: u32, process: &Path) -> std::io::Result<TerminalAttachCommand> {
    for namespace in NAMESPACE_NAMES {
        let path = process.join("ns").join(namespace);
        std::fs::symlink_metadata(&path).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("container leader {leader} has no usable {namespace} namespace: {error}"),
            )
        })?;
    }

    let current_user_namespace = process
        .parent()
        .expect("process path has a proc root")
        .join("self/ns/user");
    let target_user_namespace = process.join("ns/user");
    // setns(2) rejects re-entering the caller's current user namespace with
    // EINVAL, which is exactly the PrivateUsers=no case.
    let enter_user_namespace = !same_namespace(&current_user_namespace, &target_user_namespace)?;
    let root = process.join("root");
    let shell = select_shell(&root)?;
    let snapshot = NamespaceSnapshot::capture(leader, process)?;
    let mut args = vec!["--target".to_string(), leader.to_string()];
    if enter_user_namespace {
        args.extend([
            "--user".to_string(),
            "--setuid=0".to_string(),
            "--setgid=0".to_string(),
            "--keep-caps".to_string(),
        ]);
    }
    args.extend([
        "--mount".to_string(),
        "--uts".to_string(),
        "--ipc".to_string(),
        "--net".to_string(),
        "--pid".to_string(),
        "--root".to_string(),
        "--wdns=/".to_string(),
        "--".to_string(),
        shell.to_string(),
    ]);
    match shell {
        "/bin/bash" => args.extend(["--noprofile".into(), "--norc".into(), "-i".into()]),
        _ => args.push("-i".into()),
    }

    Ok(TerminalAttachCommand {
        kind: TerminalAttachKind::Namespace,
        program: "nsenter".to_string(),
        args,
        namespace_snapshot: Some(snapshot),
        process_environment: AttachProcessEnvironment::SanitizedNamespace,
    })
}

fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    let metadata = std::fs::metadata(path)?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn process_start_time(process: &Path) -> io::Result<u64> {
    let stat = std::fs::read_to_string(process.join("stat"))?;
    let fields = stat
        .rsplit_once(')')
        .map(|(_, fields)| fields)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "container leader stat has no command terminator",
            )
        })?;
    fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "container leader stat has no start time",
            )
        })?
        .parse()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("container leader start time is invalid: {error}"),
            )
        })
}

fn snapshot_changed(leader: u32, part: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("container leader {leader} {part} changed during terminal setup"),
    )
}

fn same_namespace(left: &Path, right: &Path) -> std::io::Result<bool> {
    let left = std::fs::metadata(left)?;
    let right = std::fs::metadata(right)?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

fn select_shell(root: &Path) -> std::io::Result<&'static str> {
    for shell in ["/bin/bash", "/bin/sh"] {
        let path = root.join(shell.trim_start_matches('/'));
        if std::fs::metadata(path)
            .map(|metadata| {
                metadata.file_type().is_file() && metadata.permissions().mode() & 0o111 != 0
            })
            .unwrap_or(false)
        {
            return Ok(shell);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "container has no executable /bin/bash or /bin/sh for namespace attachment",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(with_bash: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let proc_root = directory.path().join("proc");
        let process = proc_root.join("4242");
        std::fs::write(&state, "NAME=test-machine\nLEADER=4242\n").unwrap();
        std::fs::create_dir_all(process.join("root/run/dbus")).unwrap();
        std::fs::create_dir_all(process.join("root/bin")).unwrap();
        std::fs::create_dir_all(process.join("ns")).unwrap();
        std::fs::create_dir_all(proc_root.join("self/ns")).unwrap();
        std::fs::write(proc_root.join("self/ns/user"), []).unwrap();
        for namespace in NAMESPACE_NAMES {
            std::fs::write(process.join("ns").join(namespace), []).unwrap();
        }
        let mut stat_fields = vec!["S".to_string()];
        stat_fields.extend((0..18).map(|_| "0".to_string()));
        stat_fields.push("12345".to_string());
        std::fs::write(
            process.join("stat"),
            format!("4242 (fixture) {}\n", stat_fields.join(" ")),
        )
        .unwrap();
        let shell = if with_bash { "bash" } else { "sh" };
        let shell_path = process.join("root/bin").join(shell);
        std::fs::write(&shell_path, []).unwrap();
        std::fs::set_permissions(&shell_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (directory, state, proc_root)
    }

    #[test]
    fn system_bus_keeps_the_native_machinectl_login_path() {
        let (_directory, state, proc_root) = fixture(true);
        let name = MachineName::new("test-machine").unwrap();

        let command = select_at(&name, &state, &proc_root, true).unwrap();

        assert_eq!(command.kind(), TerminalAttachKind::Login);
        assert_eq!(command.program(), "machinectl");
        assert_eq!(command.args(), ["--", "login", "test-machine"]);
    }

    #[test]
    fn selected_user_shell_is_a_fixed_machinectl_argv() {
        let name = MachineName::new("test-machine").unwrap();
        let user = ValidatedGuestUserName::new("1000").unwrap();

        let command = shell(&name, &user);

        assert_eq!(command.kind(), TerminalAttachKind::Login);
        assert_eq!(command.program(), "machinectl");
        assert_eq!(command.args(), ["--", "shell", "1000@test-machine"]);
    }

    #[test]
    fn selected_user_shell_forwards_the_typed_terminal_and_wayland_environment() {
        let name = MachineName::new("test-machine").unwrap();
        let user = ValidatedGuestUserName::new("alice").unwrap();

        let terminal = crate::application::sessions::InteractiveShellEnvironment::new(
            "xterm-kitty".into(),
            Some("truecolor".into()),
            Some(String::new()),
        )
        .unwrap();
        let environment = crate::adapters::runtime::machine1::Machine1Environment::shell(
            terminal,
            Some(Path::new("/run/lasper/wayland/1000/wayland-1")),
        )
        .unwrap();
        let command = machine1(Machine1OpenRequest::shell(
            crate::adapters::runtime::machine1::Machine1ShellRequest::new(name, user, environment),
        ))
        .unwrap();

        assert_eq!(command.program(), "machinectl");
        assert_eq!(
            command.args(),
            [
                "--setenv=TERM=xterm-kitty",
                "--setenv=COLORTERM=truecolor",
                "--setenv=NO_COLOR=",
                "--setenv=WAYLAND_DISPLAY=/run/lasper/wayland/1000/wayland-1",
                "--",
                "shell",
                "alice@test-machine",
            ]
        );
        let command = command.into_pty_command().unwrap();
        assert_eq!(
            command.get_env("TERM"),
            Some(std::ffi::OsStr::new("xterm-kitty"))
        );
        assert_eq!(
            command.get_env("COLORTERM"),
            Some(std::ffi::OsStr::new("truecolor"))
        );
        assert_eq!(command.get_env("NO_COLOR"), Some(std::ffi::OsStr::new("")));
    }

    #[test]
    fn wayland_shell_rejects_untyped_display_paths() {
        let name = MachineName::new("test-machine").unwrap();
        let user = ValidatedGuestUserName::new("alice").unwrap();

        for path in ["wayland-0", "/run/../tmp/socket", "/run/socket\n"] {
            assert!(wayland_shell(&name, &user, Path::new(path)).is_err());
        }
    }

    #[test]
    fn machinectl_probe_preserves_the_fixed_program_and_argument_boundaries() {
        let request = crate::adapters::runtime::machine1::Machine1WaylandProbeRequest::target(
            MachineName::new("test-machine").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
            Path::new("/run/lasper/wayland/1000/wayland-1"),
        )
        .unwrap();

        let command = machine1(Machine1OpenRequest::wayland_probe(request)).unwrap();

        assert_eq!(command.program(), "machinectl");
        assert_eq!(
            &command.args()[..6],
            [
                "--quiet",
                "--",
                "shell",
                "alice@test-machine",
                "/bin/sh",
                "-c"
            ]
        );
        assert_eq!(
            command.args().last().map(String::as_str),
            Some("/run/lasper/wayland/1000/wayland-1")
        );
        let command = command.into_pty_command().unwrap();
        assert_eq!(command.get_env("TERM"), Some(std::ffi::OsStr::new("dumb")));
        assert_eq!(command.get_env("COLORTERM"), None);
        assert_eq!(command.get_env("NO_COLOR"), None);
    }

    #[test]
    fn missing_system_bus_builds_a_closed_nsenter_command() {
        let (_directory, state, proc_root) = fixture(true);
        let name = MachineName::new("test-machine").unwrap();

        let command = select_at(&name, &state, &proc_root, false).unwrap();

        assert_eq!(command.kind(), TerminalAttachKind::Namespace);
        assert_eq!(command.program(), "nsenter");
        assert_eq!(
            command.args(),
            [
                "--target",
                "4242",
                "--user",
                "--setuid=0",
                "--setgid=0",
                "--keep-caps",
                "--mount",
                "--uts",
                "--ipc",
                "--net",
                "--pid",
                "--root",
                "--wdns=/",
                "--",
                "/bin/bash",
                "--noprofile",
                "--norc",
                "-i",
            ]
        );
    }

    #[test]
    fn shared_user_namespace_omits_user_reassociation() {
        let (_directory, state, proc_root) = fixture(true);
        let target_user = proc_root.join("4242/ns/user");
        std::fs::remove_file(&target_user).unwrap();
        std::fs::hard_link(proc_root.join("self/ns/user"), target_user).unwrap();
        let name = MachineName::new("test-machine").unwrap();

        let command = select_at(&name, &state, &proc_root, false).unwrap();

        assert_eq!(command.kind(), TerminalAttachKind::Namespace);
        assert_eq!(
            command.args(),
            [
                "--target",
                "4242",
                "--mount",
                "--uts",
                "--ipc",
                "--net",
                "--pid",
                "--root",
                "--wdns=/",
                "--",
                "/bin/bash",
                "--noprofile",
                "--norc",
                "-i",
            ]
        );
    }

    #[test]
    fn namespace_attach_uses_sh_when_bash_is_absent() {
        let (_directory, state, proc_root) = fixture(false);
        let name = MachineName::new("test-machine").unwrap();

        let command = select_at(&name, &state, &proc_root, false).unwrap();

        assert_eq!(command.args().last().map(String::as_str), Some("-i"));
        assert!(command.args().iter().any(|argument| argument == "/bin/sh"));
    }

    #[test]
    fn namespace_attach_rejects_a_changed_leader_before_spawn() {
        let (_directory, state, proc_root) = fixture(true);
        let name = MachineName::new("test-machine").unwrap();
        let command = select_at(&name, &state, &proc_root, false).unwrap();

        let mut stat_fields = vec!["S".to_string()];
        stat_fields.extend((0..18).map(|_| "0".to_string()));
        stat_fields.push("54321".to_string());
        std::fs::write(
            proc_root.join("4242/stat"),
            format!("4242 (fixture) {}\n", stat_fields.join(" ")),
        )
        .unwrap();

        let error = command.into_pty_command().unwrap_err();
        assert!(error.to_string().contains("start time changed"));
    }

    #[test]
    fn malformed_registration_does_not_select_an_untrusted_pid() {
        let (_directory, state, proc_root) = fixture(true);
        std::fs::write(&state, "NAME=another-machine\nLEADER=4242\n").unwrap();
        let name = MachineName::new("test-machine").unwrap();

        let command = select_at(&name, &state, &proc_root, false).unwrap();

        assert_eq!(command.kind(), TerminalAttachKind::Login);
    }
}
