//! Data owned by the currently visible detail target.

use crate::domain::inspection::MachineProperties;
use crate::domain::machine::MachineName;
use crate::domain::runtime::{MachineEntry, MachineState};
use crate::tui::features::workspace::inspector::log_manager::LogManager;
use crate::tui::features::workspace::inspector::refresh::{
    DetailRefreshCompletion, DetailRefreshJob, DetailRefreshResult, DetailRefreshState,
    DetailRefreshWork, PreparedDetailRefresh,
};
use crate::tui::features::workspace::inspector::DetailPane;
use crate::tui::features::workspace::inspector::{
    DetailLogData, DetailRenderContext, DetailTarget,
};
use crate::tui::features::workspace::resources::catalog::ResourceCatalog;

pub(crate) struct DetailState {
    pub(crate) target: DetailTarget,
    pub(crate) properties: Result<MachineProperties, String>,
    pub(crate) config_content: Option<String>,
    pub(crate) config_path: Option<std::path::PathBuf>,
    pub(crate) config_error: Option<String>,
    pub(crate) unit_name: Option<String>,
    pub(crate) unit_drop_ins: Vec<crate::application::inspection::SystemdDropInInspection>,
    pub(crate) overview_dirty: bool,
    pub(crate) config_dirty: bool,
    pub(crate) details_dirty: bool,
    pub(crate) refresh: DetailRefreshState,
}

impl DetailState {
    pub(crate) fn new() -> Self {
        Self {
            target: DetailTarget::Empty,
            properties: Ok(MachineProperties::default()),
            config_content: None,
            config_path: None,
            config_error: None,
            unit_name: None,
            unit_drop_ins: Vec::new(),
            overview_dirty: true,
            config_dirty: true,
            details_dirty: true,
            refresh: DetailRefreshState::default(),
        }
    }

    /// Replace the target and drop data that belongs to the previous target.
    /// Returns whether the visible target actually changed.
    pub(crate) fn replace_target(&mut self, target: DetailTarget) -> bool {
        if self.target == target {
            return false;
        }
        self.refresh.cancel();
        self.target = target;
        self.properties = Ok(MachineProperties::default());
        self.overview_dirty = true;
        self.details_dirty = true;
        self.config_dirty = true;
        self.config_content = None;
        self.config_path = None;
        self.config_error = None;
        self.unit_name = None;
        self.unit_drop_ins.clear();
        true
    }

    pub(crate) fn mark_properties_dirty(&mut self) {
        self.overview_dirty = true;
        self.details_dirty = true;
    }

    /// Project the current target into borrowed panel data. Rendering can
    /// acknowledge dirty content without owning the catalog or log streams.
    pub(crate) fn render_context<'a>(
        &'a mut self,
        runtime: &'a ResourceCatalog,
        log_manager: &'a mut LogManager,
    ) -> DetailRenderContext<'a> {
        let machine = match &self.target {
            DetailTarget::Machine(name) => runtime.entries.iter().find(|entry| entry.name == *name),
            _ => None,
        };
        let image = match &self.target {
            DetailTarget::Image { name, internal } => runtime
                .active_images(*internal)
                .0
                .iter()
                .find(|image| image.name == *name),
            _ => None,
        };
        let metrics = machine.and_then(|entry| runtime.metrics.get(&entry.name));
        let logs = machine
            .and_then(|entry| log_manager.buffer_mut(&entry.name))
            .map(|buffer| DetailLogData {
                lines: &buffer.lines,
                dirty: &mut buffer.dirty,
            });
        DetailRenderContext {
            target: &self.target,
            machine,
            image,
            metrics,
            properties: self.properties.as_ref().map_err(String::as_str),
            config_content: self.config_content.as_deref(),
            config_path: self.config_path.as_deref(),
            config_error: self.config_error.as_deref(),
            unit_name: self.unit_name.as_deref(),
            unit_drop_ins: &self.unit_drop_ins,
            logs,
            overview_dirty: &mut self.overview_dirty,
            config_dirty: &mut self.config_dirty,
            details_dirty: &mut self.details_dirty,
        }
    }

    pub(crate) fn set_config(
        &mut self,
        content: Option<String>,
        path: Option<std::path::PathBuf>,
        error: Option<String>,
    ) -> bool {
        let changed = self.config_path != path
            || self.config_content != content
            || self.config_error != error;
        self.config_content = content;
        self.config_path = path;
        self.config_error = error;
        if changed {
            self.config_dirty = true;
        }
        changed
    }
}

#[derive(Default)]
pub(crate) struct DetailRefreshEffects {
    pub status: Option<(String, crate::tui::foundation::status::StatusLevel)>,
    pub reset_config_scroll: bool,
}

impl DetailState {
    pub(crate) fn prepare_refresh(
        &mut self,
        pane: DetailPane,
        machines: &[MachineEntry],
        logs: &mut LogManager,
    ) -> Option<PreparedDetailRefresh> {
        let ticket = self.refresh.take_pending()?;
        if ticket.target != self.target || ticket.pane != pane {
            return Some(PreparedDetailRefresh::Ready(DetailRefreshCompletion {
                ticket,
                result: DetailRefreshResult::Noop,
            }));
        }

        let ready = |ticket, result| {
            PreparedDetailRefresh::Ready(DetailRefreshCompletion { ticket, result })
        };
        let job = |ticket, work| PreparedDetailRefresh::Job(DetailRefreshJob { ticket, work });

        let prepared = match &ticket.target {
            DetailTarget::Empty => ready(ticket.clone(), DetailRefreshResult::Empty),
            DetailTarget::Machine(name) => {
                let Some(entry) = machines.iter().find(|entry| entry.name == *name).cloned() else {
                    return Some(ready(ticket, DetailRefreshResult::Empty));
                };
                match ticket.pane {
                    DetailPane::MachineOverview | DetailPane::Details => job(
                        ticket.clone(),
                        DetailRefreshWork::MachineProperties {
                            name: name.clone(),
                            entry,
                        },
                    ),
                    DetailPane::Logs => {
                        logs.get_or_create(name);
                        if entry.state == MachineState::Running && logs.can_start_stream(name) {
                            job(
                                ticket.clone(),
                                DetailRefreshWork::Journal { name: name.clone() },
                            )
                        } else {
                            if entry.state != MachineState::Running && logs.stop_stream(name) {
                                logs.push_line(name, "[CONTAINER STOPPED]");
                            }
                            ready(ticket.clone(), DetailRefreshResult::Noop)
                        }
                    }
                    DetailPane::Config => {
                        job(ticket.clone(), DetailRefreshWork::MachineConfig { entry })
                    }
                    DetailPane::Metrics | DetailPane::ImageOverview | DetailPane::ImageConfig => {
                        ready(ticket.clone(), DetailRefreshResult::Noop)
                    }
                }
            }
            DetailTarget::Image { name, .. } => match ticket.pane {
                DetailPane::ImageConfig => job(
                    ticket.clone(),
                    DetailRefreshWork::ImageConfig { name: name.clone() },
                ),
                DetailPane::Details => job(
                    ticket.clone(),
                    DetailRefreshWork::ImageUnit { name: name.clone() },
                ),
                DetailPane::MachineOverview
                | DetailPane::Logs
                | DetailPane::Config
                | DetailPane::Metrics
                | DetailPane::ImageOverview => ready(ticket.clone(), DetailRefreshResult::Noop),
            },
        };
        Some(prepared)
    }

    pub(crate) fn apply_refresh(
        &mut self,
        completion: DetailRefreshCompletion,
        pane: DetailPane,
        logs: &mut LogManager,
    ) -> DetailRefreshEffects {
        let mut effects = DetailRefreshEffects::default();
        if !self.refresh.finish(completion.ticket.revision) {
            return effects;
        }
        if completion.ticket.target != self.target || completion.ticket.pane != pane {
            return effects;
        }

        match completion.result {
            DetailRefreshResult::Empty => {
                self.properties = Ok(MachineProperties::default());
                self.mark_properties_dirty();
                logs.cleanup_all();
                effects.reset_config_scroll = self.apply_config_result(Ok(None));
                self.unit_name = None;
                self.unit_drop_ins.clear();
            }
            DetailRefreshResult::Noop => {}
            DetailRefreshResult::MachineProperties(result) => {
                match result {
                    Ok(query) => {
                        if let Some(fallback) = query.fallback {
                            effects.status = Some((
                                format!(
                                    "{} unavailable: {}; using {}",
                                    fallback.from.label(),
                                    fallback.reason,
                                    fallback.to.label()
                                ),
                                crate::tui::foundation::status::StatusLevel::Warn,
                            ));
                        }
                        self.properties = Ok(query.value);
                    }
                    Err(error) => {
                        log::debug!("{error}");
                        self.properties = Err(error);
                    }
                }
                self.mark_properties_dirty();
            }
            DetailRefreshResult::Journal(mut result) => {
                let Some(name) = completion.ticket.target.name() else {
                    return effects;
                };
                if let Some(handle) = result.handle.take() {
                    logs.attach_stream(name, handle);
                } else if let Some(error) = result.error {
                    logs.push_line(name, format!("Log stream error: {error}"));
                    if let Some(hint) = result.hint {
                        logs.push_line(name, hint);
                    }
                    logs.mark_stream_failed(name);
                }
            }
            DetailRefreshResult::Config(result) => {
                let name = completion.ticket.target.name().unwrap_or_default();
                if let Err(error) = &result {
                    if error.is_unsupported() {
                        log::debug!("No nspawn config route for {name}: {error}");
                    } else {
                        log::warn!("Failed to read .nspawn config for {name}: {error}");
                    }
                }
                effects.reset_config_scroll = self.apply_config_result(result);
            }
            DetailRefreshResult::ImageUnit(result) => {
                self.properties = match result.properties {
                    Ok(Some(properties)) => Ok(properties),
                    Ok(None) => Ok(MachineProperties::default()),
                    Err(error) => Err(error.to_string()),
                };
                self.overview_dirty = true;
                self.details_dirty = true;
                match result.unit {
                    Some(Ok(unit)) => {
                        self.unit_name = Some(unit.unit);
                        self.unit_drop_ins = unit.drop_ins;
                    }
                    Some(Err(error)) => {
                        let name = completion.ticket.target.name().unwrap_or_default();
                        log::debug!("Failed to read unit drop-ins for {name}: {error}");
                        self.unit_name = MachineName::new(name)
                            .ok()
                            .map(|name| name.systemd_nspawn_unit());
                        self.unit_drop_ins.clear();
                    }
                    None => {
                        self.unit_name = None;
                        self.unit_drop_ins.clear();
                    }
                }
            }
        }
        effects
    }

    fn apply_config_result(
        &mut self,
        result: Result<
            Option<crate::application::inspection::NspawnConfigInspection>,
            crate::application::inspection::ResourceInspectionError,
        >,
    ) -> bool {
        let (config, error) = match result {
            Ok(config) => (config, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let new_path = config.as_ref().map(|config| config.path.clone());
        let new_content = config.map(|config| config.content);
        self.set_config(new_content, new_path, error)
    }
}
