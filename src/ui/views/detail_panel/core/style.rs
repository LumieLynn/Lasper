use ratatui::style::{Color, Modifier, Style};

pub fn property_style(key: &str, value: &str) -> Style {
    if value == "yes" && key != "ReadOnly" {
        return Style::default().fg(Color::Green);
    }
    if value == "no" {
        return Style::default().fg(Color::DarkGray);
    }

    match key {
        "Enabled" => match value {
            "enabled" | "enabled-runtime" | "yes" => Style::default().fg(Color::Green),
            "disabled" | "no" => Style::default().fg(Color::Red),
            _ => Style::default().fg(Color::Yellow),
        },
        "State" => match value {
            "running" | "yes" => Style::default().fg(Color::Green),
            "starting" | "exiting" => Style::default().fg(Color::Cyan).add_modifier(Modifier::ITALIC),
            "poweroff" | "no" => Style::default().fg(Color::DarkGray),
            _ => Style::default().fg(Color::Yellow),
        },
        "ReadOnly" => {
            if value == "yes" {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            }
        }
        "MainPID" | "Leader" => Style::default().fg(Color::Magenta),
        "MemoryCurrent" | "Usage" => Style::default().fg(Color::Blue),
        _ => Style::default().fg(Color::White),
    }
}
