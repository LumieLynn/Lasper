use crate::nspawn::adapters::comm::backend::ContainerBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{
    ContainerEntry, ImageEntry, InspectionCompleteness, InspectionSource, MachineName,
    MachineProperties, RuntimeSnapshot, StatusUpdate,
};
use crate::nspawn::ops::SystemOperationStore;
use crate::nspawn::sys::CommandRunner;
use serde::Deserialize;
use std::time::Duration;

const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(5);

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
                .map(crate::nspawn::adapters::comm::formatting::format_size),
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
    system_operations: SystemOperationStore,
    runtime_machines_dir: std::path::PathBuf,
    nudge_rx: std::sync::Arc<parking_lot::Mutex<Option<tokio::sync::watch::Receiver<()>>>>,
}

impl CliBackend {
    #[cfg(test)]
    pub fn new(runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        let system_operations = SystemOperationStore::new(runner.clone(), None);
        Self::with_system_operations(runner, system_operations)
    }

    pub fn with_system_operations(
        runner: std::sync::Arc<dyn CommandRunner>,
        system_operations: SystemOperationStore,
    ) -> Self {
        Self {
            cmd_runner: runner,
            system_operations,
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

#[async_trait::async_trait]
impl ContainerBackend for CliBackend {
    async fn is_available(&self) -> bool {
        which::which("machinectl").is_ok()
    }

    async fn list_machines(&self) -> Result<Vec<ContainerEntry>> {
        crate::nspawn::adapters::comm::runtime_state::list_machines_at(
            self.runtime_machines_dir.clone(),
        )
        .await
    }

    async fn list_images(&self) -> Result<Vec<ImageEntry>> {
        let out = self
            .cmd_runner
            .run(
                "machinectl",
                vec![
                    "--no-ask-password".to_string(),
                    "--output=json".to_string(),
                    "--no-pager".to_string(),
                    "--all".to_string(),
                    "--".to_string(),
                    "list-images".to_string(),
                ],
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

    async fn start(&self, name: &str) -> Result<()> {
        self.system_operations.start(name).await
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        self.system_operations.terminate(name).await
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        self.system_operations.poweroff(name).await
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.system_operations.reboot(name).await
    }

    async fn enable(&self, name: &str) -> Result<()> {
        self.system_operations.enable(name).await
    }

    async fn disable(&self, name: &str) -> Result<()> {
        self.system_operations.disable(name).await
    }

    async fn kill(&self, name: &str, signal: crate::nspawn::models::AllowedSignal) -> Result<()> {
        self.system_operations.kill(name, signal).await
    }

    async fn remove(&self, name: &str) -> Result<()> {
        self.system_operations.remove_image(name).await
    }

    async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        get_properties_with_runner(name, self.cmd_runner.as_ref()).await
    }

    async fn reload_daemon(&self) -> Result<()> {
        self.system_operations.reload_daemon().await
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
                self.snapshot().await,
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
    cmd_runner: &dyn CommandRunner,
) -> Result<MachineProperties> {
    let name = parse_machine_name(name)?;
    let mut props =
        MachineProperties::from_inspection(InspectionSource::Cli, InspectionCompleteness::Full);

    let machine_out = cmd_runner
        .run(
            "machinectl",
            vec![
                "--no-ask-password".to_string(),
                "--".to_string(),
                "show".to_string(),
                name.as_str().to_string(),
            ],
        )
        .await;

    if let Ok(out) = machine_out {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some((k, v)) = line.split_once('=') {
                    let key = k.trim();
                    let val = v.trim();
                    let formatted = crate::nspawn::adapters::comm::formatting::format_property(
                        key,
                        &zbus::zvariant::Value::Str(val.into()),
                    );
                    props.insert(
                        crate::nspawn::models::GROUP_MACHINE,
                        key.to_string(),
                        formatted,
                    );
                }
            }
        }
    }

    let system_out = cmd_runner
        .run(
            "systemctl",
            vec![
                "--no-ask-password".to_string(),
                "--".to_string(),
                "show".to_string(),
                name.systemd_nspawn_unit(),
            ],
        )
        .await;

    if let Ok(out) = system_out {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some((k, v)) = line.split_once('=') {
                    let key = k.trim();
                    let val = v.trim();
                    let formatted = crate::nspawn::adapters::comm::formatting::format_property(
                        key,
                        &zbus::zvariant::Value::Str(val.into()),
                    );
                    crate::nspawn::adapters::comm::formatting::insert_systemd_property(
                        &mut props,
                        key.to_string(),
                        formatted,
                    );
                }
            }
        }
    }

    if props.groups.is_empty() {
        return Err(NspawnError::CommandFailed(
            format!("machinectl/systemctl show {}", name.as_str()),
            "No properties found".to_string(),
            "The target machine might not exist or systemd-nspawn is not managing it.".to_string(),
        ));
    }

    Ok(props)
}

fn parse_machine_name(name: &str) -> Result<MachineName> {
    MachineName::new(name).map_err(|error| NspawnError::Validation(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::models::ContainerState;
    use crate::nspawn::sys::command::MockCommandRunner;
    use std::os::unix::process::ExitStatusExt;
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
            vec![ContainerEntry {
                name: "active".into(),
                state: ContainerState::Running,
                address: None,
                all_addresses: vec![],
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
        std::fs::write(runtime.path().join("active"), "NAME=active\n").unwrap();
        let provider =
            CliBackend::with_runner(runner).with_runtime_machines_dir(runtime.path().to_path_buf());

        let machines = provider.list_machines().await.unwrap();

        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].name, "active");
        assert_eq!(machines[0].state, ContainerState::Running);
        assert!(machines[0].all_addresses.is_empty());
    }

    #[tokio::test]
    async fn list_images_preserves_systemd_image_names() {
        let runner = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run()
                .withf(|program, args| {
                    program == "machinectl"
                        && args.first().is_some_and(|arg| arg == "--no-ask-password")
                        && args.get(1).is_some_and(|arg| arg == "--output=json")
                        && args.get(4).is_some_and(|arg| arg == "--")
                        && args.get(5).is_some_and(|arg| arg == "list-images")
                })
                .returning(|_, _| {
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

        let images = provider.list_images().await.unwrap();

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
                .expect_run()
                .times(1)
                .withf(|program, args| {
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
                })
                .returning(|_, _| {
                    Ok(mock_output(
                        true,
                        r#"[{"name":"active","type":"directory","ro":false,"usage":20971520}]"#,
                        "",
                    ))
                });
            runner
        });
        let runtime = tempfile::tempdir().unwrap();
        std::fs::write(runtime.path().join("active"), "NAME=active\n").unwrap();
        let provider =
            CliBackend::with_runner(runner).with_runtime_machines_dir(runtime.path().to_path_buf());

        let snapshot = provider.snapshot().await.unwrap();

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
            r.expect_run()
                .withf(|program, args| {
                    matches!(program, "machinectl" | "systemctl")
                        && args.first().is_some_and(|arg| arg == "--no-ask-password")
                        && args.get(1).is_some_and(|arg| arg == "--")
                        && args.get(2).is_some_and(|arg| arg == "show")
                })
                .returning(|program, _args| {
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

        let props = provider.get_properties("test-ctr").await.unwrap();

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
            r.expect_run().returning(move |_, _| Ok(out1.clone()));
            r.expect_run().returning(move |_, _| Ok(out2.clone()));
            r
        });
        let provider = CliBackend::with_runner(runner);

        let result = provider.get_properties("missing-ctr").await;
        assert!(result.is_err());
    }

    // action methods

    #[tokio::test]
    async fn test_start_calls_machinectl_start() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            let out = mock_output(true, "", "");
            r.expect_run().returning(move |_, _| Ok(out.clone()));
            r
        });
        let provider = CliBackend::with_runner(runner);

        provider.start("my-ctr").await.unwrap();
    }

    #[tokio::test]
    async fn test_start_rejects_invalid_machine_name_before_machinectl() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run().never();
            r
        });
        let provider = CliBackend::with_runner(runner);

        let result = provider.start("../escape").await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }

    #[tokio::test]
    async fn test_kill_calls_machinectl_kill_with_signal() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            let out = mock_output(true, "", "");
            r.expect_run()
                .withf(|program, args| {
                    program == "machinectl"
                        && args
                            == &[
                                "-s".to_string(),
                                "SIGTERM".to_string(),
                                "--".to_string(),
                                "kill".to_string(),
                                "my-ctr".to_string(),
                            ]
                })
                .returning(move |_, _| Ok(out.clone()));
            r
        });
        let provider = CliBackend::with_runner(runner);

        provider
            .kill("my-ctr", crate::nspawn::models::AllowedSignal::Terminate)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_clone_rejects_invalid_image_name_before_machinectl() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run().never();
            r
        });
        let provider = CliBackend::with_runner(runner);

        let result = provider
            .system_operations
            .clone_image("source", "bad/name")
            .await;

        assert!(matches!(result, Err(NspawnError::Validation(_))));
    }
}
