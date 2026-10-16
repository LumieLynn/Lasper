//! systemd's own clients for the same transient unit pair as the native path.

use crate::adapters::process::{command_diagnostic, CommandRunner, DefaultCommandRunner};

use super::{ActivationSpec, UnitManager, UnitState, COMMAND_TIMEOUT, PATH_DESCRIPTION, PATH_UNIT};

pub(super) struct UserManager<R = DefaultCommandRunner> {
    runner: R,
}

impl UserManager {
    pub(super) fn new() -> Self {
        Self {
            runner: DefaultCommandRunner,
        }
    }
}

impl<R: CommandRunner> UserManager<R> {
    async fn systemctl(&self, operation: &str, unit: Option<&str>) -> Result<(), String> {
        let mut args = vec![
            "--user".into(),
            "--no-ask-password".into(),
            operation.into(),
        ];
        if let Some(unit) = unit {
            args.push(unit.into());
        }
        let output = self
            .runner
            .run_bounded("systemctl", args, COMMAND_TIMEOUT)
            .await
            .map_err(|error| format!("systemctl --user {operation}: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "systemctl --user {operation}: {}",
                command_diagnostic(&output)
            ))
        }
    }
}

#[async_trait::async_trait]
impl<R: CommandRunner> UnitManager for UserManager<R> {
    async fn inspect(&self, unit: &str) -> Result<Option<UnitState>, String> {
        let args = vec![
            "--user".into(),
            "--no-ask-password".into(),
            "show".into(),
            "--property=LoadState,Transient,Description,ActiveState".into(),
            unit.into(),
        ];
        let output = self
            .runner
            .run_bounded("systemctl", args, COMMAND_TIMEOUT)
            .await
            .map_err(|error| format!("inspect user unit {unit}: {error}"))?;
        let properties: std::collections::HashMap<_, _> = std::str::from_utf8(&output.stdout)
            .map_err(|error| format!("inspect user unit {unit}: {error}"))?
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        // show reports not-found through LoadState, independently of locale.
        if properties.get("LoadState") == Some(&"not-found") {
            return Ok(None);
        }
        if !output.status.success() {
            return Err(format!(
                "inspect user unit {unit}: {}",
                command_diagnostic(&output)
            ));
        }
        let property = |name| {
            properties
                .get(name)
                .copied()
                .ok_or_else(|| format!("inspect user unit {unit}: property {name} is missing"))
        };
        let transient = match property("Transient")? {
            "yes" => true,
            "no" => false,
            value => {
                return Err(format!(
                    "inspect user unit {unit}: invalid Transient={value}"
                ))
            }
        };
        Ok(Some(UnitState {
            transient,
            description: property("Description")?.into(),
            active_state: property("ActiveState")?.into(),
        }))
    }

    async fn create(&self, spec: &ActivationSpec, include_service: bool) -> Result<(), String> {
        let directory = spec
            .watched_directory
            .to_str()
            .ok_or_else(|| "X11 machine registration path is not valid UTF-8".to_owned())?;
        let mut args = vec![
            "--user".into(),
            "--no-ask-password".into(),
            "--quiet".into(),
            format!("--unit={PATH_UNIT}"),
            format!("--description={}", spec.service_description()),
            format!("--path-property=Description={PATH_DESCRIPTION}"),
            format!("--path-property=PathChanged={directory}"),
            "--path-property=CollectMode=inactive-or-failed".into(),
            "--property=CollectMode=inactive-or-failed".into(),
            "--property=Type=oneshot".into(),
            "--property=UMask=0077".into(),
        ];
        if include_service {
            args.extend([
                "--".into(),
                spec.executable.clone(),
                "--internal-x11-reconcile".into(),
            ]);
        }
        let output = self
            .runner
            .run_bounded("systemd-run", args, COMMAND_TIMEOUT)
            .await
            .map_err(|error| format!("create X11 transient units: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "create X11 transient units: {}",
                command_diagnostic(&output)
            ))
        }
    }

    async fn start(&self, unit: &str) -> Result<(), String> {
        self.systemctl("start", Some(unit)).await
    }

    async fn stop(&self, unit: &str) -> Result<(), String> {
        self.systemctl("stop", Some(unit)).await
    }

    async fn reset_failed(&self, unit: &str) -> Result<(), String> {
        self.systemctl("reset-failed", Some(unit)).await
    }

    async fn reload(&self) -> Result<(), String> {
        self.systemctl("daemon-reload", None).await
    }
}

#[cfg(test)]
#[path = "tests/systemd_tools.rs"]
mod tests;
