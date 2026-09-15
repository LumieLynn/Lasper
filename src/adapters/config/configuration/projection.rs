//! Read-only X11 declaration projection. Preserve the original document and
//! leave unsupported bind syntax visible as diagnostics instead of guessing.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::adapters::config::nspawn_file::{parse_nspawn_bind_fields, NspawnConfig};
use crate::application::configuration::{
    ConfigurationDiscovery, ConfigurationDocument, ConfigurationOrigin, ConfigurationSnapshot,
    ConfigurationTarget, X11BindRecommendation, X11BindingDeclaration, X11BindingScope,
};

pub(super) fn project(
    target: ConfigurationTarget,
    config: Option<NspawnConfig>,
) -> ConfigurationSnapshot {
    let discovery = match &target {
        ConfigurationTarget::Machine(_) => ConfigurationDiscovery::MachineNameCandidates,
        ConfigurationTarget::Image(_) => ConfigurationDiscovery::NamedImageCandidates,
    };
    let mut snapshot = ConfigurationSnapshot {
        target,
        discovery,
        document: None,
        candidates: Vec::new(),
        revision: None,
        write_target: None,
        x11_bindings: Vec::new(),
        x11_bind_recommendation: X11BindRecommendation::Ready {
            private_users: "default (systemd-nspawn@ -U)".into(),
            idmapped: true,
        },
        host_x11: Default::default(),
        other_bind_count: 0,
        diagnostics: Vec::new(),
    };
    if let Some(config) = config {
        snapshot.x11_bind_recommendation =
            x11_bind_recommendation(&config.content, &mut snapshot.diagnostics);
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

fn x11_bind_recommendation(content: &str, diagnostics: &mut Vec<String>) -> X11BindRecommendation {
    let mut in_exec = false;
    let mut effective = None;
    for_each_logical_line(content, |line, number| {
        let line = line.trim();
        if line.starts_with('[') {
            in_exec = line == "[Exec]";
            return;
        }
        if !in_exec {
            return;
        }
        let Some((key, value)) = line.split_once('=') else {
            return;
        };
        if key.trim() != "PrivateUsers" {
            return;
        }
        let value = value.trim();
        match parse_private_users(value) {
            Some(policy) => effective = Some(policy),
            None => diagnostics.push(format!(
                "Line {number}: invalid PrivateUsers={value} is ignored; the preceding or default value remains effective."
            )),
        }
    });
    effective.unwrap_or_else(|| X11BindRecommendation::Ready {
        private_users: "default (systemd-nspawn@ -U)".into(),
        idmapped: true,
    })
}

fn parse_private_users(value: &str) -> Option<X11BindRecommendation> {
    if matches_ignore_ascii_case(value, &["no", "false", "off", "0", "n"]) {
        return Some(X11BindRecommendation::Ready {
            private_users: "no".into(),
            idmapped: false,
        });
    }
    if matches_ignore_ascii_case(value, &["yes", "true", "on", "1", "y"]) {
        return Some(X11BindRecommendation::Ready {
            private_users: "yes".into(),
            idmapped: true,
        });
    }
    if value.eq_ignore_ascii_case("pick") {
        return Some(X11BindRecommendation::Ready {
            private_users: "pick".into(),
            idmapped: true,
        });
    }
    let normalized = value.to_ascii_lowercase();
    let reason = match normalized.as_str() {
        "managed" => {
            "PrivateUsers=managed does not support Lasper's ordinary idmapped display bind policy"
        }
        "identity" => "PrivateUsers=identity is not supported by Lasper's display bind policy",
        _ if numeric_private_users(value) => {
            "explicit PrivateUsers UID ranges are not yet supported by Lasper's display bind policy"
        }
        _ => return None,
    };
    Some(X11BindRecommendation::Unsupported {
        private_users: normalized,
        reason: reason.into(),
    })
}

fn matches_ignore_ascii_case(value: &str, choices: &[&str]) -> bool {
    choices
        .iter()
        .any(|choice| value.eq_ignore_ascii_case(choice))
}

fn numeric_private_users(value: &str) -> bool {
    let (shift, range) = value.split_once(':').unwrap_or((value, "65536"));
    let (Ok(shift), Ok(range)) = (shift.parse::<u32>(), range.parse::<u32>()) else {
        return false;
    };
    range > 0 && shift <= u32::MAX - range
}

fn read_bind_declarations(content: &str, snapshot: &mut ConfigurationSnapshot) {
    let mut in_files = false;
    for_each_logical_line(content, |line, number| {
        read_logical_line(line, number, &mut in_files, snapshot);
    });
}

pub(super) fn bind_destinations(content: &str) -> Vec<(usize, std::path::PathBuf)> {
    let mut in_files = false;
    let mut destinations = Vec::new();
    for_each_logical_line(content, |line, number| {
        let line = line.trim();
        if line.starts_with('[') {
            in_files = line.eq_ignore_ascii_case("[Files]");
            return;
        }
        if !in_files {
            return;
        }
        let Some((key, value)) = line.split_once('=') else {
            return;
        };
        if !matches!(key.trim(), "Bind" | "BindReadOnly") {
            return;
        }
        let Some(fields) = parse_nspawn_bind_fields(value.trim()) else {
            return;
        };
        if fields.is_empty() || fields.len() > 3 || fields[0].is_empty() {
            return;
        }
        let destination = fields
            .get(1)
            .filter(|destination| !destination.is_empty())
            .unwrap_or(&fields[0]);
        destinations.push((number, destination.into()));
    });
    destinations
}

fn for_each_logical_line(content: &str, mut visit: impl FnMut(&str, usize)) {
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
        visit(&logical, first_line);
        logical.clear();
    }
    if !logical.is_empty() {
        visit(&logical, first_line);
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

pub(super) fn x11_scope(source: &Path) -> Option<X11BindingScope> {
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
    fn private_users_policy_derives_the_display_bind_suffix() {
        let cases = [
            ("", true, "default (systemd-nspawn@ -U)"),
            ("PrivateUsers=no\n", false, "no"),
            ("PrivateUsers=yes\n", true, "yes"),
            ("PrivateUsers=pick\n", true, "pick"),
        ];
        for (setting, idmapped, label) in cases {
            let result = snapshot(&format!("[Exec]\n{setting}[Files]\n"));
            assert_eq!(
                result.x11_bind_recommendation,
                X11BindRecommendation::Ready {
                    private_users: label.into(),
                    idmapped,
                }
            );
        }
    }

    #[test]
    fn unsupported_private_users_modes_are_explicit() {
        for (value, label) in [
            ("managed", "managed"),
            ("identity", "identity"),
            ("100000:65536", "100000:65536"),
        ] {
            let result = snapshot(&format!("[Exec]\nPrivateUsers={value}\n[Files]\n"));
            assert!(matches!(
                result.x11_bind_recommendation,
                X11BindRecommendation::Unsupported { private_users, .. } if private_users == label
            ));
        }
    }

    #[test]
    fn invalid_private_users_value_keeps_the_preceding_effective_policy() {
        let result = snapshot("[Exec]\nPrivateUsers=no\nPrivateUsers=not-a-policy\n[Files]\n");
        assert_eq!(
            result.x11_bind_recommendation,
            X11BindRecommendation::Ready {
                private_users: "no".into(),
                idmapped: false,
            }
        );
        assert_eq!(result.diagnostics.len(), 1);
        assert!(result.diagnostics[0].contains("ignored"));
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
