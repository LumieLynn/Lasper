use std::sync::{Arc, Mutex, atomic::AtomicBool};
use crate::nspawn::ContainerEntry;
use crate::nspawn::models::{BindMount, ContainerConfig, CreateUser, NetworkMode, PortForward};
use crate::nspawn::create::{nspawn_config_content, systemd_override_content};
use crate::nspawn::storage::{StorageBackend, StorageType};
use crate::nspawn::deploy::Deployer;
use crate::nspawn::nvidia::NvidiaInfo;

/// The different methods available for acquiring a rootfs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceKind {
    Copy,
    Oci,
    Debootstrap,
    Pacstrap,
    DiskImage,
}

pub struct SourceState {
    pub kind: SourceKind,
    pub cursor: usize,
    pub oci_url: String,
    pub deboot_mirror: String,
    pub deboot_suite: String,
    pub pacstrap_pkgs: String,
    pub disk_path: String,
    pub copy_cursor: usize,
    pub clone_source: String,
    pub field_idx: usize,
}

pub struct BasicState {
    pub name: String,
    pub hostname: String,
    pub field_idx: usize,
}

pub struct StorageState {
    pub type_idx: usize,
    pub info: crate::nspawn::storage::StorageInfo,
    pub field_idx: usize,
    pub raw_size: String,
    pub raw_fs: String,
    pub raw_partition: bool,
}

pub struct UserState {
    pub root_password: String,
    pub enabled: bool,
    pub user: CreateUser,
    pub field_idx: usize,
}

pub struct NetworkState {
    pub mode: usize,
    pub bridge_name: String,
    pub bridge_list: Vec<String>,
    pub bridge_cursor: usize,
    pub port_input: String,
    pub port_list: Vec<PortForward>,
    pub field_block: usize, // 0: Mode, 1: Bridge, 2: Port Input, 3: Port List
    pub h_scroll: usize,
}

pub struct PassthroughState {
    pub generic_gpu: bool,
    pub wayland_socket: bool,
    pub nvidia_enabled: bool,
    pub field_idx: usize,
    pub nvidia: NvidiaInfo,
    pub nvidia_devices_sel: Vec<bool>,
    pub nvidia_sysro_sel: Vec<bool>,
    pub nvidia_libs_sel: Vec<bool>,
    pub bind_input: String,
    pub bind_list: Vec<BindMount>,
    pub device_cursor: usize,
    pub device_block: usize, // 0: GPU, 1: RO, 2: Libs, 3: Input, 4: List
    pub device_h_scroll: usize,
    pub nvidia_loaded: bool,
}

pub struct ReviewState {
    pub preview: String,
    pub preview_scroll: u16,
}

pub struct DeployState {
    pub logs: Arc<Mutex<Vec<String>>>,
    pub done: Arc<AtomicBool>,
    pub success: Arc<AtomicBool>,
    pub scroll: usize,
}

/// Holds the state for the multi-step container creation wizard, shared between steps.
pub struct WizardContext {
    pub entries: Vec<ContainerEntry>,
    pub source: SourceState,
    pub basic: BasicState,
    pub storage: StorageState,
    pub user: UserState,
    pub network: NetworkState,
    pub passthrough: PassthroughState,
    pub review: ReviewState,
    pub deploy: DeployState,
}

impl WizardContext {
    pub fn new(_is_root: bool) -> Self {
        Self {
            entries: Vec::new(),
            source: SourceState {
                kind: SourceKind::Copy,
                cursor: 0,
                oci_url: String::new(),
                deboot_mirror: String::new(),
                deboot_suite: String::new(),
                pacstrap_pkgs: String::new(),
                disk_path: String::new(),
                copy_cursor: 0,
                clone_source: String::new(),
                field_idx: 0,
            },
            basic: BasicState {
                name: String::new(),
                hostname: String::new(),
                field_idx: 0,
            },
            storage: StorageState {
                type_idx: 0,
                info: crate::nspawn::storage::detect_available_storage_types(),
                field_idx: 0,
                raw_size: "2G".to_string(),
                raw_fs: "ext4".to_string(),
                raw_partition: false,
            },
            user: UserState {
                root_password: String::new(),
                enabled: false,
                user: CreateUser::default(),
                field_idx: 0,
            },
            network: NetworkState {
                mode: 0,
                bridge_name: String::new(),
                bridge_list: Vec::new(),
                bridge_cursor: 0,
                port_input: String::new(),
                port_list: Vec::new(),
                field_block: 0,
                h_scroll: 0,
            },
            passthrough: PassthroughState {
                generic_gpu: false,
                wayland_socket: false,
                nvidia_enabled: false,
                field_idx: 0,
                nvidia: NvidiaInfo { devices: vec![], system_ro: vec![], driver_files: vec![] },
                nvidia_devices_sel: Vec::new(),
                nvidia_sysro_sel: Vec::new(),
                nvidia_libs_sel: Vec::new(),
                bind_input: String::new(),
                bind_list: Vec::new(),
                device_cursor: 0,
                device_block: 0,
                device_h_scroll: 0,
                nvidia_loaded: false,
            },
            review: ReviewState {
                preview: String::new(),
                preview_scroll: 0,
            },
            deploy: DeployState {
                logs: Arc::new(Mutex::new(Vec::new())),
                done: Arc::new(AtomicBool::new(false)),
                success: Arc::new(AtomicBool::new(false)),
                scroll: 0,
            },
        }
    }

    pub fn network_mode(&self) -> Option<NetworkMode> {
        match self.network.mode {
            1 => Some(NetworkMode::None),
            2 => Some(NetworkMode::Veth),
            3 => Some(NetworkMode::Bridge(self.network.bridge_name.clone())),
            _ => Some(NetworkMode::Host),
        }
    }

    pub fn build_config(&self) -> ContainerConfigWithPreview {
        let mut device_binds: Vec<String> = if self.passthrough.nvidia_enabled {
            self.passthrough.nvidia.devices.iter().enumerate()
                .filter(|(i, _)| self.passthrough.nvidia_devices_sel.get(*i).copied().unwrap_or(false))
                .map(|(_, d)| d.clone()).collect()
        } else {
            vec![]
        };
        
        let mut bind_mounts = self.passthrough.bind_list.clone();

        if self.passthrough.generic_gpu {
            if std::path::Path::new("/dev/dri").exists() { device_binds.push("/dev/dri".into()); }
            if std::path::Path::new("/dev/mali0").exists() { device_binds.push("/dev/mali0".into()); }
        }

        if self.passthrough.wayland_socket {
            bind_mounts.push(BindMount {
                source: Self::find_wayland_socket(),
                target: "/mnt/wayland-socket".into(),
                readonly: false,
            });
            bind_mounts.push(BindMount {
                source: "/tmp/.X11-unix".into(),
                target: "/tmp/.X11-unix".into(),
                readonly: false,
            });
            bind_mounts.push(BindMount {
                source: "/dev/shm".into(),
                target: "/dev/shm".into(),
                readonly: false,
            });
        }

        if self.network.mode == 0 {
            bind_mounts.push(BindMount {
                source: "/etc/resolv.conf".into(),
                target: "/etc/resolv.conf".into(),
                readonly: true,
            });
        }

        device_binds.sort();
        device_binds.dedup();

        let readonly_binds: Vec<String> = if self.passthrough.nvidia_enabled {
            let mut list: Vec<String> = self.passthrough.nvidia.system_ro.iter().enumerate()
                .filter(|(i, _)| self.passthrough.nvidia_sysro_sel.get(*i).copied().unwrap_or(false))
                .map(|(_, d)| d.clone()).collect();
            let libs: Vec<String> = self.passthrough.nvidia.driver_files.iter().enumerate()
                .filter(|(i, _)| self.passthrough.nvidia_libs_sel.get(*i).copied().unwrap_or(false))
                .map(|(_, d)| d.clone()).collect();
            list.extend(libs);
            list
        } else {
            vec![]
        };
        let storage_type = self.storage.info.types[self.storage.type_idx].0;

        let users = if self.user.enabled && !self.user.user.username.is_empty() {
            vec![self.user.user.clone()]
        } else { vec![] };

        let cfg = ContainerConfig {
            name: self.basic.name.clone(),
            hostname: if self.basic.hostname.is_empty() { self.basic.name.clone() } else { self.basic.hostname.clone() },
            network: self.network_mode(),
            port_forwards: self.network.port_list.clone(),
            bind_mounts,
            device_binds,
            readonly_binds,
            full_capabilities: self.passthrough.generic_gpu || (!self.passthrough.nvidia.devices.is_empty()
                && self.passthrough.nvidia_devices_sel.iter().any(|&b| b)),
            root_password: if self.user.root_password.is_empty() { Option::None } else { Some(self.user.root_password.clone()) },
            users,
            wayland_socket: self.passthrough.wayland_socket,
            raw_config: if storage_type == StorageType::Raw {
                Some(crate::nspawn::models::RawStorageConfig {
                    size: self.storage.raw_size.clone(),
                    fs_type: self.storage.raw_fs.clone(),
                    use_partition_table: self.storage.raw_partition,
                })
            } else { None },
        };

        if self.source.kind == SourceKind::Copy {
             let mut content = format!(" [CLONE OPERATION]\n\n Source: {}\n Destination: {}\n\n", 
                self.source.clone_source, self.basic.name);
             content.push_str(" All configuration files (.nspawn) and systemd service\n overrides will be copied automatically.");
             return ContainerConfigWithPreview { cfg, preview: content };
        }

        let mut content = format!(" [DEPLOYMENT PREVIEW — {}]\n\n", self.basic.name);
        content.push_str(&format!(" Storage: {} ({})\n", storage_type.label(), storage_type.get_path(&self.basic.name).display()));
        content.push_str(&format!(" Hostname: {}\n", cfg.hostname));
        content.push_str(&nspawn_config_content(&cfg));
        if !cfg.device_binds.is_empty() {
            content.push_str("\n# ── [systemd override.conf] ───────────────────────────\n");
            content.push_str(&systemd_override_content(&cfg.device_binds));
        }

        ContainerConfigWithPreview { cfg, preview: content }
    }

    pub fn find_wayland_socket() -> String {
        let uid = std::env::var("SUDO_UID").unwrap_or_else(|_| "1000".to_string());
        let xdg = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", uid));
        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_string());
        let path_str = format!("{}/{}", xdg, display);
        if std::path::Path::new(&path_str).exists() { return path_str; }
        if let Ok(entries) = std::fs::read_dir(&xdg) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("wayland-") && !name.ends_with(".lock") {
                    return entry.path().to_string_lossy().into_owned();
                }
            }
        }
        "/run/user/1000/wayland-0".to_string()
    }

    pub fn get_deployer_and_storage(&self) -> (Box<dyn Deployer>, Box<dyn StorageBackend>) {
        use crate::nspawn::storage::*;
        use crate::nspawn::deploy::*;

        let storage: Box<dyn StorageBackend> = match self.storage.info.types[self.storage.type_idx].0 {
            StorageType::Directory => Box::new(DirectoryBackend),
            StorageType::Subvolume => Box::new(SubvolumeBackend),
            StorageType::Raw => Box::new(RawBackend {
                config: crate::nspawn::models::RawStorageConfig {
                    size: self.storage.raw_size.clone(),
                    fs_type: self.storage.raw_fs.clone(),
                    use_partition_table: self.storage.raw_partition,
                },
            }),
        };

        let deployer: Box<dyn Deployer> = match self.source.kind {
            SourceKind::Copy => Box::new(clone::CloneDeployer { source_name: self.source.clone_source.clone() }),
            SourceKind::Oci => Box::new(image::OciDeployer { url: self.source.oci_url.clone() }),
            SourceKind::DiskImage => Box::new(image::DiskImageDeployer { path: self.source.disk_path.clone() }),
            SourceKind::Debootstrap => Box::new(bootstrap::DebootstrapDeployer { 
                mirror: self.source.deboot_mirror.clone(), 
                suite: if self.source.deboot_suite.is_empty() { "bookworm".to_string() } else { self.source.deboot_suite.clone() }
            }),
            SourceKind::Pacstrap => Box::new(bootstrap::PacstrapDeployer { packages: self.source.pacstrap_pkgs.clone() }),
        };

        (deployer, storage)
    }

    pub fn parse_port(s: &str) -> Option<PortForward> {
        let (s, proto) = if let Some(p) = s.strip_suffix("/udp") { (p, "udp") }
            else { (s.strip_suffix("/tcp").unwrap_or(s), "tcp") };
        let mut p = s.splitn(2, ':');
        let host: u16 = p.next()?.parse().ok()?;
        let container: u16 = p.next()?.parse().ok()?;
        Some(PortForward { host, container, proto: proto.to_string() })
    }

    pub fn parse_bind_mount(s: &str) -> Option<BindMount> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() < 2 { return None; }
        Some(BindMount {
            source: parts[0].to_string(),
            target: parts[1].to_string(),
            readonly: parts.get(2).map(|&r| r == "ro").unwrap_or(false),
        })
    }
}

pub struct ContainerConfigWithPreview {
    pub cfg: ContainerConfig,
    pub preview: String,
}
