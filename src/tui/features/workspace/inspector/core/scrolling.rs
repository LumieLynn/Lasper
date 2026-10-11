use crate::tui::features::workspace::inspector::{
    panes, DetailPane, DetailPanel, DetailRenderContext,
};
use crate::tui::foundation::scrolling::ScrollAction;

pub(crate) fn sync_viewport(
    panel: &mut DetailPanel,
    data: &mut DetailRenderContext<'_>,
    width: usize,
) {
    let follow_logs = panel.log_scroll.offset() == panel.log_scroll.max_offset();
    let width_changed = panel.last_rendered_width != width as u16;
    panel.last_rendered_width = width as u16;

    // Document panes measure and paint the same cached rows. Only the visible
    // pane is retained, and scrolling/height changes do not rebuild its layout.
    let pane = panel.active_pane();
    let dirty = match pane {
        DetailPane::MachineOverview | DetailPane::ImageOverview => Some(*data.overview_dirty),
        DetailPane::Details => Some(*data.details_dirty),
        DetailPane::Config | DetailPane::ImageConfig => Some(*data.config_dirty),
        DetailPane::Logs | DetailPane::Metrics => None,
    };
    if let Some(dirty) = dirty {
        panel
            .document_cache
            .update(data.target, pane, width, dirty, || {
                if width == 0 {
                    return Vec::new();
                }
                match pane {
                    DetailPane::MachineOverview => panes::machine_overview::lines(data, width),
                    DetailPane::ImageOverview => panes::image_overview::lines(data, width),
                    DetailPane::Details => panes::details::lines(data, width),
                    DetailPane::Config | DetailPane::ImageConfig => {
                        panes::configs::lines(data, width)
                    }
                    DetailPane::Logs | DetailPane::Metrics => unreachable!(),
                }
            });
        let length = panel.document_cache.lines().len();
        match pane {
            DetailPane::MachineOverview => panel.machine_overview_len = length,
            DetailPane::ImageOverview => panel.image_overview_len = length,
            DetailPane::Details => panel.details_len = length,
            DetailPane::Config | DetailPane::ImageConfig => panel.config_len = length,
            DetailPane::Logs | DetailPane::Metrics => unreachable!(),
        }
        match pane {
            DetailPane::MachineOverview | DetailPane::ImageOverview => *data.overview_dirty = false,
            DetailPane::Details => *data.details_dirty = false,
            DetailPane::Config | DetailPane::ImageConfig => *data.config_dirty = false,
            DetailPane::Logs | DetailPane::Metrics => unreachable!(),
        }
    } else {
        panel.document_cache.clear();
    }

    // Wrapped log offsets belong to one target and viewport width.
    if let Some(buf) = data.logs.as_mut() {
        let target_changed = panel.log_target.as_deref() != data.target.name();
        if *buf.dirty || width_changed || target_changed {
            let lines: Vec<&str> = buf.lines.iter().map(|s| s.as_str()).collect();
            panel.log_cache =
                panes::logs::LogRenderCache::compute(&lines, panel.last_rendered_width);
            panel.logs_len = panel.log_cache.wrapped_height;
            *buf.dirty = false;
            if target_changed {
                panel.log_target = data.target.name().map(str::to_owned);
            }
        }
    } else {
        panel.logs_len = 0;
        panel.log_cache = panes::logs::LogRenderCache::default();
        panel.log_target = None;
    }

    let height = usize::from(panel.pane_height);
    panel.log_scroll.update(panel.logs_len, height);
    if follow_logs && panel.active_pane() == DetailPane::Logs {
        panel.log_scroll.apply(ScrollAction::Last);
    }
    panel.config_scroll.update(panel.config_len, height);
    let active_details_len = if panel.active_pane() == DetailPane::ImageOverview {
        panel.image_overview_len
    } else {
        panel.details_len
    };
    panel.details_scroll.update(active_details_len, height);
    panel
        .machine_overview_scroll
        .update(panel.machine_overview_len, height);
}
