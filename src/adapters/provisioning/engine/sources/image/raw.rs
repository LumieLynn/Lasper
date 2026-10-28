//! Publication and validation of imported raw system images.

use super::validate_source;
use crate::adapters::provisioning::engine::failure::{OperationFailure, OperationResult};
use crate::adapters::provisioning::engine::host::HostResultExt;
use crate::domain::machine::MachineName;
use std::path::PathBuf;

const IMAGE_VALIDATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_IMAGE_VALIDATION_OUTPUT_BYTES: usize = 1024 * 1024;

pub(crate) async fn import_raw_system_image(
    machine: MachineName,
    source: std::fs::File,
) -> OperationResult<()> {
    validate_source(&source)?;
    let path = crate::adapters::storage::image_ops::import_raw_image(&machine, source)
        .await
        .host_context("publish imported raw image")?;
    let mut command = crate::adapters::process::new_command("systemd-dissect");
    command.args(["--validate"]).arg(&path);
    let output = match crate::adapters::process::run_bounded_child_command(
        command,
        None,
        IMAGE_VALIDATION_TIMEOUT,
        "systemd-dissect --validate",
        MAX_IMAGE_VALIDATION_OUTPUT_BYTES,
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            remove_failed_raw_import(&path).await;
            return Err(OperationFailure::Io(
                PathBuf::from("systemd-dissect"),
                error,
            ));
        }
    };
    crate::adapters::process::log_output("systemd-dissect --validate", &output);
    if !output.status.success() {
        remove_failed_raw_import(&path).await;
        return Err(OperationFailure::command_failed(
            "validate imported raw system image",
            format!("systemd-dissect --validate {}", path.display()),
            &output,
        ));
    }
    Ok(())
}

async fn remove_failed_raw_import(path: &std::path::Path) {
    if let Err(error) = tokio::fs::remove_file(path).await {
        log::warn!(
            "Failed to remove rejected raw image {}: {}",
            path.display(),
            error
        );
    }
}
