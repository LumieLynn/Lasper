use pciid_parser::Database;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static PCI_DB: OnceLock<Option<Database>> = OnceLock::new();

/// Represents a detected GPU device on the host with PCIe traceability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub display_name: String, // e.g., "AMD Radeon RX 6800 XT"
    pub driver_type: String,  // e.g., "DRM/KMS"
    pub nodes: Vec<String>,   // e.g., ["/dev/dri/card0", "/dev/dri/renderD128"]
}

/// Discover all available GPU-like hardware nodes on the host.
/// Uses Sysfs canonicalization to group cardX and renderDX nodes by physical device.
pub fn discover_host_gpus() -> Vec<GpuDevice> {
    let mut gpu_map: HashMap<PathBuf, GpuDevice> = HashMap::new();

    // 1. Initialize PCI database (cached in memory after first read)
    let pci_db = PCI_DB.get_or_init(|| Database::read().ok());

    // 2. Standard DRM/KMS Devices
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();

            // We only care about cardX and renderDX nodes
            if !file_name.starts_with("card") && !file_name.starts_with("renderD") {
                continue;
            }

            // Find the "real physical parent" by resolving the 'device' symlink
            let device_link = entry.path().join("device");
            let canonical_parent = match std::fs::canonicalize(&device_link) {
                Ok(path) => path,
                Err(_) => continue,
            };

            let node_path = format!("/dev/dri/{}", file_name);

            // If this physical GPU was already seen, add this node to it
            if let Some(gpu) = gpu_map.get_mut(&canonical_parent) {
                gpu.nodes.push(node_path);
                gpu.nodes.sort();
                continue;
            }

            // New physical GPU discovered
            let display_name = resolve_hardware_name(&canonical_parent, pci_db.as_ref());
            gpu_map.insert(
                canonical_parent,
                GpuDevice {
                    display_name,
                    driver_type: "DRM/KMS".into(),
                    nodes: vec![node_path],
                },
            );
        }
    }

    let mut result: Vec<GpuDevice> = gpu_map.into_values().collect();

    // 3. WSL2 DirectX Virtual GPU (if not already handled via DRM)
    if Path::new("/dev/dxg").exists() {
        result.push(GpuDevice {
            display_name: "WSL2 DirectX Virtual GPU".into(),
            driver_type: "WSL/DirectX".into(),
            nodes: vec!["/dev/dxg".into()],
        });
    }

    // 4. Legacy ARM Mali (if not appearing in /sys/class/drm)
    if Path::new("/dev/mali").exists() && !result.iter().any(|g| g.nodes.contains(&"/dev/mali".into())) {
        result.push(GpuDevice {
            display_name: "ARM Mali Graphics (Legacy)".into(),
            driver_type: "Mali/Proprietary".into(),
            nodes: vec!["/dev/mali".into()],
        });
    }

    // 5. Qualcomm Adreno (Legacy KGSL)
    if Path::new("/dev/kgsl-3d0").exists() {
        result.push(GpuDevice {
            display_name: "Qualcomm Adreno (Legacy KGSL)".into(),
            driver_type: "KGSL".into(),
            nodes: vec!["/dev/kgsl-3d0".into()],
        });
    }

    result.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    result
}

/// Helper function to resolve the real hardware name from Sysfs
fn resolve_hardware_name(device_path: &Path, pci_db: Option<&Database>) -> String {
    // Try reading Vendor ID and Device ID (Standard PCIe)
    if let (Ok(vendor_str), Ok(device_str)) = (
        std::fs::read_to_string(device_path.join("vendor")),
        std::fs::read_to_string(device_path.join("device")),
    ) {
        let v_id = u16::from_str_radix(vendor_str.trim().trim_start_matches("0x"), 16).unwrap_or(0);
        let d_id = u16::from_str_radix(device_str.trim().trim_start_matches("0x"), 16).unwrap_or(0);

        if let Some(db) = pci_db {
            if let Some(vendor) = db.vendors.get(&v_id) {
                if let Some(device) = vendor.devices.get(&d_id) {
                    return format!("{} {}", vendor.name, device.name);
                }
                return vendor.name.clone();
            }
        }
        return format!("PCI Device (Vendor: {:#06x}, Device: {:#06x})", v_id, d_id);
    }

    // ARM/Embedded Platform GPU (check compatible node)
    let compatible_path = device_path.join("of_node/compatible");
    if let Ok(content) = std::fs::read_to_string(compatible_path) {
        // e.g., "qcom,adreno-640.1\0qcom,adreno\0"
        let first_compatible = content.split('\0').next().unwrap_or("ARM Platform GPU");
        return first_compatible
            .replace("qcom,", "Qualcomm ")
            .replace("arm,", "ARM Mali ");
    }

    "Unknown Graphics Device".to_string()
}
