//! Query-family daemon handlers.
//!
//! Query handlers are read-only from Lasper's point of view. They may still
//! perform host I/O, so the request pump invokes them in a bounded task just
//! like the other protocol families; this module only owns their dispatch and
//! wire-result shaping.

use crate::adapters::provisioning::engine::image_operation::inspect_tar_runtime;
use crate::daemon::dispatch::backend::DaemonRuntimeQueries;
use crate::daemon::dispatch::{decode_params, encode_result, HandlerResult};
use crate::ipc::protocol::{InspectMachineRequest, RpcFamily, RpcMethod};
use serde_json::Value;

pub(super) async fn handle<B: DaemonRuntimeQueries>(
    method: RpcMethod,
    params: Value,
    dbus: &Option<B>,
) -> HandlerResult {
    debug_assert_eq!(method.family(), RpcFamily::Query);

    match method {
        RpcMethod::Ping => Ok(Value::Null),

        RpcMethod::AssessTarRuntime => {
            if params != serde_json::json!({}) {
                return Err("assess_tar_runtime does not accept parameters".into());
            }
            let assessment = inspect_tar_runtime()
                .await
                .map_err(|error| error.to_string())?;
            encode_result(assessment)
        }

        RpcMethod::SystemdToolsInspectMachine => {
            let inspection: InspectMachineRequest = decode_params(method, params)?;
            let properties = crate::adapters::runtime::systemd_tools::get_properties_with_runner(
                inspection.machine.as_str(),
                inspection.include_nspawn_unit,
                &crate::adapters::process::DefaultCommandRunner,
            )
            .await
            .map_err(|error| error.to_string())?;
            encode_result(properties)
        }

        RpcMethod::DbusGetUnitProperties | RpcMethod::SystemdToolsInspectUnit => {
            let machine = decode_params(method, params)?;
            let result = if method == RpcMethod::DbusGetUnitProperties {
                match dbus.as_ref() {
                    Some(dbus) => dbus
                        .get_unit_properties(&machine)
                        .await
                        .map_err(|error| error.to_string()),
                    None => Err("DBus not available".into()),
                }
            } else {
                crate::adapters::runtime::systemd_tools::get_unit_properties_with_runner(
                    &machine,
                    &crate::adapters::process::DefaultCommandRunner,
                )
                .await
                .map_err(|error| error.to_string())
            };
            encode_result(result?)
        }

        RpcMethod::DbusListMachines => {
            let dbus = dbus.as_ref().ok_or("DBus not available")?;
            let machines = dbus
                .list_machines()
                .await
                .map_err(|error| error.to_string())?;
            encode_result(machines)
        }

        RpcMethod::DbusListImages => {
            let dbus = dbus.as_ref().ok_or("DBus not available")?;
            let images = dbus
                .list_images()
                .await
                .map_err(|error| error.to_string())?;
            encode_result(images)
        }

        RpcMethod::DbusGetProperties => {
            let dbus = dbus.as_ref().ok_or("DBus not available")?;
            let inspection: InspectMachineRequest = decode_params(method, params)?;
            let properties = dbus
                .get_properties(inspection.machine.as_str(), inspection.include_nspawn_unit)
                .await
                .map_err(|error| error.to_string())?;
            encode_result(properties)
        }

        RpcMethod::DbusIsAvailable => match dbus {
            Some(dbus) => Ok(Value::Bool(dbus.is_available().await)),
            None => Ok(Value::Bool(false)),
        },

        _ => unreachable!("non-query method routed to query dispatcher"),
    }
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
                        | RpcMethod::SystemdToolsInspectMachine
                        | RpcMethod::SystemdToolsInspectUnit
                        | RpcMethod::DbusGetUnitProperties
                        | RpcMethod::DbusListMachines
                        | RpcMethod::DbusListImages
                        | RpcMethod::DbusGetProperties
                        | RpcMethod::DbusIsAvailable
                ));
            }
        }
    }
}
