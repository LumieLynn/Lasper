use super::{NspawnConfigSpec, ALL_DRM_DEVICES_PATH};
use crate::adapters::error::{NspawnError, Result};
use crate::adapters::wayland::WaylandBind;
use crate::domain::wayland::WaylandBindPolicy;
use ini::{EscapePolicy, Ini};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

const NVIDIA_BLOCK_BEGIN: &str = "X-Lasper-Nvidia-Begin=managed-by-lasper";
const NVIDIA_BLOCK_END: &str = "X-Lasper-Nvidia-End=true";

fn line_body(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn is_files_section(line: &str) -> bool {
    line.trim().eq_ignore_ascii_case("[files]")
}

fn is_section_header(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('[') && line.ends_with(']')
}

pub(crate) fn is_nvidia_begin_marker(line: &str) -> bool {
    line.trim() == NVIDIA_BLOCK_BEGIN
}

pub(crate) fn is_nvidia_end_marker(line: &str) -> bool {
    line.trim() == NVIDIA_BLOCK_END
}

pub(crate) fn escape_nspawn_bind_path(path: &str) -> String {
    path.replace('\\', "\\\\").replace(':', "\\:")
}

pub(crate) fn parse_nspawn_bind_paths(value: &str) -> Option<(String, String)> {
    let fields = parse_nspawn_bind_fields(value)?;
    let source = fields.first()?.trim().to_string();
    if source.is_empty() {
        return None;
    }
    let destination = fields
        .get(1)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or(&source)
        .to_string();
    Some((source, destination))
}

fn parse_nspawn_bind_fields(value: &str) -> Option<Vec<String>> {
    let mut fields = vec![String::new()];
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\\' => match chars.peek().copied() {
                Some('\\' | ':') => {
                    fields.last_mut()?.push(chars.next()?);
                }
                Some(_) | None => fields.last_mut()?.push('\\'),
            },
            ':' => fields.push(String::new()),
            _ => fields.last_mut()?.push(character),
        }
    }

    Some(fields)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NspawnBindSemantics {
    source: String,
    destination: String,
    readonly: bool,
    options: Vec<String>,
}

impl NspawnBindSemantics {
    fn from_passthrough(bind: &crate::adapters::platform::nvidia::state::PassthroughBind) -> Self {
        Self {
            source: bind.host_path.clone(),
            destination: bind.container_path.clone(),
            readonly: bind.readonly,
            options: Vec::new(),
        }
    }
}

fn parse_nspawn_bind_line(line: &str) -> Option<NspawnBindSemantics> {
    let (key, value) = line.trim().split_once('=')?;
    let readonly = match key.trim() {
        "Bind" => false,
        "BindReadOnly" => true,
        _ => return None,
    };
    parse_nspawn_bind_value(value, readonly)
}

fn parse_nspawn_bind_value(value: &str, readonly: bool) -> Option<NspawnBindSemantics> {
    let fields = parse_nspawn_bind_fields(value.trim())?;
    if fields.is_empty() || fields.len() > 3 {
        return None;
    }

    let source = fields[0].trim().to_string();
    if source.is_empty() {
        return None;
    }
    let destination = fields
        .get(1)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or(&source)
        .to_string();
    let options = fields
        .get(2)
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|option| !option.is_empty())
        .map(str::to_string)
        .collect();

    Some(NspawnBindSemantics {
        source,
        destination,
        readonly,
        options,
    })
}

/// Raw content of a `.nspawn` file and the path it was read from.
pub struct NspawnConfig {
    pub path: PathBuf,
    pub content: String,
}

/// Validates a container name matches systemd machine name constraints.
/// Defense-in-depth: the wizard UI already validates this, but backend
/// must not trust inputs blindly in case of restricted-sudo environments.
#[cfg(test)]
fn validate_machine_name(name: &str) -> Result<()> {
    crate::domain::machine::MachineName::new(name)
        .map(|_| ())
        .map_err(|error| NspawnError::Validation(error.to_string()))
}

impl NspawnConfig {
    pub fn default_path(name: &str) -> PathBuf {
        PathBuf::from(format!("/etc/systemd/nspawn/{}.nspawn", name))
    }

    /// Find declared targets for this exact host socket, including source
    /// aliases. Guest paths are configuration data, not provisioning policy.
    pub(crate) async fn wayland_targets(&self, source: &Path) -> Result<Vec<PathBuf>> {
        // Preserve nspawn's own `\:` and `\\` path escapes until the bind
        // codec below has split source, destination, and mount options.
        let conf = Ini::load_from_str_noescape(&self.content).map_err(|error| {
            NspawnError::InvalidConfig(format!("failed to parse {}: {error}", self.path.display()))
        })?;
        let mut targets = Vec::new();
        for files in conf.section_all(Some("Files")) {
            for (key, value) in files.iter() {
                let readonly = match key {
                    "Bind" => false,
                    "BindReadOnly" => true,
                    _ => continue,
                };
                let Some(bind) = parse_nspawn_bind_value(value, readonly) else {
                    continue;
                };
                let bind_source = Path::new(&bind.source);
                if !bind_source.is_absolute() {
                    continue;
                }
                if bind_source != source
                    && tokio::fs::canonicalize(bind_source).await.ok().as_deref() != Some(source)
                {
                    continue;
                }
                let target = PathBuf::from(bind.destination);
                if !targets.contains(&target) {
                    if targets.len() >= 16 {
                        return Err(NspawnError::InvalidConfig(
                            "too many targets for one Wayland socket".into(),
                        ));
                    }
                    targets.push(target);
                }
            }
        }
        Ok(targets)
    }

    /// Check if the NVIDIA GPU passthrough is enabled for this container.
    pub fn is_gpu_enabled(&self) -> Result<bool> {
        let conf = Ini::load_from_str(&self.content).map_err(|error| {
            NspawnError::InvalidConfig(format!(
                "failed to parse {} while checking NVIDIA passthrough: {error}",
                self.path.display()
            ))
        })?;
        // Read legacy marker locations as well as the current [Files] location.
        let enabled_msg = "X-Lasper-Nvidia-Enabled";
        let in_files = conf.get_from(Some("Files"), enabled_msg);
        let in_general = conf.get_from(Some("General"), enabled_msg);
        let in_global = conf.get_from(None::<&str>, enabled_msg);

        Ok(in_files
            .or(in_general)
            .or(in_global)
            .is_some_and(|value| value.eq_ignore_ascii_case("true")))
    }

    /// Scans the raw content for markers and removes the block.
    pub fn purge_nvidia_block(content: &str) -> Result<(String, Vec<String>)> {
        let mut in_files = false;
        let mut start_offset = None;
        let mut end_offset = None;
        let mut death_list = Vec::new();
        let mut offset = 0usize;

        for line in content.split_inclusive('\n') {
            let body = line_body(line);
            if is_section_header(body) {
                if start_offset.is_some() && end_offset.is_none() {
                    return Err(NspawnError::Runtime(
                        "Incomplete NVIDIA markers cross a section boundary".into(),
                    ));
                }
                in_files = is_files_section(body);
                offset += line.len();
                continue;
            }

            if in_files && is_nvidia_begin_marker(body) {
                if start_offset.is_some() {
                    return Err(NspawnError::Runtime(
                        "Duplicate X-Lasper-Nvidia-Begin marker".into(),
                    ));
                }
                start_offset = Some(offset);
            } else if in_files && is_nvidia_end_marker(body) {
                if start_offset.is_none() || end_offset.is_some() {
                    return Err(NspawnError::Runtime(
                        "X-Lasper-Nvidia-End marker has no matching begin marker".into(),
                    ));
                }
                end_offset = Some(offset + line.len());
            } else if start_offset.is_some() && end_offset.is_none() {
                let trimmed = body.trim();
                if let Some((key, value)) = trimmed.split_once('=') {
                    if matches!(key.trim(), "Bind" | "BindReadOnly") {
                        death_list.push(value.to_string());
                    }
                }
            }
            offset += line.len();
        }

        match (start_offset, end_offset) {
            (Some(start), Some(end)) => {
                let mut clean = String::with_capacity(content.len() - (end - start));
                clean.push_str(&content[..start]);
                clean.push_str(&content[end..]);
                Ok((clean, death_list))
            }
            (None, None) => Ok((content.to_string(), Vec::new())),
            _ => Err(NspawnError::Runtime(
                "Incomplete markers found: one is missing".into(),
            )),
        }
    }

    /// Pure AST surgery on a string content.
    pub fn apply_gpu_passthrough_to_content(
        content: String,
        new_state: &crate::adapters::platform::nvidia::NvidiaState,
        removed_binds: &[crate::adapters::platform::nvidia::state::PassthroughBind],
    ) -> Result<String> {
        // 1. Purge existing block using markers (preserves everything else)
        let (clean_content, _extracted_deaths) = Self::purge_nvidia_block(&content)?;

        // 2. Remove only complete semantic duplicates from marker-external
        // configuration. A destination collision with different semantics is
        // administrator-owned content and must stop the update unchanged.
        let new_binds = new_state
            .binds
            .iter()
            .map(NspawnBindSemantics::from_passthrough)
            .collect::<HashSet<_>>();
        let removed_binds = removed_binds
            .iter()
            .map(NspawnBindSemantics::from_passthrough)
            .collect::<HashSet<_>>();
        let new_destinations = new_binds
            .iter()
            .map(|bind| bind.destination.clone())
            .collect::<HashSet<_>>();
        let mut conflicts = BTreeSet::new();
        let mut result = String::with_capacity(clean_content.len());
        let mut in_files = false;
        let mut first_files_seen = false;
        let mut first_files_open = false;
        let mut insert_at = None;

        for line in clean_content.split_inclusive('\n') {
            let body = line_body(line);
            if is_section_header(body) {
                if first_files_open {
                    insert_at = Some(result.len());
                    first_files_open = false;
                }
                in_files = is_files_section(body);
                if in_files && !first_files_seen {
                    first_files_seen = true;
                    first_files_open = true;
                }
                result.push_str(line);
                continue;
            }

            if in_files {
                if let Some(bind) = parse_nspawn_bind_line(body) {
                    if new_binds.contains(&bind) || removed_binds.contains(&bind) {
                        continue;
                    }
                    if new_destinations.contains(&bind.destination) {
                        conflicts.insert(bind.destination);
                    }
                }
            }
            result.push_str(line);
        }
        if first_files_open {
            insert_at = Some(result.len());
        }

        if !conflicts.is_empty() {
            return Err(NspawnError::InvalidConfig(format!(
                "NVIDIA bind conflicts with administrator-owned entries at container path(s): {}",
                conflicts.into_iter().collect::<Vec<_>>().join(", ")
            )));
        }

        // 3. Build the new managed block from unified PassthroughBind list
        if !new_state.binds.is_empty() {
            let mut block = vec![NVIDIA_BLOCK_BEGIN.to_string()];
            for bind in &new_state.binds {
                let host_path = escape_nspawn_bind_path(&bind.host_path);
                let container_path = escape_nspawn_bind_path(&bind.container_path);
                if bind.readonly {
                    if bind.host_path == bind.container_path {
                        block.push(format!("BindReadOnly={host_path}"));
                    } else {
                        block.push(format!("BindReadOnly={host_path}:{container_path}"));
                    }
                } else if bind.host_path == bind.container_path {
                    block.push(format!("Bind={host_path}"));
                } else {
                    block.push(format!("Bind={host_path}:{container_path}"));
                }
            }
            block.push(NVIDIA_BLOCK_END.to_string());

            let line_ending = if clean_content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let preserve_final_newline = clean_content.ends_with('\n');
            result = match insert_at {
                Some(offset) => {
                    insert_block(result, offset, &block, line_ending, preserve_final_newline)
                }
                None => append_files_block(result, &block, line_ending, preserve_final_newline),
            };
        }

        Ok(result)
    }
}

fn insert_block(
    content: String,
    offset: usize,
    block: &[String],
    line_ending: &str,
    preserve_final_newline: bool,
) -> String {
    let (prefix, suffix) = content.split_at(offset);
    let mut result =
        String::with_capacity(content.len() + block.iter().map(String::len).sum::<usize>());
    result.push_str(prefix);
    if !prefix.is_empty() && !prefix.ends_with('\n') {
        result.push_str(line_ending);
    }
    result.push_str(&block.join(line_ending));
    if !suffix.is_empty() || preserve_final_newline {
        result.push_str(line_ending);
    }
    result.push_str(suffix);
    result
}

fn append_files_block(
    mut content: String,
    block: &[String],
    line_ending: &str,
    preserve_final_newline: bool,
) -> String {
    if !content.is_empty() {
        if !content.ends_with('\n') {
            content.push_str(line_ending);
        }
        if !content.ends_with(&format!("{line_ending}{line_ending}")) {
            content.push_str(line_ending);
        }
    }
    content.push_str("[Files]");
    content.push_str(line_ending);
    content.push_str(&block.join(line_ending));
    if preserve_final_newline {
        content.push_str(line_ending);
    }
    content
}

//.nspawn file generation

pub(crate) fn nspawn_config_content_from_spec_with_wayland_binds(
    spec: &NspawnConfigSpec,
    wayland_binds: &[WaylandBind],
) -> Result<String> {
    spec.validate()?;
    if !wayland_binds.is_empty() {
        validate_wayland_endpoint_available(spec)?;
    }
    let passthrough_all_drm = spec.gpu_passthrough_all
        || spec
            .device_binds
            .iter()
            .any(|path| path == ALL_DRM_DEVICES_PATH);
    let mut conf = Ini::new();

    //[Exec]
    {
        let mut exec = conf.with_section(Some("Exec"));
        if spec.boot {
            exec.set("Boot", "yes");
        } else {
            exec.set("Boot", "no");
        }

        if let Some(mode) = spec.private_users {
            exec.set("PrivateUsers", mode.as_str());
        }

        if let Some(mode) = spec.resolv_conf {
            exec.set("ResolvConf", mode.as_str());
        }

        if spec.privileged {
            exec.set("Capability", "all");
        }
        if spec.guest_hostname.as_str() != spec.machine.as_str() {
            exec.set("Hostname", spec.guest_hostname.as_str());
        }
    }

    //[Network]
    if let Some(mode) = &spec.network {
        use crate::domain::provisioning::NetworkMode;
        match mode {
            NetworkMode::Host => {
                conf.with_section(Some("Network"))
                    .set("VirtualEthernet", "no");
            }
            NetworkMode::None => {
                conf.with_section(Some("Network")).set("Private", "yes");
            }
            NetworkMode::Veth => {
                conf.with_section(Some("Network"))
                    .set("VirtualEthernet", "yes");
                let net = conf.section_mut(Some("Network")).unwrap();
                for pf in &spec.port_forwards {
                    net.append(
                        "Port",
                        format!("{}:{}:{}", pf.protocol.as_str(), pf.host, pf.container),
                    );
                }
            }
            NetworkMode::Bridge(name) => {
                conf.with_section(Some("Network"))
                    .set("VirtualEthernet", "yes")
                    .set("Bridge", name.clone());
                let net = conf.section_mut(Some("Network")).unwrap();
                for pf in &spec.port_forwards {
                    net.append(
                        "Port",
                        format!("{}:{}:{}", pf.protocol.as_str(), pf.host, pf.container),
                    );
                }
            }
            NetworkMode::MacVlan(iface) => {
                conf.with_section(Some("Network"))
                    .set("Private", "yes")
                    .set("VirtualEthernet", "no")
                    .set("MACVLAN", iface.clone());
            }
            NetworkMode::IpVlan(iface) => {
                conf.with_section(Some("Network"))
                    .set("Private", "yes")
                    .set("VirtualEthernet", "no")
                    .set("IPVLAN", iface.clone());
            }
            NetworkMode::Interface(iface) => {
                conf.with_section(Some("Network"))
                    .set("Private", "yes")
                    .set("VirtualEthernet", "no")
                    .set("Interface", iface.clone());
            }
        }
    }

    //[Files]
    let has_files = !spec.device_binds.is_empty()
        || !spec.readonly_binds.is_empty()
        || !spec.bind_mounts.is_empty()
        || !wayland_binds.is_empty()
        || passthrough_all_drm
        || spec.nvidia_gpu;

    if has_files {
        conf.with_section(Some("Files")).set("__ensure_files", "");
        let files = conf.section_mut(Some("Files")).unwrap();
        files.remove("__ensure_files");
        if spec.nvidia_gpu {
            files.append("X-Lasper-Nvidia-Enabled", "true");
        }

        if passthrough_all_drm {
            files.append("Bind", ALL_DRM_DEVICES_PATH);
        }

        for dev in &spec.device_binds {
            if dev == ALL_DRM_DEVICES_PATH {
                continue;
            }
            files.append("Bind", escape_nspawn_bind_path(dev));
        }
        for ro in &spec.readonly_binds {
            files.append("BindReadOnly", escape_nspawn_bind_path(ro));
        }
        for bm in &spec.bind_mounts {
            let source = escape_nspawn_bind_path(&bm.source);
            let target = escape_nspawn_bind_path(&bm.target);
            if bm.readonly {
                files.append("BindReadOnly", format!("{source}:{target}{}", bm.suffix));
            } else {
                files.append("Bind", format!("{source}:{target}{}", bm.suffix));
            }
        }

        for wayland_bind in wayland_binds {
            let suffix = match wayland_bind.policy() {
                WaylandBindPolicy::Idmap => ":idmap",
                WaylandBindPolicy::NoIdmap => ":noidmap",
            };
            let source = validated_nspawn_path("Wayland socket path", wayland_bind.source())?;
            let target = validated_nspawn_path("Wayland container path", wayland_bind.target())?;
            let source = escape_nspawn_bind_path(source);
            let target = escape_nspawn_bind_path(target);
            files.append("Bind", format!("{source}:{target}{suffix}"));
        }

        // Individual device binds are populated in cfg.device_binds. The
        // complete /dev/dri directory is emitted only for explicit opt-in.
    }

    let mut buffer = Vec::new();
    // Values have already been validated and systemd-specific bind escaping has
    // already been applied. Generic INI escaping would double those backslashes.
    conf.write_to_policy(&mut buffer, EscapePolicy::Nothing)
        .map_err(|e| NspawnError::Runtime(format!("Failed to serialize INI: {}", e)))?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn validate_wayland_endpoint_available(spec: &NspawnConfigSpec) -> Result<()> {
    let endpoint =
        normalized_absolute_path(Path::new(crate::adapters::wayland::CONTAINER_WAYLAND_ROOT))
            .expect("the adapter-owned Wayland endpoint is an absolute normalized path");
    let conflict = spec
        .bind_mounts
        .iter()
        .map(|bind| bind.target.as_str())
        .chain(spec.device_binds.iter().map(String::as_str))
        .chain(spec.readonly_binds.iter().map(String::as_str))
        .any(|target| {
            normalized_absolute_path(Path::new(target)).is_some_and(|target| {
                target.starts_with(&endpoint) || endpoint.starts_with(&target)
            })
        });
    if conflict {
        return Err(NspawnError::Validation(format!(
            "Bind target {} is reserved for the Wayland grant",
            crate::adapters::wayland::CONTAINER_WAYLAND_ROOT
        )));
    }
    Ok(())
}

fn normalized_absolute_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn validated_nspawn_path<'a>(label: &str, path: &'a Path) -> Result<&'a str> {
    let path = path
        .to_str()
        .ok_or_else(|| NspawnError::Validation(format!("{label} is not valid UTF-8")))?;
    if path.chars().any(char::is_control) {
        return Err(NspawnError::Validation(format!(
            "{label} contains control characters"
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::provisioning::MachineProvisioningConfig;
    use crate::domain::provisioning::{BindMount, IdmapSuffix};

    fn nspawn_config_content(cfg: &MachineProvisioningConfig) -> Result<String> {
        let spec = NspawnConfigSpec::try_from(cfg)?;
        nspawn_config_content_from_spec_with_wayland_binds(&spec, &[])
    }
    use crate::domain::provisioning::{NetworkMode, PortForward, PrivateUsersMode};

    // Validation

    #[test]
    fn test_validate_machine_name_valid() {
        assert!(validate_machine_name("my-container").is_ok());
        assert!(validate_machine_name("test-01").is_ok());
        assert!(validate_machine_name("a.b").is_ok());
    }

    #[test]
    fn test_validate_machine_name_empty() {
        assert!(validate_machine_name("").is_err());
    }

    #[test]
    fn test_validate_machine_name_boundary_length() {
        assert!(validate_machine_name("a").is_ok());
        assert!(validate_machine_name(&"a".repeat(64)).is_ok());
        assert!(validate_machine_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn test_validate_machine_name_invalid_chars() {
        assert!(validate_machine_name("foo/bar").is_err());
        assert!(validate_machine_name("a b").is_err());
        assert!(validate_machine_name("rm -rf").is_err());
    }

    #[test]
    fn test_validate_machine_name_path_traversal() {
        assert!(validate_machine_name(".hidden").is_err());
        assert!(validate_machine_name("foo..bar").is_err());
        assert!(validate_machine_name("../../../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_machine_name_injection_attacks() {
        assert!(validate_machine_name("foo\0bar").is_err());
        assert!(validate_machine_name("foo\nbar").is_err());
        assert!(validate_machine_name("foo;rm -rf /").is_err());
        assert!(validate_machine_name("$(whoami)").is_err());
    }

    // GPU enabled detection

    #[test]
    fn test_is_gpu_enabled_true() {
        let config = NspawnConfig {
            path: PathBuf::from("test.nspawn"),
            content: "[General]\nX-Lasper-Nvidia-Enabled=true".to_string(),
        };
        assert!(config.is_gpu_enabled().unwrap());
    }

    #[test]
    fn test_is_gpu_enabled_false_value() {
        let config = NspawnConfig {
            path: PathBuf::from("test.nspawn"),
            content: "[General]\nX-Lasper-Nvidia-Enabled=false".to_string(),
        };
        assert!(!config.is_gpu_enabled().unwrap());
    }

    #[test]
    fn test_is_gpu_enabled_missing_key() {
        let config = NspawnConfig {
            path: PathBuf::from("test.nspawn"),
            content: "[General]\nSomeOther=value".to_string(),
        };
        assert!(!config.is_gpu_enabled().unwrap());
    }

    #[test]
    fn test_is_gpu_enabled_empty_content() {
        let config = NspawnConfig {
            path: PathBuf::from("test.nspawn"),
            content: "".to_string(),
        };
        assert!(!config.is_gpu_enabled().unwrap());
    }

    #[test]
    fn test_is_gpu_enabled_malformed_ini() {
        let config = NspawnConfig {
            path: PathBuf::from("test.nspawn"),
            content: "not valid ini [[[[".to_string(),
        };
        let error = config.is_gpu_enabled().unwrap_err().to_string();
        assert!(error.contains("failed to parse test.nspawn"));
        assert!(error.contains("NVIDIA passthrough"));
    }

    // Purge nvidia block

    #[test]
    fn test_purge_nvidia_block_present() {
        let content = "[Files]\nLine 1\nX-Lasper-Nvidia-Begin=managed-by-lasper\nBind=/dev/nvidia0\nBindReadOnly=/usr/lib/libcuda.so\nX-Lasper-Nvidia-End=true\nLine 2";
        let (new_content, death_list) = NspawnConfig::purge_nvidia_block(content).unwrap();
        assert_eq!(new_content, "[Files]\nLine 1\nLine 2");
        assert_eq!(death_list, vec!["/dev/nvidia0", "/usr/lib/libcuda.so"]);
    }

    #[test]
    fn test_purge_nvidia_block_absent() {
        let content = "Line 1\nLine 2";
        let (new_content, death_list) = NspawnConfig::purge_nvidia_block(content).unwrap();
        assert_eq!(new_content, content);
        assert!(death_list.is_empty());
    }

    #[test]
    fn test_purge_nvidia_block_begin_only() {
        let content = "[Files]\nX-Lasper-Nvidia-Begin=managed-by-lasper\nLine 1";
        assert!(NspawnConfig::purge_nvidia_block(content).is_err());
    }

    #[test]
    fn test_purge_nvidia_block_end_only() {
        let content = "[Files]\nLine 1\nX-Lasper-Nvidia-End=true\nLine 2";
        assert!(NspawnConfig::purge_nvidia_block(content).is_err());
    }

    #[test]
    fn test_purge_nvidia_block_duplicate_begin() {
        let content = "[Files]\nX-Lasper-Nvidia-Begin=managed-by-lasper\nX-Lasper-Nvidia-Begin=managed-by-lasper\nX-Lasper-Nvidia-End=true";
        assert!(NspawnConfig::purge_nvidia_block(content).is_err());
    }

    #[test]
    fn test_purge_nvidia_block_reversed_markers() {
        let content = "[Files]\nX-Lasper-Nvidia-End=true\nBind=/dev/nvidia0\nX-Lasper-Nvidia-Begin=managed-by-lasper";
        assert!(NspawnConfig::purge_nvidia_block(content).is_err());
    }

    #[test]
    fn test_purge_nvidia_block_empty_block() {
        let content = "[Files]\nLine 1\nX-Lasper-Nvidia-Begin=managed-by-lasper\nX-Lasper-Nvidia-End=true\nLine 2";
        let (new_content, death_list) = NspawnConfig::purge_nvidia_block(content).unwrap();
        assert_eq!(new_content, "[Files]\nLine 1\nLine 2");
        assert!(death_list.is_empty());
    }

    #[test]
    fn purge_nvidia_block_ignores_marker_shaped_content_outside_files() {
        let content = "[Exec]\nX-Lasper-Nvidia-Begin=managed-by-lasper\nX-Lasper-Nvidia-End=true\n";

        let (new_content, death_list) = NspawnConfig::purge_nvidia_block(content).unwrap();

        assert_eq!(new_content, content);
        assert!(death_list.is_empty());
    }

    // Config content generation

    #[test]
    fn test_nspawn_config_content_minimal() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            boot: true,
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("[Exec]"));
        assert!(content.contains("Boot=yes"));
    }

    #[test]
    fn test_nspawn_config_content_boot_disabled() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            boot: false,
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("Boot=no"));
    }

    #[test]
    fn test_nspawn_config_content_host_network() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            network: Some(NetworkMode::Host),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("VirtualEthernet=no"));
        assert!(content.contains("ResolvConf=bind-host"));
        assert!(!content.contains("BindReadOnly=/etc/resolv.conf"));
    }

    #[test]
    fn test_nspawn_config_content_network_veth_with_ports() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            network: Some(NetworkMode::Veth),
            port_forwards: vec![
                PortForward {
                    host: 8080,
                    container: 80,
                    proto: "tcp".to_string(),
                },
                PortForward {
                    host: 4443,
                    container: 443,
                    proto: "tcp".to_string(),
                },
            ],
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("VirtualEthernet=yes"));
        assert!(content.contains("ResolvConf=off"));
        assert!(content.contains("Port=tcp:8080:80"));
        assert!(content.contains("Port=tcp:4443:443"));
    }

    #[test]
    fn test_nspawn_config_content_bridge_mode() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            network: Some(NetworkMode::Bridge("br0".into())),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("Bridge=br0"));
    }

    #[test]
    fn test_nspawn_config_content_privileged() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            privileged: true,
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("Capability=all"));
    }

    #[test]
    fn test_nspawn_config_content_private_users_explicit_no() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            private_users: Some(PrivateUsersMode::No),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("PrivateUsers=no"));
    }

    #[test]
    fn test_nspawn_config_content_private_users_explicit_yes() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            private_users: Some(PrivateUsersMode::Yes),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("PrivateUsers=yes"));
    }

    #[test]
    fn test_nspawn_config_content_private_users_pick() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            private_users: Some(PrivateUsersMode::Pick),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("PrivateUsers=pick"));
    }

    #[test]
    fn test_nspawn_config_content_private_users_managed() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            network: Some(NetworkMode::None),
            private_users: Some(PrivateUsersMode::Managed),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("PrivateUsers=managed"));
    }

    #[test]
    fn test_nspawn_config_content_private_users_identity() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            private_users: Some(PrivateUsersMode::Identity),
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("PrivateUsers=identity"));
    }

    #[test]
    fn test_nspawn_config_content_nvidia_marker() {
        let cfg = MachineProvisioningConfig {
            name: "test".to_string(),
            nvidia_gpu: true,
            ..Default::default()
        };
        let content = nspawn_config_content(&cfg).unwrap();
        assert!(content.contains("X-Lasper-Nvidia-Enabled=true"));
        assert!(content.contains("[Files]"));
        assert!(!content.contains("[General]"));
    }

    #[test]
    fn test_nspawn_config_content_rejects_invalid_name() {
        let cfg = MachineProvisioningConfig {
            name: "../escape".to_string(),
            ..Default::default()
        };
        assert!(nspawn_config_content(&cfg).is_err());
    }

    // GPU passthrough surgery

    #[test]
    fn test_apply_gpu_passthrough_to_content() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let content = "[Exec]\nBoot=yes\n".to_string();
        let new_state = crate::adapters::platform::nvidia::NvidiaState {
            binds: vec![
                PassthroughBind {
                    host_path: "/dev/nvidia0".into(),
                    container_path: "/dev/dri/by-path/nvidia-card".into(),
                    readonly: false,
                },
                PassthroughBind {
                    host_path: "/usr/lib/libcuda.so".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
            ],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(updated.contains("[Files]"));
        assert!(updated.contains("X-Lasper-Nvidia-Begin=managed-by-lasper"));
        assert!(updated.contains("Bind=/dev/nvidia0:/dev/dri/by-path/nvidia-card"));
        assert!(updated.contains("BindReadOnly=/usr/lib/libcuda.so"));
        assert!(updated.contains("X-Lasper-Nvidia-End=true"));
    }

    #[test]
    fn nspawn_bind_codec_preserves_colons_inside_paths() {
        let path = "/dev/dri/by-path/pci-0000:01:00.0-card";
        assert_eq!(
            escape_nspawn_bind_path(path),
            r"/dev/dri/by-path/pci-0000\:01\:00.0-card"
        );
        assert_eq!(
            parse_nspawn_bind_paths(
                r"/dev/dri/card0:/dev/dri/by-path/pci-0000\:01\:00.0-card:noidmap"
            ),
            Some(("/dev/dri/card0".into(), path.into()))
        );
    }

    #[tokio::test]
    async fn wayland_targets_match_only_the_selected_source_and_allow_custom_paths() {
        let config = NspawnConfig {
            path: PathBuf::from("demo.nspawn"),
            content: concat!(
                "[Files]\n",
                "Bind=/run/user/1000/wayland-0:/custom/display.sock:idmap\n",
                "Bind=\n",
                "Bind=/run/user/1000/wayland-1:/run/lasper/wayland/1000/wayland-1:idmap\n",
                "Bind=/run/user/1000/wayland-0:/custom/display.sock:idmap\n",
                "BindReadOnly=/run/user/1000/wayland-0:/other/display\\:0\n",
                "[Exec]\nBind=/run/user/1000/wayland-0:/not-a-files-bind\n",
            )
            .into(),
        };

        // Empty Bind= is invalid and ignored by systemd; it is not a reset.
        assert_eq!(
            config
                .wayland_targets(Path::new("/run/user/1000/wayland-0"))
                .await
                .unwrap(),
            vec![
                PathBuf::from("/custom/display.sock"),
                PathBuf::from("/other/display:0")
            ]
        );
        assert_eq!(
            config
                .wayland_targets(Path::new("/run/user/1000/wayland-1"))
                .await
                .unwrap(),
            vec![PathBuf::from("/run/lasper/wayland/1000/wayland-1")]
        );
        assert!(config
            .wayland_targets(Path::new("/run/user/1000/wayland-2"))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn wayland_targets_resolve_source_aliases_and_implicit_destination() {
        let runtime = tempfile::tempdir().unwrap();
        let source = runtime.path().join("compositor.sock");
        std::fs::write(&source, []).unwrap();
        let alias = runtime.path().join("wayland-1");
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        let config = NspawnConfig {
            path: PathBuf::from("demo.nspawn"),
            content: format!("[Files]\nBind={}\n", alias.display()),
        };
        assert_eq!(config.wayland_targets(&source).await.unwrap(), vec![alias]);
    }

    #[test]
    fn gpu_bind_escapes_pci_colons_in_container_destination() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let state = crate::adapters::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/dri/card0".into(),
                container_path: "/dev/dri/by-path/pci-0000:01:00.0-card".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content("[Files]\n".into(), &state, &[])
                .unwrap();
        assert!(updated.contains(r"Bind=/dev/dri/card0:/dev/dri/by-path/pci-0000\:01\:00.0-card"));
    }

    #[test]
    fn ordinary_binds_escape_colons_and_backslashes() {
        use crate::domain::provisioning::{BindMount, IdmapSuffix};

        let cfg = MachineProvisioningConfig {
            name: "test".into(),
            device_binds: vec![r"/dev/dri/by-path/pci-0000:01:00.0-card".into()],
            readonly_binds: vec![r"/srv/driver\archive:current".into()],
            bind_mounts: vec![BindMount {
                source: r"/srv/source:one\two".into(),
                target: r"/mnt/target:one\two".into(),
                readonly: true,
                suffix: IdmapSuffix::Noidmap,
            }],
            ..Default::default()
        };

        let content = nspawn_config_content(&cfg).unwrap();
        assert!(
            content.contains(r"Bind=/dev/dri/by-path/pci-0000\:01\:00.0-card"),
            "{content}"
        );
        assert!(
            content.contains(r"BindReadOnly=/srv/driver\\archive\:current"),
            "{content}"
        );
        assert!(
            content.contains(r"BindReadOnly=/srv/source\:one\\two:/mnt/target\:one\\two:noidmap"),
            "{content}"
        );
    }

    #[test]
    fn all_drm_passthrough_is_explicit_and_deduplicated() {
        let cfg = MachineProvisioningConfig {
            name: "test".into(),
            graphics_acceleration: true,
            gpu_passthrough_all: true,
            device_binds: vec![ALL_DRM_DEVICES_PATH.into(), "/dev/dri/card0".into()],
            ..Default::default()
        };

        let content = nspawn_config_content(&cfg).unwrap();
        assert_eq!(content.matches("Bind=/dev/dri\n").count(), 1, "{content}");
        assert!(content.contains("Bind=/dev/dri/card0"), "{content}");
    }

    #[test]
    fn wayland_passthrough_binds_only_the_verified_socket() {
        let cfg = MachineProvisioningConfig {
            name: "test".into(),
            network: Some(NetworkMode::Veth),
            private_users: Some(PrivateUsersMode::Pick),
            ..Default::default()
        };
        let spec = NspawnConfigSpec::try_from(&cfg).unwrap();
        let runtime = Path::new("/run/user/1000/wayland-0");
        let display = crate::domain::wayland::WaylandDisplay::new("wayland-0").unwrap();
        let bind = WaylandBind::new(runtime, 1000, &display, WaylandBindPolicy::Idmap);
        let content = nspawn_config_content_from_spec_with_wayland_binds(&spec, &[bind]).unwrap();

        assert!(content
            .contains("Bind=/run/user/1000/wayland-0:/run/lasper/wayland/1000/wayland-0:idmap"));
        assert!(!content.contains("X11"));
        assert!(!content.contains("host-x11"));
        assert!(!content.contains("Bind=/dev/dri"));
    }

    #[test]
    fn wayland_passthrough_emits_each_selected_display_under_one_user_namespace() {
        let spec = NspawnConfigSpec::try_from(&MachineProvisioningConfig {
            name: "test".into(),
            private_users: Some(PrivateUsersMode::Pick),
            ..Default::default()
        })
        .unwrap();
        let first = WaylandBind::new(
            "/run/user/1000/wayland-0",
            1000,
            &crate::domain::wayland::WaylandDisplay::new("wayland-0").unwrap(),
            WaylandBindPolicy::Idmap,
        );
        let second = WaylandBind::new(
            "/run/user/1000/wayland-2",
            1000,
            &crate::domain::wayland::WaylandDisplay::new("wayland-2").unwrap(),
            WaylandBindPolicy::Idmap,
        );

        let content =
            nspawn_config_content_from_spec_with_wayland_binds(&spec, &[first, second]).unwrap();

        assert!(content
            .contains("Bind=/run/user/1000/wayland-0:/run/lasper/wayland/1000/wayland-0:idmap"));
        assert!(content
            .contains("Bind=/run/user/1000/wayland-2:/run/lasper/wayland/1000/wayland-2:idmap"));
        assert_eq!(content.matches("/run/lasper/wayland/1000/").count(), 2);
    }

    #[test]
    fn wayland_passthrough_rejects_a_conflicting_custom_bind_target() {
        let cfg = MachineProvisioningConfig {
            name: "test".into(),
            private_users: Some(PrivateUsersMode::Pick),
            bind_mounts: vec![BindMount {
                source: "/srv/custom-socket".into(),
                target: "/run/lasper/wayland/1000/custom".into(),
                readonly: false,
                suffix: IdmapSuffix::Noidmap,
            }],
            ..Default::default()
        };
        let spec = NspawnConfigSpec::try_from(&cfg).unwrap();

        let display = crate::domain::wayland::WaylandDisplay::new("wayland-0").unwrap();
        let bind = WaylandBind::new(
            "/run/user/1000/wayland-0",
            1000,
            &display,
            WaylandBindPolicy::Idmap,
        );
        let error = nspawn_config_content_from_spec_with_wayland_binds(&spec, &[bind]).unwrap_err();

        assert!(error.to_string().contains("reserved for the Wayland grant"));
    }

    #[test]
    fn test_apply_gpu_appends_to_existing_files_section() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let content = "[Exec]\nBoot=yes\n\n[Files]\nBind=/home/user:/home/user\n".to_string();
        let new_state = crate::adapters::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/nvidia0".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(
            updated.contains("Bind=/home/user:/home/user"),
            "User bind should survive"
        );
        assert!(updated.contains("X-Lasper-Nvidia-Begin=managed-by-lasper"));
    }

    #[test]
    fn test_apply_gpu_preserves_comments() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let content = "[Exec]\nBoot=yes\n# My custom comment\n".to_string();
        let new_state = crate::adapters::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/nvidia0".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(updated.contains("# My custom comment"));
    }

    #[test]
    fn gpu_update_preserves_crlf_and_final_newline() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let content =
            "[Exec]\r\nBoot=yes\r\n\r\n[Files]\r\nBind=/srv/data\r\n[Network]\r\nPrivate=yes\r\n"
                .to_string();
        let state = crate::adapters::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/nvidia0".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated = NspawnConfig::apply_gpu_passthrough_to_content(content, &state, &[]).unwrap();

        assert!(updated.ends_with("\r\n"));
        assert!(!updated.replace("\r\n", "").contains('\n'));
        assert!(updated.contains("Bind=/srv/data\r\nX-Lasper-Nvidia-Begin="));
        assert!(updated.contains("X-Lasper-Nvidia-End=true\r\n[Network]"));
    }

    #[test]
    fn gpu_update_preserves_missing_final_newline() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let state = crate::adapters::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/nvidia0".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated = NspawnConfig::apply_gpu_passthrough_to_content(
            "[Files]\nBind=/srv/data".into(),
            &state,
            &[],
        )
        .unwrap();

        assert!(updated.ends_with(NVIDIA_BLOCK_END));
    }

    #[test]
    fn test_apply_gpu_dedup_legacy_binds() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let content = "[Exec]\nBoot=yes\n\n[Files]\nBind=/dev/nvidia0\n".to_string();
        let new_state = crate::adapters::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/nvidia0".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        let count = updated.matches("Bind=/dev/nvidia0").count();
        assert_eq!(
            count, 1,
            "Legacy duplicate should be removed, got:\n{}",
            updated
        );
    }

    #[test]
    fn gpu_update_preserves_same_source_at_another_destination() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let content = "[Files]\nBind=/dev/nvidia0:/srv/user-device\n".to_string();
        let new_state = crate::adapters::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/dev/nvidia0".into(),
                container_path: "/dev/nvidia0".into(),
                readonly: false,
            }],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(updated.contains("Bind=/dev/nvidia0:/srv/user-device"));
        assert!(updated.contains("Bind=/dev/nvidia0"));
    }

    #[test]
    fn gpu_update_rejects_marker_external_destination_conflicts() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let content =
            "[Files]\nBindReadOnly = /srv/user-lib:/usr/lib/libcuda.so:noidmap\n".to_string();
        let new_state = crate::adapters::platform::nvidia::NvidiaState {
            binds: vec![PassthroughBind {
                host_path: "/host/libcuda.so".into(),
                container_path: "/usr/lib/libcuda.so".into(),
                readonly: true,
            }],
            ..Default::default()
        };

        let error =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap_err();
        assert!(error.to_string().contains("/usr/lib/libcuda.so"));
        assert!(error.to_string().contains("administrator-owned"));
    }

    #[test]
    fn gpu_update_removes_only_exact_legacy_state_binds() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let content = concat!(
            "[Files]\n",
            "BindReadOnly = /old/libcuda.so:/usr/lib/old-libcuda.so\n",
            "BindReadOnly=/old/libcuda.so:/usr/lib/old-libcuda.so:noidmap\n",
            "Bind=/user/source:/usr/lib/old-libcuda.so\n",
        )
        .to_string();
        let removed = vec![PassthroughBind {
            host_path: "/old/libcuda.so".into(),
            container_path: "/usr/lib/old-libcuda.so".into(),
            readonly: true,
        }];

        let updated = NspawnConfig::apply_gpu_passthrough_to_content(
            content,
            &crate::adapters::platform::nvidia::NvidiaState::default(),
            &removed,
        )
        .unwrap();
        assert!(!updated.contains("BindReadOnly = /old/libcuda.so:/usr/lib/old-libcuda.so\n"));
        assert!(updated.contains("BindReadOnly=/old/libcuda.so:/usr/lib/old-libcuda.so:noidmap"));
        assert!(updated.contains("Bind=/user/source:/usr/lib/old-libcuda.so"));
    }

    #[test]
    fn test_apply_gpu_empty_state_is_noop() {
        let content = "[Exec]\nBoot=yes\n".to_string();
        let empty_state = crate::adapters::platform::nvidia::NvidiaState::default();

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content.clone(), &empty_state, &[])
                .unwrap();
        assert!(!updated.contains("[Files]"));
        assert!(!updated.contains("X-Lasper-Nvidia-Begin"));
    }

    #[test]
    fn test_apply_gpu_with_symlink_as_bind() {
        use crate::adapters::platform::nvidia::state::PassthroughBind;

        let content = "[Exec]\nBoot=yes\n".to_string();
        let new_state = crate::adapters::platform::nvidia::NvidiaState {
            binds: vec![
                PassthroughBind {
                    host_path: "/host/libcuda.so.595.58.03".into(),
                    container_path: "/usr/lib/libcuda.so.1".into(),
                    readonly: true,
                },
                PassthroughBind {
                    host_path: "/host/libcuda.so.595.58.03".into(),
                    container_path: "/usr/lib/libcuda.so".into(),
                    readonly: true,
                },
            ],
            ..Default::default()
        };

        let updated =
            NspawnConfig::apply_gpu_passthrough_to_content(content, &new_state, &[]).unwrap();
        assert!(updated.contains("BindReadOnly=/host/libcuda.so.595.58.03:/usr/lib/libcuda.so.1"));
        assert!(updated.contains("BindReadOnly=/host/libcuda.so.595.58.03:/usr/lib/libcuda.so"));
    }
}
