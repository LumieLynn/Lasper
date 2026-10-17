//! Catalog observations and bounded journal evidence for lifecycle decisions.

use crate::adapters::process::CommandRunner;
use crate::application::machine_lifecycle::{
    MachineObservation, MachineStartDiagnostics, StartFailureEvidence,
};
use crate::application::runtime::RuntimeResult;
use crate::application::RuntimeCatalog;
use crate::domain::inspection::{InspectionCompleteness, InspectionSource, MachineProperties};
use crate::domain::machine::MachineName;
use crate::domain::runtime::MachineEntry;
use std::sync::Arc;

pub(super) struct CatalogMachineObservation {
    pub(super) runtime: Arc<RuntimeCatalog>,
    pub(super) systemd_tools: Arc<dyn CommandRunner>,
}

#[async_trait::async_trait]
impl MachineObservation for CatalogMachineObservation {
    async fn inspect(
        &self,
        machine: &MachineName,
        entry: &MachineEntry,
    ) -> RuntimeResult<MachineProperties> {
        let query = self.runtime.inspect(machine.as_str(), entry).await?;
        if query.value.completeness == InspectionCompleteness::RuntimeOnly {
            let mut properties = query.value;
            match crate::adapters::runtime::systemd_tools::append_nspawn_unit_properties_with_runner(
                machine,
                self.systemd_tools.as_ref(),
                &mut properties,
            )
            .await
            {
                Ok(true) => {
                    properties.source = InspectionSource::SystemdTools;
                    properties.completeness = InspectionCompleteness::Full;
                }
                Ok(false) => {}
                Err(error) => log::debug!(
                    "systemd-tools lifecycle inspection for {} is not ready: {}",
                    machine,
                    error
                ),
            }
            return Ok(properties);
        }
        Ok(query.value)
    }

    fn invalidate(&self) {
        self.runtime.invalidate();
    }
}

pub(super) struct LocalStartDiagnostics {
    pub(super) runner: Arc<dyn CommandRunner>,
}

#[async_trait::async_trait]
impl MachineStartDiagnostics for LocalStartDiagnostics {
    async fn collect(
        &self,
        machine: &MachineName,
        invocation_id: Option<String>,
        started_epoch: u64,
    ) -> StartFailureEvidence {
        let unit = machine.systemd_nspawn_unit();
        let (mut args, selector_display) = if let Some(invocation_id) = invocation_id.as_deref() {
            (
                vec![format!("_SYSTEMD_INVOCATION_ID={invocation_id}")],
                format!("_SYSTEMD_INVOCATION_ID={invocation_id}"),
            )
        } else {
            (
                vec![
                    "-u".into(),
                    unit,
                    "--since".into(),
                    format!("@{started_epoch}"),
                ],
                format!(
                    "-u {} --since @{started_epoch}",
                    machine.systemd_nspawn_unit()
                ),
            )
        };
        args.extend([
            "-n".into(),
            "40".into(),
            "--no-pager".into(),
            "--quiet".into(),
            "--output=short".into(),
        ]);
        let journal = self
            .runner
            .run_bounded("journalctl", args, std::time::Duration::from_secs(5))
            .await
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|output| !output.is_empty());
        StartFailureEvidence {
            journal_command: format!("journalctl {selector_display} --no-pager"),
            journal,
        }
    }
}
