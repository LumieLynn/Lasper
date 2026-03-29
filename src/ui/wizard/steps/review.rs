use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};
use crossterm::event::{KeyCode, KeyEvent};
use crate::ui::wizard::{IStep, StepAction, WizardContext};
use crate::ui::wizard::render_hint;
use crate::ui::widgets::input::Input;
use crate::nspawn::deploy::run_deploy_task;
use async_trait::async_trait;

pub struct ReviewStep;

impl ReviewStep {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl IStep for ReviewStep {
    fn title(&self) -> String { "Review Configuration".into() }

    fn next_step(&self, _context: &WizardContext) -> Option<Box<dyn IStep>> {
        Some(Box::new(crate::ui::wizard::steps::deploy::DeployStep::new()))
    }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        Input::new(" Preview .nspawn configuration ", &context.review.preview)
            .focused(true)
            .scroll(context.review.preview_scroll as u16)
            .render(f, chunks[0]);

        render_hint(f, chunks[1], &["[↑/↓] scroll", "[Enter] deploy!", "[Esc] back"][..]);
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => StepAction::Prev,
            KeyCode::Up => {
                if context.review.preview_scroll > 0 { context.review.preview_scroll -= 1; }
                StepAction::None
            }
            KeyCode::Down => {
                context.review.preview_scroll += 1;
                StepAction::None
            }
            KeyCode::Enter => {
                let cp = context.build_config();
                let (deployer, storage) = context.get_deployer_and_storage();
                tokio::spawn(run_deploy_task(
                    deployer,
                    storage,
                    context.basic.name.clone(),
                    cp.cfg,
                    context.deploy.logs.clone(),
                    context.deploy.done.clone(),
                    context.deploy.success.clone(),
                ));
                StepAction::Next
            }
            _ => StepAction::None,
        }
    }
}
