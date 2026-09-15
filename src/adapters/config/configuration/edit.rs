//! Finite X11 bind edits for Configure. Preview and apply share the same
//! policy checks and byte-preserving patch calculation.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use super::inspection::{inspect, inspect_at};
use super::projection::{bind_destinations, x11_scope};
use crate::adapters::config::nspawn_file::{escape_nspawn_bind_path, is_nvidia_begin_marker};
use crate::adapters::error::Result;
use crate::adapters::filesystem::AsyncLockedWriter;
use crate::application::configuration::{
    ConfigurationActivation, ConfigurationApplyReport, ConfigurationEdit, ConfigurationOrigin,
    ConfigurationPreview, ConfigurationSnapshot, X11BindRecommendation, X11BindingChange,
    X11BindingDeclaration, X11BindingScope,
};
use crate::domain::machine::MachineName;

const MAX_X11_CHANGES: usize = 128;
const MAX_CONFIG_PATH_BYTES: usize = 4096;
const MAX_DIFF_BYTES: usize = 256 * 1024;
const DIFF_CONTEXT_LINES: usize = 2;

pub(crate) async fn preview(edit: ConfigurationEdit) -> Result<ConfigurationPreview> {
    let snapshot = inspect(edit.target.clone()).await?;
    Ok(match prepare(&snapshot, &edit) {
        Preparation::Ready(change) => ConfigurationPreview::Ready {
            path: change.path,
            diff: change.diff,
            activation: ConfigurationActivation::NextMachineStart,
        },
        Preparation::Unchanged(path) => ConfigurationPreview::Unchanged { path },
        Preparation::Blocked(reason) => ConfigurationPreview::Blocked { reason },
        Preparation::Conflict(reason) => ConfigurationPreview::Conflict { reason },
    })
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
        PathBuf::from("/etc/systemd/nspawn"),
        PathBuf::from("/run/systemd/nspawn"),
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

enum Preparation {
    Ready(PreparedChange),
    Unchanged(PathBuf),
    Blocked(String),
    Conflict(String),
}

impl Preparation {
    fn into_apply_report(self) -> ConfigurationApplyReport {
        match self {
            Self::Ready(_) => unreachable!("ready changes are written by the caller"),
            Self::Unchanged(path) => ConfigurationApplyReport::Unchanged { path },
            Self::Blocked(reason) => ConfigurationApplyReport::Blocked { reason },
            Self::Conflict(reason) => ConfigurationApplyReport::Conflict { reason },
        }
    }
}

struct PreparedChange {
    path: PathBuf,
    after: String,
    diff: String,
}

fn prepare(snapshot: &ConfigurationSnapshot, edit: &ConfigurationEdit) -> Preparation {
    if snapshot.target != edit.target {
        return Preparation::Blocked(
            "The draft target does not match the inspected resource".into(),
        );
    }
    let Some(revision) = &snapshot.revision else {
        return Preparation::Blocked(
            "Configuration discovery was incomplete; resolve the source error before editing"
                .into(),
        );
    };
    if revision != &edit.base_revision {
        return Preparation::Conflict(
            "The selected configuration source or file revision changed; refresh before saving"
                .into(),
        );
    }
    let Some(document) = &snapshot.document else {
        return Preparation::Blocked(
            "No existing administrator configuration contains this declaration".into(),
        );
    };
    if document.origin != ConfigurationOrigin::Administrator {
        return Preparation::Blocked(format!(
            "{} is inspect-only; creating an administrator file would change source precedence",
            document.origin.label()
        ));
    }
    let Some(write_target) = &snapshot.write_target else {
        return Preparation::Blocked(
            "This resource cannot be mapped to a safe administrator write target".into(),
        );
    };
    if !write_target.exists || write_target.path != document.path {
        return Preparation::Blocked(
            "The selected source is not the existing administrator write target".into(),
        );
    }
    if edit.x11_changes.len() > MAX_X11_CHANGES {
        return Preparation::Blocked(format!(
            "A single draft may change at most {MAX_X11_CHANGES} X11 declarations"
        ));
    }

    let mutations = match calculate_mutations(
        &document.content,
        &snapshot.x11_bindings,
        &snapshot.x11_bind_recommendation,
        edit,
    ) {
        Ok(mutations) => mutations,
        Err(reason) => return Preparation::Blocked(reason),
    };
    if mutations.is_empty() {
        return Preparation::Unchanged(document.path.clone());
    }
    let diff = render_diff(&document.path, &document.content, &mutations);
    if diff.len() > MAX_DIFF_BYTES {
        return Preparation::Blocked(format!(
            "The generated diff exceeds the {MAX_DIFF_BYTES}-byte preview limit; edit this declaration manually"
        ));
    }
    let mut after = document.content.clone();
    for mutation in mutations.iter().rev() {
        after.replace_range(
            mutation.start..mutation.end,
            mutation.replacement.as_deref().unwrap_or_default(),
        );
    }
    Preparation::Ready(PreparedChange {
        path: document.path.clone(),
        after,
        diff,
    })
}

struct PhysicalLine<'a> {
    number: usize,
    start: usize,
    end: usize,
    body: &'a str,
    ending: &'a str,
}

#[derive(Debug)]
struct Mutation {
    line: usize,
    start: usize,
    end: usize,
    old: Vec<String>,
    new: Vec<String>,
    replacement: Option<String>,
}

fn calculate_mutations(
    content: &str,
    declarations: &[X11BindingDeclaration],
    recommendation: &X11BindRecommendation,
    edit: &ConfigurationEdit,
) -> std::result::Result<Vec<Mutation>, String> {
    let lines = physical_lines(content);
    let mut requested = BTreeMap::new();
    let mut additions = Vec::new();
    for change in &edit.x11_changes {
        match change.declaration_line() {
            Some(0) => return Err("Declaration line numbers start at one".into()),
            Some(line) => {
                if requested.insert(line, change).is_some() {
                    return Err(format!(
                        "Line {line} is changed more than once in this draft"
                    ));
                }
            }
            None => additions.push(change),
        }
    }

    let mut mutations = Vec::with_capacity(requested.len());
    let final_targets = bind_destinations(content)
        .into_iter()
        .filter_map(|(line, target)| match requested.get(&line) {
            Some(X11BindingChange::Add { .. }) => {
                unreachable!("additions are not indexed by declaration line")
            }
            Some(X11BindingChange::Remove { .. }) => None,
            Some(X11BindingChange::Update { guest_target, .. }) => {
                Some((line, guest_target.clone()))
            }
            None => Some((line, target)),
        })
        .collect::<Vec<_>>();
    for (line_number, change) in requested {
        let Some(declaration) = declarations
            .iter()
            .find(|declaration| declaration.line == line_number)
        else {
            return Err(format!(
                "Line {line_number} is not a recognized X11 bind in this revision"
            ));
        };
        let Some(line) = lines.get(line_number - 1) else {
            return Err(format!("Line {line_number} no longer exists"));
        };
        if has_continuation(line.body) {
            return Err(format!(
                "Line {line_number} uses continuation syntax and must be edited manually"
            ));
        }

        let new = match change {
            X11BindingChange::Add { .. } => {
                unreachable!("additions are not indexed by declaration line")
            }
            X11BindingChange::Remove { .. } => None,
            X11BindingChange::Update {
                source,
                guest_target,
                readonly,
                ..
            } => {
                validate_source(source)
                    .map_err(|reason| format!("Line {line_number}: {reason}"))?;
                validate_guest_target(guest_target)
                    .map_err(|reason| format!("Line {line_number}: {reason}"))?;
                if guest_target != &declaration.guest_target
                    && final_targets.iter().any(|(other_line, target)| {
                        *other_line != line_number && target == guest_target
                    })
                {
                    return Err(format!(
                        "Line {line_number}: guest target {} is already used by another bind",
                        guest_target.display()
                    ));
                }
                if source == &declaration.source
                    && guest_target == &declaration.guest_target
                    && readonly == &declaration.readonly
                {
                    continue;
                }
                Some(render_binding(
                    line.body,
                    line.ending,
                    declaration,
                    source,
                    guest_target,
                    *readonly,
                ))
            }
        };
        mutations.push(Mutation {
            line: line.number,
            start: line.start,
            end: line.end,
            old: vec![line.body.to_string()],
            new: new
                .as_ref()
                .map(|replacement| {
                    vec![replacement
                        .strip_suffix(line.ending)
                        .unwrap_or(replacement)
                        .to_string()]
                })
                .unwrap_or_default(),
            replacement: new,
        });
    }

    if !additions.is_empty() {
        let idmapped = match recommendation {
            X11BindRecommendation::Ready { idmapped, .. } => *idmapped,
            X11BindRecommendation::Unsupported { reason, .. } => return Err(reason.clone()),
        };
        let mut rendered = Vec::with_capacity(additions.len());
        let mut occupied_targets = final_targets
            .into_iter()
            .map(|(_, target)| target)
            .collect::<Vec<_>>();
        for change in additions {
            let X11BindingChange::Add { source } = change else {
                unreachable!("changes without declaration lines are additions")
            };
            validate_source(source).map_err(|reason| format!("New X11 bind: {reason}"))?;
            if !matches!(x11_scope(source), Some(X11BindingScope::Socket { .. })) {
                return Err(format!(
                    "New X11 bind: {} is not an individual X11 socket endpoint",
                    source.display()
                ));
            }
            if occupied_targets.iter().any(|target| target == source) {
                return Err(format!(
                    "New X11 bind: guest target {} is already used by another bind",
                    source.display()
                ));
            }
            occupied_targets.push(source.clone());
            rendered.push(render_new_binding(source, idmapped));
        }
        mutations.push(insertion_mutation(content, &lines, &rendered));
    }
    Ok(mutations)
}

fn physical_lines(content: &str) -> Vec<PhysicalLine<'_>> {
    let mut offset = 0;
    content
        .split_inclusive('\n')
        .enumerate()
        .map(|(index, raw)| {
            let (body, ending) = if let Some(body) = raw.strip_suffix("\r\n") {
                (body, "\r\n")
            } else if let Some(body) = raw.strip_suffix('\n') {
                (body, "\n")
            } else {
                (raw, "")
            };
            let line = PhysicalLine {
                number: index + 1,
                start: offset,
                end: offset + raw.len(),
                body,
                ending,
            };
            offset += raw.len();
            line
        })
        .collect()
}

fn has_continuation(line: &str) -> bool {
    line.bytes().rev().take_while(|byte| *byte == b'\\').count() % 2 == 1
}

fn validate_source(path: &Path) -> std::result::Result<(), &'static str> {
    validate_plain_absolute_path(path, "X11 source")?;
    if x11_scope(path).is_none() {
        return Err("source must be /tmp/.X11-unix or one of its X<number> endpoints");
    }
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

fn render_binding(
    old_body: &str,
    ending: &str,
    declaration: &X11BindingDeclaration,
    source: &Path,
    guest_target: &Path,
    readonly: bool,
) -> String {
    let indent_len = old_body.len() - old_body.trim_start().len();
    let indent = &old_body[..indent_len];
    let key = if readonly { "BindReadOnly" } else { "Bind" };
    let source = escape_nspawn_bind_path(source.to_str().expect("validated UTF-8 path"));
    let target =
        escape_nspawn_bind_path(guest_target.to_str().expect("validated UTF-8 guest target"));
    let mut rendered = format!("{indent}{key}={source}:{target}");
    if !declaration.options.is_empty() {
        rendered.push(':');
        rendered.push_str(&declaration.options.join(","));
    }
    rendered.push_str(ending);
    rendered
}

fn render_new_binding(source: &Path, idmapped: bool) -> String {
    let source = escape_nspawn_bind_path(source.to_str().expect("validated UTF-8 path"));
    let suffix = if idmapped { ":idmap" } else { "" };
    format!("BindReadOnly={source}:{source}{suffix}")
}

fn insertion_mutation(content: &str, lines: &[PhysicalLine<'_>], bindings: &[String]) -> Mutation {
    let ending = preferred_line_ending(content);
    let mut in_files = false;
    let mut files_seen = false;
    let mut offset = content.len();
    let mut before_line = lines.len().saturating_add(1);

    for line in lines {
        let trimmed = line.body.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if trimmed.eq_ignore_ascii_case("[Files]") {
                in_files = true;
                files_seen = true;
                offset = line.end;
                before_line = line.number.saturating_add(1);
            } else if in_files {
                in_files = false;
                offset = line.start;
                before_line = line.number;
            }
        } else if in_files && is_nvidia_begin_marker(line.body) {
            offset = line.start;
            before_line = line.number;
            break;
        } else if in_files {
            offset = line.end;
            before_line = line.number.saturating_add(1);
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
    let new = physical_lines(&replacement)
        .into_iter()
        .map(|line| line.body.to_string())
        .collect();
    Mutation {
        line: before_line.max(1),
        start: offset,
        end: offset,
        old: Vec::new(),
        new,
        replacement: Some(replacement),
    }
}

fn preferred_line_ending(content: &str) -> &'static str {
    match content.find('\n') {
        Some(index) if content.as_bytes().get(index.wrapping_sub(1)) == Some(&b'\r') => "\r\n",
        _ => "\n",
    }
}

fn render_diff(path: &Path, content: &str, mutations: &[Mutation]) -> String {
    let lines = physical_lines(content);
    let mut diff = format!("--- {}\n+++ {}\n", path.display(), path.display());
    if lines.is_empty() {
        let inserted = mutations
            .iter()
            .flat_map(|mutation| mutation.new.iter())
            .collect::<Vec<_>>();
        diff.push_str(&format!("@@ -0,0 +1,{} @@\n", inserted.len()));
        for line in inserted {
            diff.push('+');
            diff.push_str(line);
            diff.push('\n');
        }
        return diff;
    }

    let mut ranges = Vec::<(usize, usize)>::new();
    for mutation in mutations {
        let (start, end) = if mutation.old.is_empty() {
            let point = mutation.line.clamp(1, lines.len() + 1);
            (
                point.saturating_sub(DIFF_CONTEXT_LINES).max(1),
                point
                    .saturating_add(DIFF_CONTEXT_LINES.saturating_sub(1))
                    .min(lines.len()),
            )
        } else {
            (
                mutation.line.saturating_sub(DIFF_CONTEXT_LINES).max(1),
                mutation
                    .line
                    .saturating_add(DIFF_CONTEXT_LINES)
                    .min(lines.len()),
            )
        };
        match ranges.last_mut() {
            Some((_, previous_end)) if start <= previous_end.saturating_add(1) => {
                *previous_end = (*previous_end).max(end);
            }
            _ => ranges.push((start, end)),
        }
    }
    let replacements = mutations
        .iter()
        .filter(|mutation| !mutation.old.is_empty())
        .map(|mutation| (mutation.line, mutation))
        .collect::<BTreeMap<_, _>>();
    for (start, end) in ranges {
        let delta_before = mutations
            .iter()
            .filter(|mutation| mutation.line < start)
            .map(|mutation| mutation.new.len() as isize - mutation.old.len() as isize)
            .sum::<isize>();
        let old_count = end - start + 1;
        let delta_here = mutations
            .iter()
            .filter(|mutation| {
                if mutation.old.is_empty() {
                    (start..=end.saturating_add(1)).contains(&mutation.line)
                } else {
                    (start..=end).contains(&mutation.line)
                }
            })
            .map(|mutation| mutation.new.len() as isize - mutation.old.len() as isize)
            .sum::<isize>();
        let new_start = (start as isize + delta_before).max(0) as usize;
        let new_count = (old_count as isize + delta_here).max(0) as usize;
        diff.push_str(&format!(
            "@@ -{start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for line_number in start..=end {
            for mutation in mutations
                .iter()
                .filter(|mutation| mutation.old.is_empty() && mutation.line == line_number)
            {
                for new in &mutation.new {
                    diff.push('+');
                    diff.push_str(new);
                    diff.push('\n');
                }
            }
            if let Some(mutation) = replacements.get(&line_number) {
                for old in &mutation.old {
                    diff.push('-');
                    diff.push_str(old);
                    diff.push('\n');
                }
                for new in &mutation.new {
                    diff.push('+');
                    diff.push_str(new);
                    diff.push('\n');
                }
            } else if let Some(line) = lines.get(line_number - 1) {
                diff.push(' ');
                diff.push_str(line.body);
                diff.push('\n');
            }
        }
        for mutation in mutations
            .iter()
            .filter(|mutation| mutation.old.is_empty() && mutation.line == end + 1)
        {
            for new in &mutation.new {
                diff.push('+');
                diff.push_str(new);
                diff.push('\n');
            }
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::configuration::projection::project;
    use crate::adapters::config::nspawn_file::NspawnConfig;
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
