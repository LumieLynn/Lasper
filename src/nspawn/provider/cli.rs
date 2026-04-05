use crate::nspawn::errors::{NspawnError, Result};
use crate::nspawn::models::{ContainerEntry, ContainerState, MachineProperties};
use std::collections::HashMap;
use crate::nspawn::utils::new_command;
use tokio::process::Command;

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
            .output()
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
            .output()
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
            .output()
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

        entries.sort_by(|a, b| {
            let a_run = a.state.is_running();
            let b_run = b.state.is_running();
            b_run.cmp(&a_run).then(a.name.cmp(&b.name))
        });

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

    pub async fn get_logs(&self, name: &str, lines: usize) -> Result<Vec<String>> {
        let out = new_command("journalctl")
            .args([
                "-M",
                name,
                "-n",
                &lines.to_string(),
                "--no-pager",
                "--output=short",
            ])
            .output()
            .await
            .map_err(|e| NspawnError::Io(std::path::PathBuf::from("journalctl"), e))?;

        if !out.status.success() {
            log::warn!(
                "journalctl -M {} failed: {}",
                name,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect())
    }

    pub async fn get_properties(&self, name: &str) -> Result<MachineProperties> {
        let mut map = HashMap::new();

        let machine_out = new_command("machinectl")
            .args(["show", name])
            .output()
            .await;

        if let Ok(out) = machine_out {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        map.entry(k.trim().to_string())
                            .or_insert_with(|| v.trim().to_string());
                    }
                }
            }
        }

        let system_out = new_command("systemctl")
            .args(["show", &format!("systemd-nspawn@{}.service", name)])
            .output()
            .await;

        if let Ok(out) = system_out {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let key = k.trim();
                        if key == "UnitFileState" || !map.contains_key(key) {
                            map.insert(key.to_string(), v.trim().to_string());
                        }
                    }
                }
            }
        }

        if map.is_empty() {
            return Err(NspawnError::CommandFailed(
                format!("machinectl/systemctl show {}", name),
                "No properties found".to_string(),
                "The target machine might not exist or systemd-nspawn is not managing it.".to_string(),
            ));
        }

        Ok(MachineProperties {
            properties: map,
            ..Default::default()
        })
    }
}
