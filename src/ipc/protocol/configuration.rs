//! Closed editor operations, separate from provisioning's nspawn file writes.

use crate::application::configuration::{
    ConfigurationApplyReport, ConfigurationEdit, ConfigurationError, ConfigurationPreview,
    ConfigurationSnapshot, ConfigurationTarget,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ConfigurationOperation {
    Inspect(ConfigurationTarget),
    Preview(Box<ConfigurationEdit>),
    Apply(Box<ConfigurationEdit>),
}

impl ConfigurationOperation {
    pub(crate) fn is_mutation(&self) -> bool {
        matches!(self, Self::Apply(_))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ConfigurationValue {
    Inspection(Box<ConfigurationSnapshot>),
    Preview(ConfigurationPreview),
    Apply(ConfigurationApplyReport),
}

pub(crate) type ConfigurationResult = Result<ConfigurationValue, ConfigurationError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::machine::MachineName;
    use std::path::Path;
    #[test]
    fn configuration_edit_wire_keeps_wayland_intents() {
        let operation = ConfigurationOperation::Preview(Box::new(ConfigurationEdit {
            target: ConfigurationTarget::Machine(MachineName::new("archlinux").unwrap()),
            base_revision: crate::application::configuration::ConfigurationRevision {
                discovery: "machine-name".into(),
                read_source: Some("administrator".into()),
                write_target: Some("administrator".into()),
            },
            x11_changes: Vec::new(),
            wayland_changes: vec![
                crate::application::configuration::WaylandBindingChange::Add {
                    source: "/run/user/1000/wayland-1".into(),
                    guest_target: "/run/lasper/wayland/1000/wayland-1".into(),
                },
            ],
        }));

        let wire = serde_json::to_vec(&operation).unwrap();
        let decoded: ConfigurationOperation = serde_json::from_slice(&wire).unwrap();
        let ConfigurationOperation::Preview(edit) = decoded else {
            panic!("configuration operation changed while crossing the RPC wire");
        };
        assert!(matches!(
            edit.wayland_changes.as_slice(),
            [crate::application::configuration::WaylandBindingChange::Add {
                source,
                guest_target,
            }] if source == Path::new("/run/user/1000/wayland-1")
                && guest_target == Path::new("/run/lasper/wayland/1000/wayland-1")
        ));
    }
}
