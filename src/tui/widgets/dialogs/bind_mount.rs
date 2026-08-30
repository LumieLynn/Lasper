use crate::domain::provisioning::{BindMount, IdmapSuffix};
use crate::tui::core::{AppMessage, Component, FocusTracker, WizardMessage};

use crate::tui::widgets::inputs::button::Button;
use crate::tui::widgets::inputs::path_box::PathBox;
use crate::tui::widgets::selectors::checkbox::Checkbox;
use crate::tui::widgets::selectors::radio_group::RadioGroup;

const IDMAP_SUFFIX_OPTIONS: &[(&str, IdmapSuffix)] = &[
    ("None", IdmapSuffix::None),
    ("noidmap", IdmapSuffix::Noidmap),
    ("idmap", IdmapSuffix::Idmap),
    ("rootidmap", IdmapSuffix::Rootidmap),
    ("owneridmap", IdmapSuffix::Owneridmap),
];

fn idmap_suffix_options() -> Vec<String> {
    IDMAP_SUFFIX_OPTIONS
        .iter()
        .map(|(label, _)| (*label).to_string())
        .collect()
}

fn idmap_suffix_index(suffix: &IdmapSuffix) -> Option<usize> {
    IDMAP_SUFFIX_OPTIONS
        .iter()
        .position(|(_, option)| option == suffix)
}

fn idmap_suffix_from_index(index: usize) -> Option<IdmapSuffix> {
    IDMAP_SUFFIX_OPTIONS
        .get(index)
        .map(|(_, suffix)| suffix.clone())
}

pub(crate) fn idmap_suffix_label(suffix: &IdmapSuffix) -> &'static str {
    IDMAP_SUFFIX_OPTIONS
        .iter()
        .find(|(_, option)| option == suffix)
        .map(|(label, _)| *label)
        .unwrap_or("unknown")
}

macro_rules! active_comps {
    ($self:ident) => {{
        let comps: Vec<&mut dyn Component> = vec![
            &mut $self.source_path,
            &mut $self.target_path,
            &mut $self.readonly,
            &mut $self.suffix,
            &mut $self.btn_ok,
            &mut $self.btn_cancel,
        ];
        comps
    }};
}

pub struct BindMountBox {
    source_path: PathBox,
    target_path: PathBox,
    readonly: Checkbox,
    suffix: RadioGroup,
    btn_ok: Button,
    btn_cancel: Button,
    focus: FocusTracker,
    on_submit: Box<dyn Fn(BindMount) -> AppMessage>,
}

impl BindMountBox {
    pub fn new(on_submit: impl Fn(BindMount) -> AppMessage + 'static) -> Self {
        Self {
            source_path: PathBox::new("Source Path", "/".to_string()).with_validator(|v| {
                let path = std::path::Path::new(v.trim());
                if v.trim().is_empty() {
                    return Err("Path required".into());
                }
                if !path.is_absolute() {
                    return Err("Must be absolute path".into());
                }
                if !path.exists() {
                    return Err("Path does not exist".into());
                }
                Ok(())
            }),
            target_path: PathBox::new("Target Path (optional, defaults to source)", "".to_string())
                .with_validator(|v| {
                    let trimmed = v.trim();
                    if trimmed.is_empty() {
                        return Ok(());
                    }
                    if !std::path::Path::new(trimmed).is_absolute() {
                        return Err("Must be absolute path".into());
                    }
                    Ok(())
                }),
            readonly: Checkbox::new("Read Only", false),
            suffix: RadioGroup::new("ID Mapping", idmap_suffix_options(), 0),
            btn_ok: Button::new("OK", || AppMessage::Wizard(WizardMessage::DialogSubmit)),
            btn_cancel: Button::new("Cancel", || AppMessage::Wizard(WizardMessage::DialogCancel)),

            focus: FocusTracker::new(),
            on_submit: Box::new(on_submit),
        }
    }

    pub fn with_mount(mut self, bm: &BindMount) -> Self {
        self.source_path = PathBox::new("Source Path", bm.source.clone()).with_validator(|v| {
            let path = std::path::Path::new(v.trim());
            if v.trim().is_empty() {
                return Err("Path required".into());
            }
            if !path.is_absolute() {
                return Err("Must be absolute path".into());
            }
            if !path.exists() {
                return Err("Path does not exist".into());
            }
            Ok(())
        });
        self.target_path = PathBox::new("Target Path (optional)", bm.target.clone())
            .with_validator(|v| {
                let trimmed = v.trim();
                if trimmed.is_empty() {
                    return Ok(());
                }
                if !std::path::Path::new(trimmed).is_absolute() {
                    return Err("Must be absolute path".into());
                }
                Ok(())
            });
        self.readonly = Checkbox::new("Read Only", bm.readonly);
        self.suffix = RadioGroup::new(
            "ID Mapping",
            idmap_suffix_options(),
            idmap_suffix_index(&bm.suffix).unwrap_or(0),
        );
        self.update_focus();
        self
    }

    fn try_submit(&mut self) -> Option<AppMessage> {
        let mut valid = true;
        if self.source_path.validate().is_err() {
            valid = false;
        }
        if self.target_path.validate().is_err() {
            valid = false;
        }
        if !valid {
            return None;
        }
        let source = self.source_path.value().trim().to_string();
        let mut target = self.target_path.value().trim().to_string();
        if target.is_empty() {
            target = source.clone();
        }
        let suffix = idmap_suffix_from_index(self.suffix.selected_idx())?;
        Some((self.on_submit)(BindMount {
            source,
            target,
            readonly: self.readonly.checked(),
            suffix,
        }))
    }
}

form_dialog!(
    BindMountBox,
    " Add Bind Mount ",
    (45, 55),
    [source_path, target_path, readonly, suffix]
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idmap_selector_round_trips_configuration_values() {
        for suffix in [
            IdmapSuffix::None,
            IdmapSuffix::Noidmap,
            IdmapSuffix::Idmap,
            IdmapSuffix::Rootidmap,
            IdmapSuffix::Owneridmap,
        ] {
            let index = idmap_suffix_index(&suffix).expect("all config values are selectable");
            assert_eq!(idmap_suffix_from_index(index), Some(suffix));
        }
    }

    #[test]
    fn idmap_selector_rejects_out_of_range_values() {
        assert_eq!(idmap_suffix_from_index(usize::MAX), None);
        assert_eq!(idmap_suffix_label(&IdmapSuffix::Idmap), "idmap");
    }
}
