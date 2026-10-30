//! Read-only runtime registration and machinectl/systemctl property queries.
//!
//! Shared by local reads, daemon dispatch, and lifecycle enrichment. Values
//! printed by systemd tools are classified without formatting them twice.

use super::HOST_QUERY_TIMEOUT;
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::process::CommandRunner;
use crate::domain::inspection::{InspectionCompleteness, InspectionSource, MachineProperties};
use crate::domain::machine::MachineName;
use std::io;

/// Resolve the running instance through the same bounded machined runtime
/// state reader used by the systemd-tools backend. This avoids introducing a
/// hidden D-Bus call when `--systemd-tools` selected the route.
pub(crate) async fn machine_leader(machine: &MachineName) -> io::Result<u32> {
    let machine = machine.clone();
    tokio::task::spawn_blocking(move || {
        crate::adapters::runtime::registration::leader_pid(&machine)
    })
    .await
    .map_err(|error| io::Error::other(format!("machine leader task failed: {error}")))?
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UnitInspection {
    Present,
    NotFound(String),
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
    use crate::adapters::runtime::source::RuntimeSource;
    use crate::adapters::runtime::systemd_tools::SystemdToolsBackend;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Output;

    fn mock_output(status: bool, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(if status { 0 } else { 256 }),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

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
