use std::sync::Mutex;

use super::{ActivationSpec, UnitManager, UnitState, PATH_DESCRIPTION, PATH_UNIT, SERVICE_UNIT};

#[derive(Default)]
pub(super) struct FakeUnitManager {
    pub(super) units: Mutex<std::collections::HashMap<String, UnitState>>,
    pub(super) operations: Mutex<Vec<String>>,
    pub(super) create_error: Option<&'static str>,
    pub(super) service_exits_before_rearm: bool,
    pub(super) rearm_error: Option<&'static str>,
}

impl FakeUnitManager {
    pub(super) fn installed(spec: &ActivationSpec, service_state: &str) -> Self {
        Self {
            units: Mutex::new(std::collections::HashMap::from([
                (
                    PATH_UNIT.into(),
                    UnitState {
                        transient: true,
                        description: PATH_DESCRIPTION.into(),
                        active_state: "active".into(),
                    },
                ),
                (
                    SERVICE_UNIT.into(),
                    UnitState {
                        transient: true,
                        description: spec.service_description(),
                        active_state: service_state.into(),
                    },
                ),
            ])),
            ..Default::default()
        }
    }

    fn record(&self, operation: &str, unit: &str) {
        self.operations
            .lock()
            .unwrap()
            .push(format!("{operation} {unit}"));
    }
}

#[async_trait::async_trait]
impl UnitManager for FakeUnitManager {
    async fn inspect(&self, unit: &str) -> Result<Option<UnitState>, String> {
        let mut units = self.units.lock().unwrap();
        // Model the manager collecting an unreferenced inactive companion.
        if unit == SERVICE_UNIT
            && !units.contains_key(PATH_UNIT)
            && units
                .get(SERVICE_UNIT)
                .is_some_and(|state| state.active_state == "inactive")
        {
            units.remove(SERVICE_UNIT);
        }
        Ok(units.get(unit).map(|state| UnitState {
            transient: state.transient,
            description: state.description.clone(),
            active_state: state.active_state.clone(),
        }))
    }

    async fn create(&self, spec: &ActivationSpec, include_service: bool) -> Result<(), String> {
        self.record(
            if include_service {
                "create"
            } else {
                "create-path"
            },
            PATH_UNIT,
        );
        if let Some(error) = self.create_error {
            return Err(error.into());
        }
        let mut units = self.units.lock().unwrap();
        if include_service {
            units.insert(
                SERVICE_UNIT.into(),
                UnitState {
                    transient: true,
                    description: spec.service_description(),
                    active_state: "inactive".into(),
                },
            );
        } else {
            if self.service_exits_before_rearm {
                units.remove(SERVICE_UNIT);
            }
            if let Some(error) = self.rearm_error {
                return Err(error.into());
            }
        }
        units.insert(
            PATH_UNIT.into(),
            UnitState {
                transient: true,
                description: PATH_DESCRIPTION.into(),
                active_state: "active".into(),
            },
        );
        Ok(())
    }

    async fn start(&self, unit: &str) -> Result<(), String> {
        self.record("start", unit);
        Ok(())
    }

    async fn stop(&self, unit: &str) -> Result<(), String> {
        self.record("stop", unit);
        let mut units = self.units.lock().unwrap();
        units.remove(unit);
        if !units.get(SERVICE_UNIT).is_some_and(UnitState::running) {
            units.remove(SERVICE_UNIT);
        }
        Ok(())
    }

    async fn reset_failed(&self, unit: &str) -> Result<(), String> {
        self.record("reset-failed", unit);
        self.units.lock().unwrap().remove(unit);
        Ok(())
    }

    async fn reload(&self) -> Result<(), String> {
        self.record("reload", "manager");
        Ok(())
    }
}
