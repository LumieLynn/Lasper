//! Query-family daemon handlers.
//!
//! Query handlers are read-only from Lasper's point of view. They may still
//! perform host I/O, so the request pump invokes them in a bounded task just
//! like the other protocol families; this module only owns their dispatch and
//! wire-result shaping.

use super::handler::{DaemonRuntimeQueries, HandleOutcome};
use crate::adapters::provisioning::engine::image_operation::inspect_tar_runtime;
use crate::domain::machine::MachineName;
use crate::ipc::protocol::{error_code, RpcFamily, RpcMethod};
use serde_json::Value;

pub(super) async fn handle<B: DaemonRuntimeQueries>(
    method: RpcMethod,
    id: u64,
    params: Value,
    dbus: &Option<B>,
    out_tx: &tokio::sync::mpsc::Sender<String>,
) -> HandleOutcome {
    debug_assert_eq!(method.family(), RpcFamily::Query);

    match method {
        RpcMethod::Ping => HandleOutcome::Sync(Ok(Value::Null)),

        RpcMethod::AssessTarRuntime => {
            if params != serde_json::json!({}) {
                return HandleOutcome::Sync(Err(
                    "assess_tar_runtime does not accept parameters".into()
                ));
            }
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(inspect_tar_runtime).await;
                let response = match result {
                    Ok(Ok(assessment)) => {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":assessment})
                    }
                    Ok(Err(error)) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":error_code::INTERNAL_ERROR,
                        "message":error.to_string(),
                    }}),
                    Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":error_code::INTERNAL_ERROR,
                        "message":format!("tar runtime inspection task failed: {error}"),
                    }}),
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        RpcMethod::CliInspectMachine => {
            let inspection: crate::ipc::protocol::CliInspectMachineRequest =
                match serde_json::from_value(params) {
                    Ok(request) => request,
                    Err(error) => {
                        return HandleOutcome::Sync(Err(format!(
                            "invalid cli_inspect_machine request: {error}"
                        )));
                    }
                };
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let response = match crate::adapters::runtime::cli::get_properties_with_runner(
                    inspection.machine.as_str(),
                    &crate::adapters::process::DefaultCommandRunner,
                )
                .await
                {
                    Ok(properties) => match serde_json::to_value(properties) {
                        Ok(result) => serde_json::json!({
                            "jsonrpc":"2.0","id":id,"result":result
                        }),
                        Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":error_code::INTERNAL_ERROR,
                            "message":error.to_string(),
                        }}),
                    },
                    Err(error) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                        "code":error_code::INTERNAL_ERROR,
                        "message":error.to_string(),
                    }}),
                };
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = out_tx.send(line).await;
                }
            });
            HandleOutcome::Spawned
        }

        RpcMethod::DbusListMachines => {
            let dbus = match dbus.as_ref() {
                Some(dbus) => dbus,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            match dbus.list_machines().await {
                Ok(machines) => match serde_json::to_value(machines) {
                    Ok(value) => HandleOutcome::Sync(Ok(value)),
                    Err(error) => HandleOutcome::Sync(Err(error.to_string())),
                },
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::DbusListImages => {
            let dbus = match dbus.as_ref() {
                Some(dbus) => dbus,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            match dbus.list_images().await {
                Ok(images) => match serde_json::to_value(images) {
                    Ok(value) => HandleOutcome::Sync(Ok(value)),
                    Err(error) => HandleOutcome::Sync(Err(error.to_string())),
                },
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::DbusGetProperties => {
            let dbus = match dbus.as_ref() {
                Some(dbus) => dbus,
                None => return HandleOutcome::Sync(Err("DBus not available".into())),
            };
            let name = match request_machine_name(&params) {
                Ok(name) => name,
                Err(error) => return HandleOutcome::Sync(Err(error)),
            };
            match dbus.get_properties(name.as_str()).await {
                Ok(properties) => match serde_json::to_value(properties) {
                    Ok(value) => HandleOutcome::Sync(Ok(value)),
                    Err(error) => HandleOutcome::Sync(Err(error.to_string())),
                },
                Err(error) => HandleOutcome::Sync(Err(error.to_string())),
            }
        }

        RpcMethod::DbusIsAvailable => match dbus {
            Some(dbus) => HandleOutcome::Sync(Ok(Value::Bool(dbus.is_available().await))),
            None => HandleOutcome::Sync(Ok(Value::Bool(false))),
        },

        _ => unreachable!("non-query method routed to query dispatcher"),
    }
}

pub(crate) fn request_machine_name(params: &Value) -> Result<MachineName, String> {
    let name = params["name"]
        .as_str()
        .ok_or_else(|| "missing name".to_string())?;
    MachineName::try_from(name).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_query_methods_are_routed_to_this_family() {
        for method in RpcMethod::ALL {
            if method.family() == RpcFamily::Query {
                assert!(matches!(
                    method,
                    RpcMethod::Ping
                        | RpcMethod::AssessTarRuntime
                        | RpcMethod::CliInspectMachine
                        | RpcMethod::DbusListMachines
                        | RpcMethod::DbusListImages
                        | RpcMethod::DbusGetProperties
                        | RpcMethod::DbusIsAvailable
                ));
            }
        }
    }
}
