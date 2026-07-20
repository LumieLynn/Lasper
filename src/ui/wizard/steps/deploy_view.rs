use crate::nspawn::ops::provision::{DeployLogEvent, DeployProgress};
use crate::ui::core::{AppMessage, Component, EventResult, WizardMessage};
use crate::ui::widgets::display::text_block::TextBlock;
use crate::ui::widgets::lists::selectable_list::SelectableList;
use crate::ui::wizard::context::WizardContext;
use crate::ui::wizard::steps::StepComponent;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::{Block, BorderType, Borders, Gauge},
    Frame,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tokio::sync::broadcast;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub struct DeployStepView {
    log_rx: Option<broadcast::Receiver<DeployLogEvent>>,
    done: Arc<AtomicBool>,
    success: Arc<AtomicBool>,
    status_block: TextBlock,
    log_list: SelectableList<String>,
    internal_logs: Vec<String>,
    progress: Option<DeployProgress>,
}

impl DeployStepView {
    pub fn new(
        log_rx: broadcast::Receiver<DeployLogEvent>,
        done: Arc<AtomicBool>,
        success: Arc<AtomicBool>,
    ) -> Self {
        Self {
            log_rx: Some(log_rx),
            done,
            success,
            status_block: TextBlock::new(" Status ", "Deploying...".to_string()),
            log_list: SelectableList::new(" Deployment logs ", vec![], |s| s.clone()),
            internal_logs: vec![],
            progress: None,
        }
    }

    fn update_logs(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &mut self.log_rx {
            loop {
                match rx.try_recv() {
                    Ok(DeployLogEvent::Line(log)) => {
                        self.progress = None;
                        self.internal_logs.push(log);
                        changed = true;
                    }
                    Ok(DeployLogEvent::Progress(progress)) => {
                        self.progress = Some(progress);
                        changed = true;
                    }
                    Err(broadcast::error::TryRecvError::Lagged(n)) => {
                        self.internal_logs
                            .push(format!("[{} logs skipped due to lag]", n));
                        changed = true;
                    }
                    Err(_) => break, // Empty or Closed
                }
            }
        }
        changed
    }
}

fn truncate_to_width(input: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(input) <= max_width {
        return input.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let content_width = max_width - 3;
    let mut width = 0;
    let mut output = String::new();
    for character in input.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > content_width {
            break;
        }
        output.push(character);
        width += character_width;
    }
    output.push_str("...");
    output
}

impl Component for DeployStepView {
    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let done = self.done.load(Ordering::SeqCst);
        let success = self.success.load(Ordering::SeqCst);

        let status = if !done {
            "Deploying... please wait.".to_string()
        } else if success {
            "SUCCESS: Container deployed and started!".to_string()
        } else {
            "FAILED: Deployment encountered an error.".to_string()
        };
        self.status_block.set_content(status);

        let logs_changed = self.update_logs();
        let log_width = chunks[1].width.saturating_sub(5) as usize;
        self.log_list.set_items(
            self.internal_logs
                .iter()
                .map(|line| truncate_to_width(line, log_width))
                .collect(),
        );
        if logs_changed {
            self.log_list.select_last();
        }

        if let Some(progress) = self.progress.as_ref().filter(|_| !done) {
            let ratio = f64::from(progress.permille) / 1000.0;
            let label = format!("{}.{:01}%", progress.permille / 10, progress.permille % 10);
            let theme = crate::ui::theme::theme();
            let gauge = Gauge::default()
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(format!(" {} ", progress.label)),
                )
                .gauge_style(Style::default().fg(theme.accent))
                .ratio(ratio)
                .label(label);
            f.render_widget(gauge, chunks[0]);
        } else {
            self.status_block.render(f, chunks[0]);
        }
        self.log_list.render(f, chunks[1]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventResult {
        let done = self.done.load(Ordering::SeqCst);
        if !done {
            // Block all close attempts while deploying; allow log scrolling only
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                    return EventResult::Consumed; // refuse silently
                }
                _ => return self.log_list.handle_key(key),
            }
        }
        // Deployment finished — allow closing
        match key.code {
            KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc => {
                EventResult::Message(AppMessage::Wizard(WizardMessage::Close))
            }

            _ => self.log_list.handle_key(key),
        }
    }

    fn validate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl StepComponent for DeployStepView {
    fn commit_to_context(&self, _ctx: &mut WizardContext) {
        // Deploy is terminal state
    }

    fn render_step(&mut self, f: &mut Frame, area: Rect, _context: &WizardContext) {
        self.render(f, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_log_lines_are_clipped_by_display_width() {
        assert_eq!(truncate_to_width("abcdefghij", 8), "abcde...");
        assert_eq!(
            truncate_to_width("\u{5bb9}\u{5668}\u{4e0b}\u{8f7d}\u{5b8c}\u{6210}", 9),
            "\u{5bb9}\u{5668}\u{4e0b}..."
        );
        assert_eq!(truncate_to_width("abc", 2), "..");
    }
}
