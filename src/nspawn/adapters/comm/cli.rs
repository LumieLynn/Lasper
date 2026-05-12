use crate::nspawn::adapters::comm::backend::ContainerBackend;
use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, ContainerState, MachineProperties};
use crate::nspawn::ops::provision::backend::ProvisionBackend;
use crate::nspawn::sys::{CommandRunner, DefaultCommandRunner};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct CliBackend {
    is_root: bool,
    cmd_runner: std::sync::Arc<dyn CommandRunner>,
    nudge_rx: std::sync::Arc<parking_lot::Mutex<Option<tokio::sync::watch::Receiver<()>>>>,
}

impl CliBackend {
    pub fn new(is_root: bool) -> Self {
        Self {
            is_root,
            cmd_runner: std::sync::Arc::new(DefaultCommandRunner),
            nudge_rx: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    pub fn set_nudge(&self, rx: tokio::sync::watch::Receiver<()>) {
        *self.nudge_rx.lock() = Some(rx);
    }

    #[cfg(test)]
    fn with_runner(is_root: bool, runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        Self {
            is_root,
            cmd_runner: runner,
            nudge_rx: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    async fn run_machinectl(&self, args: &[&str]) -> Result<()> {
        let out = self
            .cmd_runner
            .run("machinectl", args.iter().map(|s| s.to_string()).collect())
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        crate::nspawn::sys::log_output("machinectl", &out);

        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "machinectl execution",
                format!("machinectl {}", args.join(" ")),
                &out,
            ));
        }
        Ok(())
    }

    /// Returns a map of running machine names to their IP addresses.
    pub(crate) async fn running_map(&self) -> Result<HashMap<String, Vec<String>>> {
        let out = self
            .cmd_runner
            .run(
                "machinectl",
                vec![
                    "list".to_string(),
                    "-l".to_string(),
                    "--no-legend".to_string(),
                    "--no-pager".to_string(),
                ],
            )
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.is_empty() && !stderr.contains("No machines") {
                return Err(NspawnError::cmd_failed(
                    "machinectl list",
                    "machinectl list -l --no-legend --no-pager",
                    &out,
                ));
            }
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        let mut current_machine = String::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with(|c: char| c.is_whitespace()) {
                let ip = line.trim();
                if !current_machine.is_empty() && !ip.is_empty() {
                    if let Some(ips) = map.get_mut(&current_machine) {
                        ips.push(ip.to_string());
                    }
                }
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                continue;
            }
            current_machine = parts[0].to_string();
            if current_machine == ".host" {
                continue;
            }
            let mut ips = Vec::new();
            if let Some(addr) = parts.get(5).copied() {
                if !addr.is_empty() && addr != "-" {
                    ips.push(addr.to_string());
                }
            }
            map.insert(current_machine.clone(), ips);
        }
        Ok(map)
    }
}

#[async_trait::async_trait]
impl ContainerBackend for CliBackend {
    async fn is_available(&self) -> bool {
        which::which("machinectl").is_ok()
    }

    async fn list_all(&self) -> Result<Vec<ContainerEntry>> {
        let running = self.running_map().await?;

        if !self.is_root {
            return Ok(running
                .into_iter()
                .filter(|(name, _)| name != ".host")
                .map(|(name, addrs)| ContainerEntry {
                    state: ContainerState::Running,
                    name,
                    image_type: None,
                    readonly: false,
                    usage: None,
                    address: addrs.first().cloned().filter(|s| !s.is_empty()),
                    all_addresses: addrs,
                })
                .collect());
        }

        let out = self
            .cmd_runner
            .run(
                "machinectl",
                vec![
                    "list-images".to_string(),
                    "-l".to_string(),
                    "--no-legend".to_string(),
                    "--no-pager".to_string(),
                ],
            )
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "machinectl list-images",
                "machinectl list-images -l --no-legend --no-pager",
                &out,
            ));
        }

        let mut entries: Vec<ContainerEntry> = Vec::new();

        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                continue;
            }
            let name = parts[0].to_string();
            if name == ".host" {
                continue;
            }
            let addrs = running.get(&name).cloned().unwrap_or_default();
            let addr = addrs.first().cloned();
            let state = if running.contains_key(&name) {
                ContainerState::Running
            } else {
                ContainerState::Off
            };

            entries.push(ContainerEntry {
                state,
                name,
                image_type: Some(parts[1].to_string()),
                readonly: parts[2] == "yes",
                usage: parts.get(3).map(|s| s.to_string()),
                address: addr.filter(|s| !s.is_empty()),
                all_addresses: addrs,
            });
        }

        for (name, addrs) in &running {
            if name == ".host" {
                continue;
            }
            if !entries.iter().any(|e| &e.name == name) {
                entries.push(ContainerEntry {
                    name: name.clone(),
                    state: ContainerState::Running,
                    image_type: None,
                    readonly: false,
                    usage: None,
                    address: addrs.first().cloned().filter(|s| !s.is_empty()),
                    all_addresses: addrs.clone(),
                });
            }
        }

        entries.sort();

        Ok(entries)
    }

    async fn start(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["start", name]).await
    }

    async fn terminate(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["terminate", name]).await
    }

    async fn poweroff(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["poweroff", name]).await
    }

    async fn reboot(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["reboot", name]).await
    }

    async fn enable(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["enable", name]).await
    }

    async fn disable(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["disable", name]).await
    }

    async fn kill(&self, name: &str, signal: &str) -> Result<()> {
        self.run_machinectl(&["kill", "-s", signal, name]).await
    }

    async fn remove(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["remove", name]).await
    }

    async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        let mut props = MachineProperties::default();

        let machine_out = self
            .cmd_runner
            .run("machinectl", vec!["show".to_string(), name.to_string()])
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

        let system_out = self
            .cmd_runner
            .run(
                "systemctl",
                vec![
                    "show".to_string(),
                    format!("systemd-nspawn@{}.service", name),
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
                format!("machinectl/systemctl show {}", name),
                "No properties found".to_string(),
                "The target machine might not exist or systemd-nspawn is not managing it."
                    .to_string(),
            ));
        }

        Ok(props)
    }

    async fn reload_daemon(&self) -> Result<()> {
        let out = self
            .cmd_runner
            .run("systemctl", vec!["daemon-reload".to_string()])
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("systemctl"), e))?;

        crate::nspawn::sys::log_output("systemctl", &out);

        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "systemctl daemon-reload",
                "systemctl daemon-reload",
                &out,
            ));
        }
        Ok(())
    }

    async fn watch_events(&self, tx: tokio::sync::mpsc::Sender<()>) -> Result<()> {
        let mut nudge_rx = self.nudge_rx.lock().take().ok_or_else(|| {
            NspawnError::Dbus(zbus::Error::Failure(
                "watch_events: no nudge channel set on CliBackend".into(),
            ))
        })?;

        let mut prev: HashSet<String> = HashSet::new();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(WATCH_POLL_INTERVAL) => {}
                _ = nudge_rx.changed() => {}
            }

            match self.list_all().await {
                Ok(entries) => {
                    let current: HashSet<_> = entries.into_iter().map(|e| e.name).collect();
                    if current != prev {
                        let _ = tx.send(()).await;
                        prev = current;
                    }
                }
                Err(e) => {
                    log::warn!("CLI watch_events: list_all failed: {}", e);
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl ProvisionBackend for CliBackend {
    async fn clone_image(&self, source: &str, dest: &str) -> Result<()> {
        self.run_machinectl(&["clone", source, dest]).await
    }

    async fn reload_daemon(&self) -> Result<()> {
        // Delegates to the same `systemctl daemon-reload` used by the runtime backend.
        ContainerBackend::reload_daemon(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    // running_map

    #[tokio::test]
    async fn test_running_map_parses_list_output() {
        let stdout = "machine1  container systemd-nspawn running -  1.2.3.4\n\
                       machine2  container systemd-nspawn running -  10.0.0.1\n";
        let out = mock_output(true, stdout, "");

        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run().returning(move |_, _| Ok(out.clone()));
            r
        });

        let provider = CliBackend::with_runner(true, runner);
        let map = provider.running_map().await.unwrap();

        assert_eq!(map.len(), 2);
        assert_eq!(map.get("machine1").unwrap(), &vec!["1.2.3.4"]);
        assert_eq!(map.get("machine2").unwrap(), &vec!["10.0.0.1"]);
    }

    #[tokio::test]
    async fn test_running_map_no_machines_returns_empty() {
        let out = mock_output(false, "", "No machines.");

        let runner = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run().returning(move |_, _| Ok(out.clone()));
            r
        });

        let provider = CliBackend::with_runner(true, runner);
        let map = provider.running_map().await.unwrap();
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn test_running_map_skips_host() {
        let stdout = ".host       container systemd-nspawn running -  -\n\
                       my-ctr      container systemd-nspawn running -  192.168.1.1\n";
        let out = mock_output(true, stdout, "");

        let runner = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run().returning(move |_, _| Ok(out.clone()));
            r
        });

        let provider = CliBackend::with_runner(true, runner);
        let map = provider.running_map().await.unwrap();

        assert_eq!(map.len(), 1);
        assert!(map.contains_key("my-ctr"));
        assert!(!map.contains_key(".host"));
    }

    // list_all

    #[tokio::test]
    async fn test_list_all_merges_images_and_running() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run().returning(|_, args| {
                if args.iter().any(|a| a == "list-images") {
                    Ok(mock_output(
                        true,
                        "my-ctr directory no 123456 -\n\
                             other   directory no 789012 -\n",
                        "",
                    ))
                } else {
                    Ok(mock_output(
                        true,
                        "my-ctr container systemd-nspawn running - 10.0.0.1\n",
                        "",
                    ))
                }
            });
            r
        });
        let provider = CliBackend::with_runner(true, runner);

        let entries = provider.list_all().await.unwrap();

        assert_eq!(entries.len(), 2);
        let my_ctr = entries.iter().find(|e| e.name == "my-ctr").unwrap();
        assert_eq!(my_ctr.state, ContainerState::Running);
        assert_eq!(my_ctr.address.as_deref(), Some("10.0.0.1"));

        let other = entries.iter().find(|e| e.name == "other").unwrap();
        assert_eq!(other.state, ContainerState::Off);
        assert!(other.address.is_none());
    }

    #[tokio::test]
    async fn test_list_all_non_root_only_running() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            let out_list = mock_output(
                true,
                "ctr1 container systemd-nspawn running - 10.0.0.1\n",
                "",
            );
            r.expect_run().returning(move |_, _| Ok(out_list.clone()));
            r
        });
        let provider = CliBackend::with_runner(false, runner);

        let entries = provider.list_all().await.unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, ContainerState::Running);
        assert_eq!(entries[0].image_type, None);
    }

    // get_properties

    #[tokio::test]
    async fn test_get_properties_parses_machinectl_and_systemctl_output() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            r.expect_run().returning(|program, _args| {
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
        let provider = CliBackend::with_runner(true, runner);

        let props = provider.get_properties("test-ctr").await.unwrap();

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
        let provider = CliBackend::with_runner(true, runner);

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
        let provider = CliBackend::with_runner(true, runner);

        provider.start("my-ctr").await.unwrap();
    }

    #[tokio::test]
    async fn test_kill_calls_machinectl_kill_with_signal() {
        let runner: std::sync::Arc<dyn CommandRunner> = std::sync::Arc::new({
            let mut r = MockCommandRunner::new();
            let out = mock_output(true, "", "");
            r.expect_run().returning(move |_, _| Ok(out.clone()));
            r
        });
        let provider = CliBackend::with_runner(true, runner);

        provider.kill("my-ctr", "SIGTERM").await.unwrap();
    }
}
