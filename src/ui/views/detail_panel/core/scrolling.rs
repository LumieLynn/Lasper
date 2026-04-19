use ratatui::{
    layout::Rect,
    widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use crate::app::{AppData, DetailPane};
use super::super::DetailPanel;

pub fn sync_data_lengths(panel: &mut DetailPanel, data: &AppData, width: usize) {
    let old_logs_len = panel.logs_len;

    let val_width = if width > 6 {
        (width as f32 * 0.65) as usize
    } else {
        10
    };
    let val_width = val_width.saturating_sub(3).max(10);

    panel.details_len = data
        .properties
        .as_ref()
        .map(|p| {
            p.groups
                .iter()
                .filter(|g| !g.properties.is_empty())
                .map(|g| {
                    let mut group_lines = 2; // Header + Spacer
                    for (_, v) in &g.properties {
                        group_lines += v
                            .lines()
                            .map(|l| {
                                let count = l.chars().count();
                                if count == 0 {
                                    1
                                } else {
                                    (count + val_width - 1) / val_width
                                }
                            })
                            .sum::<usize>()
                            .max(1);
                    }
                    group_lines
                })
                .sum()
        })
        .unwrap_or(0);

    panel.properties_len = data
        .properties
        .as_ref()
        .map(|p| {
            p.get_summary()
                .iter()
                .map(|(_, v)| {
                    v.lines()
                        .map(|l| {
                            let count = l.chars().count();
                            if count == 0 {
                                1
                            } else {
                                (count + val_width - 1) / val_width
                            }
                        })
                        .sum::<usize>()
                        .max(1)
                })
                .sum()
        })
        .unwrap_or(0);

    panel.logs_len = data
        .log_lines
        .iter()
        .map(|l| {
            // Very simple tab expansion for width calculation
            let mut visual_width = 0;
            for c in l.chars() {
                if c == '\t' {
                    visual_width += 8 - (visual_width % 8);
                } else {
                    visual_width += 1;
                }
            }
            if visual_width == 0 {
                1
            } else {
                (visual_width + width - 1) / width
            }
        })
        .sum();

    panel.config_len = data
        .config_content
        .as_ref()
        .map(|c| {
            c.lines()
                .map(|l| {
                    let count = l.chars().count();
                    if count == 0 {
                        1
                    } else {
                        (count + width - 1) / width
                    }
                })
                .sum()
        })
        .unwrap_or(0);

    // Sticky autoscroll for Logs
    // We scroll to bottom if:
    // 1. New logs arrived while we were at the old bottom
    // 2. The viewport shrunk while we were at the old bottom
    if panel.active_pane == DetailPane::Logs {
        let max_scroll_old = old_logs_len.saturating_sub(panel.old_pane_height as usize) as u16;
        let at_bottom = panel.log_scroll >= max_scroll_old;

        if at_bottom && (panel.logs_len > old_logs_len || panel.pane_height < panel.old_pane_height) {
            let max_scroll_new = panel.logs_len.saturating_sub(panel.pane_height as usize);
            panel.log_scroll = max_scroll_new.min(u16::MAX as usize) as u16;
        }
    }

    // Always clamp scroll to the current max so a width change never
    // leaves us past the real bottom of the content.
    let log_max = panel.logs_len.saturating_sub(panel.pane_height as usize);
    panel.log_scroll = panel.log_scroll.min(log_max.min(u16::MAX as usize) as u16);

    let cfg_max = panel.config_len.saturating_sub(panel.pane_height as usize);
    panel.config_scroll = panel.config_scroll.min(cfg_max.min(u16::MAX as usize) as u16);

    let det_max = panel.details_len.saturating_sub(panel.pane_height as usize);
    panel.details_scroll = panel.details_scroll.min(det_max.min(u16::MAX as usize) as u16);

    let prop_max = panel.properties_len.saturating_sub(panel.pane_height as usize);
    panel.properties_scroll =
        panel.properties_scroll.min(prop_max.min(u16::MAX as usize) as u16);
}

pub fn render_scrollbar(panel: &DetailPanel, f: &mut Frame, area: Rect) {
    let (max_scroll, position) = match panel.active_pane {
        DetailPane::Logs if panel.logs_len > panel.pane_height as usize => (
            panel.logs_len.saturating_sub(panel.pane_height as usize),
            panel.log_scroll as usize,
        ),
        DetailPane::Config if panel.config_len > panel.pane_height as usize => (
            panel.config_len.saturating_sub(panel.pane_height as usize),
            panel.config_scroll as usize,
        ),
        DetailPane::Details if panel.details_len > panel.pane_height as usize => (
            panel.details_len.saturating_sub(panel.pane_height as usize),
            panel.details_scroll as usize,
        ),
        DetailPane::Properties if panel.properties_len > panel.pane_height as usize => (
            panel.properties_len.saturating_sub(panel.pane_height as usize),
            panel.properties_scroll as usize,
        ),
        _ => return,
    };

    let mut state = ScrollbarState::new(max_scroll).position(position);

    let scrollbar = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"));

    let scrollbar_area = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(2),
    };

    f.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
}
