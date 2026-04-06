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
            // Filter out common virtual interfaces
            let is_virtual = path.join("bridge").is_dir()
                || path.join("brport").is_dir()
                || path.join("tun_flags").exists()
                || name.starts_with("veth")
                || name.starts_with("docker")
                || name.starts_with("br-");

            if !is_virtual {
                interfaces.push(name);
            }
        }
    }
    interfaces.sort();
    interfaces
}
