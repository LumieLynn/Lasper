use ratatui::style::{Modifier, Style};

use crate::tui::theme;

pub fn property_style(key: &str, value: &str) -> Style {
    let t = theme::theme();

    if value == "yes" && key != "ReadOnly" {
        return Style::default().fg(t.prop_enabled);
    }
    if value == "no" {
        return Style::default().fg(t.prop_disabled);
    }

    match key {
        "Enabled" => match value {
            "enabled" | "enabled-runtime" | "yes" => Style::default().fg(t.prop_enabled),
            "disabled" | "no" => Style::default().fg(t.prop_disabled),
            _ => Style::default().fg(t.prop_unknown),
        },
        "State" => match value {
            "running" | "yes" => Style::default().fg(t.prop_enabled),
            "starting" | "exiting" => Style::default()
                .fg(t.prop_transitional)
                .add_modifier(Modifier::ITALIC),
            "poweroff" | "no" => Style::default().fg(t.prop_readonly_no),
            _ => Style::default().fg(t.prop_unknown),
        },
        "ReadOnly" => {
            if value == "yes" {
                Style::default()
                    .fg(t.prop_readonly_yes)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.prop_readonly_no)
            }
        }
        "MainPID" | "Leader" => Style::default().fg(t.prop_pid),
        "MemoryCurrent" | "Usage" => Style::default().fg(t.prop_memory),
        _ => Style::default().fg(t.prop_default),
    }
}
