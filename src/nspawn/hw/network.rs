use std::fs;

/// Autodetect bridges available on the host.
pub fn detect_bridges() -> Vec<String> {
    let mut bridges = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("bridge").is_dir() {
                if let Some(name) = path.file_name() {
                    bridges.push(name.to_string_lossy().into_owned());
                }
            }
        }
    }
    bridges.sort();
    bridges
}

/// Autodetect physical (non-virtual) network interfaces on the host.
#[allow(dead_code)]
pub fn detect_physical_interfaces() -> Vec<String> {
    let mut interfaces = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "lo" {
                continue;
            }

            let path = entry.path();

            if let Ok(real_path) = fs::canonicalize(&path) {
                if !real_path.to_string_lossy().contains("/devices/virtual/") {
                    interfaces.push(name);
                }
            }
        }
    }
    interfaces.sort();
    interfaces
}
