use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, ContainerState, MachineProperties};
use crate::nspawn::sys::{new_command, CommandLogged};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Clone)]
pub struct CliProvider {
    is_root: bool,
}

impl CliProvider {
    pub fn new(is_root: bool) -> Self {
        Self { is_root }
    }

    async fn run_machinectl(&self, args: &[&str]) -> Result<()> {
        let out = new_command("machinectl")
            .args(args)
            .logged_output("machinectl")
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("machinectl"), e))?;

        if !out.status.success() {
            return Err(NspawnError::cmd_failed(
                "machinectl execution",
                format!("machinectl {}", args.join(" ")),
                &out,
            ));
        }
        Ok(())
    }

    pub async fn running_map(&self) -> Result<HashMap<String, Vec<String>>> {
        let out = new_command("machinectl")
            .args(["list", "-l", "--no-legend", "--no-pager"])
            .logged_output("machinectl")
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

    pub async fn list_all(&self) -> Result<Vec<ContainerEntry>> {
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

        let out = new_command("machinectl")
            .args(["list-images", "-l", "--no-legend", "--no-pager"])
            .logged_output("machinectl")
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

    pub async fn start(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["start", name]).await
    }

    pub async fn terminate(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["terminate", name]).await
    }

    pub async fn poweroff(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["poweroff", name]).await
    }

    pub async fn reboot(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["reboot", name]).await
    }

    pub async fn enable(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["enable", name]).await
    }

    pub async fn disable(&self, name: &str) -> Result<()> {
        self.run_machinectl(&["disable", name]).await
    }

    pub async fn kill(&self, name: &str, signal: &str) -> Result<()> {
        self.run_machinectl(&["kill", "-s", signal, name]).await
    }

    pub fn spawn_log_stream(
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

    pub async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        let mut props = MachineProperties::default();

        let machine_out = new_command("machinectl")
            .args(["show", name])
            .logged_output("machinectl")
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
                        props.insert("Machine", key.to_string(), formatted);
                    }
                }
            }
        }

        let system_out = new_command("systemctl")
            .args(["show", &format!("systemd-nspawn@{}.service", name)])
            .logged_output("systemctl")
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
                                props.insert("Dependencies", key.to_string(), formatted);
                            }
                        } else {
                            props.insert("Systemd Unit", key.to_string(), formatted);
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
