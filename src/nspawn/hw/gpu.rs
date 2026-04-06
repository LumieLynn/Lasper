use std::path::Path;
use serde::{Deserialize, Serialize};

/// Represents a detected GPU device on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub name: String,
    pub paths: Vec<String>,
}

/// Discover all available GPU-like hardware nodes on the host.
pub fn discover_host_gpus() -> Vec<GpuDevice> {
    let mut gpus = Vec::new();

    // 1. Standard DRM/KMS Devices (Mesa, Intel, AMD, etc.)
    if let Ok(mut entries) = std::fs::read_dir("/dev/dri") {
        let mut paths = Vec::new();
        while let Some(Ok(entry)) = entries.next() {
            paths.push(entry.path().to_string_lossy().into_owned());
        }
        if !paths.is_empty() {
            paths.sort();
            gpus.push(GpuDevice {
                name: "Standard DRM/DRI GPU".into(),
                paths,
            });
        }
    } else {
        // Fallback for bare /dev/card* or /dev/renderD*
        let mut bare_paths = Vec::new();
        if let Ok(mut entries) = std::fs::read_dir("/dev") {
            while let Some(Ok(entry)) = entries.next() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("card") || name.starts_with("renderD") {
                    bare_paths.push(entry.path().to_string_lossy().into_owned());
                }
            }
        }
        if !bare_paths.is_empty() {
            bare_paths.sort();
            gpus.push(GpuDevice {
                name: "Legacy DRM Node".into(),
                paths: bare_paths,
            });
        }
    }

    // 2. WSL2 DirectX Virtual GPU
    if Path::new("/dev/dxg").exists() {
        gpus.push(GpuDevice {
            name: "WSL2 DirectX GPU".into(),
            paths: vec!["/dev/dxg".into()],
        });
    }

    // 3. ARM Mali GPU (Legacy/Embedded)
    if Path::new("/dev/mali").exists() {
        gpus.push(GpuDevice {
            name: "ARM Mali Graphics".into(),
            paths: vec!["/dev/mali".into()],
        });
    }

    // 4. Qualcomm Adreno (Downstream Kernels/Android-based)
    if Path::new("/dev/kgsl-3d0").exists() {
        gpus.push(GpuDevice {
            name: "Qualcomm Adreno GPU".into(),
            paths: vec!["/dev/kgsl-3d0".into()],
        });
    }

    gpus
}
