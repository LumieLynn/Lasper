use crate::domain::nvidia::{NvidiaFileCategory, NvidiaPassthroughMode};
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

#[derive(Debug, Clone, PartialEq)]
pub struct NvidiaConfigResult {
    pub gpu_device: String,
    pub mode: NvidiaPassthroughMode,
    pub category_destinations: Vec<(NvidiaFileCategory, String)>,
    pub inject_env: bool,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum NvidiaControl {
    Device,
    Mode,
    Destination(usize),
    Environment,
    Submit,
    Cancel,
}
struct Destination {
    category: NvidiaFileCategory,
    input: Input,
    error: Option<String>,
}

pub struct NvidiaConfigDialog {
    device_options: Vec<String>,
    gpu_device: RadioState,
    mode: RadioState,
    destinations: Vec<Destination>,
    inject_env: bool,
    form: FormState<NvidiaControl>,
    on_submit: Box<dyn Fn(NvidiaConfigResult) -> AppMessage>,
}

fn destinations(
    active: Vec<NvidiaFileCategory>,
    saved: &[(NvidiaFileCategory, String)],
) -> Vec<Destination> {
    let mut categories = NvidiaFileCategory::all_static();
    for category in active {
        if !categories.contains(&category) {
            categories.push(category);
        }
    }
    categories.sort_by_key(|category| format!("{category:?}"));
    categories
        .into_iter()
        .map(|category| {
            let default = match category {
                NvidiaFileCategory::Lib64 => "/usr/lib",
                NvidiaFileCategory::Lib32 => "/usr/lib32",
                NvidiaFileCategory::Bin => "/usr/bin",
                NvidiaFileCategory::Firmware => "/lib/firmware/nvidia",
                NvidiaFileCategory::Config => "/usr/share",
                NvidiaFileCategory::Xorg => "/usr/lib/xorg/modules",
                NvidiaFileCategory::Vdpau => "/usr/lib/vdpau",
                NvidiaFileCategory::Gbm => "/usr/lib/gbm",
                NvidiaFileCategory::Other => "",
            };
            let value = saved
                .iter()
                .find(|(item, _)| item == &category)
                .map(|(_, value)| value.as_str())
                .unwrap_or(default);
            Destination {
                category,
                input: Input::from(value),
                error: None,
            }
        })
        .collect()
}

impl NvidiaConfigDialog {
    pub fn new(
        gpu_devices: Vec<String>,
        active_categories: Vec<NvidiaFileCategory>,
        on_submit: impl Fn(NvidiaConfigResult) -> AppMessage + 'static,
    ) -> Self {
        Self {
            device_options: if gpu_devices.is_empty() {
                vec!["all".into()]
            } else {
                gpu_devices
            },
            gpu_device: RadioState::default(),
            mode: RadioState::default(),
            destinations: destinations(active_categories, &[]),
            inject_env: false,
            form: FormState::new(NvidiaControl::Device),
            on_submit: Box::new(on_submit),
        }
    }
    pub fn with_profile(
        mut self,
        device: &str,
        mode: &NvidiaPassthroughMode,
        saved: &[(NvidiaFileCategory, String)],
        inject_env: bool,
        active: Vec<NvidiaFileCategory>,
    ) -> Self {
        self.gpu_device = RadioState::new(
            self.device_options
                .iter()
                .position(|item| item == device)
                .unwrap_or(0),
        );
        self.mode = RadioState::new(usize::from(*mode == NvidiaPassthroughMode::Categorized));
        self.destinations = destinations(active, saved);
        self.inject_env = inject_env;
        self
    }
    fn with_form<R>(&mut self, visit: impl FnOnce(&mut Form<'_, NvidiaControl>) -> R) -> R {
        let categorized = self.mode.selected_idx() == 1;
        let mut device =
            RadioControl::new(&mut self.gpu_device, "GPU Device", &self.device_options);
        let mut mode = RadioControl::new(
            &mut self.mode,
            "Passthrough Mode",
            &["Mirror Host", "Categorized"],
        );
        let mut destinations: Vec<_> = self
            .destinations
            .iter_mut()
            .map(|destination| {
                TextInput::new(
                    &mut destination.input,
                    destination.category.label(),
                    destination.error.as_deref(),
                )
            })
            .collect();
        let mut env = CheckboxControl::new(
            &mut self.inject_env,
            "Inject environment (/etc/environment)",
        );
        let mut submit = ButtonControl::new("OK");
        let mut cancel = ButtonControl::new("Cancel");
        let mut items = vec![
            FormItem::field(NvidiaControl::Device, &mut device),
            FormItem::field(NvidiaControl::Mode, &mut mode),
        ];
        if categorized {
            items.extend(
                destinations.iter_mut().enumerate().map(|(index, input)| {
                    FormItem::field(NvidiaControl::Destination(index), input)
                }),
            );
        }
        items.extend([
            FormItem::spacer(),
            FormItem::field(NvidiaControl::Environment, &mut env),
            FormItem::row([
                FormItem::field(NvidiaControl::Submit, &mut submit).with_width(60),
                FormItem::field(NvidiaControl::Cancel, &mut cancel).with_width(60),
            ]),
        ]);
        visit(
            &mut Form::new(&mut self.form, items)
                .with_actions(NvidiaControl::Submit, NvidiaControl::Cancel),
        )
    }
    fn try_submit(&mut self) -> Option<AppMessage> {
        let mode = if self.mode.selected_idx() == 0 {
            NvidiaPassthroughMode::Mirror
        } else {
            NvidiaPassthroughMode::Categorized
        };
        if mode == NvidiaPassthroughMode::Categorized {
            for destination in &mut self.destinations {
                let path = destination.input.value().trim();
                destination.error = (path.is_empty() || !std::path::Path::new(path).is_absolute())
                    .then(|| "Absolute destination path required".into());
            }
            if self
                .destinations
                .iter()
                .any(|destination| destination.error.is_some())
            {
                return None;
            }
        }
        Some((self.on_submit)(NvidiaConfigResult {
            gpu_device: self
                .device_options
                .get(self.gpu_device.selected_idx())
                .cloned()
                .unwrap_or_else(|| "all".into()),
            mode,
            category_destinations: self
                .destinations
                .iter()
                .map(|destination| {
                    (
                        destination.category.clone(),
                        destination.input.value().trim().into(),
                    )
                })
                .collect(),
            inject_env: self.inject_env,
        }))
    }
    fn finish_input(&mut self, outcome: FormOutcome<NvidiaControl>) -> EventResult {
        match outcome {
            FormOutcome::Activated(NvidiaControl::Submit) => self
                .try_submit()
                .map_or(EventResult::Consumed, EventResult::Message),
            FormOutcome::Activated(NvidiaControl::Cancel) => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::DialogCancel))
            }
            FormOutcome::Ignored => EventResult::Ignored,
            _ => EventResult::Consumed,
        }
    }
}
impl Component for NvidiaConfigDialog {
    fn register_footer_hints(&self, footer: &mut FooterRegistry) {
        self.form.register_footer_hints(footer);
    }
    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let dialog = centered_rect(50, 70, area);
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" NVIDIA Passthrough ")
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
