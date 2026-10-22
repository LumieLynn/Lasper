use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Represents a detected GPU device on the host with PCIe traceability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub display_name: String, // e.g., "AMD Radeon RX 6800 XT"
    pub driver_type: String,  // e.g., "DRM/KMS"
    pub nodes: Vec<String>,   // e.g., ["/dev/dri/card0", "/dev/dri/renderD128"]
}

/// Discover all available GPU-like hardware nodes on the host.
/// Uses Sysfs canonicalization to group cardX and renderDX nodes by physical device.
pub async fn discover_host_gpus() -> Vec<GpuDevice> {
    let mut gpu_map: HashMap<PathBuf, GpuDevice> = HashMap::new();

    // 2. Standard DRM/KMS Devices
    if let Ok(mut entries) = tokio::fs::read_dir("/sys/class/drm").await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let file_name = entry.file_name().to_string_lossy().to_string();

            // Connector entries such as card0-HDMI-A-1 also live here, but only
            // cardN and renderDN correspond to device nodes under /dev/dri.
            if !is_drm_device_node_name(&file_name) {
                continue;
            }

            // Find the "real physical parent" by resolving the 'device' symlink
            let device_link = entry.path().join("device");
            let canonical_parent = match tokio::fs::canonicalize(&device_link).await {
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
            let display_name = resolve_hardware_name(&canonical_parent).await;
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
    if tokio::fs::try_exists("/dev/dxg").await.unwrap_or(false) {
        result.push(GpuDevice {
            display_name: "WSL2 DirectX Virtual GPU".into(),
            driver_type: "WSL/DirectX".into(),
            nodes: vec!["/dev/dxg".into()],
        });
    }

    // 4. Legacy ARM Mali (if not appearing in /sys/class/drm)
    if tokio::fs::try_exists("/dev/mali").await.unwrap_or(false)
        && !result.iter().any(|g| g.nodes.contains(&"/dev/mali".into()))
    {
        result.push(GpuDevice {
            display_name: "ARM Mali Graphics (Legacy)".into(),
            driver_type: "Mali/Proprietary".into(),
            nodes: vec!["/dev/mali".into()],
        });
    }

    // 5. Qualcomm Adreno (Legacy KGSL)
    if tokio::fs::try_exists("/dev/kgsl-3d0")
        .await
        .unwrap_or(false)
    {
        result.push(GpuDevice {
            display_name: "Qualcomm Adreno (Legacy KGSL)".into(),
            driver_type: "KGSL".into(),
            nodes: vec!["/dev/kgsl-3d0".into()],
        });
    }

    result.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    result
}

fn is_drm_device_node_name(name: &str) -> bool {
    let number = name
        .strip_prefix("card")
        .or_else(|| name.strip_prefix("renderD"));
    number.is_some_and(|number| {
        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// Helper function to resolve the real hardware name from Sysfs
async fn resolve_hardware_name(device_path: &Path) -> String {
    // Try reading Vendor ID and Device ID (Standard PCIe)
    let vendor_file = device_path.join("vendor");
    let device_file = device_path.join("device");

    if let (Ok(vendor_str), Ok(device_str)) = (
        tokio::fs::read_to_string(&vendor_file).await,
        tokio::fs::read_to_string(&device_file).await,
    ) {
        let v_id = u16::from_str_radix(vendor_str.trim().trim_start_matches("0x"), 16).unwrap_or(0);
        let d_id = u16::from_str_radix(device_str.trim().trim_start_matches("0x"), 16).unwrap_or(0);

        if let Ok(Some(name)) =
            tokio::task::spawn_blocking(move || lookup_pci_name(v_id, d_id)).await
        {
            return name;
        }
        return format!("PCI Device (Vendor: {:#06x}, Device: {:#06x})", v_id, d_id);
    }

    // ARM/Embedded Platform GPU (check compatible node)
    let compatible_path = device_path.join("of_node/compatible");
    if let Ok(content) = tokio::fs::read_to_string(compatible_path).await {
        // e.g., "qcom,adreno-640.1\0qcom,adreno\0"
        let first_compatible = content.split('\0').next().unwrap_or("ARM Platform GPU");
        return first_compatible
            .replace("qcom,", "Qualcomm ")
            .replace("arm,", "ARM Mali ");
    }

    "Unknown Graphics Device".to_string()
}

fn lookup_pci_name(vendor: u16, device: u16) -> Option<String> {
    let file = crate::paths::pci_ids_paths()
        .iter()
        .find_map(|path| File::open(path).ok())?;
    read_pci_name(BufReader::new(file), vendor, device)
        .ok()
        .flatten()
}

/// Scan one vendor's device entries without retaining the PCI catalogue.
/// Subsystem and class names are irrelevant to GPU display labels.
fn read_pci_name(mut reader: impl BufRead, vendor: u16, device: u16) -> io::Result<Option<String>> {
    let mut line = String::new();
    let mut vendor_name = None;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let entry = line.trim_end();
        if entry.is_empty() || entry.starts_with('#') || entry.starts_with("\t\t") {
            continue;
        }
        if let Some(entry) = entry.strip_prefix('\t') {
            if let Some(vendor_name) = &vendor_name {
                if let Some((id, name)) = parse_pci_name(entry) {
                    if id == device {
                        return Ok(Some(format!("{vendor_name} {name}")));
                    }
                }
            }
        } else {
            // A new vendor (or the class table) ends the selected vendor.
            if vendor_name.is_some() || entry.starts_with("C ") {
                break;
            }
            if let Some((id, name)) = parse_pci_name(entry) {
                if id == vendor {
                    vendor_name = Some(name.to_owned());
                }
            }
        }
    }
    Ok(vendor_name)
}

fn parse_pci_name(entry: &str) -> Option<(u16, &str)> {
    let (id, name) = entry.split_once(char::is_whitespace)?;
    if id.len() != 4 {
        return None;
    }
    Some((u16::from_str_radix(id, 16).ok()?, name.trim()))
}

#[cfg(test)]
mod tests {
    use super::{is_drm_device_node_name, read_pci_name};

    #[test]
    fn drm_device_names_exclude_connectors_and_incomplete_names() {
        for valid in ["card0", "card12", "renderD128", "renderD999"] {
            assert!(is_drm_device_node_name(valid), "{valid}");
        }
        for invalid in [
            "card",
            "renderD",
            "card0-HDMI-A-1",
            "card0-DP-1",
            "cardx",
            "renderD12x",
            "controlD64",
        ] {
            assert!(!is_drm_device_node_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn pci_lookup_ignores_subsystems_and_other_vendors_and_classes() {
        let catalogue = b"# PCI names\n1002  AMD\n\t0001  First GPU\n\t\t10de 0002  Subsystem\n10de  NVIDIA\n\t0001  Second GPU\n\t0002  Selected GPU\n\n1234  Another vendor\n\t0003  Unrelated GPU\nC 03  Display controller\n\t00  VGA controller\n";
        for (vendor, device, expected) in [
            (0x1002, 1, Some("AMD First GPU")),
            (0x10de, 2, Some("NVIDIA Selected GPU")),
            (0x1002, 2, Some("AMD")),
            (0x10de, 3, Some("NVIDIA")),
            (0xffff, 0, None),
        ] {
            assert_eq!(
                read_pci_name(catalogue.as_slice(), vendor, device)
                    .unwrap()
                    .as_deref(),
                expected
            );
        }
    }

    #[test]
    fn pci_lookup_accepts_crlf_and_an_unterminated_last_line() {
        assert_eq!(
            read_pci_name(&b"10de  NVIDIA\r\n\t0002  Selected GPU"[..], 0x10de, 2)
                .unwrap()
                .as_deref(),
            Some("NVIDIA Selected GPU")
        );
    }
}
