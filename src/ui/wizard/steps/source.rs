use crate::ui::widgets::input::Input;
use crate::ui::widgets::list::ScrollableList;
use crate::ui::wizard::render_hint;
use crate::ui::wizard::{IStep, SourceKind, StepAction, WizardContext};
use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{ListItem, Paragraph},
    Frame,
};

pub struct SourceStep;

impl SourceStep {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl IStep for SourceStep {
    fn title(&self) -> String {
        "Source Selection".into()
    }

    fn next_step(&self, context: &WizardContext) -> Option<Box<dyn IStep>> {
        if context.source.kind == crate::ui::wizard::context::SourceKind::Copy {
            Some(Box::new(crate::ui::wizard::steps::copy_select::CopySelectStep::new()))
        } else {
            Some(Box::new(crate::ui::wizard::steps::basic::BasicStep::new()))
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, context: &WizardContext) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Min(0),    // list
                Constraint::Length(3), // input 1
                Constraint::Length(3), // input 2 (secondary)
                Constraint::Length(1), // hint
            ])
            .split(area);

        let mut items: Vec<ListItem> = Vec::new();
        let ops = [
            (
                "  [copy/clone]  ",
                "copy an existing container",
                Color::Green,
            ),
            (
                "  [OCI]         ",
                "import from registry (docker.io/…)",
                Color::Yellow,
            ),
            (
                "  [debootstrap] ",
                "bootstrap Debian/Ubuntu container",
                Color::Yellow,
            ),
            (
                "  [pacstrap]    ",
                "bootstrap Arch Linux container",
                Color::Yellow,
            ),
            (
                "  [disk]        ",
                "import local disk image (.raw, .tar)",
                Color::Yellow,
            ),
        ];

        for (tag, desc, color) in ops {
            items.push(ListItem::new(Line::from(vec![
                Span::styled(tag, Style::default().fg(color)),
                Span::styled(desc, Style::default().fg(Color::DarkGray)),
            ])));
        }

        ScrollableList::new(" Select base — [↑/↓] navigate, [Enter] select ", items)
            .selected(Some(context.source.cursor))
            .render(f, chunks[0]);

        let cur = context.source.cursor;
        match cur {
            1 => {
                let val = if context.source.field_idx == 0 {
                    format!("{}█", context.source.oci_url)
                } else {
                    context.source.oci_url.clone()
                };
                Input::new(
                    " OCI URL (e.g. alpine, docker://ubuntu, nvcr.io/...) ",
                    &val,
                )
                .focused(context.source.field_idx == 0)
                .render(f, chunks[1]);

                f.render_widget(
                    Paragraph::new(vec![
                        Line::from(vec![
                            Span::styled(" Note: ", Style::default().fg(Color::Yellow)),
                            Span::styled(
                                "Currently, OCI image will be unpacked as a rootfs directory, which means ",
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]),
                        Line::from(vec![Span::styled(
                            "       you will need to handle the issue of missing init programs yourself.",
                            Style::default().fg(Color::DarkGray),
                        )]),
                    ]),
                    chunks[2],
                );
            }
            2 => {
                let m_val = if context.source.field_idx == 0 {
                    format!("{}█", context.source.deboot_mirror)
                } else {
                    context.source.deboot_mirror.clone()
                };
                Input::new(" Mirror (leave blank for default) ", &m_val)
                    .focused(context.source.field_idx == 0)
                    .render(f, chunks[1]);

                let s_val = if context.source.field_idx == 1 {
                    format!("{}█", context.source.deboot_suite)
                } else {
                    context.source.deboot_suite.clone()
                };
                Input::new(" Suite (default: bookworm) ", &s_val)
                    .focused(context.source.field_idx == 1)
                    .render(f, chunks[2]);
            }
            3 => {
                let val = if context.source.field_idx == 0 {
                    format!("{}█", context.source.pacstrap_pkgs)
                } else {
                    context.source.pacstrap_pkgs.clone()
                };
                Input::new(" Packages (space separated) ", &val)
                    .focused(context.source.field_idx == 0)
                    .render(f, chunks[1]);
            }
            4 => {
                let val = if context.source.field_idx == 0 {
                    format!("{}█", context.source.disk_path)
                } else {
                    context.source.disk_path.clone()
                };
                Input::new(" Local file path (.raw, .tar) ", &val)
                    .focused(context.source.field_idx == 0)
                    .render(f, chunks[1]);
            }
            _ => {
                f.render_widget(Paragraph::new(""), chunks[1]);
                f.render_widget(Paragraph::new(""), chunks[2]);
            }
        }

        render_hint(
            f,
            chunks[3],
            &[
                "[↑/↓] nav",
                "[Tab] switch field",
                "[Enter] select",
                "[Esc] cancel",
            ][..],
        );
    }

    async fn handle_key(&mut self, key: KeyEvent, context: &mut WizardContext) -> StepAction {
        match key.code {
            KeyCode::Esc => StepAction::Close,
            KeyCode::Up => {
                if context.source.cursor > 0 {
                    context.source.cursor -= 1;
                }
                context.source.field_idx = 0;
                StepAction::None
            }
            KeyCode::Down => {
                context.source.cursor = (context.source.cursor + 1).min(4);
                context.source.field_idx = 0;
                StepAction::None
            }
            KeyCode::Enter => {
                let cur = context.source.cursor;
                if cur == 0 {
                    context.source.kind = SourceKind::Copy;
                    StepAction::Next
                } else {
                    context.source.kind = match cur {
                        1 => SourceKind::Oci,
                        2 => SourceKind::Debootstrap,
                        3 => SourceKind::Pacstrap,
                        _ => SourceKind::DiskImage,
                    };
                    context.source.field_idx = 0;
                    StepAction::Next
                }
            }
            KeyCode::Tab => {
                if context.source.cursor == 2 {
                    context.source.field_idx = 1 - context.source.field_idx;
                }
                StepAction::None
            }
            KeyCode::Backspace => {
                match context.source.cursor {
                    1 => {
                        context.source.oci_url.pop();
                    }
                    2 => {
                        if context.source.field_idx == 0 {
                            context.source.deboot_mirror.pop();
                        } else {
                            context.source.deboot_suite.pop();
                        }
                    }
                    3 => {
                        context.source.pacstrap_pkgs.pop();
                    }
                    4 => {
                        context.source.disk_path.pop();
                    }
                    _ => {}
                }
                StepAction::None
            }
            KeyCode::Char(c) => {
                match context.source.cursor {
                    1 => {
                        context.source.oci_url.push(c);
                    }
                    2 => {
                        if context.source.field_idx == 0 {
                            context.source.deboot_mirror.push(c);
                        } else {
                            context.source.deboot_suite.push(c);
                        }
                    }
                    3 => {
                        context.source.pacstrap_pkgs.push(c);
                    }
                    4 => {
                        context.source.disk_path.push(c);
                    }
                    _ => {}
                }
                StepAction::None
            }
            _ => StepAction::None,
        }
    }
}
