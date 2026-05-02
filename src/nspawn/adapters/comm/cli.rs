use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, ContainerState, MachineProperties};
use crate::nspawn::sys::{CommandRunner, DefaultCommandRunner};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

#[cfg_attr(test, mockall::automock)]
#[async_trait::async_trait]
pub trait CliProvider: Send + Sync + 'static {
    async fn list_all(&self) -> Result<Vec<ContainerEntry>>;
    async fn running_map(&self) -> Result<HashMap<String, Vec<String>>>;
    async fn start(&self, name: &str) -> Result<()>;
    async fn terminate(&self, name: &str) -> Result<()>;
    async fn poweroff(&self, name: &str) -> Result<()>;
    async fn reboot(&self, name: &str) -> Result<()>;
    async fn enable(&self, name: &str) -> Result<()>;
    async fn disable(&self, name: &str) -> Result<()>;
    async fn kill(&self, name: &str, signal: &str) -> Result<()>;
    async fn remove(&self, name: &str) -> Result<()>;
    fn spawn_log_stream(
        &self,
        name: &str,
        tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    ) -> tokio::task::JoinHandle<()>;
    async fn get_properties(&self, name: &str) -> Result<MachineProperties>;
}

#[derive(Clone)]
pub struct DefaultCliProvider {
    is_root: bool,
    cmd_runner: std::sync::Arc<dyn CommandRunner>,
}

impl DefaultCliProvider {
    pub fn new(is_root: bool) -> Self {
        Self {
            is_root,
            cmd_runner: std::sync::Arc::new(DefaultCommandRunner),
        }
    }

    #[cfg(test)]
    fn with_runner(is_root: bool, runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        Self {
            is_root,
            cmd_runner: runner,
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
}

#[async_trait::async_trait]
impl CliProvider for DefaultCliProvider {
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

    async fn running_map(&self) -> Result<HashMap<String, Vec<String>>> {
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

    fn spawn_log_stream(
        &self,
        name: &str,
        tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    ) -> tokio::task::JoinHandle<()> {
        let name = name.to_string();
        tokio::spawn(async move {
            let res: std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
                let mut child = tokio::process::Command::new("journalctl")
                    .args(["-M", &name, "-n", "1000", "-f", "--no-pager", "--output=short"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;

                let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();

                loop {
                    tokio::select! {
                        line_res = lines.next_line() => {
                            if let Ok(Some(line)) = line_res {
                                tx.send(crate::events::AppEvent::LogLine(line)).await.map_err(|_| "Channel closed")?;
                            } else {
                                break;
                            }
                        }
                        _ = child.wait() => break,
                    }
                }
                Ok(())
            }
            .await;

            if let Err(e) = res {
                tx.send(crate::events::AppEvent::LogLine(format!(
                    "Log stream stopped: {e}"
                )))
                .await
                .ok();
            }
        })
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
                        props.insert(crate::nspawn::models::GROUP_MACHINE, key.to_string(), formatted);
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
                        if matches!(
                            key,
                            "After"
                                | "Before"
                                | "Wants"
                                | "WantedBy"
                                | "Requires"
                                | "RequiredBy"
                                | "Conflicts"
                                | "ConflictedBy"
                        ) {
                            if !formatted.is_empty() && formatted != "[]" {
                                props.insert(crate::nspawn::models::GROUP_DEPENDENCIES, key.to_string(), formatted);
                            }
                        } else {
                            props.insert(crate::nspawn::models::GROUP_SYSTEMD_UNIT, key.to_string(), formatted);
                        }
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

        let provider = DefaultCliProvider::with_runner(true, runner);
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

        let provider = DefaultCliProvider::with_runner(true, runner);
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

        let provider = DefaultCliProvider::with_runner(true, runner);
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
        let provider = DefaultCliProvider::with_runner(true, runner);

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
        let provider = DefaultCliProvider::with_runner(false, runner);

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
        let provider = DefaultCliProvider::with_runner(true, runner);

        let props = provider.get_properties("test-ctr").await.unwrap();

        assert!(props.groups.iter().any(|g| g.name == "Machine"));
        assert!(props.groups.iter().any(|g| g.name == "Systemd Unit"));
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
        let provider = DefaultCliProvider::with_runner(true, runner);

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
        let provider = DefaultCliProvider::with_runner(true, runner);

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
        let provider = DefaultCliProvider::with_runner(true, runner);

        provider.kill("my-ctr", "SIGTERM").await.unwrap();
    }
}
