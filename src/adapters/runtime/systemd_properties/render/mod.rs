//! Typed systemd property presentation. The generic printer handles new
//! properties; scalar and compound handlers cover systemd's semantic cases.

mod bitmap;
mod compound;
mod execution;
mod generic;
mod scalar;
mod time;

use crate::domain::inspection::PropertyValue;

pub(super) fn render(name: &str, value: &PropertyValue) -> String {
    // Tool output is already canonical. Even a numeric-looking value is not
    // reparsed, so both runtime routes retain the same systemd representation.
    if let PropertyValue::CanonicalText(text) = value {
        return text.clone();
    }
    if let PropertyValue::Variant(value) = value {
        return render(name, value);
    }
    scalar::render(name, value)
        .or_else(|| compound::render(name, value))
        .unwrap_or_else(|| {
            if generic::is_supported(value) {
                generic::generic(value)
            } else {
                "[unprintable]".into()
            }
        })
}
