use crate::adapters::elevated::ElevatedDaemon;
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::CommandRunner;
use crate::adapters::runtime::source::RuntimeSource;
use crate::adapters::session::terminal_attach::TerminalAttachCommand;
use crate::adapters::session::{MachineSessionRequest, MachineShellRequest};
use crate::application::operations::ExecutionRoute;
use crate::domain::inspection::{InspectionCompleteness, InspectionSource, MachineProperties};
use crate::domain::machine::MachineName;
use crate::domain::runtime::{ImageEntry, MachineEntry};
use serde::Deserialize;
use std::io;
use std::time::Duration;

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
            usage: row.usage.map(ImageEntry::format_usage),
            dbus_object_path: None,
        })
        .collect::<Vec<_>>();
    images.sort();
    Ok(images)
}

#[derive(Clone)]
pub struct SystemdToolsBackend {
    cmd_runner: std::sync::Arc<dyn CommandRunner>,
    property_query_route: SystemdToolsPropertyQueryRoute,
    runtime_machines_dir: std::path::PathBuf,
}

#[derive(Clone)]
enum SystemdToolsPropertyQueryRoute {
    Local(std::sync::Arc<dyn CommandRunner>),
    Elevated(std::sync::Arc<ElevatedDaemon>),
}

impl SystemdToolsPropertyQueryRoute {
    fn route(&self) -> ExecutionRoute {
        match self {
            Self::Local(_) => ExecutionRoute::LocalSystemdTools,
            Self::Elevated(_) => ExecutionRoute::ElevatedSystemdTools,
        }
    }
}

impl SystemdToolsBackend {
    pub fn new(runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        Self {
            cmd_runner: std::sync::Arc::clone(&runner),
            property_query_route: SystemdToolsPropertyQueryRoute::Local(runner),
            runtime_machines_dir: crate::paths::runtime_machines_dir(),
        }
    }

    pub(crate) fn elevated(
        runner: std::sync::Arc<dyn CommandRunner>,
        daemon: std::sync::Arc<ElevatedDaemon>,
    ) -> Self {
        Self {
            cmd_runner: runner,
            property_query_route: SystemdToolsPropertyQueryRoute::Elevated(daemon),
            runtime_machines_dir: crate::paths::runtime_machines_dir(),
        }
    }

    pub(crate) fn property_query_route(&self) -> ExecutionRoute {
        self.property_query_route.route()
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
/// the request semantics, while this module owns the systemd tools command shape.
pub(crate) fn machine_session_command(
    request: MachineSessionRequest,
) -> io::Result<TerminalAttachCommand> {
    match request {
        MachineSessionRequest::Shell(request) => selected_user_shell(request),
        MachineSessionRequest::LoginPrompt(request) => Ok(
            crate::adapters::session::terminal_attach::login(request.machine()),
        ),
        MachineSessionRequest::WaylandProbe(request) => wayland_probe(request),
        MachineSessionRequest::X11ProjectionProbe(request) => x11_projection_probe(request),
    }
}

/// Resolve the running instance through the same bounded machined runtime
/// state reader used by the systemd-tools backend. This avoids introducing a
/// hidden D-Bus call when `--systemd-tools` selected the route.
pub(crate) async fn machine_leader(machine: &MachineName) -> io::Result<u32> {
    let machine = machine.clone();
    tokio::task::spawn_blocking(move || crate::adapters::runtime::state::leader_pid(&machine))
        .await
        .map_err(|error| io::Error::other(format!("machine leader task failed: {error}")))?
}

fn selected_user_shell(request: MachineShellRequest) -> io::Result<TerminalAttachCommand> {
    let terminal = request.environment().terminal_environment().clone();
    let mut args = vec!["--quiet".to_string()];
    args.extend(
        request
            .environment()
            .assignments()
            .into_iter()
            .map(|assignment| format!("--setenv={assignment}")),
    );
    args.extend([
        "--".to_string(),
        "shell".to_string(),
        format!("{}@{}", request.user(), request.machine()),
    ]);
    if let Some(command) = request.command() {
        args.extend(command.argv());
    }
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

fn x11_projection_probe(
    request: crate::adapters::session::X11ProjectionProbeRequest,
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
impl RuntimeSource for SystemdToolsBackend {
    async fn get_unit_properties(&self, machine: &MachineName) -> Result<MachineProperties> {
        match &self.property_query_route {
            SystemdToolsPropertyQueryRoute::Local(runner) => {
                get_unit_properties_with_runner(machine, runner.as_ref()).await
            }
            SystemdToolsPropertyQueryRoute::Elevated(daemon) => daemon
                .systemd_tools_inspect_unit(machine)
                .await
                .map_err(|error| {
                    NspawnError::Io(std::path::PathBuf::from("elevated unit inspection"), error)
                }),
        }
    }

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
        match &self.property_query_route {
            SystemdToolsPropertyQueryRoute::Local(runner) => {
                get_properties_with_runner(name, include_nspawn_unit, runner.as_ref()).await
            }
            SystemdToolsPropertyQueryRoute::Elevated(daemon) => daemon
                .systemd_tools_inspect_machine(name, include_nspawn_unit)
                .await
                .map_err(|error| {
                    NspawnError::Io(
                        std::path::PathBuf::from("elevated systemd tools inspection"),
                        error,
                    )
                }),
        }
    }
}

/// Fixed, non-interactive systemd tools inspection shared with the elevated daemon.
pub(crate) async fn get_properties_with_runner(
    name: &str,
    include_nspawn_unit: bool,
    cmd_runner: &dyn CommandRunner,
) -> Result<MachineProperties> {
    let name = parse_machine_name(name)?;
    let mut props = MachineProperties::from_inspection(
        InspectionSource::SystemdTools,
        InspectionCompleteness::Full,
    );

    let machine_args = vec![
        "--no-ask-password".to_string(),
        "--all".to_string(),
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
            "partial systemd tools inspection for {}: {}",
            name,
            failures.join("; ")
        );
    }

    Ok(props)
}

/// Inspect only the systemd-nspawn unit associated with an image.
///
/// The catalog validates image names before reaching this backend; query
/// failures (including an absent unit) remain errors.
pub(crate) async fn get_unit_properties_with_runner(
    name: &MachineName,
    cmd_runner: &dyn CommandRunner,
) -> Result<MachineProperties> {
    let mut props = MachineProperties::from_inspection(
        InspectionSource::SystemdTools,
        InspectionCompleteness::Full,
    );
    match append_systemd_unit_properties(name, cmd_runner, &mut props).await? {
        UnitInspection::Present => Ok(props),
        UnitInspection::NotFound(diagnostic) => {
            let unit = name.systemd_nspawn_unit();
            Err(NspawnError::CommandFailed(
                format!("systemctl show {unit}"),
                format!("systemctl --no-ask-password --all -- show {unit}"),
                diagnostic,
            ))
        }
    }
}

fn insert_machine_properties(props: &mut MachineProperties, output: &[u8]) -> usize {
    let mut inserted = 0usize;
    for (key, val) in crate::adapters::runtime::systemd_properties::tool_properties(output) {
        // `machinectl show` already ran systemd's property printer. Running
        // the text through the D-Bus formatter would reinterpret values such
        // as byte counts and durations a second time, producing a different
        // representation from the command-line backend.
        props.insert(crate::domain::inspection::GROUP_MACHINE, key, val);
        inserted = inserted.saturating_add(1);
    }
    inserted
}

async fn append_systemd_unit_properties(
    name: &MachineName,
    cmd_runner: &dyn CommandRunner,
    props: &mut MachineProperties,
) -> Result<UnitInspection> {
    let unit = name.systemd_nspawn_unit();
    let args = vec![
        "--no-ask-password".to_string(),
        "--all".to_string(),
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

    let properties =
        crate::adapters::runtime::systemd_properties::tool_properties(&system_out.stdout);
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
        // `systemctl show` has already applied systemd's canonical formatter;
        // retain its text verbatim and only classify the property group.
        crate::adapters::runtime::systemd_properties::append_tool_property(props, key, val);
    }

    Ok(UnitInspection::Present)
}

/// Enrich a runtime-state snapshot with the nspawn unit properties needed for
/// lifecycle confirmation. The machine registration reader intentionally does
/// not invoke systemctl; callers opt into this small second query when they
/// need ActiveState/Result.
pub(crate) async fn append_nspawn_unit_properties_with_runner(
    name: &MachineName,
    cmd_runner: &dyn CommandRunner,
    props: &mut MachineProperties,
) -> Result<bool> {
    match append_systemd_unit_properties(name, cmd_runner, props).await? {
        UnitInspection::Present => Ok(true),
        UnitInspection::NotFound(_) => Ok(false),
    }
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
        X11ProjectionProbeRequest,
    };
    use crate::application::sessions::{
        GuestCommand, InteractiveShellEnvironment, ValidatedGuestUserName,
    };
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
            [
                "--quiet",
                "--setenv=TERM=dumb",
                "--",
                "shell",
                "1000@test-machine"
            ]
        );
    }

    #[test]
    fn login_prompt_uses_machinectl_login_without_shell_arguments() {
        let command = machine_session_command(MachineSessionRequest::login_prompt(
            MachineName::new("test-machine").unwrap(),
        ))
        .unwrap();

        assert_eq!(command.program(), "machinectl");
        assert_eq!(command.args(), ["--", "login", "test-machine"]);
    }

    #[test]
    fn selected_user_shell_appends_guest_command_argv_without_requoting() {
        let name = MachineName::new("test-machine").unwrap();
        let user = ValidatedGuestUserName::new("alice").unwrap();
        let request = MachineSessionRequest::shell(
            MachineShellRequest::new(name, user, MachineShellEnvironment::default()).with_command(
                GuestCommand::new(
                    "/usr/bin/kitty",
                    vec!["--class".into(), "a b".into(), "".into()],
                )
                .unwrap(),
            ),
        );

        let command = machine_session_command(request).unwrap();

        assert_eq!(
            command.args(),
            [
                "--quiet",
                "--setenv=TERM=dumb",
                "--",
                "shell",
                "alice@test-machine",
                "/usr/bin/kitty",
                "--class",
                "a b",
                "",
            ]
        );
    }

    #[test]
    fn selected_user_shell_forwards_the_closed_typed_environment() {
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
            Some(1),
        )
        .unwrap();
        let request =
            MachineSessionRequest::shell(MachineShellRequest::new(name, user, environment));

        let command = machine_session_command(request).unwrap();

        assert_eq!(command.program(), "machinectl");
        assert_eq!(
            command.args(),
            [
                "--quiet",
                "--setenv=TERM=xterm-kitty",
                "--setenv=COLORTERM=truecolor",
                "--setenv=NO_COLOR=",
                "--setenv=WAYLAND_DISPLAY=/run/lasper/wayland/1000/wayland-1",
                "--setenv=DISPLAY=:1",
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
        assert_eq!(command.get_env("DISPLAY"), None);
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
    fn machinectl_x11_probe_preserves_both_validated_paths_as_arguments() {
        let request = X11ProjectionProbeRequest::new(
            MachineName::new("test-machine").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
            Path::new("/mnt/x11/X0"),
            Path::new("/tmp/.X11-unix/X0"),
        )
        .unwrap();
        let command =
            machine_session_command(MachineSessionRequest::x11_projection_probe(request)).unwrap();
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
            &command.args()[command.args().len() - 2..],
            ["/mnt/x11/X0", "/tmp/.X11-unix/X0"]
        );
        let command = command.into_pty_command().unwrap();
        assert_eq!(command.get_env("TERM"), Some(std::ffi::OsStr::new("dumb")));
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
        let provider = SystemdToolsBackend::with_runner(runner)
            .with_runtime_machines_dir(runtime.path().to_path_buf());

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
        let provider = SystemdToolsBackend::with_runner(runner);

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
        let provider = SystemdToolsBackend::with_runner(runner)
            .with_runtime_machines_dir(runtime.path().to_path_buf());

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
                        && args.get(1).is_some_and(|arg| arg == "--all")
                        && args.get(2).is_some_and(|arg| arg == "--")
                        && args.get(3).is_some_and(|arg| arg == "show")
                        && *timeout == HOST_QUERY_TIMEOUT
                })
                .returning(|program, _args, _timeout| {
                    if program == "systemctl" {
                        Ok(mock_output(
                            true,
                            "ActiveState=active\nLoadState=loaded\nDescription=  preserve spaces  \nType=simple\nMainPID=123\nEnvironment=\nDeviceAllow=char-rtc r\nDeviceAllow=char-* rw\n",
                            "",
                        ))
                    } else {
                        Ok(mock_output(true, "State=running\nLeader=12345\n", ""))
                    }
                });
            r
        });
        let provider = SystemdToolsBackend::with_runner(runner);

        let props = RuntimeSource::get_properties(&provider, "test-ctr", true)
            .await
            .unwrap();

        assert_eq!(props.source, InspectionSource::SystemdTools);
        assert_eq!(props.completeness, InspectionCompleteness::Full);
        assert_eq!(
            props.get_display(crate::domain::inspection::GROUP_SYSTEMD_UNIT, "Description"),
            Some("  preserve spaces  ")
        );
        assert_eq!(
            props.get_display(
                crate::domain::inspection::GROUP_SYSTEMD_SERVICE,
                "Environment"
            ),
            Some("")
        );
        assert_eq!(
            props.get_display(
                crate::domain::inspection::GROUP_SYSTEMD_SERVICE,
                "DeviceAllow"
            ),
            Some("char-rtc r char-* rw")
        );
        assert!(props.groups.iter().any(|g| g.name == "Machine"));
        assert!(props
            .groups
            .iter()
            .any(|g| g.name == crate::domain::inspection::GROUP_SYSTEMD_UNIT));
        assert_eq!(
            props
                .get_group(crate::domain::inspection::GROUP_SYSTEMD_SERVICE)
                .and_then(|group| group.get("Type"))
                .map(|entry| entry.display.as_str()),
            Some("simple")
        );
        assert_eq!(
            props
                .get_group(crate::domain::inspection::GROUP_SYSTEMD_SERVICE)
                .and_then(|group| group.get("MainPID"))
                .map(|entry| entry.display.as_str()),
            Some("123")
        );
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
        let provider = SystemdToolsBackend::with_runner(runner);

        let result = RuntimeSource::get_properties(&provider, "missing-ctr", true).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn runtime_state_enrichment_reads_the_nspawn_unit_properties() {
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run_bounded()
            .times(1)
            .withf(|program, args, _| {
                program == "systemctl"
                    && *args
                        == [
                            "--no-ask-password",
                            "--all",
                            "--",
                            "show",
                            "systemd-nspawn@test-machine.service",
                        ]
                        .map(String::from)
            })
            .returning(|_, _, _| {
                Ok(mock_output(
                    true,
                    "ActiveState=active\nResult=success\n",
                    "",
                ))
            });

        let mut properties = MachineProperties::from_inspection(
            InspectionSource::RuntimeState,
            InspectionCompleteness::RuntimeOnly,
        );
        let name = MachineName::new("test-machine").unwrap();
        let present = append_nspawn_unit_properties_with_runner(&name, &runner, &mut properties)
            .await
            .unwrap();

        assert!(present);
        assert_eq!(
            properties
                .get_group(crate::domain::inspection::GROUP_SYSTEMD_UNIT)
                .unwrap()["ActiveState"]
                .display,
            "active"
        );
    }

    #[tokio::test]
    async fn machine_inspection_preserves_both_systemd_tools_failure_reasons() {
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
                .map(|entry| entry.display.as_str()),
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
                .map(|entry| entry.display.as_str()),
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
                            "--all".to_string(),
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

        let properties =
            get_unit_properties_with_runner(&MachineName::new("test-image").unwrap(), &runner)
                .await
                .unwrap();

        let systemd = properties
            .get_group(crate::domain::inspection::GROUP_SYSTEMD_UNIT)
            .unwrap();
        assert_eq!(
            systemd
                .get("ActiveState")
                .map(|entry| entry.display.as_str()),
            Some("inactive")
        );
        assert_eq!(
            systemd.get("LoadState").map(|entry| entry.display.as_str()),
            Some("loaded")
        );
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

        let error =
            get_unit_properties_with_runner(&MachineName::new("test-image").unwrap(), &runner)
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

        let error =
            get_unit_properties_with_runner(&MachineName::new("test-image").unwrap(), &runner)
                .await
                .unwrap_err()
                .to_string();

        assert!(error.contains("inactive"));
    }
}
