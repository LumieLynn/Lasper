use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::nspawn::platform::nvidia::classify::NvidiaFileCategory;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NvidiaPassthroughMode {
    Mirror,      // Use CDI containerPath as-is
    Categorized, // Use category_destinations for remapping
}

impl Default for NvidiaPassthroughMode {
    fn default() -> Self {
        Self::Mirror
    }
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
}

impl Default for NvidiaPassthroughProfile {
    fn default() -> Self {
        Self {
            gpu_device: "all".to_string(),
            mode: NvidiaPassthroughMode::Mirror,
            category_destinations: HashMap::new(),
            inject_env: true,
        }
    }
}

impl NvidiaPassthroughProfile {
    pub fn get_profile_path(name: &str) -> std::path::PathBuf {
        super::state::get_state_dir().join(format!("{}.nvidia-profile.json", name))
    }

    pub async fn load(name: &str) -> crate::nspawn::errors::Result<Self> {
        let path = Self::get_profile_path(name);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Self::default());
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| crate::nspawn::errors::NspawnError::Io(path.clone(), e))?;
        let profile: Self = serde_json::from_str(&content)?;
        Ok(profile)
    }

    pub async fn save(&self, name: &str) -> crate::nspawn::errors::Result<()> {
        let path = Self::get_profile_path(name);
        let content = serde_json::to_string_pretty(self)?;
        crate::nspawn::sys::io::AsyncLockedWriter::write_locked(&path, |_| Ok(content)).await?;
        Ok(())
    }
}

pub struct ProfileTemplate {
    pub name: String,
    pub destinations: HashMap<NvidiaFileCategory, String>,
}

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
            ].into_iter().collect(),
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
            ].into_iter().collect(),
        },
    ]
}
