//! Application-owned configuration change assembly.
//!
//! Presentation layers submit bounded intents to this type. It keeps the
//! accumulated changes independent of focus widgets and constructs the single
//! revision-bound edit sent to the configuration port. File ownership,
//! validation, and byte-level mutation calculation remain adapter concerns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{ConfigurationEdit, ConfigurationSnapshot, ConfigurationTarget, X11BindingChange};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum X11IntentKey {
    Declaration(usize),
    Addition(PathBuf),
}

/// A bounded set of configuration intents accumulated across configuration
/// pages. It is independent of TUI focus and rendering state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigurationDraft {
    x11: BTreeMap<X11IntentKey, X11BindingChange>,
}

impl ConfigurationDraft {
    pub fn clear(&mut self) {
        self.x11.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.x11.is_empty()
    }

    pub fn toggle_x11_declaration(&mut self, line: usize) {
        let key = X11IntentKey::Declaration(line);
        if self.x11.remove(&key).is_none() {
            self.set_x11_change(X11BindingChange::Remove { line });
        }
    }

    /// Replace the pending intent for one X11 declaration or source. A page
    /// can use this for future update controls without owning the change map.
    pub fn set_x11_change(&mut self, change: X11BindingChange) {
        let key = match &change {
            X11BindingChange::Add { source } => X11IntentKey::Addition(source.clone()),
            X11BindingChange::Update { line, .. } | X11BindingChange::Remove { line } => {
                X11IntentKey::Declaration(*line)
            }
        };
        self.x11.insert(key, change);
    }

    pub fn toggle_x11_source(&mut self, source: &Path) {
        let key = X11IntentKey::Addition(source.to_path_buf());
        if self.x11.remove(&key).is_none() {
            self.set_x11_change(X11BindingChange::Add {
                source: source.to_path_buf(),
            });
        }
    }

    pub fn x11_change_for_declaration(&self, line: usize) -> Option<&X11BindingChange> {
        self.x11.get(&X11IntentKey::Declaration(line))
    }

    pub fn x11_change_for_source(&self, source: &Path) -> Option<&X11BindingChange> {
        self.x11.get(&X11IntentKey::Addition(source.to_path_buf()))
    }

    /// Build the single application edit submitted for preview or apply.
    /// The snapshot revision binds all accumulated page intents to one
    /// inspected resource.
    pub fn edit(
        &self,
        target: &ConfigurationTarget,
        snapshot: &ConfigurationSnapshot,
    ) -> Option<ConfigurationEdit> {
        Some(ConfigurationEdit {
            target: target.clone(),
            base_revision: snapshot.revision.clone()?,
            x11_changes: self.x11.values().cloned().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggling_an_x11_intent_is_reversible() {
        let mut draft = ConfigurationDraft::default();
        draft.toggle_x11_declaration(7);
        assert!(matches!(
            draft.x11_change_for_declaration(7),
            Some(X11BindingChange::Remove { line: 7 })
        ));
        draft.toggle_x11_declaration(7);
        assert!(draft.x11_change_for_declaration(7).is_none());
        draft.set_x11_change(X11BindingChange::Update {
            line: 7,
            source: "/tmp/.X11-unix/X1".into(),
            guest_target: "/mnt/X1".into(),
            readonly: true,
        });
        assert!(matches!(
            draft.x11_change_for_declaration(7),
            Some(X11BindingChange::Update { line: 7, .. })
        ));

        draft.toggle_x11_source(Path::new("/tmp/.X11-unix/X0"));
        assert!(matches!(
            draft.x11_change_for_source(Path::new("/tmp/.X11-unix/X0")),
            Some(X11BindingChange::Add { source }) if source == Path::new("/tmp/.X11-unix/X0")
        ));
        draft.clear();
        assert!(draft.is_empty());
    }
}
