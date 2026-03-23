use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use crate::ui::wizard::{IStep, StepAction};
use crate::ui::wizard::context::{WizardContext, SourceKind};
use crate::ui::wizard::steps;
use crate::nspawn::ContainerEntry;

/// The state for the multi-step container creation wizard.
pub struct Wizard {
    pub context: WizardContext,
    pub steps: Vec<Box<dyn IStep>>,
    pub current_step_idx: usize,
}

impl Wizard {
    pub fn new(is_root: bool) -> Self {
        let context = WizardContext::new(is_root);
        let steps: Vec<Box<dyn IStep>> = vec![
            Box::new(steps::source::SourceStep::new()),
        ];
        Self {
            context,
            steps,
            current_step_idx: 0,
        }
    }

    pub fn current_step_title(&self) -> String {
        self.steps.get(self.current_step_idx).map(|s| s.title()).unwrap_or_default()
    }

    pub fn current_step(&self) -> f32 {
        if self.context.source.kind == SourceKind::Copy {
            match self.current_step_idx {
                0 => 1.0,
                1 => 2.0,
                2 => 3.0,
                3 => 4.0,
                4 => 5.0,
                _ => (self.current_step_idx + 1) as f32
            }
        } else {
            (self.current_step_idx + 1) as f32
        }
    }

    pub fn total_steps(&self) -> usize {
        if self.context.source.kind == SourceKind::Copy { 5 } else { 9 }
    }

    pub async fn handle_key(&mut self, key: KeyEvent, entries: &[ContainerEntry], _is_root: bool) -> StepAction {
        self.context.entries = entries.to_vec();
        if let Some(step) = self.steps.get_mut(self.current_step_idx) {
            let action = step.handle_key(key, &mut self.context).await;
            match action {
                StepAction::Next => {
                    self.next_step();
                    StepAction::None
                }
                StepAction::Prev => {
                    self.prev_step();
                    StepAction::None
                }
                _ => action,
            }
        } else {
            StepAction::None
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        use ratatui::widgets::{Block, Borders, Clear};
        let area = centered_rect(65, 75, area);
        f.render_widget(Clear, area);

        let title = self.current_step_title();
        let step = self.current_step();
        let total = self.total_steps();
        let header = format!(" {} (Step {:.1} / {}) ", title, step, total);

        let block = Block::default()
            .title(header)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        
        let inner = block.inner(area);
        f.render_widget(block, area);

        if let Some(step_impl) = self.steps.get_mut(self.current_step_idx) {
            step_impl.render(f, inner, &self.context);
        }
    }

    fn next_step(&mut self) {
        let next: Option<Box<dyn IStep>> = match self.current_step_idx {
            0 => { // From Source
                if self.context.source.kind == SourceKind::Copy {
                    Some(Box::new(steps::copy_select::CopySelectStep::new()))
                } else {
                    Some(Box::new(steps::basic::BasicStep::new()))
                }
            }
            1 => { // From CopySelect or Basic
                 if self.context.source.kind == SourceKind::Copy {
                     Some(Box::new(steps::basic::BasicStep::new()))
                 } else {
                     Some(Box::new(steps::storage::StorageStep::new()))
                 }
            }
            2 => { // From Basic (Clone) or Storage
                 if self.context.source.kind == SourceKind::Copy {
                     Some(Box::new(steps::review::ReviewStep::new()))
                 } else {
                     Some(Box::new(steps::user::UserStep::new()))
                 }
            }
            3 => { // From Review (Clone) or User
                 if self.context.source.kind == SourceKind::Copy {
                     Some(Box::new(steps::deploy::DeployStep::new()))
                 } else {
                     Some(Box::new(steps::network::NetworkStep::new()))
                 }
            }
            4 => Some(Box::new(steps::passthrough::PassthroughStep::new())),
            5 => Some(Box::new(steps::devices::DevicesStep::new())),
            6 => Some(Box::new(steps::review::ReviewStep::new())),
            7 => Some(Box::new(steps::deploy::DeployStep::new())),
            _ => None,
        };

        if let Some(s) = next {
            self.steps.push(s);
            self.current_step_idx += 1;
        }
    }

    fn prev_step(&mut self) {
        if self.current_step_idx > 0 {
            self.steps.pop();
            self.current_step_idx -= 1;
        }
    }
}

pub fn centered_rect(w_pct: u16, h_pct: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - h_pct) / 2),
            Constraint::Percentage(h_pct),
            Constraint::Percentage((100 - h_pct) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - w_pct) / 2),
            Constraint::Percentage(w_pct),
            Constraint::Percentage((100 - w_pct) / 2),
        ])
        .split(vert[1])[1]
}

pub fn render_hint(f: &mut Frame, area: Rect, hints: &[&str]) {
    let mut spans = vec![Span::raw("  ")];
    for hint in hints {
        if let Some(sp) = hint.find(' ') {
            let (k, d) = hint.split_at(sp);
            spans.push(Span::styled(k.to_string(), Style::default().fg(Color::Cyan)));
            spans.push(Span::styled(format!("{}  ", d), Style::default().fg(Color::DarkGray)));
        } else {
            spans.push(Span::styled(hint.to_string(), Style::default().fg(Color::Cyan)));
            spans.push(Span::raw("  "));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
