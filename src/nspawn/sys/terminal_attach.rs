//! Select a closed, typed terminal attachment command for a running machine.

pub use crate::domain::session::TerminalAttachmentKind as TerminalAttachKind;
use crate::nspawn::adapters::comm::runtime_state;
use crate::nspawn::models::MachineName;
use portable_pty::CommandBuilder;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalAttachCommand {
    kind: TerminalAttachKind,
    program: String,
    args: Vec<String>,
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

    pub fn into_pty_command(self) -> CommandBuilder {
        let mut command = CommandBuilder::new(&self.program);
        command.args(&self.args);
        if self.kind == TerminalAttachKind::Namespace {
            // Do not copy sudo/daemon environment variables into a root shell
            // inside a potentially untrusted container.
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
        command
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
    }
}

pub(crate) fn login(name: &MachineName) -> TerminalAttachCommand {
    login_command(name)
}

fn namespace_command(leader: u32, process: &Path) -> std::io::Result<TerminalAttachCommand> {
    for namespace in ["user", "mnt", "uts", "ipc", "net", "pid"] {
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
    })
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
        for namespace in ["user", "mnt", "uts", "ipc", "net", "pid"] {
            std::fs::write(process.join("ns").join(namespace), []).unwrap();
        }
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
    fn malformed_registration_does_not_select_an_untrusted_pid() {
        let (_directory, state, proc_root) = fixture(true);
        std::fs::write(&state, "NAME=another-machine\nLEADER=4242\n").unwrap();
        let name = MachineName::new("test-machine").unwrap();

        let command = select_at(&name, &state, &proc_root, false).unwrap();

        assert_eq!(command.kind(), TerminalAttachKind::Login);
    }
}
