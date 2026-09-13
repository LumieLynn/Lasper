use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRoute {
    DirectDbus,
    ElevatedDbus,
    LocalSystemdTools,
    ElevatedSystemdTools,
}

impl ExecutionRoute {
    pub fn is_dbus(self) -> bool {
        matches!(self, Self::DirectDbus | Self::ElevatedDbus)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::DirectDbus => "direct D-Bus",
            Self::ElevatedDbus => "elevated D-Bus",
            Self::LocalSystemdTools => "local systemd tools",
            Self::ElevatedSystemdTools => "elevated systemd tools",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteFallback {
    pub from: ExecutionRoute,
    pub to: ExecutionRoute,
    pub reason: String,
}
