use crate::domain::provisioning::{BindMount, IdmapSuffix};
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::widgets::inputs::{
    button::ButtonControl,
    text_input::{Input, TextInput},
};
use crate::tui::foundation::widgets::selectors::{
    checkbox::CheckboxControl,
    radio::{RadioControl, RadioState},
};
use crate::tui::foundation::{footer::FooterRegistry, geometry::centered_rect, theme};
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Borders, Clear},
    Frame,
};

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum BindControl {
    Source,
    Target,
    Readonly,
    Suffix,
    Submit,
    Cancel,
}

pub struct BindMountBox {
    source_path: Input,
    target_path: Input,
    readonly: bool,
    suffix: RadioState,
    source_error: Option<String>,
    target_error: Option<String>,
    form: FormState<BindControl>,
    on_submit: Box<dyn Fn(BindMount) -> AppMessage>,
}

impl BindMountBox {
    pub fn new(on_submit: impl Fn(BindMount) -> AppMessage + 'static) -> Self {
        Self {
            source_path: Input::from("/"),
            target_path: Input::default(),
            readonly: false,
            suffix: RadioState::default(),
            source_error: None,
            target_error: None,
            form: FormState::new(BindControl::Source),
            on_submit: Box::new(on_submit),
        }
    }
    pub fn with_mount(mut self, mount: &BindMount) -> Self {
        self.source_path = Input::from(mount.source.clone());
        self.target_path = Input::from(mount.target.clone());
        self.readonly = mount.readonly;
        self.suffix = RadioState::new(idmap_suffix_index(&mount.suffix).unwrap_or(0));
        self
    }
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, BindControl>) -> R) -> R {
        let options = idmap_suffix_options();
        let mut source = TextInput::new(
            &mut self.source_path,
            "Source Path",
            self.source_error.as_deref(),
        );
        let mut target = TextInput::new(
            &mut self.target_path,
            "Target Path (optional, defaults to source)",
            self.target_error.as_deref(),
        );
        let mut readonly = CheckboxControl::new(&mut self.readonly, "Read-only");
        let mut suffix = RadioControl::new(&mut self.suffix, "ID Mapping", &options);
        let mut submit = ButtonControl::new("OK");
        let mut cancel = ButtonControl::new("Cancel");
        visit(
            &mut Form::new(
                &mut self.form,
                [
                    FormItem::field(BindControl::Source, &mut source),
                    FormItem::field(BindControl::Target, &mut target),
                    FormItem::field(BindControl::Readonly, &mut readonly),
                    FormItem::field(BindControl::Suffix, &mut suffix),
                    FormItem::spacer(),
                    FormItem::row([
                        FormItem::field(BindControl::Submit, &mut submit).with_width(60),
                        FormItem::field(BindControl::Cancel, &mut cancel).with_width(60),
                    ]),
                ],
            )
            .with_actions(BindControl::Submit, BindControl::Cancel),
        )
    }
    fn try_submit(&mut self) -> Option<AppMessage> {
        let source = self.source_path.value().trim();
        let target = self.target_path.value().trim();
        self.source_error = validate_source(source).err();
        self.target_error = if !target.is_empty() && !std::path::Path::new(target).is_absolute() {
            Some("Must be absolute path".into())
        } else {
            None
        };
        if self.source_error.is_some() || self.target_error.is_some() {
            return None;
        }
        Some((self.on_submit)(BindMount {
            source: source.into(),
            target: if target.is_empty() { source } else { target }.into(),
            readonly: self.readonly,
            suffix: idmap_suffix_from_index(self.suffix.selected_idx())?,
        }))
    }
    fn finish_input(&mut self, outcome: FormOutcome<BindControl>) -> EventResult {
        match outcome {
            FormOutcome::Activated(BindControl::Submit) => self
                .try_submit()
                .map_or(EventResult::Consumed, EventResult::Message),
            FormOutcome::Activated(BindControl::Cancel) => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel))
            }
            FormOutcome::Ignored => EventResult::Ignored,
            _ => EventResult::Consumed,
        }
    }
}
fn validate_source(source: &str) -> Result<(), String> {
    let path = std::path::Path::new(source);
    if source.is_empty() {
        Err("Path required".into())
    } else if !path.is_absolute() {
        Err("Must be absolute path".into())
    } else if !path.exists() {
        Err("Path does not exist".into())
    } else {
        Ok(())
    }
}
impl Component for BindMountBox {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        self.form.register_footer_hints(footer);
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let dialog = centered_rect(45, 55, area);
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Add Bind Mount ")
            .border_style(Style::default().fg(theme::theme().dialog_border));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        self.with_form(|form| form.render(frame, inner));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let outcome = self.with_form(|form| form.handle_key(key));
        self.finish_input(outcome)
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        let outcome = self.with_form(|form| form.handle_mouse(mouse));
        self.finish_input(outcome)
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
        self.with_form(|_| ());
    }
    fn is_focused(&self) -> bool {
        self.form.is_focused()
    }
}

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
