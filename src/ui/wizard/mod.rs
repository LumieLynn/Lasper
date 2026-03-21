//! State management and logic for the container creation wizard.

pub mod traits;
pub mod steps;

use std::sync::{Arc, Mutex, atomic::AtomicBool};
use crossterm::event::KeyEvent;

use crate::nspawn::ContainerEntry;
use crate::nspawn::models::{BindMount, ContainerConfig, CreateUser, NetworkMode, PortForward};
use crate::nspawn::create::{nspawn_config_content, systemd_override_content};
use crate::nspawn::storage::{StorageBackend, StorageType};
use crate::nspawn::deploy::Deployer;
use crate::nspawn::nvidia::NvidiaInfo;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub use self::traits::{IStep, StepAction};

// ── Shared Context ────────────────────────────────────────────────────────────

/// The different methods available for acquiring a rootfs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceKind {
    Copy,
    Oci,
    Debootstrap,
    Pacstrap,
    DiskImage,
}

/// Holds the state for the multi-step container creation wizard, shared between steps.
pub struct WizardContext {
    pub entries: Vec<ContainerEntry>,

    // Step 1 — Source
    pub source_kind: SourceKind,
    pub source_cursor: usize,
    pub oci_url: String,
    pub deboot_mirror: String,
    pub deboot_suite: String,
    pub pacstrap_pkgs: String,
    pub disk_path: String,
    
    // Step 1.5 - CopySelect
    pub copy_cursor: usize,
    pub clone_source: String,

    // Step 2 — Basic
    pub name: String,
    pub hostname: String,
    pub field_idx: usize,

    // Step 3 — Storage
    pub storage_type_idx: usize,
    pub storage_info: crate::nspawn::storage::StorageInfo,
    pub storage_field: usize,
    pub raw_size: String,
    pub raw_fs: String,
    pub raw_partition: bool,

    // Step 4 — User
    pub root_password: String,
    pub user_enabled: bool,
    pub user: CreateUser,
    pub user_field: usize,

    // Step 5 — Network
    pub net_mode: usize,
    pub bridge_name: String,
    pub bridge_list: Vec<String>,
    pub bridge_cursor: usize,
    pub port_input: String,
    pub port_list: Vec<PortForward>,
    pub net_block: usize, // 0: Mode, 1: Bridge, 2: Port Input, 3: Port List
    pub net_h_scroll: usize,

    // Step 6 — Passthrough Toggles
    pub generic_gpu: bool,
    pub wayland_socket: bool,
    pub nvidia_enabled: bool,
    pub passthrough_field: usize,
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

    // Step 7 — Review
    pub preview: String,
    pub preview_scroll: u16,

    // Step 8 — Deploy
    pub deploy_logs: Arc<Mutex<Vec<String>>>,
    pub deploy_done: Arc<AtomicBool>,
    pub deploy_success: Arc<AtomicBool>,
    pub deploy_scroll: usize,
}

impl WizardContext {
    pub fn new(_is_root: bool) -> Self {
        Self {
            entries: Vec::new(),
            source_kind: SourceKind::Copy,
            source_cursor: 0,
            oci_url: String::new(),
            deboot_mirror: String::new(),
            deboot_suite: String::new(),
            pacstrap_pkgs: String::new(),
            disk_path: String::new(),
            copy_cursor: 0,
            clone_source: String::new(),
            name: String::new(),
            hostname: String::new(),
            field_idx: 0,
            storage_type_idx: 0,
            storage_info: crate::nspawn::storage::detect_available_storage_types(),
            storage_field: 0,
            raw_size: "2G".to_string(),
            raw_fs: "ext4".to_string(),
            raw_partition: false,
            root_password: String::new(),
            user_enabled: false,
            user: CreateUser::default(),
            user_field: 0,
            net_mode: 0,
            bridge_name: String::new(),
            bridge_list: Vec::new(),
            bridge_cursor: 0,
            port_input: String::new(),
            port_list: Vec::new(),
            net_block: 0,
            net_h_scroll: 0,
            generic_gpu: false,
            wayland_socket: false,
            nvidia_enabled: false,
            passthrough_field: 0,
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
            preview: String::new(),
            preview_scroll: 0,
            deploy_logs: Arc::new(Mutex::new(Vec::new())),
            deploy_done: Arc::new(AtomicBool::new(false)),
            deploy_success: Arc::new(AtomicBool::new(false)),
            deploy_scroll: 0,
        }
    }

    pub fn network_mode(&self) -> Option<NetworkMode> {
        match self.net_mode {
            1 => Some(NetworkMode::None),
            2 => Some(NetworkMode::Veth),
            3 => Some(NetworkMode::Bridge(self.bridge_name.clone())),
            _ => Some(NetworkMode::Host),
        }
    }

    pub fn build_config(&self) -> ContainerConfigWithPreview {
        let mut device_binds: Vec<String> = if self.nvidia_enabled {
            self.nvidia.devices.iter().enumerate()
                .filter(|(i, _)| self.nvidia_devices_sel.get(*i).copied().unwrap_or(false))
                .map(|(_, d)| d.clone()).collect()
        } else {
            vec![]
        };
        
        let mut bind_mounts = self.bind_list.clone();

        if self.generic_gpu {
            if std::path::Path::new("/dev/dri").exists() { device_binds.push("/dev/dri".into()); }
            if std::path::Path::new("/dev/mali0").exists() { device_binds.push("/dev/mali0".into()); }
        }

        if self.wayland_socket {
            bind_mounts.push(BindMount {
                source: Self::find_wayland_socket(),
                target: "/mnt/wayland-socket".into(),
                readonly: false,
            });
            bind_mounts.push(BindMount {
                source: "/dev/shm".into(),
                target: "/dev/shm".into(),
                readonly: false,
            });
        }

        if self.net_mode == 0 {
            bind_mounts.push(BindMount {
                source: "/etc/resolv.conf".into(),
                target: "/etc/resolv.conf".into(),
                readonly: true,
            });
        }

        device_binds.sort();
        device_binds.dedup();

        let readonly_binds: Vec<String> = if self.nvidia_enabled {
            let mut list: Vec<String> = self.nvidia.system_ro.iter().enumerate()
                .filter(|(i, _)| self.nvidia_sysro_sel.get(*i).copied().unwrap_or(false))
                .map(|(_, d)| d.clone()).collect();
            let libs: Vec<String> = self.nvidia.driver_files.iter().enumerate()
                .filter(|(i, _)| self.nvidia_libs_sel.get(*i).copied().unwrap_or(false))
                .map(|(_, d)| d.clone()).collect();
            list.extend(libs);
            list
        } else {
            vec![]
        };
        let storage_type = self.storage_info.types[self.storage_type_idx].0;

        let users = if self.user_enabled && !self.user.username.is_empty() {
            vec![self.user.clone()]
        } else { vec![] };

        let cfg = ContainerConfig {
            name: self.name.clone(),
            hostname: if self.hostname.is_empty() { self.name.clone() } else { self.hostname.clone() },
            network: self.network_mode(),
            port_forwards: self.port_list.clone(),
            bind_mounts,
            device_binds,
            readonly_binds,
            full_capabilities: self.generic_gpu || (!self.nvidia.devices.is_empty()
                && self.nvidia_devices_sel.iter().any(|&b| b)),
            root_password: if self.root_password.is_empty() { Option::None } else { Some(self.root_password.clone()) },
            users,
            wayland_socket: self.wayland_socket,
            raw_config: if storage_type == StorageType::Raw {
                Some(crate::nspawn::models::RawStorageConfig {
                    size: self.raw_size.clone(),
                    fs_type: self.raw_fs.clone(),
                    use_partition_table: self.raw_partition,
                })
            } else { None },
        };

        if self.source_kind == SourceKind::Copy {
             let mut content = format!(" [CLONE OPERATION]\n\n Source: {}\n Destination: {}\n\n", 
                self.clone_source, self.name);
             content.push_str(" All configuration files (.nspawn) and systemd service\n overrides will be copied automatically.");
             return ContainerConfigWithPreview { cfg, preview: content };
        }

        let mut content = format!(" [DEPLOYMENT PREVIEW — {}]\n\n", self.name);
        content.push_str(&format!(" Storage: {} ({})\n", storage_type.label(), storage_type.get_path(&self.name).display()));
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

        let storage: Box<dyn StorageBackend> = match self.storage_info.types[self.storage_type_idx].0 {
            StorageType::Directory => Box::new(DirectoryBackend),
            StorageType::Subvolume => Box::new(SubvolumeBackend),
            StorageType::Raw => Box::new(RawBackend {
                config: crate::nspawn::models::RawStorageConfig {
                    size: self.raw_size.clone(),
                    fs_type: self.raw_fs.clone(),
                    use_partition_table: self.raw_partition,
                },
            }),
        };

        let deployer: Box<dyn Deployer> = match self.source_kind {
            SourceKind::Copy => Box::new(clone::CloneDeployer { source_name: self.clone_source.clone() }),
            SourceKind::Oci => Box::new(image::OciDeployer { url: self.oci_url.clone() }),
            SourceKind::DiskImage => Box::new(image::DiskImageDeployer { path: self.disk_path.clone() }),
            SourceKind::Debootstrap => Box::new(bootstrap::DebootstrapDeployer { 
                mirror: self.deboot_mirror.clone(), 
                suite: if self.deboot_suite.is_empty() { "bookworm".to_string() } else { self.deboot_suite.clone() }
            }),
            SourceKind::Pacstrap => Box::new(bootstrap::PacstrapDeployer { packages: self.pacstrap_pkgs.clone() }),
        };

        (deployer, storage)
    }

    pub fn parse_port(s: &str) -> Option<crate::nspawn::models::PortForward> {
        let (s, proto) = if let Some(p) = s.strip_suffix("/udp") { (p, "udp") }
            else { (s.strip_suffix("/tcp").unwrap_or(s), "tcp") };
        let mut p = s.splitn(2, ':');
        let host: u16 = p.next()?.parse().ok()?;
        let container: u16 = p.next()?.parse().ok()?;
        Some(crate::nspawn::models::PortForward { host, container, proto: proto.to_string() })
    }

    pub fn parse_bind_mount(s: &str) -> Option<crate::nspawn::models::BindMount> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() < 2 { return None; }
        Some(crate::nspawn::models::BindMount {
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

// ── Wizard Manager ────────────────────────────────────────────────────────────

/// The state for the multi-step container creation wizard.
pub struct Wizard {
    pub context: WizardContext,
    pub steps: Vec<Box<dyn IStep>>,
    pub current_step_idx: usize,
}

impl Wizard {
    pub fn new(is_root: bool) -> Self {
        let context = WizardContext::new(is_root);
        let steps: Vec<Box<dyn IStep>> = vec![
            Box::new(steps::source::SourceStep::new()),
        ];
        Self {
            context,
            steps,
            current_step_idx: 0,
        }
    }

    pub fn current_step_title(&self) -> String {
        self.steps.get(self.current_step_idx).map(|s| s.title()).unwrap_or_default()
    }

    pub fn current_step(&self) -> f32 {
        if self.context.source_kind == SourceKind::Copy {
            match self.current_step_idx {
                0 => 1.0,
                1 => 2.0,
                2 => 3.0,
                3 => 4.0,
                4 => 5.0,
                _ => (self.current_step_idx + 1) as f32
            }
        } else {
            (self.current_step_idx + 1) as f32
        }
    }

    pub fn total_steps(&self) -> usize {
        if self.context.source_kind == SourceKind::Copy { 5 } else { 9 }
    }

    pub async fn handle_key(&mut self, key: KeyEvent, entries: &[ContainerEntry], _is_root: bool) -> StepAction {
        self.context.entries = entries.to_vec();
        if let Some(step) = self.steps.get_mut(self.current_step_idx) {
            let action = step.handle_key(key, &mut self.context).await;
            match action {
                StepAction::Next => {
                    self.next_step();
                    StepAction::None
                }
                StepAction::Prev => {
                    self.prev_step();
                    StepAction::None
                }
                _ => action,
            }
        } else {
            StepAction::None
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        use ratatui::widgets::{Block, Borders, Clear};
        let area = centered_rect(65, 75, area);
        f.render_widget(Clear, area);

        let title = self.current_step_title();
        let step = self.current_step();
        let total = self.total_steps();
        let header = format!(" {} (Step {:.1} / {}) ", title, step, total);

        let block = Block::default()
            .title(header)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        
        let inner = block.inner(area);
        f.render_widget(block, area);

        if let Some(step_impl) = self.steps.get_mut(self.current_step_idx) {
            step_impl.render(f, inner, &self.context);
        }
    }

    fn next_step(&mut self) {
        // Here we decide what the next step is based on the current step and context.
        // This replaces the old WizardStep enum transitions.
        let next: Option<Box<dyn IStep>> = match self.current_step_idx {
            0 => { // From Source
                if self.context.source_kind == SourceKind::Copy {
                    Some(Box::new(steps::copy_select::CopySelectStep::new()))
                } else {
                    Some(Box::new(steps::basic::BasicStep::new()))
                }
            }
            1 => { // From CopySelect or Basic
                 if self.context.source_kind == SourceKind::Copy {
                     Some(Box::new(steps::basic::BasicStep::new()))
                 } else {
                     Some(Box::new(steps::storage::StorageStep::new()))
                 }
            }
            2 => { // From Basic (Clone) or Storage
                 if self.context.source_kind == SourceKind::Copy {
                     Some(Box::new(steps::review::ReviewStep::new()))
                 } else {
                     Some(Box::new(steps::user::UserStep::new()))
                 }
            }
            3 => { // From Review (Clone) or User
                 if self.context.source_kind == SourceKind::Copy {
                     Some(Box::new(steps::deploy::DeployStep::new()))
                 } else {
                     Some(Box::new(steps::network::NetworkStep::new()))
                 }
            }
            4 => Some(Box::new(steps::passthrough::PassthroughStep::new())),
            5 => Some(Box::new(steps::devices::DevicesStep::new())),
            6 => Some(Box::new(steps::review::ReviewStep::new())),
            7 => Some(Box::new(steps::deploy::DeployStep::new())),
            _ => None,
        };

        if let Some(s) = next {
            self.steps.push(s);
            self.current_step_idx += 1;
        }
    }

    fn prev_step(&mut self) {
        if self.current_step_idx > 0 {
            self.steps.pop();
            self.current_step_idx -= 1;
        }
    }
}

pub fn centered_rect(w_pct: u16, h_pct: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - h_pct) / 2),
            Constraint::Percentage(h_pct),
            Constraint::Percentage((100 - h_pct) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - w_pct) / 2),
            Constraint::Percentage(w_pct),
            Constraint::Percentage((100 - w_pct) / 2),
        ])
        .split(vert[1])[1]
}

pub fn render_hint(f: &mut Frame, area: Rect, hints: &[&str]) {
    let mut spans = vec![Span::raw("  ")];
    for hint in hints {
        if let Some(sp) = hint.find(' ') {
            let (k, d) = hint.split_at(sp);
            spans.push(Span::styled(k.to_string(), Style::default().fg(Color::Cyan)));
            spans.push(Span::styled(format!("{}  ", d), Style::default().fg(Color::DarkGray)));
        } else {
            spans.push(Span::styled(hint.to_string(), Style::default().fg(Color::Cyan)));
            spans.push(Span::raw("  "));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
