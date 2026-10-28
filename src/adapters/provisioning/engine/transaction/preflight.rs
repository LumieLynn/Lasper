//! Read-only inspection of existing configuration before deployment effects.

use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::adapters::provisioning::engine::host::DirectProvisioningCapabilities;
use crate::adapters::provisioning::engine::host::HostResultExt;

pub(super) async fn inspect_deployment_sidecars(
    name: &str,
    host: &DirectProvisioningCapabilities,
) -> OperationResult<Vec<String>> {
    if let Some(config) = host
        .nspawn
        .inspect(name)
        .await
        .host_context("inspect existing nspawn configuration")?
    {
        return Err(OperationFailure::InvalidInput(format!(
            "Deployment target has existing .nspawn configuration: {}",
            config.path.display()
        )));
    }
    let mut warnings = Vec::new();
    match host.nvidia_state.read(name).await {
        Ok(Some(_)) => warnings.push(format!(
            "Deployment target {name:?} has existing NVIDIA state; it will be replaced only when current Lasper ownership can be proven."
        )),
        Ok(None) => {}
        Err(error) => warnings.push(format!(
            "Existing NVIDIA state could not be safely inspected and will be preserved: {error}"
        )),
    }

    match host.systemd_unit.read(name).await {
        Ok(unit) => {
            for drop_in in unit.drop_ins {
                let file_name = std::path::Path::new(&drop_in.path)
                    .file_name()
                    .and_then(|name| name.to_str());
                if file_name == Some("90-lasper.conf") {
                    warnings.push(format!(
                        "Existing systemd service drop-in {} will be replaced only when current Lasper ownership can be proven.",
                        drop_in.path
                    ));
                } else {
                    warnings.push(format!(
                        "Existing systemd service drop-in {} will be preserved.",
                        drop_in.path
                    ));
                }
            }
        }
        Err(error) => warnings.push(format!(
            "Existing systemd service drop-ins could not be safely inspected and will be preserved unless a validated Lasper-owned target is updated: {error}"
        )),
    }
    Ok(warnings)
}
