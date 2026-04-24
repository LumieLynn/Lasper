use crate::nspawn::models::BindMount;
use crate::nspawn::platform::nvidia::cdi::{CdiHook, CdiMount};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

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
}

impl NvidiaFileCategory {
    pub fn all_static() -> Vec<Self> {
        vec![
            Self::Lib64,
            Self::Lib32,
            Self::Bin,
            Self::Firmware,
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
        }
    }

    /// Returns the well-known container root path for this category.
    /// Used by the categorized remapping engine to preserve subdirectory
    /// structure below this root when the user sets a custom destination.
    /// Returns empty string for categories with no single canonical root
    /// (e.g. Config files live under both /etc/ and /usr/share/).
    pub fn default_container_root(&self) -> &str {
        match self {
            Self::Lib64 => "/usr/lib",
            Self::Lib32 => "/usr/lib32",
            Self::Bin => "/usr/bin",
            Self::Firmware => "/lib/firmware/nvidia",
            Self::Config => "",
            Self::Xorg => "/usr/lib/xorg/modules",
            Self::Vdpau => "/usr/lib/vdpau",
            Self::Gbm => "/usr/lib/gbm",
        }
    }
}

/// A single classified CDI mount entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClassifiedEntry {
    pub host_path: String,
    pub default_container_path: String,
    pub category: NvidiaFileCategory,
}

/// Symlink to be created inside the container, parsed from CDI hooks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SymlinkEntry {
    pub target: String,
    pub link_path: String,
}

/// Extracts classification results from CDI mounts.
pub fn classify_mounts(mounts: Vec<CdiMount>) -> (Vec<ClassifiedEntry>, Vec<BindMount>) {
    let mut classified = Vec::new();
    let mut unclassified = Vec::new();

    for m in mounts {
        let path = m.host_path.clone();
        let lower = path.to_lowercase();

        let category = if lower.contains("xorg") || lower.contains("modules/drivers") {
            Some(NvidiaFileCategory::Xorg)
        } else if lower.contains("vdpau") {
            Some(NvidiaFileCategory::Vdpau)
        } else if lower.contains("gbm") {
            Some(NvidiaFileCategory::Gbm)
        } else if lower.ends_with(".json")
            && (lower.contains("vulkan") || lower.contains("egl") || lower.contains("glvnd"))
        {
            Some(NvidiaFileCategory::Config)
        } else if lower.ends_with(".so") || lower.contains(".so.") {
            // 32-bit vs 64-bit detection
            if lower.contains("/lib32/")
                || lower.contains("/i386-linux-gnu/")
                || lower.contains("/i686/")
            {
                Some(NvidiaFileCategory::Lib32)
            } else {
                Some(NvidiaFileCategory::Lib64)
            }
        } else if lower.contains("/bin/") {
            Some(NvidiaFileCategory::Bin)
        } else if lower.contains("/lib/firmware/")
            || lower.contains("/share/nvidia/")
            || lower.ends_with(".bin")
        {
            Some(NvidiaFileCategory::Firmware)
        } else {
            None
        };

        if let Some(cat) = category {
            classified.push(ClassifiedEntry {
                host_path: m.host_path,
                default_container_path: m.container_path,
                category: cat,
            });
        } else {
            unclassified.push(BindMount {
                source: m.host_path,
                target: m.container_path,
                readonly: true,
            });
        }
    }

    (classified, unclassified)
}

/// Parses CDI hooks for symlink creation.
pub fn parse_symlink_hooks(hooks: &[CdiHook]) -> Vec<SymlinkEntry> {
    let mut symlinks = Vec::new();
    for hook in hooks {
        if hook.hook_name == "create-symlinks" {
            if let Some(args) = &hook.args {
                let mut i = 0;
                while i < args.len() {
                    if args[i] == "--link" && i + 1 < args.len() {
                        let link_pair = &args[i + 1];
                        if let Some((target, link_path)) = link_pair.split_once("::") {
                            symlinks.push(SymlinkEntry {
                                target: target.to_string(),
                                link_path: link_path.to_string(),
                            });
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
        }
    }
    symlinks
}

/// Parses CDI hooks for ldcache folders.
pub fn parse_ldcache_folders(hooks: &[CdiHook]) -> Vec<String> {
    let mut folders = Vec::new();
    for hook in hooks {
        if hook.hook_name == "update-ldcache" {
            if let Some(args) = &hook.args {
                let mut i = 0;
                while i < args.len() {
                    if args[i] == "--folder" && i + 1 < args.len() {
                        folders.push(args[i + 1].clone());
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
        }
    }
    folders
}

/// Parses CDI environment variables.
pub fn parse_env_vars(env: &[String]) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for entry in env {
        if let Some((key, val)) = entry.split_once('=') {
            vars.push((key.to_string(), val.to_string()));
        }
    }
    vars
}

/// Detects which categories are actually used in a set of entries.
pub fn detect_active_categories(entries: &[ClassifiedEntry]) -> Vec<NvidiaFileCategory> {
    let mut active = HashSet::new();
    for e in entries {
        active.insert(e.category.clone());
    }
    let mut result: Vec<_> = active.into_iter().collect();
    // Sort to keep UI consistent
    result.sort_by_key(|c| format!("{:?}", c));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nspawn::platform::nvidia::cdi::{CdiHook, CdiMount};

    #[test]
    fn test_classify_mounts_all_categories() {
        let mounts = vec![
            CdiMount {
                host_path: "/usr/lib/libcuda.so.1".into(),
                container_path: "/usr/lib/libcuda.so.1".into(),
                options: None,
            },
            CdiMount {
                host_path: "/usr/lib32/libcuda.so.1".into(),
                container_path: "/usr/lib32/libcuda.so.1".into(),
                options: None,
            },
            CdiMount {
                host_path: "/usr/i386-linux-gnu/libcuda.so.1".into(),
                container_path: "/usr/i386-linux-gnu/libcuda.so.1".into(),
                options: None,
            },
            CdiMount {
                host_path: "/usr/bin/nvidia-smi".into(),
                container_path: "/usr/bin/nvidia-smi".into(),
                options: None,
            },
            CdiMount {
                host_path: "/lib/firmware/nvidia/gsp.bin".into(),
                container_path: "/lib/firmware/nvidia/gsp.bin".into(),
                options: None,
            },
            CdiMount {
                host_path: "/usr/share/vulkan/icd.d/nvidia_icd.json".into(),
                container_path: "/usr/share/vulkan/icd.d/nvidia_icd.json".into(),
                options: None,
            },
            CdiMount {
                host_path: "/usr/lib/nvidia/xorg/libglxserver_nvidia.so".into(),
                container_path: "/usr/lib/nvidia/xorg/libglxserver_nvidia.so".into(),
                options: None,
            },
            CdiMount {
                host_path: "/usr/lib/vdpau/libvdpau_nvidia.so".into(),
                container_path: "/usr/lib/vdpau/libvdpau_nvidia.so".into(),
                options: None,
            },
            CdiMount {
                host_path: "/usr/lib/gbm/nvidia-drm_gbm.so".into(),
                container_path: "/usr/lib/gbm/nvidia-drm_gbm.so".into(),
                options: None,
            },
        ];

        let (classified, unclassified) = classify_mounts(mounts);
        assert_eq!(unclassified.len(), 0);

        let find_cat = |cat: NvidiaFileCategory| classified.iter().any(|e| e.category == cat);

        assert!(find_cat(NvidiaFileCategory::Lib64));
        assert!(find_cat(NvidiaFileCategory::Lib32));
        assert!(find_cat(NvidiaFileCategory::Bin));
        assert!(find_cat(NvidiaFileCategory::Firmware));
        assert!(find_cat(NvidiaFileCategory::Config));
        assert!(find_cat(NvidiaFileCategory::Xorg));
        assert!(find_cat(NvidiaFileCategory::Vdpau));
        assert!(find_cat(NvidiaFileCategory::Gbm));

        // Verify specific lib32 detections
        let lib32_entries: Vec<_> = classified
            .iter()
            .filter(|e| e.category == NvidiaFileCategory::Lib32)
            .collect();
        assert_eq!(lib32_entries.len(), 2);
    }

    #[test]
    fn test_parse_symlink_hooks() {
        let hooks = vec![CdiHook {
            hook_name: "create-symlinks".into(),
            path: "/usr/bin/nvidia-cdi-hook".into(),
            args: Some(vec![
                "nvidia-cdi-hook".into(),
                "create-symlinks".into(),
                "--link".into(),
                "target1::link1".into(),
                "--link".into(),
                "target2::link2".into(),
            ]),
        }];

        let symlinks = parse_symlink_hooks(&hooks);
        assert_eq!(symlinks.len(), 2);
        assert_eq!(symlinks[0].target, "target1");
        assert_eq!(symlinks[0].link_path, "link1");
        assert_eq!(symlinks[1].target, "target2");
        assert_eq!(symlinks[1].link_path, "link2");
    }

    #[test]
    fn test_parse_ldcache_folders() {
        let hooks = vec![CdiHook {
            hook_name: "update-ldcache".into(),
            path: "/usr/bin/nvidia-cdi-hook".into(),
            args: Some(vec![
                "nvidia-cdi-hook".into(),
                "update-ldcache".into(),
                "--folder".into(),
                "/usr/lib".into(),
                "--folder".into(),
                "/usr/lib32".into(),
                "--folder".into(),
                "/usr/lib/i386-linux-gnu".into(),
                "--folder".into(),
                "/usr/lib/x86_64-linux-gnu".into(),
                "--folder".into(),
                "/usr/lib/vdpau".into(),
            ]),
        }];

        let folders = parse_ldcache_folders(&hooks);
        assert_eq!(folders.len(), 5);
        assert_eq!(folders[0], "/usr/lib");
        assert_eq!(folders[1], "/usr/lib32");
        assert_eq!(folders[2], "/usr/lib/i386-linux-gnu");
        assert_eq!(folders[3], "/usr/lib/x86_64-linux-gnu");
        assert_eq!(folders[4], "/usr/lib/vdpau");
    }

    #[test]
    fn test_detect_active_categories() {
        let entries = vec![
            ClassifiedEntry {
                host_path: "".into(),
                default_container_path: "".into(),
                category: NvidiaFileCategory::Bin,
            },
            ClassifiedEntry {
                host_path: "".into(),
                default_container_path: "".into(),
                category: NvidiaFileCategory::Lib64,
            },
            ClassifiedEntry {
                host_path: "".into(),
                default_container_path: "".into(),
                category: NvidiaFileCategory::Bin,
            },
        ];

        let active = detect_active_categories(&entries);
        assert_eq!(active.len(), 2);
        // Sorted by debug name (Bin, Lib64)
        assert_eq!(active[0], NvidiaFileCategory::Bin);
        assert_eq!(active[1], NvidiaFileCategory::Lib64);
    }
}
