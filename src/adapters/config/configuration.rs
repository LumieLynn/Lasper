//! Read-only Configure projection over the already-composed configuration
//! store. Parsing stays local; privileged reads use the store's typed route.

use std::path::Path;

use sha2::{Digest, Sha256};

use super::nspawn_file::{parse_nspawn_bind_fields, NspawnConfig};
use super::NspawnConfigStore;
use crate::application::configuration::{
    ConfigurationDiscovery, ConfigurationDocument, ConfigurationOrigin, ConfigurationPort,
    ConfigurationSnapshot, ConfigurationTarget, X11BindingDeclaration, X11BindingScope,
};
use crate::application::inspection::ResourceInspectionError;

pub(crate) struct StoreConfiguration {
    store: NspawnConfigStore,
}

impl StoreConfiguration {
    pub(crate) fn new(store: NspawnConfigStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl ConfigurationPort for StoreConfiguration {
    async fn inspect(
        &self,
        target: &ConfigurationTarget,
    ) -> Result<ConfigurationSnapshot, ResourceInspectionError> {
        let config = match target {
            ConfigurationTarget::Machine(name) => self.store.read(name.as_str()).await,
            ConfigurationTarget::Image(name) => self.store.inspect(name.as_str()).await,
        }
        .map_err(ResourceInspectionError::backend)?;
        Ok(project(target.clone(), config))
    }
}

fn project(target: ConfigurationTarget, config: Option<NspawnConfig>) -> ConfigurationSnapshot {
    let discovery = match &target {
        ConfigurationTarget::Machine(_) => ConfigurationDiscovery::MachineAdministratorFile,
        ConfigurationTarget::Image(_) => ConfigurationDiscovery::NamedImageCandidates,
    };
    let mut snapshot = ConfigurationSnapshot {
        target,
        discovery,
        document: None,
        x11_bindings: Vec::new(),
        other_bind_count: 0,
        diagnostics: Vec::new(),
    };
    if let Some(config) = config {
        read_bind_declarations(&config.content, &mut snapshot);
        let origin = match config.path.parent() {
            Some(path) if path == Path::new("/etc/systemd/nspawn") => {
                ConfigurationOrigin::Administrator
            }
            Some(path) if path == Path::new("/run/systemd/nspawn") => ConfigurationOrigin::Runtime,
            _ => ConfigurationOrigin::ImageAdjacent,
        };
        snapshot.document = Some(ConfigurationDocument {
            content_sha256: format!("{:x}", Sha256::digest(config.content.as_bytes())),
            path: config.path,
            origin,
            content: config.content,
        });
    }
    snapshot
}

fn read_bind_declarations(content: &str, snapshot: &mut ConfigurationSnapshot) {
    let mut in_files = false;
    let mut logical = String::new();
    let mut first_line = 1;
    // Match conf-parser.c: ignore comment lines even within a continuation;
    // replace an unescaped final backslash with a space. Keep physical source
    // locations without modifying the document that Raw displays.
    for (index, line) in content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .lines()
        .enumerate()
    {
        if line.trim_start().starts_with(['#', ';']) {
            continue;
        }
        if logical.is_empty() {
            first_line = index + 1;
        }
        logical.push_str(line);
        if logical
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'\\')
            .count()
            % 2
            == 1
        {
            logical.pop();
            logical.push(' ');
            continue;
        }
        read_logical_line(&logical, first_line, &mut in_files, snapshot);
        logical.clear();
    }
    if !logical.is_empty() {
        read_logical_line(&logical, first_line, &mut in_files, snapshot);
    }
}

fn read_logical_line(
    line: &str,
    number: usize,
    in_files: &mut bool,
    snapshot: &mut ConfigurationSnapshot,
) {
    let line = line.trim();
    if line.starts_with('[') {
        *in_files = line == "[Files]";
        return;
    }
    if !*in_files {
        return;
    }
    let Some((key, value)) = line.split_once('=') else {
        return;
    };
    let readonly = match key.trim() {
        "Bind" => false,
        "BindReadOnly" => true,
        _ => return,
    };
    let Some(fields) = parse_nspawn_bind_fields(value.trim()) else {
        return;
    };
    let source = fields.first().map(String::as_str).unwrap_or_default();
    let destination = fields.get(1).map(String::as_str).unwrap_or(source);
    // Empty Bind= is rejected by nspawn's bind_mount_parse; it is not a reset.
    // Keep relative/specifier-based and otherwise unsupported declarations in
    // Raw instead of guessing their host source or removing preceding rows.
    if fields.len() > 3
        || !Path::new(source).is_absolute()
        || !Path::new(destination).is_absolute()
        || source.contains('%')
        || destination.contains('%')
    {
        snapshot.diagnostics.push(format!(
            "Line {number}: bind cannot be resolved by this view; inspect the original declaration in Raw."
        ));
        return;
    }
    let Some(scope) = x11_scope(Path::new(source)) else {
        snapshot.other_bind_count += 1;
        return;
    };
    snapshot.x11_bindings.push(X11BindingDeclaration {
        line: number,
        source: source.into(),
        guest_target: destination.into(),
        readonly,
        options: fields
            .get(2)
            .map(|value| value.split(',').map(str::to_owned).collect())
            .unwrap_or_default(),
        scope,
    });
}

fn x11_scope(source: &Path) -> Option<X11BindingScope> {
    let directory = Path::new("/tmp/.X11-unix");
    if source == directory {
        return Some(X11BindingScope::Directory);
    }
    if source.parent()? != directory {
        return None;
    }
    let name = source.file_name()?.to_str()?.strip_prefix('X')?;
    let alternate = name.ends_with('_');
    let number = name.strip_suffix('_').unwrap_or(name);
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(X11BindingScope::Socket {
        display: number.parse().ok()?,
        alternate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::ImageName;

    fn snapshot(content: &str) -> ConfigurationSnapshot {
        project(
            ConfigurationTarget::Image(ImageName::new("arch-image").unwrap()),
            Some(NspawnConfig {
                path: "/etc/systemd/nspawn/arch-image.nspawn".into(),
                content: content.into(),
            }),
        )
    }

    #[test]
    fn declarations_preserve_shared_directory_custom_target_and_alternate_endpoint() {
        let source = "# untouched\r\n[Exec]\r\nEnvironment=SECRET=hidden\r\n[Files]\r\nBindReadOnly=/tmp/.X11-unix:/mnt/host-x11:idmap\r\nBind=/tmp/.X11-unix/X0_:/tmp/.X11-unix/X0\r\nBind=/tmp/.X11-unix/X1:/mnt/X1\r\n";
        let result = snapshot(source);
        assert_eq!(result.x11_bindings.len(), 3);
        assert_eq!(result.x11_bindings[0].scope, X11BindingScope::Directory);
        assert!(result.x11_bindings[0].readonly);
        assert_eq!(
            result.x11_bindings[0].guest_target,
            Path::new("/mnt/host-x11")
        );
        assert_eq!(result.x11_bindings[0].options, ["idmap"]);
        assert_eq!(
            result.x11_bindings[1].scope,
            X11BindingScope::Socket {
                display: 0,
                alternate: true
            }
        );
        assert_eq!(result.document.as_ref().unwrap().content, source);
        assert!(!format!("{result:?}").contains("hidden"));
    }

    #[test]
    fn repeated_sections_and_continuations_keep_line_identity_without_empty_bind_reset() {
        let result = snapshot("[Exec]\nBind=/tmp/.X11-unix/X9\n[Files]\nBind=/tmp/.X11-unix/X0\nBind=\n[Other]\nUnknown=ok\n[Files]\nBindReadOnly=/tmp/.X11-unix/X1:\\\n# ignored even here\n /mnt/one\nBind=/tmp/.X11-unix/X0\n");
        // A continuation includes the leading space of its second line. It
        // cannot be normalized away inside an absolute destination.
        assert_eq!(result.x11_bindings.len(), 2);
        assert_eq!(result.x11_bindings[0].line, 4);
        assert_eq!(result.x11_bindings[1].line, 12);
        assert_eq!(result.diagnostics.len(), 2);
    }

    #[test]
    fn custom_escaped_destination_is_preserved_and_unrelated_binds_are_counted() {
        let result = snapshot("[Files]\nBind=/tmp/.X11-unix/X2:/mnt/x\\:2\\\\socket:idmap\nBind=/dev/dri\nBind=/tmp/.X11-unix/X0::idmap\nBind=/tmp/.X11-unix/X3:/mnt/%u\n");
        assert_eq!(result.x11_bindings.len(), 1);
        assert_eq!(
            result.x11_bindings[0].guest_target,
            Path::new("/mnt/x:2\\socket")
        );
        assert_eq!(result.other_bind_count, 1);
        assert_eq!(result.diagnostics.len(), 2);
    }

    #[test]
    fn source_only_inspection_retains_image_identity_and_fingerprints_displayed_bytes() {
        let target = ConfigurationTarget::Image(ImageName::new("image with spaces").unwrap());
        let empty = project(target.clone(), None);
        assert_eq!(empty.target, target);
        assert_eq!(
            empty.discovery,
            ConfigurationDiscovery::NamedImageCandidates
        );
        assert!(empty.document.is_none());
        let a = snapshot("[Files]\n");
        let b = snapshot("[Files]\r\n");
        assert_ne!(
            a.document.unwrap().content_sha256,
            b.document.unwrap().content_sha256
        );
    }
}
