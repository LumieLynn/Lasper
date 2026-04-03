use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    symbols,
    text::Span,
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType},
    Frame,
};

use crate::app::AppData;

pub fn render(f: &mut Frame, data: &AppData, area: Rect) {
    let container_name = match data.entries.get(data.selected) {
        Some(e) => &e.name,
        None => return,
    };

    let metrics = match data.metrics.get(container_name) {
        Some(m) => m,
        None => {
            let waiting = Block::default()
                .borders(Borders::ALL)
                .title(" Realtime Metrics ");
            let inner = waiting.inner(area);
            f.render_widget(waiting, area);
            f.render_widget(
                ratatui::widgets::Paragraph::new("Waiting for metrics data..."),
                inner,
            );
            return;
        }
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // CPU Chart
    let cpu_data = &metrics.cpu_history;
    let max_cpu_x = cpu_data.last().map(|(x, _)| *x).unwrap_or(0.0);
    let min_cpu_x = (max_cpu_x - 60.0).max(0.0);

    let cpu_dataset = Dataset::default()
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan))
        .data(cpu_data);

    // Calculate Y bounds for CPU based on min/max in current window
    let (min_cpu_val, max_cpu_val) = if cpu_data.is_empty() {
        (0.0, 10.0)
    } else {
        let min = cpu_data
            .iter()
            .map(|(_, y)| *y)
            .fold(f64::INFINITY, f64::min);
        let max = cpu_data
            .iter()
            .map(|(_, y)| *y)
            .fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 0.1 {
            ((min - 5.0).max(0.0), max + 5.0)
        } else {
            let margin = (max - min) * 0.1;
            ((min - margin).max(0.0), max + margin)
        }
    };

    let cpu_chart = Chart::new(vec![cpu_dataset])
        .block(
            Block::default()
                .title(" CPU Usage (%) ")
                .borders(Borders::ALL),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::Gray))
                .bounds([min_cpu_x, max_cpu_x])
                .labels(vec![Span::raw("-60s"), Span::raw("now")]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::Gray))
                .bounds([min_cpu_val, max_cpu_val])
                .labels(get_ticks(min_cpu_val, max_cpu_val, format_cpu)),
        );

    f.render_widget(cpu_chart, chunks[0]);

    // RAM Chart
    let ram_data = &metrics.ram_history;
    let max_ram_x = ram_data.last().map(|(x, _)| *x).unwrap_or(0.0);
    let min_ram_x = (max_ram_x - 60.0).max(0.0);

    // Dynamically calculate Y bounds for RAM
    let (min_ram_val, max_ram_val) = if ram_data.is_empty() {
        (0.0, 10.0)
    } else {
        let min = ram_data
            .iter()
            .map(|(_, y)| *y)
            .fold(f64::INFINITY, f64::min);
        let max = ram_data
            .iter()
            .map(|(_, y)| *y)
            .fold(f64::NEG_INFINITY, f64::max);
        if (max - min).abs() < 1.0 {
            ((min - 10.0).max(0.0), max + 10.0)
        } else {
            let margin = (max - min) * 0.1;
            ((min - margin).max(0.0), max + margin)
        }
    };

    let ram_dataset = Dataset::default()
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Magenta))
        .data(ram_data);

    let ram_chart = Chart::new(vec![ram_dataset])
        .block(
            Block::default()
                .title(" Memory Usage ")
                .borders(Borders::ALL),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::Gray))
                .bounds([min_ram_x, max_ram_x])
                .labels(vec![Span::raw("-60s"), Span::raw("now")]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::Gray))
                .bounds([min_ram_val, max_ram_val])
                .labels(get_ticks(min_ram_val, max_ram_val, format_memory)),
        );

    f.render_widget(ram_chart, chunks[1]);
}

fn format_memory(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.2}G", mb / 1024.0)
    } else {
        format!("{:.1}M", mb)
    }
}

fn format_cpu(cpu: f64) -> String {
    if cpu >= 100.0 {
        format!("{:.0}%", cpu)
    } else {
        format!("{:.1}%", cpu)
    }
}

fn get_ticks(min: f64, max: f64, formatter: fn(f64) -> String) -> Vec<Span<'static>> {
    let range = max - min;
    if range <= 0.1 {
        return vec![Span::raw(formatter(min)), Span::raw(formatter(max))];
    }

    // Determine number of ticks based on range visibility
    let num_ticks = if range < 5.0 {
        3
    } else if range < 20.0 {
        4
    } else {
        5
    };

    let mut ticks = Vec::new();
    for i in 0..num_ticks {
        let val = min + (range * i as f64 / (num_ticks - 1) as f64);
        ticks.push(Span::raw(formatter(val)));
    }
    ticks
}
