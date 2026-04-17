//! nspawn-specific metrics collection logic.

use crate::app::CpuRepresentation;
use crate::events::AppEvent;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;

/// Scans the systemd-nspawn cgroup machine slice to discover active containers.
async fn discover_containers() -> Vec<(String, PathBuf)> {
    let machine_slice = "/sys/fs/cgroup/machine.slice";
    let mut entries = match tokio::fs::read_dir(machine_slice).await {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut containers = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name_str = entry.file_name().to_string_lossy().to_string();
        if name_str.starts_with("systemd-nspawn@") && name_str.ends_with(".service") {
            if let Some(container_name) = name_str
                .strip_prefix("systemd-nspawn@")
                .and_then(|s| s.strip_suffix(".service"))
            {
                containers.push((container_name.to_string(), entry.path()));
            }
        }
    }
    containers
}

/// Read basic usage statistics from a container's cgroup path.
async fn read_container_stats(cgroup_path: &Path) -> (f64, Option<u64>) {
    // RAM
    let ram_mb = match tokio::fs::read_to_string(cgroup_path.join("memory.current")).await {
        Ok(mem_str) => mem_str.trim().parse::<f64>().unwrap_or(0.0) / 1024.0 / 1024.0,
        Err(_) => 0.0,
    };

    // CPU
    let cpu_usec = match tokio::fs::read_to_string(cgroup_path.join("cpu.stat")).await {
        Ok(cpu_str) => cpu_str
            .lines()
            .find(|l| l.starts_with("usage_usec"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|usec_str| usec_str.parse::<u64>().ok()),
        Err(_) => None,
    };

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

            for (name, path) in discover_containers().await {
                let (ram_mb, cpu_usec) = read_container_stats(&path).await;

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
