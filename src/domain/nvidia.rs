//! Pure NVIDIA passthrough configuration values.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NvidiaFileCategory {
    Lib64,
    Lib32,
    Bin,
    Firmware,
    Config,
    Xorg,
    Vdpau,
    Gbm,
    Other,
}

impl NvidiaFileCategory {
    pub fn all_static() -> Vec<Self> {
        vec![Self::Lib64, Self::Lib32, Self::Bin, Self::Firmware]
    }

    pub fn all() -> Vec<Self> {
        vec![
            Self::Lib64,
            Self::Lib32,
            Self::Bin,
            Self::Firmware,
            Self::Config,
            Self::Xorg,
            Self::Vdpau,
            Self::Gbm,
            Self::Other,
        ]
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Lib64 => "Libraries (64-bit)",
            Self::Lib32 => "Libraries (32-bit)",
            Self::Bin => "Binaries",
            Self::Firmware => "Firmware",
            Self::Config => "Vulkan/EGL Config",
            Self::Xorg => "Xorg Modules",
            Self::Vdpau => "VDPAU",
            Self::Gbm => "GBM",
            Self::Other => "Other / Unclassified",
        }
    }

    pub fn default_container_root(&self) -> &str {
        match self {
            Self::Lib64 => "/usr/lib",
            Self::Lib32 => "/usr/lib32",
            Self::Bin => "/usr/bin",
            Self::Firmware => "/lib/firmware/nvidia",
            Self::Config | Self::Other => "",
            Self::Xorg => "/usr/lib/xorg/modules",
            Self::Vdpau => "/usr/lib/vdpau",
            Self::Gbm => "/usr/lib/gbm",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum NvidiaPassthroughMode {
    #[default]
    Mirror,
    Categorized,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NvidiaPassthroughProfile {
    pub gpu_device: String,
    pub mode: NvidiaPassthroughMode,
    pub category_destinations: HashMap<NvidiaFileCategory, String>,
    pub inject_env: bool,
    #[serde(default)]
    pub manual_classifications: Vec<ManualClassification>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManualClassification {
    pub host_path: String,
    pub category: NvidiaFileCategory,
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
