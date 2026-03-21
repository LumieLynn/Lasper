//! NVIDIA GPU and driver detection logic for host passthrough.

use std::path::{Path, PathBuf};
use tokio::process::Command;
use super::errors::{NspawnError, Result};

/// Hardware and driver information detected on the host.
#[derive(Debug, Clone)]
pub struct NvidiaInfo {
    /// Device files found in /dev (e.g., /dev/nvidia0).
    pub devices: Vec<String>,
    /// Read-only system paths (e.g., /proc/driver/nvidia).
    pub system_ro: Vec<String>,
    /// Library/binary files found via package manager, ldconfig, or fallback scan.
    pub driver_files: Vec<String>,
}

/// Perform a comprehensive scan of the host for NVIDIA devices and drivers.
pub async fn detect_nvidia() -> NvidiaInfo {
    log::info!("Starting NVIDIA host scan...");
    let devices = detect_nvidia_devices();
    let system_ro = detect_nvidia_system_ro();
    let driver_files = detect_nvidia_driver_files().await;
    
    log::info!("NVIDIA scan complete: {} devices, {} system paths, {} driver files found", 
        devices.len(), system_ro.len(), driver_files.len());
    
    NvidiaInfo { devices, system_ro, driver_files }
}

fn detect_nvidia_devices() -> Vec<String> {
    let candidates = [
        "/dev/nvidia0", "/dev/nvidia1", "/dev/nvidia2", "/dev/nvidia3",
        "/dev/nvidia4", "/dev/nvidia5", "/dev/nvidia6", "/dev/nvidia7",
        "/dev/nvidiactl", "/dev/nvidia-modeset",
        "/dev/nvidia-uvm", "/dev/nvidia-uvm-tools",
        "/dev/nvidia-caps", "/dev/nvidia-nvswitchctl",
        "/dev/dri",
    ];
    candidates.iter()
        .filter(|p| Path::new(p).exists())
        .map(|p| p.to_string())
        .collect()
}

fn detect_nvidia_system_ro() -> Vec<String> {
    let candidates = [
        "/proc/driver/nvidia",
        "/sys/devices/pci0000:00",
        "/sys/class/drm",
        "/sys/module/nvidia",
        "/sys/module/nvidia_modeset",
        "/sys/module/nvidia_drm",
        "/sys/module/nvidia_uvm",
    ];
    candidates.iter()
        .filter(|p| Path::new(p).exists())
        .map(|p| p.to_string())
        .collect()
}

/// Detect driver libraries using the system package manager and ldconfig.
async fn detect_nvidia_driver_files() -> Vec<String> {
    let mut all_files = Vec::new();

    // 1. Try ldconfig (fast and accurate for libraries)
    if let Ok(ld_files) = query_ldconfig().await {
        all_files.extend(ld_files);
    }

    // 2. Try package managers for binaries and other files
    if which("pacman") {
        if let Ok(files) = query_pacman().await {
            all_files.extend(files);
        }
    } else if which("dpkg") {
        if let Ok(files) = query_dpkg().await {
            all_files.extend(files);
        }
    } else if which("rpm") {
        if let Ok(files) = query_rpm().await {
            all_files.extend(files);
        }
    }

    // 3. Last resort fallback
    if all_files.is_empty() {
        all_files.extend(fallback_nvidia_scan());
    }

    dedup(all_files)
}

/// Use ldconfig -p to find libraries.
async fn query_ldconfig() -> Result<Vec<String>> {
    let out = Command::new("ldconfig")
        .arg("-p")
        .output()
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("ldconfig"), e))?;
    
    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.contains("libnvidia-") || line.contains("libcuda.so") || line.contains("libnvcuvid.so") {
            if let Some(pos) = line.rfind("=>") {
                let path = line[pos + 2..].trim();
                if Path::new(path).exists() {
                    files.push(path.to_string());
                }
            }
        }
    }
    Ok(files)
}

/// pacman: detect installed nvidia packages first, then query each.
async fn query_pacman() -> Result<Vec<String>> {
    let out = Command::new("pacman")
        .args(["-Qq"])
        .output()
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("pacman"), e))?;

    let installed: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("nvidia") || l.contains("cuda"))
        .map(|l| l.to_string())
        .collect();

    if installed.is_empty() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for pkg in &installed {
        let out = Command::new("pacman")
            .args(["-Ql", pkg])
            .output()
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("pacman"), e))?;

        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            if is_nvidia_driver_file(&path) {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// dpkg: find nvidia packages, then query files.
async fn query_dpkg() -> Result<Vec<String>> {
    let out = Command::new("dpkg-query")
        .args(["-W", "-f=${Package} ${Status}\n"])
        .output()
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("dpkg-query"), e))?;

    let pkgs: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("install ok installed") && (l.contains("nvidia") || l.contains("cuda")))
        .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
        .collect();

    if pkgs.is_empty() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for pkg in &pkgs {
        let out = Command::new("dpkg")
            .args(["-L", pkg])
            .output()
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("dpkg"), e))?;
        for path in String::from_utf8_lossy(&out.stdout).lines() {
            if is_nvidia_driver_file(path) {
                files.push(path.to_string());
            }
        }
    }
    Ok(files)
}

/// rpm: find nvidia packages, then query files.
async fn query_rpm() -> Result<Vec<String>> {
    let out = Command::new("rpm")
        .args(["-qa"])
        .output()
        .await
        .map_err(|e| NspawnError::Io(PathBuf::from("rpm"), e))?;

    let pkgs: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("nvidia") || l.contains("cuda"))
        .map(|l| l.to_string())
        .collect();

    if pkgs.is_empty() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for pkg in &pkgs {
        let out = Command::new("rpm")
            .args(["-ql", pkg])
            .output()
            .await
            .map_err(|e| NspawnError::Io(PathBuf::from("rpm"), e))?;
        for path in String::from_utf8_lossy(&out.stdout).lines() {
            if is_nvidia_driver_file(path) {
                files.push(path.to_string());
            }
        }
    }
    Ok(files)
}

/// Last resort: scan common paths for nvidia files.
fn fallback_nvidia_scan() -> Vec<String> {
    let search_dirs = [
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib32",
        "/usr/bin",
    ];
    let mut found = Vec::new();
    for dir in &search_dirs {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let name = entry.file_name();
                let s = name.to_string_lossy();
                if s.starts_with("libnvidia") || s.starts_with("libcuda")
                    || s.starts_with("nvidia-") || s.starts_with("nvcc")
                {
                    found.push(entry.path().to_string_lossy().to_string());
                }
            }
        }
    }
    found
}

/// Filter predicate: is this path a relevant nvidia driver file?
fn is_nvidia_driver_file(path: &str) -> bool {
    if path.is_empty() || path.ends_with('/') {
        return false;
    }
    let p = Path::new(path);
    let file = p.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();

    let is_match = (file.starts_with("libnvidia")
        || file.starts_with("libcuda")
        || file.starts_with("libnvcuvid")
        || file.starts_with("libnvoptix")
        || file.starts_with("nvidia-"))
        && (file.contains(".so") || !file.contains('.'));
    
    if cfg!(test) {
        is_match
    } else {
        is_match && p.is_file()
    }
}

fn dedup(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .unwrap_or_default()
        .to_string_lossy()
        .split(':')
        .any(|d| Path::new(d).join(cmd).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_nvidia_driver_file() {
        assert!(is_nvidia_driver_file("/usr/lib/libnvidia-glcore.so.535.104.05"));
        assert!(is_nvidia_driver_file("/usr/lib/libcuda.so.1"));
        assert!(is_nvidia_driver_file("/usr/bin/nvidia-smi"));
        
        assert!(!is_nvidia_driver_file("/usr/lib/libc.so.6"));
        assert!(!is_nvidia_driver_file("/usr/lib/"));
        assert!(!is_nvidia_driver_file(""));
    }

    #[test]
    fn test_dedup() {
        let input = vec!["b".into(), "a".into(), "b".into(), "c".into(), "a".into()];
        let expected = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(dedup(input), expected);
    }
}
