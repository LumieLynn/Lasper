//! Pure NVIDIA passthrough configuration values.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How Lasper obtains the NVIDIA CDI document used for one operation.
///
/// This is carried in deployment requests so direct and elevated execution
/// consume the same input policy. Existing state files deliberately store the
/// resulting projection rather than treating this acquisition choice as part
/// of the container's persistent NVIDIA profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum NvidiaCdiSource {
    #[default]
    Generate,
    Existing {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
}

impl NvidiaCdiSource {
    pub fn existing(path: Option<PathBuf>) -> Self {
        Self::Existing { path }
    }

    pub fn explicit_path(&self) -> Option<&Path> {
        match self {
            Self::Generate => None,
            Self::Existing { path } => path.as_deref(),
        }
    }

    pub fn is_generate(&self) -> bool {
        matches!(self, Self::Generate)
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Generate => "nvidia-ctk generated CDI",
            Self::Existing { .. } => "existing NVIDIA CDI file",
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(path) = self.explicit_path() {
            if !path.is_absolute() {
                return Err("the NVIDIA CDI file path must be absolute".into());
            }
            if path.as_os_str().is_empty() {
                return Err("the NVIDIA CDI file path cannot be empty".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    pub category_destinations: BTreeMap<NvidiaFileCategory, String>,
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
            category_destinations: BTreeMap::new(),
            inject_env: false,
            manual_classifications: Vec::new(),
        }
    }
}

#[allow(dead_code)]
pub struct ProfileTemplate {
    pub name: String,
    pub destinations: BTreeMap<NvidiaFileCategory, String>,
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
