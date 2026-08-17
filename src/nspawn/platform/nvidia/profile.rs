use crate::nspawn::platform::nvidia::classify::NvidiaFileCategory;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum NvidiaPassthroughMode {
    #[default]
    Mirror, // Use CDI containerPath as-is
    Categorized, // Use category_destinations for remapping
}

/// User-configured destination directories for each file category.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NvidiaPassthroughProfile {
    /// Which GPU device to pass through (CDI device name: "0", "all", or UUID)
    pub gpu_device: String,
    /// Passthrough mode
    pub mode: NvidiaPassthroughMode,
    /// Destination overrides per category.
    pub category_destinations: HashMap<NvidiaFileCategory, String>,
    /// Whether to inject env vars into /etc/environment
    pub inject_env: bool,
    /// User-assigned reclassifications for files the auto-classifier can't categorize.
    #[serde(default)]
    pub manual_classifications: Vec<ManualClassification>,
}

/// User-assigned reclassification for a single CDI file that couldn't be
/// auto-categorized. Each entry maps a host_path to a category, destination,
/// and bind type. Stored in the profile so it persists across sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManualClassification {
    pub host_path: String,
    pub category: NvidiaFileCategory,
    /// Container path override (empty = use CDI default container_path).
    pub destination: String,
    pub readonly: bool,
}

impl Default for NvidiaPassthroughProfile {
    fn default() -> Self {
        Self {
            gpu_device: "all".to_string(),
            mode: NvidiaPassthroughMode::Mirror,
            category_destinations: HashMap::new(),
            inject_env: false,
            manual_classifications: Vec::new(),
        }
    }
}

#[allow(dead_code)]
pub struct ProfileTemplate {
    pub name: String,
    pub destinations: HashMap<NvidiaFileCategory, String>,
}

#[allow(dead_code)]
pub fn builtin_templates() -> Vec<ProfileTemplate> {
    vec![
        ProfileTemplate {
            name: "Standard FHS".into(),
            destinations: [
                (NvidiaFileCategory::Lib64, "/usr/lib".into()),
                (NvidiaFileCategory::Lib32, "/usr/lib32".into()),
                (NvidiaFileCategory::Bin, "/usr/bin".into()),
                (NvidiaFileCategory::Firmware, "/lib/firmware/nvidia".into()),
                (NvidiaFileCategory::Config, "/etc/vulkan/icd.d".into()),
            ]
            .into_iter()
            .collect(),
        },
        ProfileTemplate {
            name: "Isolated Prefix".into(),
            destinations: [
                (NvidiaFileCategory::Lib64, "/opt/nvidia/lib64".into()),
                (NvidiaFileCategory::Lib32, "/opt/nvidia/lib32".into()),
                (NvidiaFileCategory::Bin, "/opt/nvidia/bin".into()),
                (NvidiaFileCategory::Firmware, "/opt/nvidia/firmware".into()),
                (NvidiaFileCategory::Config, "/opt/nvidia/config".into()),
                (NvidiaFileCategory::Xorg, "/opt/nvidia/xorg".into()),
                (NvidiaFileCategory::Vdpau, "/opt/nvidia/vdpau".into()),
                (NvidiaFileCategory::Gbm, "/opt/nvidia/gbm".into()),
            ]
            .into_iter()
            .collect(),
        },
    ]
}
