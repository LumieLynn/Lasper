use crate::tui::features::workspace::inspector::{
    panes, DetailPane, DetailPanel, DetailRenderContext,
};
use crate::tui::foundation::scrolling::ScrollAction;
use crate::tui::foundation::widgets::display::text::{TextContent, TextEntryIndex, WrappedEntries};
use ratatui::layout::Rect;

pub(crate) fn sync_viewport(
    panel: &mut DetailPanel,
    data: &mut DetailRenderContext<'_>,
    area: Rect,
) {
    let width = usize::from(area.width);

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

    // Retain one log index for the displayed source. The active pane owns
    // follow-tail policy; layout only remaps the old top row.
    if let Some(logs) = data.logs.as_ref() {
        if panel.log_source != Some(logs.id) {
            panel.log_index = TextEntryIndex::default();
            panel.log_source = Some(logs.id);
        }
        if pane == DetailPane::Logs {
            let following = panel.log_scroll.offset() == panel.log_scroll.max_offset();
            let layout = WrappedEntries::new(logs.lines, logs.first, &mut panel.log_index)
                .layout(area.width, panel.log_scroll.offset());
            panel.logs_len = layout.height;
            panel
                .log_scroll
                .update(layout.height, usize::from(area.height));
            panel.log_scroll.reset();
            panel.log_scroll.apply(ScrollAction::Down(layout.top_row));
            if following {
                panel.log_scroll.apply(ScrollAction::Last);
            }
        }
    } else {
        panel.logs_len = 0;
        panel.log_index = TextEntryIndex::default();
        panel.log_source = None;
        panel.log_scroll.update(0, usize::from(area.height));
    }

    let height = usize::from(panel.pane_height);
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
