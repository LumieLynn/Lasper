use ratatui::style::{Modifier, Style};

use crate::tui::foundation::theme;

pub fn property_style(key: &str, value: &str) -> Style {
    let t = theme::theme();

    if value == "yes" && key != "ReadOnly" {
        return Style::default().fg(t.prop_enabled);
    }
    if value == "no" && key != "ReadOnly" {
        return Style::default().fg(t.prop_disabled);
    }

    match key {
        "Enabled" | "UnitFileState" => match value {
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
        "ActiveState" | "SubState" => match value {
            "active" | "running" => Style::default().fg(t.prop_enabled),
            "activating" | "deactivating" | "reloading" | "start" | "stop" => Style::default()
                .fg(t.prop_transitional)
                .add_modifier(Modifier::ITALIC),
            "failed" => Style::default().fg(t.error),
            "inactive" | "dead" => Style::default().fg(t.prop_disabled),
            _ => Style::default().fg(t.prop_unknown),
        },
        "LoadState" => match value {
            "loaded" => Style::default().fg(t.prop_enabled),
            "error" | "bad-setting" => Style::default().fg(t.error),
            _ => Style::default().fg(t.prop_unknown),
        },
        "Result" => match value {
            "success" => Style::default().fg(t.prop_enabled),
            "[not set]" | "[unprintable]" => Style::default().fg(t.prop_unknown),
            _ => Style::default().fg(t.error),
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
        "MemoryCurrent" | "MemoryPeak" | "MemoryMax" | "EffectiveMemoryMax" | "Usage" => {
            Style::default().fg(t.prop_memory)
        }
        _ => Style::default().fg(t.prop_default),
    }
}
