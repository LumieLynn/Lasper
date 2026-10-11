use crate::domain::nvidia::NvidiaPassthroughMode;
use crate::domain::provisioning::{BindMount, IdmapSuffix};
use crate::tui::features::wizard::dialogs::bind_mount::idmap_suffix_label;
use crate::tui::features::wizard::draft::{PassthroughConfig, UnclassifiedFile, WizardDraft};
use crate::tui::features::wizard::steps::{field_event, register_step_form, StepComponent};
use crate::tui::features::wizard::StepAction;
use crate::tui::foundation::components::form::{Form, FormItem, FormOutcome, FormState};
use crate::tui::foundation::footer::{FooterHint, FooterRegistry};
use crate::tui::foundation::widgets::display::content::ContentControl;
use crate::tui::foundation::widgets::lists::list::{ListControl, ListControlState, ListEntry};
use crate::tui::foundation::widgets::selectors::checkbox::CheckboxControl;
use crate::tui::foundation::{text::soft_wrap_text, theme};
use crate::tui::protocol::{AppMessage, Component, EventResult, WizardMessage};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Margin, Rect},
    style::Style,
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame,
};

const NVIDIA_TOGGLE_LABEL: &str = " NVIDIA GPU Passthrough";
const UNAVAILABLE: &str = "The configured NVIDIA CDI source is unavailable on this host.";
#[derive(Clone, Copy, PartialEq, Eq)]
enum BindingsControl {
    Nvidia,
    Unavailable,
    Unclassified,
    Binds,
}
pub struct BindMountsStepView {
    binds: Vec<BindMount>,
    bind_selection: ListControlState,
    unclassified: Vec<UnclassifiedFile>,
    unclassified_selection: ListControlState,
    nvidia_toggle: bool,
    nvidia_enabled: bool,
    nvidia_mode: NvidiaPassthroughMode,
    nvidia_cdi_available: bool,
    form: FormState<BindingsControl>,
}
impl BindMountsStepView {
    pub fn new(
        initial: &PassthroughConfig,
        unclassified: &[UnclassifiedFile],
        nvidia_cdi_available: bool,
    ) -> Self {
        let nvidia_enabled = initial.nvidia_gpu && nvidia_cdi_available;
        let nvidia_mode = initial
            .nvidia_profile
            .as_ref()
            .map(|profile| profile.mode.clone())
            .unwrap_or(NvidiaPassthroughMode::Mirror);
        let focus = if nvidia_enabled
            && nvidia_mode == NvidiaPassthroughMode::Categorized
            && !unclassified.is_empty()
        {
            BindingsControl::Unclassified
        } else {
            BindingsControl::Nvidia
        };
        let mut view = Self {
            binds: initial.bind_mounts.clone(),
            bind_selection: ListControlState::new(0),
            unclassified: unclassified.to_vec(),
            unclassified_selection: ListControlState::new(0),
            nvidia_toggle: nvidia_enabled,
            nvidia_enabled,
            nvidia_mode,
            nvidia_cdi_available,
            form: FormState::new(focus),
        };
        view.form.set_focus(true);
        view.with_form(80, |_| ());
        view
    }
    fn has_unclassified(&self) -> bool {
        self.nvidia_enabled
            && self.nvidia_mode == NvidiaPassthroughMode::Categorized
            && !self.unclassified.is_empty()
    }
    fn with_form<R>(
        &mut self,
        width: u16,
        visit: impl FnOnce(&mut Form<'_, BindingsControl>) -> R,
    ) -> R {
        let has_unclassified = self.has_unclassified();
        let bind_entries: Vec<_> = self
            .binds
            .iter()
            .map(|bind| {
                let suffix = if bind.suffix == IdmapSuffix::None {
                    String::new()
                } else {
                    format!(" [{}]", idmap_suffix_label(&bind.suffix))
                };
                ListEntry::new(format!(
                    "  {}:{} ({}){}",
                    bind.source,
                    bind.target,
                    if bind.readonly { "ro" } else { "rw" },
                    suffix
                ))
            })
            .collect();
        let file_entries: Vec<_> = self
            .unclassified
            .iter()
            .map(|file| {
                let category = file
                    .assigned_category
                    .as_ref()
                    .map(|category| category.label())
                    .unwrap_or("Unclassified");
                let destination = if file.custom_destination.is_empty() {
                    &file.default_container_path
                } else {
                    &file.custom_destination
                };
                ListEntry::new(format!(
                    "  {} -> {} ({}) [{}]",
                    file.host_path,
                    destination,
                    category,
                    if file.readonly { "ro" } else { "rw" }
                ))
            })
            .collect();
        let mut binds = ListControl::new(
            &mut self.bind_selection,
            " Configured Bind Mounts ",
            &bind_entries,
        );
        let mut files = ListControl::new(
            &mut self.unclassified_selection,
            " Unclassified CDI Files (E to reclassify) ",
            &file_entries,
        );
        let mut nvidia = CheckboxControl::new(&mut self.nvidia_toggle, NVIDIA_TOGGLE_LABEL);
        let mut unavailable = ContentControl::new(
            Paragraph::new(UNAVAILABLE)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(" NVIDIA CDI SOURCE REQUIRED ")
                        .border_style(Style::default().fg(theme::theme().border_unfocused)),
                ),
        );
        let height = soft_wrap_text(UNAVAILABLE, usize::from(width.saturating_sub(2).max(1)))
            .len()
            .saturating_add(2)
            .min(usize::from(u16::MAX)) as u16;
        let mut items = vec![FormItem::field(BindingsControl::Nvidia, &mut nvidia)
            .with_enabled(self.nvidia_cdi_available)];
        if !self.nvidia_cdi_available {
            items.push(
                FormItem::field(BindingsControl::Unavailable, &mut unavailable).height(height),
            );
        }
        if has_unclassified {
            items.push(FormItem::field(BindingsControl::Unclassified, &mut files).fill(5));
        }
        items.push(FormItem::field(BindingsControl::Binds, &mut binds).fill(4));
        visit(&mut Form::new(&mut self.form, items))
    }
    fn finish_input(&mut self, outcome: FormOutcome<BindingsControl>) -> EventResult {
        if outcome == FormOutcome::Changed(BindingsControl::Nvidia) {
            if self.nvidia_toggle {
                return EventResult::Message(AppMessage::Wizard(
                    WizardMessage::OpenNvidiaConfigDialog,
                ));
            }
            self.nvidia_enabled = false;
        }
        field_event(outcome)
    }
}
impl Component for BindMountsStepView {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        register_step_form(footer, &self.form);
        if self.form.is_selected(BindingsControl::Binds) {
            footer.register(FooterHint::new("a", "add bind").on_key(KeyCode::Char('a')));
            if self.bind_selection.selected().is_some() {
                footer.register(FooterHint::new("e", "edit bind").on_key(KeyCode::Char('e')));
                footer.register(FooterHint::new("d", "delete").on_key(KeyCode::Char('d')));
            }
        } else if self.form.is_selected(BindingsControl::Unclassified)
            && self.unclassified_selection.selected().is_some()
        {
            footer.register(FooterHint::new("e", "reclassify file").on_key(KeyCode::Char('e')));
        }
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let area = area.inner(Margin::new(1, 1));
        self.with_form(area.width, |form| form.render(frame, area));
    }
    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        if self.form.is_selected(BindingsControl::Binds) {
            match key.code {
                KeyCode::Char('a' | 'A') => {
                    return EventResult::Message(AppMessage::Wizard(WizardMessage::OpenBindDialog))
                }
                KeyCode::Char('e' | 'E') => {
                    if let Some(index) = self.bind_selection.selected() {
                        if let Some(bind) = self.binds.get(index) {
                            return EventResult::Message(AppMessage::Wizard(
                                WizardMessage::OpenBindEditDialog(index, bind.clone()),
                            ));
                        }
                    }
                }
                KeyCode::Delete | KeyCode::Char('d' | 'D') => {
                    if let Some(index) = self
                        .bind_selection
                        .selected()
                        .filter(|index| *index < self.binds.len())
                    {
                        self.binds.remove(index);
                        self.bind_selection.removed(index, self.binds.len());
                        return EventResult::Message(AppMessage::Wizard(
                            WizardMessage::BindMountRemoved(index),
                        ));
                    }
                }
                _ => {}
            }
        } else if self.form.is_selected(BindingsControl::Unclassified)
            && matches!(key.code, KeyCode::Char('e' | 'E'))
        {
            if let Some(index) = self.unclassified_selection.selected() {
                if let Some(file) = self.unclassified.get(index) {
                    return EventResult::Message(AppMessage::Wizard(
                        WizardMessage::OpenUnclassifiedEditDialog(index, file.clone()),
                    ));
                }
            }
        }
        let outcome = self.with_form(self.form.viewport_width(), |form| form.handle_key(key));
        self.finish_input(outcome)
    }
    fn handle_mouse(&mut self, mouse: MouseEvent) -> EventResult {
        let outcome = self.with_form(self.form.viewport_width(), |form| form.handle_mouse(mouse));
        self.finish_input(outcome)
    }
    fn set_focus(&mut self, focused: bool) {
        self.form.set_focus(focused);
    }
}
impl StepComponent for BindMountsStepView {
    fn handle_message(&mut self, message: &AppMessage) -> StepAction {
        match message {
            AppMessage::Wizard(WizardMessage::BindMountAdded(bind)) => {
                self.binds.push(bind.clone())
            }
            AppMessage::Wizard(WizardMessage::BindMountUpdated(index, bind)) => {
                if let Some(current) = self.binds.get_mut(*index) {
                    *current = bind.clone();
                }
            }
            AppMessage::Wizard(WizardMessage::NvidiaConfigSaved(result)) => {
                self.nvidia_enabled = true;
                self.nvidia_toggle = true;
                self.nvidia_mode = result.mode.clone();
                if self.has_unclassified() {
                    self.form.select(BindingsControl::Unclassified);
                }
            }
            AppMessage::Wizard(WizardMessage::UnclassifiedFileUpdated(index, file)) => {
                if let Some(current) = self.unclassified.get_mut(*index) {
                    *current = file.clone();
                }
            }
            AppMessage::Wizard(WizardMessage::DialogCancel) => {
                if !self.nvidia_enabled {
                    self.nvidia_toggle = false;
                }
            }
            _ => return StepAction::None,
        }
        self.with_form(self.form.viewport_width(), |_| ());
        StepAction::CloseDialog
    }
    fn commit_to_draft(&self, context: &mut WizardDraft) {
        context.passthrough.bind_mounts = self.binds.clone();
        context.passthrough.unclassified_files = self.unclassified.clone();
        context.passthrough.nvidia_gpu = self.nvidia_enabled;
    }
    fn render_step(&mut self, frame: &mut Frame, area: Rect, _: &WizardDraft) {
        self.render(frame, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn passthrough_config(nvidia_gpu: bool) -> PassthroughConfig {
        PassthroughConfig {
            bind_mounts: Vec::new(),
            device_binds: Vec::new(),
            privileged: false,
            private_users: None,
            graphics_acceleration: false,
            gpu_passthrough_all: false,
            nvidia_gpu,
            nvidia_profile: None,
            x11_projection: crate::tui::features::wizard::draft::X11ProjectionSelection::None,
        }
    }

    #[test]
    fn missing_nvidia_cdi_source_disables_and_explains_passthrough() {
        crate::tui::foundation::theme::init_theme(crate::tui::foundation::theme::Theme::dark());
        let mut view = BindMountsStepView::new(&passthrough_config(true), &[], false);
        assert!(!view.nvidia_enabled);
        assert!(!view.nvidia_cdi_available);

        let mut terminal = Terminal::new(TestBackend::new(50, 18)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("NVIDIA GPU Passthrough"));
        assert!(rendered.contains("NVIDIA CDI SOURCE"));
        assert!(!rendered.contains("toggle NVIDIA"));
    }
}
