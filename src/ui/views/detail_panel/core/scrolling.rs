use ratatui::{
    layout::Rect,
    text::Line,
    widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use super::super::DetailPanel;
use crate::app::{AppData, DetailPane};

pub fn sync_data_lengths(panel: &mut DetailPanel, data: &mut AppData, width: usize) {
    let old_logs_len = panel.logs_len;
    let width_changed = panel.last_rendered_width != width as u16;
    panel.last_rendered_width = width as u16;

    let val_width = if width > 6 {
        (width as f32 * 0.65) as usize
    } else {
        10
    };
    let val_width = val_width.saturating_sub(3).max(10);

    // 1. Properties
    if data.properties_dirty || width_changed {
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
        data.properties_dirty = false;
    }

    // 2. Details
    if data.details_dirty || width_changed {
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
        data.details_dirty = false;
    }

    // 3. Config
    if data.config_dirty || width_changed {
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
        data.config_dirty = false;
    }

    // 4. Logs (The hot path)
    if data.logs_dirty || width_changed {
        if width_changed || data.log_lines.len() < data.log_offset_index.len() {
            // Full re-calculate index
            data.log_offset_index.clear();
            let mut current_y = 0;
            for line in &data.log_lines {
                data.log_offset_index.push(current_y);
                current_y += calculate_line_wrapped_height(line, width);
            }
            data.log_wrapped_height = current_y;
        } else {
            // Incremental append
            let mut current_y = data.log_wrapped_height;
            let start_idx = data.log_offset_index.len();
            for i in start_idx..data.log_lines.len() {
                data.log_offset_index.push(current_y);
                current_y += calculate_line_wrapped_height(&data.log_lines[i], width);
            }
            data.log_wrapped_height = current_y;
        }
        panel.logs_len = data.log_wrapped_height;
        data.logs_dirty = false;
    }

    // Sticky autoscroll for Logs
    if panel.active_pane == DetailPane::Logs {
        let max_scroll_old = old_logs_len.saturating_sub(panel.old_pane_height as usize) as u16;
        let at_bottom = panel.log_scroll >= max_scroll_old;

        if at_bottom && (panel.logs_len > old_logs_len || panel.pane_height < panel.old_pane_height)
        {
            let max_scroll_new = panel.logs_len.saturating_sub(panel.pane_height as usize);
            panel.log_scroll = max_scroll_new.min(u16::MAX as usize) as u16;
        }
    }

    // Always clamp scroll to current max
    let log_max = panel.logs_len.saturating_sub(panel.pane_height as usize);
    panel.log_scroll = panel.log_scroll.min(log_max.min(u16::MAX as usize) as u16);

    let cfg_max = panel.config_len.saturating_sub(panel.pane_height as usize);
    panel.config_scroll = panel
        .config_scroll
        .min(cfg_max.min(u16::MAX as usize) as u16);

    let det_max = panel.details_len.saturating_sub(panel.pane_height as usize);
    panel.details_scroll = panel
        .details_scroll
        .min(det_max.min(u16::MAX as usize) as u16);

    let prop_max = panel
        .properties_len
        .saturating_sub(panel.pane_height as usize);
    panel.properties_scroll = panel
        .properties_scroll
        .min(prop_max.min(u16::MAX as usize) as u16);
}

fn calculate_line_wrapped_height(line: &Line, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut lines = 0;
    let mut current_line_width = 0;

    for span in &line.spans {
        // Split by whitespace to simulate word wrapping
        for word in span.content.split_inclusive(' ') {
            let mut word_width = 0;
            for c in word.chars() {
                if c == '\t' {
                    word_width += 8 - (word_width % 8);
                } else {
                    word_width += 1;
                }
            }

            if current_line_width + word_width > width {
                if word_width > width {
                    // Single long word wraps multiple times
                    if current_line_width > 0 {
                        lines += 1;
                    }
                    lines += (word_width - 1) / width;
                    current_line_width = word_width % width;
                } else {
                    // Word moves to new line
                    lines += 1;
                    current_line_width = word_width;
                }
            } else {
                current_line_width += word_width;
            }
        }
    }
    if current_line_width > 0 || lines == 0 {
        lines += 1;
    }
    lines
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
            panel
                .properties_len
                .saturating_sub(panel.pane_height as usize),
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
