use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::CommandRunner;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::session::terminal_attach::TerminalAttachCommand;
use crate::adapters::session::{MachineSessionRequest, MachineShellRequest};
use crate::domain::inspection::{InspectionCompleteness, InspectionSource, MachineProperties};
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, MachineEntry, RuntimeSnapshot, StatusUpdate};
use serde::Deserialize;
use std::io;
use std::time::Duration;

const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(5);
const HOST_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
enum UnitInspection {
    Present,
    NotFound(String),
}

#[derive(Deserialize)]
struct MachinectlImageRow {
    name: String,
    #[serde(rename = "type")]
    image_type: String,
    ro: bool,
    usage: Option<u64>,
}

fn parse_list_images_json(output: &[u8]) -> Result<Vec<ImageEntry>> {
    let rows: Vec<MachinectlImageRow> = serde_json::from_slice(output).map_err(|error| {
        NspawnError::Runtime(format!(
            "failed to parse machinectl list-images JSON output: {error}"
        ))
    })?;
    let mut images = rows
        .into_iter()
        .map(|row| ImageEntry {
            name: row.name,
            image_type: row.image_type,
            readonly: row.ro,
            usage: row
                .usage
                .map(crate::adapters::runtime::formatting::format_size),
            dbus_object_path: None,
        })
        .collect::<Vec<_>>();
    images.sort();
    Ok(images)
}

fn snapshot_update(
    previous: &mut Option<RuntimeSnapshot>,
    consecutive_failures: &mut u32,
    result: Result<RuntimeSnapshot>,
) -> Option<StatusUpdate> {
    match result {
        Ok(snapshot) => {
            let recovered = *consecutive_failures > 0;
            *consecutive_failures = 0;
            let changed = previous.as_ref() != Some(&snapshot);
            if changed || recovered {
                *previous = Some(snapshot.clone());
                Some(StatusUpdate::Snapshot(snapshot))
            } else {
                None
            }
        }
        Err(error) => {
            *consecutive_failures = consecutive_failures.saturating_add(1);
            Some(StatusUpdate::BackendFailure {
                message: error.to_string(),
                consecutive_failures: *consecutive_failures,
            })
        }
    }
}

#[derive(Clone)]
pub struct CliBackend {
    cmd_runner: std::sync::Arc<dyn CommandRunner>,
    runtime_machines_dir: std::path::PathBuf,
    nudge_rx: std::sync::Arc<parking_lot::Mutex<Option<tokio::sync::watch::Receiver<()>>>>,
}

impl CliBackend {
    pub fn new(runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        Self {
            cmd_runner: runner,
            runtime_machines_dir: crate::paths::runtime_machines_dir(),
            nudge_rx: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    pub fn set_nudge(&self, rx: tokio::sync::watch::Receiver<()>) {
        *self.nudge_rx.lock() = Some(rx);
    }

    #[cfg(test)]
    fn with_runner(runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        Self::new(runner)
    }

    #[cfg(test)]
    fn with_runtime_machines_dir(mut self, path: std::path::PathBuf) -> Self {
        self.runtime_machines_dir = path;
        self
    }
}

/// Encode one of the closed machine-session requests as a `machinectl`
/// command.  This is a runtime transport operation: the session layer owns
/// the request semantics, while this module owns the CLI wire shape.
pub(crate) fn machine_session_command(
    request: MachineSessionRequest,
) -> io::Result<TerminalAttachCommand> {
    match request {
        MachineSessionRequest::Shell(request) => selected_user_shell(request),
        MachineSessionRequest::WaylandProbe(request) => wayland_probe(request),
    }
}

fn selected_user_shell(request: MachineShellRequest) -> io::Result<TerminalAttachCommand> {
    let terminal = request.environment().terminal_environment().clone();
    let mut args = request
        .environment()
        .assignments()
        .into_iter()
        .map(|assignment| format!("--setenv={assignment}"))
        .collect::<Vec<_>>();
    args.extend([
        "--".to_string(),
        "shell".to_string(),
        format!("{}@{}", request.user(), request.machine()),
    ]);
    Ok(TerminalAttachCommand::with_terminal_environment(
        args, terminal,
    ))
}

fn wayland_probe(
    request: crate::adapters::session::WaylandProbeRequest,
) -> io::Result<TerminalAttachCommand> {
    let mut args = vec![
        "--quiet".to_string(),
        "--".to_string(),
        "shell".to_string(),
        format!("{}@{}", request.user(), request.machine()),
    ];
    args.extend(request.args());
    Ok(TerminalAttachCommand::with_dumb_environment(args))
}

#[async_trait::async_trait]
impl RuntimeSource for CliBackend {
    async fn is_available(&self) -> bool {
        which::which("machinectl").is_ok()
    }

    async fn list_machines(&self) -> Result<Vec<MachineEntry>> {
        crate::adapters::runtime::state::list_machines_at(self.runtime_machines_dir.clone()).await
    }

    async fn list_images(&self) -> Result<Vec<ImageEntry>> {
        let out = self
            .cmd_runner
            .run_bounded(
                "machinectl",
                vec![
                    "--no-ask-password".to_string(),
                    "--output=json".to_string(),
                    "--no-pager".to_string(),
                    "--all".to_string(),
                    "--".to_string(),
                    "list-images".to_string(),
                ],
                HOST_QUERY_TIMEOUT,
            )
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "machinectl list-images",
                "machinectl --no-ask-password --output=json --no-pager --all -- list-images",
                &out,
            ));
        }

        parse_list_images_json(&out.stdout)
    }

    async fn get_properties(
        &self,
        name: &str,
        include_nspawn_unit: bool,
    ) -> Result<MachineProperties> {
        get_properties_with_runner(name, include_nspawn_unit, self.cmd_runner.as_ref()).await
    }

    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<StatusUpdate>) -> Result<()> {
        let mut nudge_rx = self.nudge_rx.lock().take().ok_or_else(|| {
            NspawnError::Dbus(zbus::Error::Failure(
                "watch_events: no nudge channel set on CliBackend".into(),
            ))
        })?;

        let mut previous = None;
        let mut consecutive_failures = 0;
        let mut nudge_open = true;
        let mut interval = tokio::time::interval(WATCH_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if nudge_open {
                tokio::select! {
                    _ = interval.tick() => {}
                    changed = nudge_rx.changed() => {
                        if changed.is_err() {
                            nudge_open = false;
                        }
                    }
                }
            } else {
                interval.tick().await;
            }
            if nudge_open {
                // Coalesce a nudge that arrived with the interval tick. A
                // nudge arriving during the snapshot remains pending and
                // triggers the next pass.
                let _ = nudge_rx.borrow_and_update();
            }

            if let Some(update) = snapshot_update(
                &mut previous,
                &mut consecutive_failures,
                RuntimeSource::snapshot(self).await,
            ) {
                if tx.send(update).await.is_err() {
                    break;
                }
            }
        }
        Ok(())
    }
}

/// Fixed, non-interactive CLI inspection shared with the elevated daemon.
pub(crate) async fn get_properties_with_runner(
    name: &str,
    include_nspawn_unit: bool,
    cmd_runner: &dyn CommandRunner,
) -> Result<MachineProperties> {
    let name = parse_machine_name(name)?;
    let mut props =
        MachineProperties::from_inspection(InspectionSource::Cli, InspectionCompleteness::Full);

    let machine_args = vec![
        "--no-ask-password".to_string(),
        "--".to_string(),
        "show".to_string(),
        name.as_str().to_string(),
    ];
    let machine_command = format!("machinectl {}", machine_args.join(" "));
    let mut failures = Vec::new();

    let machine_out = cmd_runner
        .run_bounded("machinectl", machine_args.clone(), HOST_QUERY_TIMEOUT)
        .await;

    match machine_out {
        Ok(out) if out.status.success() => {
            let inserted = insert_machine_properties(&mut props, &out.stdout);
            if inserted == 0 {
                failures.push("machinectl returned no machine properties".to_string());
            }
        }
        Ok(out) => failures.push(format!(
            "machinectl: {}",
            crate::adapters::process::command_diagnostic(&out)
        )),
        Err(error) => failures.push(format!("machinectl: {error}")),
    }

    if include_nspawn_unit {
        match append_systemd_unit_properties(&name, cmd_runner, &mut props).await {
            Ok(UnitInspection::Present) => {}
            Ok(UnitInspection::NotFound(diagnostic)) => {
                if props.groups.is_empty() {
                    failures.push(format!("systemctl: {diagnostic}"));
                }
            }
            Err(error) => failures.push(format!("systemctl: {error}")),
        }
    }

    if props.groups.is_empty() {
        let (operation, command) = if include_nspawn_unit {
            let unit = name.systemd_nspawn_unit();
            (
                format!("machinectl/systemctl show {}", name.as_str()),
                format!("{machine_command}; systemctl show {unit}"),
            )
        } else {
            (
                format!("machinectl show {}", name.as_str()),
                machine_command.clone(),
            )
        };
        return Err(NspawnError::CommandFailed(
            operation,
            command,
            if failures.is_empty() {
                "The target machine might not exist or its provider did not expose properties."
                    .into()
            } else {
                failures.join("; ")
            },
        ));
    }

    if !failures.is_empty() {
        log::warn!(
            "partial CLI inspection for {}: {}",
            name,
            failures.join("; ")
        );
    }

    Ok(props)
}

/// Inspect only the systemd-nspawn unit associated with an image.
///
/// Image names follow filesystem component rules and are broader than machine
/// names. `None` means the image cannot have a corresponding nspawn machine
/// unit; command or systemd failures remain errors.
pub(crate) async fn get_image_unit_properties_with_runner(
    name: &str,
    cmd_runner: &dyn CommandRunner,
) -> Result<Option<MachineProperties>> {
    let Ok(name) = MachineName::new(name) else {
        return Ok(None);
    };
    let mut props =
        MachineProperties::from_inspection(InspectionSource::Cli, InspectionCompleteness::Full);
    match append_systemd_unit_properties(&name, cmd_runner, &mut props).await? {
        UnitInspection::Present => Ok(Some(props)),
        UnitInspection::NotFound(diagnostic) => {
            let unit = name.systemd_nspawn_unit();
            Err(NspawnError::CommandFailed(
                format!("systemctl show {unit}"),
                format!("systemctl --no-ask-password -- show {unit}"),
                diagnostic,
            ))
        }
    }
}

fn insert_machine_properties(props: &mut MachineProperties, output: &[u8]) -> usize {
    let mut inserted = 0usize;
    for (key, val) in parse_property_lines(output) {
        let formatted = crate::adapters::runtime::formatting::format_property(
            &key,
            &zbus::zvariant::Value::Str(val.as_str().into()),
        );
        props.insert(crate::domain::inspection::GROUP_MACHINE, key, formatted);
        inserted = inserted.saturating_add(1);
    }
    inserted
}

fn parse_property_lines(output: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

async fn append_systemd_unit_properties(
    name: &MachineName,
    cmd_runner: &dyn CommandRunner,
    props: &mut MachineProperties,
) -> Result<UnitInspection> {
    let unit = name.systemd_nspawn_unit();
    let args = vec![
        "--no-ask-password".to_string(),
        "--".to_string(),
        "show".to_string(),
        unit.clone(),
    ];
    let system_out = cmd_runner
        .run_bounded("systemctl", args.clone(), HOST_QUERY_TIMEOUT)
        .await
        .map_err(|error| NspawnError::Io(std::path::PathBuf::from("systemctl"), error))?;

    if !system_out.status.success() {
        return Err(NspawnError::cmd_failed(
            format!("systemctl show {unit}"),
            format!("systemctl {}", args.join(" ")),
            &system_out,
        ));
    }

    let properties = parse_property_lines(&system_out.stdout);
    if properties
        .iter()
        .any(|(key, value)| key == "LoadState" && value == "not-found")
    {
        let load_error = properties
            .iter()
            .find(|(key, _)| key == "LoadError")
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty());
        let diagnostic = match load_error {
            Some(load_error) => format!("LoadState=not-found; LoadError={load_error}"),
            None => format!("Unit {unit} was not found (LoadState=not-found)"),
        };
        return Ok(UnitInspection::NotFound(diagnostic));
    }

    if properties.is_empty() {
        return Err(NspawnError::CommandFailed(
            format!("systemctl show {unit}"),
            format!("systemctl {}", args.join(" ")),
            "No properties found".to_string(),
        ));
    }

    for (key, val) in properties {
        let formatted = crate::adapters::runtime::formatting::format_property(
            &key,
            &zbus::zvariant::Value::Str(val.as_str().into()),
        );
        crate::adapters::runtime::formatting::insert_systemd_property(props, key, formatted);
    }

    Ok(UnitInspection::Present)
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::process::MockCommandRunner;
    use crate::adapters::session::{
        MachineShellEnvironment, MachineShellRequest, WaylandProbeRequest,
    };
    use crate::application::sessions::{InteractiveShellEnvironment, ValidatedGuestUserName};
    use crate::domain::runtime::MachineState;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::Output;

    fn mock_output(status: bool, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(if status { 0 } else { 256 }),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn observer_snapshot() -> RuntimeSnapshot {
        RuntimeSnapshot::new(
            vec![MachineEntry {
                name: "active".into(),
                class: MachineEntry::NSPAWN_CLASS.into(),
                service: MachineEntry::NSPAWN_SERVICE.into(),
                state: MachineState::Running,
                addresses: Default::default(),
            }],
            vec![ImageEntry {
                name: "active".into(),
                image_type: "directory".into(),
                readonly: false,
                usage: None,
                dbus_object_path: None,
            }],
        )
    }

    #[test]
    fn selected_user_shell_is_a_fixed_machinectl_argv() {
        let name = MachineName::new("test-machine").unwrap();
        let user = ValidatedGuestUserName::new("1000").unwrap();
        let request = MachineSessionRequest::shell(MachineShellRequest::new(
            name,
            user,
            MachineShellEnvironment::default(),
        ));

        let command = machine_session_command(request).unwrap();

        assert_eq!(
            command.kind(),
            crate::domain::session::TerminalAttachmentKind::Login
        );
        assert_eq!(command.program(), "machinectl");
        assert_eq!(
            command.args(),
            ["--setenv=TERM=dumb", "--", "shell", "1000@test-machine"]
        );
    }

    #[test]
    fn selected_user_shell_forwards_the_typed_terminal_and_wayland_environment() {
        let name = MachineName::new("test-machine").unwrap();
        let user = ValidatedGuestUserName::new("alice").unwrap();
        let terminal = InteractiveShellEnvironment::new(
            "xterm-kitty".into(),
            Some("truecolor".into()),
            Some(String::new()),
        )
        .unwrap();
        let environment = MachineShellEnvironment::shell(
            terminal,
            Some(Path::new("/run/lasper/wayland/1000/wayland-1")),
        )
        .unwrap();
        let request =
            MachineSessionRequest::shell(MachineShellRequest::new(name, user, environment));

        let command = machine_session_command(request).unwrap();

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
    fn machinectl_probe_preserves_the_fixed_program_and_argument_boundaries() {
        let request = WaylandProbeRequest::target(
            MachineName::new("test-machine").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
            Path::new("/run/lasper/wayland/1000/wayland-1"),
        )
        .unwrap();

        let command =
            machine_session_command(MachineSessionRequest::wayland_probe(request)).unwrap();

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
    fn snapshot_observer_publishes_changes_failures_and_recovery() {
        let snapshot = observer_snapshot();
        let mut previous = None;
        let mut failures = 0;

        assert!(matches!(
            snapshot_update(&mut previous, &mut failures, Ok(snapshot.clone())),
            Some(StatusUpdate::Snapshot(_))
        ));
        assert!(snapshot_update(&mut previous, &mut failures, Ok(snapshot.clone())).is_none());

        let failure = snapshot_update(
            &mut previous,
            &mut failures,
            Err(NspawnError::Runtime("temporary failure".into())),
        );
        assert!(matches!(
            failure,
            Some(StatusUpdate::BackendFailure {
                consecutive_failures: 1,
                ..
            })
        ));

        assert!(matches!(
            snapshot_update(&mut previous, &mut failures, Ok(snapshot)),
            Some(StatusUpdate::Snapshot(_))
        ));
        assert_eq!(failures, 0);
    }

    #[tokio::test]
    async fn list_machines_uses_runtime_registrations_without_commands() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run().never();
            r
        });
        let runtime = tempfile::tempdir().unwrap();
        std::fs::write(
            runtime.path().join("active"),
            "NAME=active\nCLASS=container\nSERVICE=systemd-nspawn\n",
        )
        .unwrap();
        let provider =
            CliBackend::with_runner(runner).with_runtime_machines_dir(runtime.path().to_path_buf());

        let machines = RuntimeSource::list_machines(&provider).await.unwrap();

        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].name, "active");
        assert_eq!(machines[0].state, MachineState::Running);
        assert!(matches!(
            machines[0].addresses,
            crate::domain::runtime::MachineAddressObservation::Unsupported(_)
        ));
    }

    #[tokio::test]
    async fn list_images_preserves_systemd_image_names() {
        let runner = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run_bounded()
                .withf(|program, args, timeout| {
                    program == "machinectl"
                        && args.first().is_some_and(|arg| arg == "--no-ask-password")
                        && args.get(1).is_some_and(|arg| arg == "--output=json")
                        && args.get(4).is_some_and(|arg| arg == "--")
                        && args.get(5).is_some_and(|arg| arg == "list-images")
                        && *timeout == HOST_QUERY_TIMEOUT
                })
                .returning(|_, _, _| {
                    Ok(mock_output(
                        true,
                        r#"[
                            {"name":".host","type":"subvolume","ro":true,"usage":null,"created":null,"modified":null},
                            {"name":".oci-sha256:abc","type":"subvolume","ro":true,"usage":10485760,"created":0,"modified":0},
                            {"name":"Ubuntu Resolute 镜像","type":"directory","ro":false,"usage":20971520,"created":0,"modified":0},
                            {"name":" edge spaced ","type":"raw","ro":false,"usage":0,"created":0,"modified":0}
                        ]"#,
                        "",
                    ))
                });
            r
        });
        let provider = CliBackend::with_runner(runner);

        let images = RuntimeSource::list_images(&provider).await.unwrap();

        assert_eq!(images.len(), 4);
        assert!(
            images
                .iter()
                .all(|image| image.name == ".host"
                    || image.is_hidden() == image.name.starts_with('.'))
        );
        assert!(images.iter().any(|image| image.name == ".host"));
        let ubuntu = images
            .iter()
            .find(|image| image.name == "Ubuntu Resolute 镜像")
            .unwrap();
        assert!(!ubuntu.is_hidden());
        assert_eq!(ubuntu.image_type, "directory");
        assert_eq!(ubuntu.usage.as_deref(), Some("20.0M"));
        assert!(images.iter().any(|image| image.name == " edge spaced "));
        assert_eq!(
            images
                .iter()
                .find(|image| image.name == ".host")
                .and_then(|image| image.usage.as_deref()),
            None
        );
    }

    #[test]
    fn list_images_rejects_non_json_output() {
        let error = parse_list_images_json(b"ubuntu directory no 20M\n").unwrap_err();

        assert!(error
            .to_string()
            .contains("failed to parse machinectl list-images JSON output"));
    }

    #[tokio::test]
    async fn automatic_snapshot_runs_only_the_noninteractive_image_query() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut runner = MockCommandRunner::new();
            runner
                .expect_run_bounded()
                .times(1)
                .withf(|program, args, timeout| {
                    program == "machinectl"
                        && args
                            == &[
                                "--no-ask-password".to_string(),
                                "--output=json".to_string(),
                                "--no-pager".to_string(),
                                "--all".to_string(),
                                "--".to_string(),
                                "list-images".to_string(),
                            ]
                        && *timeout == HOST_QUERY_TIMEOUT
                })
                .returning(|_, _, _| {
                    Ok(mock_output(
                        true,
                        r#"[{"name":"active","type":"directory","ro":false,"usage":20971520}]"#,
                        "",
                    ))
                });
            runner
        });
        let runtime = tempfile::tempdir().unwrap();
        std::fs::write(
            runtime.path().join("active"),
            "NAME=active\nCLASS=container\nSERVICE=systemd-nspawn\n",
        )
        .unwrap();
        let provider =
            CliBackend::with_runner(runner).with_runtime_machines_dir(runtime.path().to_path_buf());

        let snapshot = RuntimeSource::snapshot(&provider).await.unwrap();

        assert_eq!(snapshot.machines.len(), 1);
        assert_eq!(snapshot.machines[0].name, "active");
        assert_eq!(snapshot.images.len(), 1);
        assert_eq!(snapshot.images[0].name, "active");
    }

    // get_properties

    #[tokio::test]
    async fn test_get_properties_parses_machinectl_and_systemctl_output() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run_bounded()
                .withf(|program, args, timeout| {
                    matches!(program, "machinectl" | "systemctl")
                        && args.first().is_some_and(|arg| arg == "--no-ask-password")
                        && args.get(1).is_some_and(|arg| arg == "--")
                        && args.get(2).is_some_and(|arg| arg == "show")
                        && *timeout == HOST_QUERY_TIMEOUT
                })
                .returning(|program, _args, _timeout| {
                    if program == "systemctl" {
                        Ok(mock_output(
                            true,
                            "ActiveState=active\nLoadState=loaded\n",
                            "",
                        ))
                    } else {
                        Ok(mock_output(true, "State=running\nLeader=12345\n", ""))
                    }
                });
            r
        });
        let provider = CliBackend::with_runner(runner);

        let props = RuntimeSource::get_properties(&provider, "test-ctr", true)
            .await
            .unwrap();

        assert_eq!(props.source, InspectionSource::Cli);
        assert_eq!(props.completeness, InspectionCompleteness::Full);
        assert!(props.groups.iter().any(|g| g.name == "Machine"));
        assert!(props.groups.iter().any(|g| g.name == "Systemd"));
    }

    #[tokio::test]
    async fn test_get_properties_empty_when_no_output() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            let out1 = mock_output(false, "", "");
            let out2 = mock_output(false, "", "");
            r.expect_run_bounded()
                .returning(move |_, _, _| Ok(out1.clone()));
            r.expect_run_bounded()
                .returning(move |_, _, _| Ok(out2.clone()));
            r
        });
        let provider = CliBackend::with_runner(runner);

        let result = RuntimeSource::get_properties(&provider, "missing-ctr", true).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn machine_inspection_preserves_both_cli_failure_reasons() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .times(1)
            .withf(|program, _, _| program == "machinectl")
            .returning(|_, _, _| {
                Ok(mock_output(
                    false,
                    "",
                    "Could not get path to machine: No machine 'missing-ctr' known\n",
                ))
            });
        runner
            .expect_run_bounded()
            .times(1)
            .withf(|program, _, _| program == "systemctl")
            .returning(|_, _, _| {
                Ok(mock_output(
                    true,
                    "Id=systemd-nspawn@missing-ctr.service\nLoadState=not-found\nLoadError=org.freedesktop.systemd1.NoSuchUnit \"Unit missing\"\n",
                    "",
                ))
            });

        let error = get_properties_with_runner("missing-ctr", true, &runner)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("No machine 'missing-ctr' known"));
        assert!(error.contains("LoadState=not-found"));
        assert!(error.contains("org.freedesktop.systemd1.NoSuchUnit"));
        assert!(!error.contains("The target machine might not exist"));
    }

    #[tokio::test]
    async fn machine_inspection_keeps_machine_properties_when_unit_is_not_found() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .times(1)
            .withf(|program, _, _| program == "machinectl")
            .returning(|_, _, _| Ok(mock_output(true, "State=running\nLeader=12345\n", "")));
        runner
            .expect_run_bounded()
            .times(1)
            .withf(|program, _, _| program == "systemctl")
            .returning(|_, _, _| {
                Ok(mock_output(
                    true,
                    "Id=systemd-nspawn@test-ctr.service\nLoadState=not-found\n",
                    "",
                ))
            });

        let properties = get_properties_with_runner("test-ctr", true, &runner)
            .await
            .unwrap();

        assert_eq!(
            properties
                .get_group(crate::domain::inspection::GROUP_MACHINE)
                .and_then(|group| group.get("State"))
                .map(String::as_str),
            Some("running")
        );
        assert!(properties
            .get_group(crate::domain::inspection::GROUP_SYSTEMD_UNIT)
            .is_none());
    }

    #[tokio::test]
    async fn foreign_machine_inspection_never_queries_an_nspawn_unit() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .times(1)
            .withf(|program, _, _| program == "machinectl")
            .returning(|_, _, _| {
                Ok(mock_output(
                    true,
                    "Name=guest-vm\nClass=vm\nService=systemd-vmspawn\nState=running\n",
                    "",
                ))
            });

        let properties = get_properties_with_runner("guest-vm", false, &runner)
            .await
            .unwrap();

        assert_eq!(
            properties
                .get_group(crate::domain::inspection::GROUP_MACHINE)
                .and_then(|group| group.get("Service"))
                .map(String::as_str),
            Some("systemd-vmspawn")
        );
        assert!(properties
            .get_group(crate::domain::inspection::GROUP_SYSTEMD_UNIT)
            .is_none());
    }

    #[tokio::test]
    async fn image_unit_inspection_runs_only_systemctl() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .times(1)
            .withf(|program, args, timeout| {
                program == "systemctl"
                    && args
                        == &[
                            "--no-ask-password".to_string(),
                            "--".to_string(),
                            "show".to_string(),
                            "systemd-nspawn@test-image.service".to_string(),
                        ]
                    && *timeout == HOST_QUERY_TIMEOUT
            })
            .returning(|_, _, _| {
                Ok(mock_output(
                    true,
                    "ActiveState=inactive\nLoadState=loaded\n",
                    "",
                ))
            });

        let properties = get_image_unit_properties_with_runner("test-image", &runner)
            .await
            .unwrap()
            .expect("valid machine name has a unit");

        let systemd = properties.get_group("Systemd").unwrap();
        assert_eq!(
            systemd.get("ActiveState").map(String::as_str),
            Some("inactive")
        );
        assert_eq!(systemd.get("LoadState").map(String::as_str), Some("loaded"));
    }

    #[tokio::test]
    async fn image_unit_inspection_rejects_systemctl_not_found_state() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .times(1)
            .withf(|program, _, _| program == "systemctl")
            .returning(|_, _, _| {
                Ok(mock_output(
                    true,
                    "Id=systemd-nspawn@test-image.service\nLoadState=not-found\nLoadError=org.freedesktop.systemd1.NoSuchUnit \"Unit missing\"\n",
                    "",
                ))
            });

        let error = get_image_unit_properties_with_runner("test-image", &runner)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("LoadState=not-found"));
        assert!(error.contains("org.freedesktop.systemd1.NoSuchUnit"));
        assert!(error.contains("systemd-nspawn@test-image.service"));
    }

    #[tokio::test]
    async fn image_unit_inspection_preserves_stdout_only_systemctl_failure() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .times(1)
            .returning(|_, _, _| Ok(mock_output(false, "inactive\n", "")));

        let error = get_image_unit_properties_with_runner("test-image", &runner)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("inactive"));
    }

    #[tokio::test]
    async fn image_unit_inspection_skips_non_machine_image_names() {
        let mut runner = MockCommandRunner::new();
        runner.expect_run().never();

        let properties = get_image_unit_properties_with_runner("Ubuntu Resolute 镜像", &runner)
            .await
            .unwrap();

        assert!(properties.is_none());
    }
}
