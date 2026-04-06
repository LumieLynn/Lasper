//! nspawn-specific metrics collection logic.

use crate::app::CpuRepresentation;
use crate::events::AppEvent;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;

/// Scans the systemd-nspawn cgroup machine slice to discover active containers.
fn discover_containers() -> Vec<(String, PathBuf)> {
    let machine_slice = "/sys/fs/cgroup/machine.slice";
    let entries = match fs::read_dir(machine_slice) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let name_str = entry.file_name().to_string_lossy().to_string();
            if name_str.starts_with("systemd-nspawn@") && name_str.ends_with(".service") {
                let container_name = name_str
                    .strip_prefix("systemd-nspawn@")?
                    .strip_suffix(".service")?
                    .to_string();
                Some((container_name, entry.path()))
            } else {
                None
            }
        })
        .collect()
}

/// Read basic usage statistics from a container's cgroup path.
fn read_container_stats(cgroup_path: &Path) -> (f64, Option<u64>) {
    // RAM
    let ram_mb = (|| -> Option<f64> {
        let mem_str = fs::read_to_string(cgroup_path.join("memory.current")).ok()?;
        let bytes = mem_str.trim().parse::<f64>().ok()?;
        Some(bytes / 1024.0 / 1024.0)
    })()
    .unwrap_or(0.0);

    // CPU
    let cpu_usec = (|| -> Option<u64> {
        let cpu_str = fs::read_to_string(cgroup_path.join("cpu.stat")).ok()?;
        let line = cpu_str.lines().find(|l| l.starts_with("usage_usec"))?;
        let usec_str = line.split_whitespace().nth(1)?;
        usec_str.parse::<u64>().ok()
    })();

    (ram_mb, cpu_usec)
}

/// Start the metrics collection daemon for systemd-nspawn containers.
pub fn spawn_collector(tx: Sender<AppEvent>, cpu_cores: usize, cpu_rep: CpuRepresentation) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let start_time = Instant::now();
        let mut last_cpu_map: HashMap<String, (u64, Instant)> = HashMap::new();

        loop {
            interval.tick().await;

            for (name, path) in discover_containers() {
                let (ram_mb, cpu_usec) = read_container_stats(&path);

                // Core Calculation Logic
                if let Some(current_usec) = cpu_usec {
                    let now = Instant::now();
                    if let Some((last_usec, last_time)) = last_cpu_map.get(&name) {
                        let delta_usec = current_usec.saturating_sub(*last_usec);
                        let delta_time = now.duration_since(*last_time).as_micros();

                        if delta_time > 0 {
                            let mut cpu_pct = (delta_usec as f64 / delta_time as f64) * 100.0;
                            if cpu_rep == CpuRepresentation::Normalized {
                                cpu_pct /= cpu_cores as f64;
                            }

                            let time_x = start_time.elapsed().as_secs_f64();
                            let _ = tx
                                .send(AppEvent::MetricsUpdate(
                                    name.clone(),
                                    time_x,
                                    cpu_pct,
                                    ram_mb,
                                ))
                                .await;
                        }
                    }
                    last_cpu_map.insert(name, (current_usec, now));
                }
            }
        }
    });
}
