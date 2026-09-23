//! Shared preparation for source-preserving configuration writes.
//!
//! Page adapters provide only a bounded mutation calculation. Revision,
//! ownership, diff limits, and byte-preserving application belong here so a
//! future page cannot accidentally weaken those preconditions.

use std::path::PathBuf;

use super::document::NspawnDocument;
use super::patch::{apply_mutations, render_diff, SourceMutation};
use crate::application::configuration::{
    ConfigurationApplyReport, ConfigurationPreview, ConfigurationRevision, ConfigurationSnapshot,
    ConfigurationTarget, ConfigurationWriteError,
};

const MAX_DIFF_BYTES: usize = 256 * 1024;

pub(super) enum Preparation {
    Ready(PreparedChange),
    Unchanged(PathBuf),
    Blocked(String),
    Conflict(String),
}

impl Preparation {
    pub(super) fn into_preview(self) -> ConfigurationPreview {
        match self {
            Self::Ready(change) => ConfigurationPreview::Ready {
                path: change.path,
                diff: change.diff,
                activation:
                    crate::application::configuration::ConfigurationActivation::NextMachineStart,
            },
            Self::Unchanged(path) => ConfigurationPreview::Unchanged { path },
            Self::Blocked(reason) => ConfigurationPreview::Blocked { reason },
            Self::Conflict(reason) => ConfigurationPreview::Conflict { reason },
        }
    }

    pub(super) fn into_apply_report(self) -> ConfigurationApplyReport {
        match self {
            Self::Ready(_) => unreachable!("ready changes are written by the caller"),
            Self::Unchanged(path) => ConfigurationApplyReport::Unchanged { path },
            Self::Blocked(reason) => ConfigurationApplyReport::Blocked { reason },
            Self::Conflict(reason) => ConfigurationApplyReport::Conflict { reason },
        }
    }
}

pub(super) struct PreparedChange {
    pub(super) path: PathBuf,
    pub(super) after: String,
    pub(super) diff: String,
}

pub(super) fn prepare_patch<'a, F>(
    snapshot: &'a ConfigurationSnapshot,
    target: &ConfigurationTarget,
    base_revision: &ConfigurationRevision,
    calculate: F,
) -> Preparation
where
    F: FnOnce(&NspawnDocument<'a>) -> Result<Vec<SourceMutation>, String>,
{
    let context =
        match snapshot.write_context(target, base_revision) {
            Ok(context) => context,
            Err(ConfigurationWriteError::RevisionChanged) => return Preparation::Conflict(
                "The selected configuration source or file revision changed; refresh before saving"
                    .into(),
            ),
            Err(error) => return Preparation::Blocked(error.to_string()),
        };
    let source = context.document;
    let document = NspawnDocument::new(&source.content);
    let mutations = match calculate(&document) {
        Ok(mutations) => mutations,
        Err(reason) => return Preparation::Blocked(reason),
    };
    if mutations.is_empty() {
        return Preparation::Unchanged(source.path.clone());
    }
    let diff = render_diff(&source.path, &document, &mutations);
    if diff.len() > MAX_DIFF_BYTES {
        return Preparation::Blocked(format!(
            "The generated diff exceeds the {MAX_DIFF_BYTES}-byte preview limit; edit this declaration manually"
        ));
    }
    let after = apply_mutations(document.content(), &mutations);
    Preparation::Ready(PreparedChange {
        path: source.path.clone(),
        after,
        diff,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::configuration::patch::SourceMutation;

    #[test]
    fn patch_preparation_keeps_a_bounded_mutation_result() {
        let source = crate::application::configuration::ConfigurationDocument {
            path: "/etc/systemd/nspawn/arch.nspawn".into(),
            origin: crate::application::configuration::ConfigurationOrigin::Administrator,
            content: "[Files]\nBind=/dev/dri\n".into(),
            content_sha256: String::new(),
        };
        let snapshot = crate::application::configuration::ConfigurationSnapshot {
            target: ConfigurationTarget::Machine(
                crate::domain::machine::MachineName::new("arch").unwrap(),
            ),
            discovery:
                crate::application::configuration::ConfigurationDiscovery::MachineNameCandidates,
            document: Some(source),
            candidates: Vec::new(),
            revision: Some(ConfigurationRevision {
                discovery: "d".into(),
                read_source: Some("r".into()),
                write_target: Some("w".into()),
            }),
            write_target: Some(
                crate::application::configuration::ConfigurationWriteTarget {
                    path: "/etc/systemd/nspawn/arch.nspawn".into(),
                    exists: true,
                },
            ),
            x11_bindings: Vec::new(),
            x11_bind_recommendation:
                crate::application::configuration::X11BindRecommendation::Ready {
                    private_users: "no".into(),
                    idmapped: false,
                },
            host_x11: Default::default(),
            other_bind_count: 0,
            diagnostics: Vec::new(),
        };
        let target = snapshot.target.clone();
        let revision = snapshot.revision.clone().unwrap();
        let prepared = prepare_patch(&snapshot, &target, &revision, |document| {
            let line = &document.lines()[1];
            Ok(vec![SourceMutation {
                line: 2,
                start: line.start(),
                end: line.end(),
                old: vec![line.body().to_owned()],
                new: vec!["Bind=/dev/dri".into()],
                replacement: Some("Bind=/dev/dri\n".into()),
            }])
        });
        assert!(matches!(prepared, Preparation::Ready(_)));
    }
}
