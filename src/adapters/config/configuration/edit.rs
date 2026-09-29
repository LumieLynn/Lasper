//! Finite display-bind edits for Configure. Preview and apply share the same
//! policy checks and byte-preserving patch calculation.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use super::document::NspawnDocument;
use super::inspection::{inspect, inspect_at};
use super::patch::SourceMutation;
use super::projection::x11_scope;
use super::write::{prepare_patch, Preparation};
use crate::adapters::config::nspawn_file::{
    encode_nspawn_bind_value, is_nvidia_begin_marker, nspawn_bind_key,
};
use crate::adapters::error::Result;
use crate::adapters::filesystem::AsyncLockedWriter;
use crate::application::configuration::{
    ConfigurationActivation, ConfigurationApplyReport, ConfigurationEdit, ConfigurationPreview,
    ConfigurationSnapshot, DisplayBindRecommendation, WaylandBindingChange,
    WaylandBindingDeclaration, X11BindingChange, X11BindingDeclaration, X11BindingScope,
};
use crate::domain::machine::MachineName;
use crate::domain::wayland::WaylandDisplay;
use crate::domain::x11::X11_SOCKET_BIND_READ_ONLY;

const MAX_X11_CHANGES: usize = 128;
const MAX_WAYLAND_CHANGES: usize = 128;
const MAX_CONFIG_PATH_BYTES: usize = 4096;

pub(crate) async fn preview(edit: ConfigurationEdit) -> Result<ConfigurationPreview> {
    let snapshot = inspect(edit.target.clone()).await?;
    Ok(prepare(&snapshot, &edit).into_preview())
}

pub(crate) async fn apply(edit: ConfigurationEdit) -> Result<ConfigurationApplyReport> {
    if MachineName::new(edit.target.name()).is_err() {
        return Ok(ConfigurationApplyReport::Blocked {
            reason: "This image name cannot identify an administrator .nspawn file".into(),
        });
    }

    let initial = inspect(edit.target.clone()).await?;
    apply_with_sources(
        edit,
        initial,
        crate::paths::nspawn_config_dir(),
        crate::paths::nspawn_runtime_config_dir(),
        crate::paths::machines_dir(),
    )
    .await
}

async fn apply_with_sources(
    edit: ConfigurationEdit,
    initial: ConfigurationSnapshot,
    admin: PathBuf,
    runtime: PathBuf,
    images: PathBuf,
) -> Result<ConfigurationApplyReport> {
    let path = match prepare(&initial, &edit) {
        Preparation::Ready(change) => change.path,
        other => return Ok(other.into_apply_report()),
    };
    let write_path = path.clone();
    AsyncLockedWriter::apply_locked(&path, move |existing| {
        // The stable machine lock is held by the operation executor and this
        // closure runs under the target's sidecar lock. Re-discover every
        // source here so the preview revision is an actual apply condition.
        let current = inspect_at(edit.target.clone(), &admin, &runtime, &images);
        let prepared = prepare(&current, &edit);
        match prepared {
            Preparation::Ready(change) => {
                if current.document.as_ref().map(|document| &document.content)
                    != existing.as_ref()
                {
                    return Ok((
                        None,
                        ConfigurationApplyReport::Conflict {
                            reason: "The administrator configuration changed while saving; refresh and review the new diff"
                                .into(),
                        },
                    ));
                }
                Ok((
                    Some(change.after),
                    ConfigurationApplyReport::Applied {
                        path: write_path,
                        activation: ConfigurationActivation::NextMachineStart,
                    },
                ))
            }
            other => Ok((None, other.into_apply_report())),
        }
    })
    .await
}

fn prepare(snapshot: &ConfigurationSnapshot, edit: &ConfigurationEdit) -> Preparation {
    if edit.x11_changes.len() > MAX_X11_CHANGES {
        return Preparation::Blocked(format!(
            "A single draft may change at most {MAX_X11_CHANGES} X11 declarations"
        ));
    }
    if edit.wayland_changes.len() > MAX_WAYLAND_CHANGES {
        return Preparation::Blocked(format!(
            "A single draft may change at most {MAX_WAYLAND_CHANGES} Wayland declarations"
        ));
    }

    prepare_patch(snapshot, &edit.target, &edit.base_revision, |document| {
        plan_display_mutations(
            document,
            &snapshot.x11_bindings,
            &snapshot.x11_bind_recommendation,
            &snapshot.wayland_bindings,
            &snapshot.wayland_bind_recommendation,
            edit,
        )
    })
}

fn plan_display_mutations(
    document: &NspawnDocument<'_>,
    x11_declarations: &[X11BindingDeclaration],
    x11_recommendation: &DisplayBindRecommendation,
    wayland_declarations: &[WaylandBindingDeclaration],
    wayland_recommendation: &DisplayBindRecommendation,
    edit: &ConfigurationEdit,
) -> std::result::Result<Vec<SourceMutation>, String> {
    enum Requested<'a> {
        X11(&'a X11BindingChange),
        Wayland(&'a WaylandBindingChange),
    }
    enum Addition<'a> {
        X11(&'a Path),
        Wayland {
            source: &'a Path,
            guest_target: &'a Path,
        },
    }

    let lines = document.lines();
    let mut requested = BTreeMap::new();
    let mut additions = Vec::new();
    for change in &edit.x11_changes {
        match change.declaration_line() {
            Some(0) => return Err("Declaration line numbers start at one".into()),
            Some(line) => {
                if requested.insert(line, Requested::X11(change)).is_some() {
                    return Err(format!(
                        "Line {line} is changed more than once in this draft"
                    ));
                }
            }
            None => match change {
                X11BindingChange::Add { source } => additions.push(Addition::X11(source)),
                _ => unreachable!("X11 changes without declaration lines are additions"),
            },
        }
    }
    for change in &edit.wayland_changes {
        match change.declaration_line() {
            Some(0) => return Err("Declaration line numbers start at one".into()),
            Some(line) => {
                if requested.insert(line, Requested::Wayland(change)).is_some() {
                    return Err(format!(
                        "Line {line} is changed more than once in this draft"
                    ));
                }
            }
            None => match change {
                WaylandBindingChange::Add {
                    source,
                    guest_target,
                } => additions.push(Addition::Wayland {
                    source,
                    guest_target,
                }),
                _ => unreachable!("Wayland changes without declaration lines are additions"),
            },
        }
    }

    let mut mutations = Vec::with_capacity(requested.len());
    let mut occupied_targets = document
        .bind_destinations()
        .into_iter()
        .filter_map(|(line, target)| match requested.get(&line) {
            Some(Requested::X11(X11BindingChange::Remove { .. }))
            | Some(Requested::Wayland(WaylandBindingChange::Remove { .. })) => None,
            Some(Requested::X11(X11BindingChange::Update { guest_target, .. }))
            | Some(Requested::Wayland(WaylandBindingChange::Update { guest_target, .. })) => {
                Some((line, guest_target.clone()))
            }
            Some(_) => unreachable!("additions are not indexed by declaration line"),
            None => Some((line, target)),
        })
        .collect::<Vec<_>>();
    for (line_number, change) in requested {
        let Some(line) = lines.get(line_number - 1) else {
            return Err(format!("Line {line_number} no longer exists"));
        };
        if line.has_continuation() {
            return Err(format!(
                "Line {line_number} uses continuation syntax and must be edited manually"
            ));
        }

        let new = match change {
            Requested::X11(change) => calculate_x11_replacement(
                line_number,
                line.body(),
                line.ending(),
                x11_declarations,
                &occupied_targets,
                change,
            )?,
            Requested::Wayland(change) => calculate_wayland_replacement(
                line_number,
                line.body(),
                line.ending(),
                wayland_declarations,
                &occupied_targets,
                change,
            )?,
        };
        let unchanged = format!("{}{}", line.body(), line.ending());
        if new.as_ref() == Some(&unchanged) {
            continue;
        }
        mutations.push(SourceMutation {
            line: line.number(),
            start: line.start(),
            end: line.end(),
            old: vec![line.body().to_string()],
            new: new
                .as_ref()
                .map(|replacement| {
                    vec![replacement
                        .strip_suffix(line.ending())
                        .unwrap_or(replacement)
                        .to_string()]
                })
                .unwrap_or_default(),
            replacement: new,
        });
    }

    let mut encoded_additions = Vec::with_capacity(additions.len());
    for addition in additions {
        let (target, binding) = match addition {
            Addition::X11(source) => {
                let idmapped = recommendation_idmap(x11_recommendation, "X11")?;
                validate_x11_source(source).map_err(|reason| format!("New X11 bind: {reason}"))?;
                if !matches!(x11_scope(source), Some(X11BindingScope::Socket { .. })) {
                    return Err(format!(
                        "New X11 bind: {} is not an individual X11 socket endpoint",
                        source.display()
                    ));
                }
                (
                    source.to_path_buf(),
                    encode_new_x11_directive(source, idmapped),
                )
            }
            Addition::Wayland {
                source,
                guest_target,
            } => {
                let idmapped = recommendation_idmap(wayland_recommendation, "Wayland")?;
                validate_wayland_source(source)
                    .map_err(|reason| format!("New Wayland bind: {reason}"))?;
                validate_guest_target(guest_target)
                    .map_err(|reason| format!("New Wayland bind: {reason}"))?;
                (
                    guest_target.to_path_buf(),
                    encode_new_wayland_directive(source, guest_target, idmapped),
                )
            }
        };
        if occupied_targets
            .iter()
            .any(|(_, occupied)| occupied == &target)
        {
            return Err(format!(
                "New display bind: guest target {} is already used by another bind",
                target.display()
            ));
        }
        occupied_targets.push((0, target));
        encoded_additions.push(binding);
    }
    if !encoded_additions.is_empty() {
        mutations.push(insertion_mutation(document, &encoded_additions));
    }
    Ok(mutations)
}

fn recommendation_idmap(
    recommendation: &DisplayBindRecommendation,
    label: &str,
) -> std::result::Result<bool, String> {
    match recommendation {
        DisplayBindRecommendation::Ready { idmapped, .. } => Ok(*idmapped),
        DisplayBindRecommendation::Unsupported { reason, .. } => {
            Err(format!("New {label} bind: {reason}"))
        }
    }
}

fn calculate_x11_replacement(
    line_number: usize,
    old_body: &str,
    ending: &str,
    declarations: &[X11BindingDeclaration],
    occupied_targets: &[(usize, PathBuf)],
    change: &X11BindingChange,
) -> std::result::Result<Option<String>, String> {
    let declaration = declarations
        .iter()
        .find(|declaration| declaration.line == line_number)
        .ok_or_else(|| {
            format!("Line {line_number} is not a recognized X11 bind in this revision")
        })?;
    match change {
        X11BindingChange::Add { .. } => {
            unreachable!("additions are not indexed by declaration line")
        }
        X11BindingChange::Remove { .. } => Ok(None),
        X11BindingChange::Update {
            source,
            guest_target,
            readonly,
            ..
        } => {
            validate_x11_source(source)
                .map_err(|reason| format!("Line {line_number}: {reason}"))?;
            validate_guest_target(guest_target)
                .map_err(|reason| format!("Line {line_number}: {reason}"))?;
            reject_occupied_target(line_number, guest_target, occupied_targets)?;
            if source == &declaration.source
                && guest_target == &declaration.guest_target
                && readonly == &declaration.readonly
            {
                return Ok(Some(format!("{old_body}{ending}")));
            }
            Ok(Some(encode_x11_replacement(
                old_body,
                ending,
                declaration,
                source,
                guest_target,
                *readonly,
            )))
        }
    }
}

fn calculate_wayland_replacement(
    line_number: usize,
    old_body: &str,
    ending: &str,
    declarations: &[WaylandBindingDeclaration],
    occupied_targets: &[(usize, PathBuf)],
    change: &WaylandBindingChange,
) -> std::result::Result<Option<String>, String> {
    let declaration = declarations
        .iter()
        .find(|declaration| declaration.line == line_number)
        .ok_or_else(|| {
            format!("Line {line_number} is not a recognized Wayland bind in this revision")
        })?;
    match change {
        WaylandBindingChange::Add { .. } => {
            unreachable!("additions are not indexed by declaration line")
        }
        WaylandBindingChange::Remove { .. } => Ok(None),
        WaylandBindingChange::Update {
            source,
            guest_target,
            readonly,
            ..
        } => {
            validate_wayland_source(source)
                .map_err(|reason| format!("Line {line_number}: {reason}"))?;
            validate_guest_target(guest_target)
                .map_err(|reason| format!("Line {line_number}: {reason}"))?;
            reject_occupied_target(line_number, guest_target, occupied_targets)?;
            if source == &declaration.source
                && guest_target == &declaration.guest_target
                && readonly == &declaration.readonly
            {
                return Ok(Some(format!("{old_body}{ending}")));
            }
            Ok(Some(encode_wayland_replacement(
                old_body,
                ending,
                declaration,
                source,
                guest_target,
                *readonly,
            )))
        }
    }
}

fn reject_occupied_target(
    line_number: usize,
    guest_target: &Path,
    occupied_targets: &[(usize, PathBuf)],
) -> std::result::Result<(), String> {
    if occupied_targets
        .iter()
        .any(|(other_line, target)| *other_line != line_number && target == guest_target)
    {
        return Err(format!(
            "Line {line_number}: guest target {} is already used by another bind",
            guest_target.display()
        ));
    }
    Ok(())
}

fn validate_x11_source(path: &Path) -> std::result::Result<(), &'static str> {
    validate_plain_absolute_path(path, "X11 source")?;
    if x11_scope(path).is_none() {
        return Err("source must be /tmp/.X11-unix or one of its X<number> endpoints");
    }
    Ok(())
}

fn validate_wayland_source(path: &Path) -> std::result::Result<(), &'static str> {
    validate_plain_absolute_path(path, "Wayland source")?;
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err("Wayland source has no display name");
    };
    WaylandDisplay::new(name.to_owned())
        .map_err(|_| "Wayland source has an invalid display name")?;
    Ok(())
}

fn validate_guest_target(path: &Path) -> std::result::Result<(), &'static str> {
    validate_plain_absolute_path(path, "guest target")?;
    if path == Path::new("/") {
        return Err("guest target cannot replace the container root");
    }
    Ok(())
}

fn validate_plain_absolute_path(
    path: &Path,
    label: &'static str,
) -> std::result::Result<(), &'static str> {
    let Some(value) = path.to_str() else {
        return Err("path is not valid UTF-8");
    };
    if value.len() > MAX_CONFIG_PATH_BYTES {
        return Err("path exceeds the configuration editor limit");
    }
    if !path.is_absolute() {
        return Err(match label {
            "X11 source" => "X11 source must be absolute",
            "Wayland source" => "Wayland source must be absolute",
            _ => "guest target must be absolute",
        });
    }
    if value.contains('%') || value.chars().any(char::is_control) {
        return Err("path cannot contain systemd specifiers or control characters");
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err("path must not contain '.' or '..' components");
    }
    Ok(())
}

fn encode_x11_replacement(
    old_body: &str,
    ending: &str,
    declaration: &X11BindingDeclaration,
    source: &Path,
    guest_target: &Path,
    readonly: bool,
) -> String {
    encode_display_replacement(
        old_body,
        ending,
        source,
        guest_target,
        readonly,
        &declaration.options,
    )
}

fn encode_wayland_replacement(
    old_body: &str,
    ending: &str,
    declaration: &WaylandBindingDeclaration,
    source: &Path,
    guest_target: &Path,
    readonly: bool,
) -> String {
    encode_display_replacement(
        old_body,
        ending,
        source,
        guest_target,
        readonly,
        &declaration.options,
    )
}

fn encode_display_replacement(
    old_body: &str,
    ending: &str,
    source: &Path,
    guest_target: &Path,
    readonly: bool,
    options: &[String],
) -> String {
    let indent_len = old_body.len() - old_body.trim_start().len();
    let indent = &old_body[..indent_len];
    let options = (!options.is_empty()).then(|| options.join(","));
    format!(
        "{indent}{}={}{}",
        nspawn_bind_key(readonly),
        encode_nspawn_bind_value(source, Some(guest_target), options.as_deref()),
        ending
    )
}

fn encode_new_x11_directive(source: &Path, idmapped: bool) -> String {
    let suffix = idmapped.then_some("idmap");
    format!(
        "{}={}",
        nspawn_bind_key(X11_SOCKET_BIND_READ_ONLY),
        encode_nspawn_bind_value(source, Some(source), suffix)
    )
}

fn encode_new_wayland_directive(source: &Path, target: &Path, idmapped: bool) -> String {
    let suffix = idmapped.then_some("idmap");
    format!(
        "{}={}",
        nspawn_bind_key(false),
        encode_nspawn_bind_value(source, Some(target), suffix)
    )
}

fn insertion_mutation(document: &NspawnDocument<'_>, bindings: &[String]) -> SourceMutation {
    let content = document.content();
    let lines = document.lines();
    let ending = document.preferred_line_ending();
    let mut in_files = false;
    let mut files_seen = false;
    let mut offset = content.len();
    let mut before_line = lines.len().saturating_add(1);

    for line in lines {
        let trimmed = line.body().trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if trimmed.eq_ignore_ascii_case("[Files]") {
                in_files = true;
                files_seen = true;
                offset = line.end();
                before_line = line.number().saturating_add(1);
            } else if in_files {
                in_files = false;
                offset = line.start();
                before_line = line.number();
            }
        } else if in_files && is_nvidia_begin_marker(line.body()) {
            offset = line.start();
            before_line = line.number();
            break;
        } else if in_files {
            offset = line.end();
            before_line = line.number().saturating_add(1);
        }
    }

    let mut replacement = String::new();
    if files_seen {
        if offset > 0 && !content[..offset].ends_with('\n') {
            replacement.push_str(ending);
        }
    } else {
        if !content.is_empty() {
            if !content.ends_with('\n') {
                replacement.push_str(ending);
            }
            let double_ending = format!("{ending}{ending}");
            if !content.ends_with(&double_ending) {
                replacement.push_str(ending);
            }
        }
        replacement.push_str("[Files]");
        replacement.push_str(ending);
    }
    for binding in bindings {
        replacement.push_str(binding);
        replacement.push_str(ending);
    }
    let inserted = NspawnDocument::new(&replacement);
    let new = inserted
        .lines()
        .iter()
        .map(|line| line.body().to_string())
        .collect();
    SourceMutation {
        line: before_line.max(1),
        start: offset,
        end: offset,
        old: Vec::new(),
        new,
        replacement: Some(replacement),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::configuration::projection::project;
    use crate::adapters::config::nspawn_file::NspawnConfig;
    use crate::application::configuration::ConfigurationOrigin;
    use crate::application::configuration::{
        ConfigurationRevision, ConfigurationTarget, ConfigurationWriteTarget,
    };
    use crate::domain::machine::MachineName;

    fn fixture(content: &str) -> (ConfigurationSnapshot, ConfigurationRevision) {
        let target = ConfigurationTarget::Machine(MachineName::new("arch").unwrap());
        let path = PathBuf::from("/etc/systemd/nspawn/arch.nspawn");
        let mut snapshot = project(
            target,
            Some(NspawnConfig {
                path: path.clone(),
                content: content.into(),
            }),
        );
        let revision = ConfigurationRevision {
            discovery: "discovery".into(),
            read_source: Some("source".into()),
            write_target: Some("target".into()),
        };
        snapshot.revision = Some(revision.clone());
        snapshot.write_target = Some(ConfigurationWriteTarget { path, exists: true });
        (snapshot, revision)
    }

    fn edit(revision: ConfigurationRevision, changes: Vec<X11BindingChange>) -> ConfigurationEdit {
        ConfigurationEdit {
            target: ConfigurationTarget::Machine(MachineName::new("arch").unwrap()),
            base_revision: revision,
            x11_changes: changes,
            wayland_changes: vec![],
        }
    }

    #[test]
    fn updates_one_declaration_without_reformatting_other_content() {
        let source = "# keep\r\n[Files]\r\n  Bind=/dev/dri\r\n\tBindReadOnly=/tmp/.X11-unix:/mnt/x11:idmap,nofollow\r\n[Network]\r\nVirtualEthernet=yes\r\n";
        let (snapshot, revision) = fixture(source);
        let request = edit(
            revision,
            vec![X11BindingChange::Update {
                line: 4,
                source: "/tmp/.X11-unix/X0_".into(),
                guest_target: "/tmp/.X11-unix/X0".into(),
                readonly: false,
            }],
        );
        let Preparation::Ready(change) = prepare(&snapshot, &request) else {
            panic!("expected a ready change");
        };
        assert_eq!(
            change.after,
            "# keep\r\n[Files]\r\n  Bind=/dev/dri\r\n\tBind=/tmp/.X11-unix/X0_:/tmp/.X11-unix/X0:idmap,nofollow\r\n[Network]\r\nVirtualEthernet=yes\r\n"
        );
        assert!(change.diff.contains("-\tBindReadOnly="));
        assert!(change.diff.contains("+\tBind="));
        assert!(!change.diff.contains("VirtualEthernet=yes\r"));
    }

    #[test]
    fn removes_only_the_selected_repeated_declaration() {
        let source =
            "[Files]\nBind=/tmp/.X11-unix/X0\nBind=/dev/dri\nBind=/tmp/.X11-unix/X1:/mnt/X1\n";
        let (snapshot, revision) = fixture(source);
        let request = edit(revision, vec![X11BindingChange::Remove { line: 2 }]);
        let Preparation::Ready(change) = prepare(&snapshot, &request) else {
            panic!("expected a ready change");
        };
        assert_eq!(
            change.after,
            "[Files]\nBind=/dev/dri\nBind=/tmp/.X11-unix/X1:/mnt/X1\n"
        );
    }

    #[test]
    fn adds_bind_to_the_last_files_section_without_reformatting_crlf_content() {
        let source =
            "[Exec]\r\nBoot=yes\r\n[Files]\r\nBind=/dev/dri\r\n[Network]\r\nPrivate=yes\r\n";
        let (snapshot, revision) = fixture(source);
        let request = edit(
            revision,
            vec![X11BindingChange::Add {
                source: "/tmp/.X11-unix/X0_".into(),
            }],
        );
        let Preparation::Ready(change) = prepare(&snapshot, &request) else {
            panic!("expected a ready change");
        };
        assert_eq!(
            change.after,
            "[Exec]\r\nBoot=yes\r\n[Files]\r\nBind=/dev/dri\r\nBindReadOnly=/tmp/.X11-unix/X0_:/tmp/.X11-unix/X0_:idmap\r\n[Network]\r\nPrivate=yes\r\n"
        );
        assert!(change
            .diff
            .contains("+BindReadOnly=/tmp/.X11-unix/X0_:/tmp/.X11-unix/X0_:idmap"));
        assert!(!change.diff.contains("-Bind=/dev/dri"));
    }

    #[test]
    fn adds_a_files_section_when_the_administrator_document_has_none() {
        let (snapshot, revision) = fixture("[Exec]\nBoot=yes");
        let request = edit(
            revision,
            vec![X11BindingChange::Add {
                source: "/tmp/.X11-unix/X0".into(),
            }],
        );
        let Preparation::Ready(change) = prepare(&snapshot, &request) else {
            panic!("expected a ready change");
        };
        assert_eq!(
            change.after,
            "[Exec]\nBoot=yes\n\n[Files]\nBindReadOnly=/tmp/.X11-unix/X0:/tmp/.X11-unix/X0:idmap\n"
        );
        assert!(change.diff.contains("+[Files]"));
        assert!(change
            .diff
            .contains("+BindReadOnly=/tmp/.X11-unix/X0:/tmp/.X11-unix/X0:idmap"));
    }

    #[test]
    fn adds_to_an_empty_administrator_document() {
        let (snapshot, revision) = fixture("");
        let request = edit(
            revision,
            vec![X11BindingChange::Add {
                source: "/tmp/.X11-unix/X1".into(),
            }],
        );
        let Preparation::Ready(change) = prepare(&snapshot, &request) else {
            panic!("expected a ready change");
        };
        assert_eq!(
            change.after,
            "[Files]\nBindReadOnly=/tmp/.X11-unix/X1:/tmp/.X11-unix/X1:idmap\n"
        );
        assert!(change.diff.contains("@@ -0,0 +1,2 @@"));
    }

    #[test]
    fn adds_to_the_last_repeated_files_section() {
        let source = "[Files]\nBind=/dev/dri\n[Exec]\nBoot=yes\n[Files]\nBind=/dev/snd\n[Network]\nPrivate=yes\n";
        let (snapshot, revision) = fixture(source);
        let request = edit(
            revision,
            vec![X11BindingChange::Add {
                source: "/tmp/.X11-unix/X2".into(),
            }],
        );
        let Preparation::Ready(change) = prepare(&snapshot, &request) else {
            panic!("expected a ready change");
        };
        assert_eq!(
            change.after,
            "[Files]\nBind=/dev/dri\n[Exec]\nBoot=yes\n[Files]\nBind=/dev/snd\nBindReadOnly=/tmp/.X11-unix/X2:/tmp/.X11-unix/X2:idmap\n[Network]\nPrivate=yes\n"
        );
    }

    #[test]
    fn additions_reject_existing_and_intra_draft_target_collisions() {
        let (snapshot, revision) = fixture("[Files]\nBind=/dev/dri:/tmp/.X11-unix/X0\n");
        let collision = edit(
            revision.clone(),
            vec![X11BindingChange::Add {
                source: "/tmp/.X11-unix/X0".into(),
            }],
        );
        assert!(matches!(
            prepare(&snapshot, &collision),
            Preparation::Blocked(_)
        ));

        let duplicate = edit(
            revision,
            vec![
                X11BindingChange::Add {
                    source: "/tmp/.X11-unix/X0".into(),
                },
                X11BindingChange::Add {
                    source: "/tmp/.X11-unix/X0".into(),
                },
            ],
        );
        assert!(matches!(
            prepare(&snapshot, &duplicate),
            Preparation::Blocked(_)
        ));
    }

    #[test]
    fn addition_omits_idmap_only_for_private_users_no() {
        let (snapshot, revision) = fixture("[Exec]\nPrivateUsers=no\n[Files]\nBind=/dev/dri\n");
        let request = edit(
            revision,
            vec![X11BindingChange::Add {
                source: "/tmp/.X11-unix/X0".into(),
            }],
        );
        let Preparation::Ready(change) = prepare(&snapshot, &request) else {
            panic!("expected a ready change");
        };
        assert!(change
            .after
            .contains("BindReadOnly=/tmp/.X11-unix/X0:/tmp/.X11-unix/X0\n"));
        assert!(!change.after.contains("X0:idmap"));
    }

    #[test]
    fn unsupported_private_users_policy_blocks_addition() {
        let (snapshot, revision) = fixture("[Exec]\nPrivateUsers=managed\n[Files]\n");
        let request = edit(
            revision,
            vec![X11BindingChange::Add {
                source: "/tmp/.X11-unix/X0".into(),
            }],
        );
        let Preparation::Blocked(reason) = prepare(&snapshot, &request) else {
            panic!("expected a blocked change");
        };
        assert!(reason.contains("PrivateUsers=managed"));
    }

    #[test]
    fn additions_are_inserted_before_the_managed_nvidia_block() {
        let source = "[Files]\nBind=/dev/dri\nX-Lasper-Nvidia-Begin=managed-by-lasper\nBind=/dev/nvidia0\nX-Lasper-Nvidia-End=true\n";
        let (snapshot, revision) = fixture(source);
        let request = edit(
            revision,
            vec![X11BindingChange::Add {
                source: "/tmp/.X11-unix/X0".into(),
            }],
        );
        let Preparation::Ready(change) = prepare(&snapshot, &request) else {
            panic!("expected a ready change");
        };
        assert_eq!(
            change.after,
            "[Files]\nBind=/dev/dri\nBindReadOnly=/tmp/.X11-unix/X0:/tmp/.X11-unix/X0:idmap\nX-Lasper-Nvidia-Begin=managed-by-lasper\nBind=/dev/nvidia0\nX-Lasper-Nvidia-End=true\n"
        );
    }

    #[test]
    fn x11_and_wayland_additions_share_one_ordered_insertion() {
        let source = "[Files]\nBind=/dev/dri\nX-Lasper-Nvidia-Begin=managed-by-lasper\nBind=/dev/nvidia0\nX-Lasper-Nvidia-End=true\n";
        let (snapshot, revision) = fixture(source);
        let mut request = edit(
            revision,
            vec![X11BindingChange::Add {
                source: "/tmp/.X11-unix/X0".into(),
            }],
        );
        request.wayland_changes = vec![WaylandBindingChange::Add {
            source: "/run/user/1000/wayland-1".into(),
            guest_target: "/run/lasper/wayland/1000/wayland-1".into(),
        }];
        let Preparation::Ready(change) = prepare(&snapshot, &request) else {
            panic!("expected a ready change");
        };
        assert_eq!(
            change.after,
            "[Files]\nBind=/dev/dri\nBindReadOnly=/tmp/.X11-unix/X0:/tmp/.X11-unix/X0:idmap\nBind=/run/user/1000/wayland-1:/run/lasper/wayland/1000/wayland-1:idmap\nX-Lasper-Nvidia-Begin=managed-by-lasper\nBind=/dev/nvidia0\nX-Lasper-Nvidia-End=true\n"
        );
    }

    #[test]
    fn removes_a_custom_target_wayland_declaration_by_source_line() {
        let source =
            "[Files]\nBind=/run/user/1000/wayland-1:/mnt/wayland-socket:idmap\nBind=/dev/dri\n";
        let (snapshot, revision) = fixture(source);
        let mut request = edit(revision, vec![]);
        request.wayland_changes = vec![WaylandBindingChange::Remove { line: 2 }];
        let Preparation::Ready(change) = prepare(&snapshot, &request) else {
            panic!("expected a ready change");
        };
        assert_eq!(change.after, "[Files]\nBind=/dev/dri\n");
    }

    #[test]
    fn additions_reject_directory_wide_or_non_x11_sources() {
        for source in ["/tmp/.X11-unix", "/tmp/not-x11/X0"] {
            let (snapshot, revision) = fixture("[Files]\n");
            let request = edit(
                revision,
                vec![X11BindingChange::Add {
                    source: source.into(),
                }],
            );
            assert!(matches!(
                prepare(&snapshot, &request),
                Preparation::Blocked(_)
            ));
        }
    }

    #[test]
    fn unchanged_semantics_do_not_normalize_the_line() {
        let source = "[Files]\n  BindReadOnly = /tmp/.X11-unix:/mnt/x11:idmap\n";
        let (snapshot, revision) = fixture(source);
        let request = edit(
            revision,
            vec![X11BindingChange::Update {
                line: 2,
                source: "/tmp/.X11-unix".into(),
                guest_target: "/mnt/x11".into(),
                readonly: true,
            }],
        );
        assert!(matches!(
            prepare(&snapshot, &request),
            Preparation::Unchanged(_)
        ));
    }

    #[test]
    fn stale_revision_and_non_admin_sources_are_not_patchable() {
        let (mut snapshot, revision) = fixture("[Files]\nBind=/tmp/.X11-unix\n");
        let mut request = edit(revision, vec![X11BindingChange::Remove { line: 2 }]);
        request.base_revision.discovery = "stale".into();
        assert!(matches!(
            prepare(&snapshot, &request),
            Preparation::Conflict(_)
        ));

        request.base_revision = snapshot.revision.clone().unwrap();
        snapshot.document.as_mut().unwrap().origin = ConfigurationOrigin::Runtime;
        assert!(matches!(
            prepare(&snapshot, &request),
            Preparation::Blocked(_)
        ));
    }

    #[test]
    fn invalid_paths_and_continued_declarations_are_blocked() {
        let (snapshot, revision) = fixture("[Files]\nBind=/tmp/.X11-unix/X0:\\\n /mnt/x11\n");
        let continued = edit(revision.clone(), vec![X11BindingChange::Remove { line: 2 }]);
        assert!(matches!(
            prepare(&snapshot, &continued),
            Preparation::Blocked(_)
        ));

        let (snapshot, revision) = fixture("[Files]\nBind=/tmp/.X11-unix/X0:/mnt/x11\n");
        let invalid = edit(
            revision,
            vec![X11BindingChange::Update {
                line: 2,
                source: "/run/user/1000/wayland-1".into(),
                guest_target: "/mnt/../x11".into(),
                readonly: false,
            }],
        );
        assert!(matches!(
            prepare(&snapshot, &invalid),
            Preparation::Blocked(_)
        ));
    }

    #[test]
    fn update_rejects_a_target_collision_but_allows_an_atomic_swap() {
        let source = "[Files]\nBind=/tmp/.X11-unix/X0:/mnt/X0\nBind=/tmp/.X11-unix/X1:/mnt/X1\nBind=/dev/dri:/mnt/dri\n";
        let (snapshot, revision) = fixture(source);
        let collision = edit(
            revision.clone(),
            vec![X11BindingChange::Update {
                line: 2,
                source: "/tmp/.X11-unix/X0".into(),
                guest_target: "/mnt/dri".into(),
                readonly: false,
            }],
        );
        assert!(matches!(
            prepare(&snapshot, &collision),
            Preparation::Blocked(_)
        ));

        let swap = edit(
            revision,
            vec![
                X11BindingChange::Update {
                    line: 2,
                    source: "/tmp/.X11-unix/X0".into(),
                    guest_target: "/mnt/X1".into(),
                    readonly: false,
                },
                X11BindingChange::Update {
                    line: 3,
                    source: "/tmp/.X11-unix/X1".into(),
                    guest_target: "/mnt/X0".into(),
                    readonly: false,
                },
            ],
        );
        assert!(matches!(prepare(&snapshot, &swap), Preparation::Ready(_)));
    }

    #[tokio::test]
    async fn apply_writes_atomically_after_revalidating_the_discovery_revision() {
        let root = tempfile::tempdir().unwrap();
        let admin = root.path().join("etc");
        let runtime = root.path().join("run");
        let images = root.path().join("images");
        std::fs::create_dir_all(&admin).unwrap();
        let path = admin.join("arch.nspawn");
        std::fs::write(
            &path,
            "[Files]\nBind=/tmp/.X11-unix/X0:/mnt/x11\nBind=/dev/dri\n",
        )
        .unwrap();
        let target = ConfigurationTarget::Machine(MachineName::new("arch").unwrap());
        let snapshot = inspect_at(target.clone(), &admin, &runtime, &images);
        let request = ConfigurationEdit {
            target,
            base_revision: snapshot.revision.clone().unwrap(),
            x11_changes: vec![X11BindingChange::Update {
                line: 2,
                source: "/tmp/.X11-unix/X0_".into(),
                guest_target: "/tmp/.X11-unix/X0".into(),
                readonly: true,
            }],
            wayland_changes: vec![],
        };

        let report = apply_with_sources(request, snapshot, admin, runtime, images)
            .await
            .unwrap();
        assert!(matches!(report, ConfigurationApplyReport::Applied { .. }));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "[Files]\nBindReadOnly=/tmp/.X11-unix/X0_:/tmp/.X11-unix/X0\nBind=/dev/dri\n"
        );
    }

    #[tokio::test]
    async fn apply_keeps_external_changes_and_returns_a_conflict() {
        let root = tempfile::tempdir().unwrap();
        let admin = root.path().join("etc");
        let runtime = root.path().join("run");
        let images = root.path().join("images");
        std::fs::create_dir_all(&admin).unwrap();
        let path = admin.join("arch.nspawn");
        std::fs::write(&path, "[Files]\nBind=/tmp/.X11-unix\n").unwrap();
        let target = ConfigurationTarget::Machine(MachineName::new("arch").unwrap());
        let snapshot = inspect_at(target.clone(), &admin, &runtime, &images);
        let request = ConfigurationEdit {
            target,
            base_revision: snapshot.revision.clone().unwrap(),
            x11_changes: vec![X11BindingChange::Remove { line: 2 }],
            wayland_changes: vec![],
        };
        std::fs::write(&path, "# external edit\n[Files]\nBind=/tmp/.X11-unix\n").unwrap();

        let report = apply_with_sources(request, snapshot, admin, runtime, images)
            .await
            .unwrap();
        assert!(matches!(report, ConfigurationApplyReport::Conflict { .. }));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "# external edit\n[Files]\nBind=/tmp/.X11-unix\n"
        );
    }
}
