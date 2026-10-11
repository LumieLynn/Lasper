//! Shared systemd inspection pipeline.
//!
//! Transport adapters retrieve properties, then feed either typed values or
//! canonical tool text into this module. It preserves raw values and actual
//! interfaces, classifies presentation groups, and renders systemd semantics
//! once before the snapshot crosses application/IPC boundaries.

mod catalog;
mod decode;
mod network;
mod render;

use crate::domain::inspection::{MachineProperties, PropertyEntry, PropertyValue};
pub(crate) use catalog::PropertyScope;
pub(crate) use decode::tool_properties;
pub(crate) use network::format_ip_address;
use std::collections::HashMap;
use zbus::zvariant::{OwnedValue, Value};

pub(crate) fn append_dbus(
    properties: &mut MachineProperties,
    scope: PropertyScope,
    values: HashMap<String, OwnedValue>,
) {
    for (name, value) in values {
        let value: Value<'_> = value.into();
        let value = decode::decode(&value);
        let mut entry = typed_entry(&name, value);
        entry.interface = Some(scope.interface().into());
        properties
            .get_group_mut(scope.group(&name))
            .insert(name, entry);
    }
}

pub(crate) fn append_tool_property(properties: &mut MachineProperties, name: String, text: String) {
    let group = catalog::tool_scope(&name).group(&name);
    properties.insert(group, name, text);
}

fn typed_entry(name: &str, value: PropertyValue) -> PropertyEntry {
    let display = render::render(name, &value);
    PropertyEntry::typed(value, display)
}

#[cfg(test)]
mod tests;
