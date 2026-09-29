//! The same locked editor execution is used directly and inside the daemon.

use crate::adapters::config::editor::{edit, inspection};
use crate::adapters::config::settings_lock;
use crate::adapters::error::NspawnError;
use crate::application::configuration::ConfigurationError;
use crate::domain::machine::MachineName;
use crate::ipc::protocol::configuration::{
    ConfigurationOperation, ConfigurationResult, ConfigurationValue,
};

pub(crate) async fn execute(operation: ConfigurationOperation) -> ConfigurationResult {
    match operation {
        ConfigurationOperation::Inspect(target) => inspection::inspect(target)
            .await
            .map(|snapshot| ConfigurationValue::Inspection(Box::new(snapshot)))
            .map_err(map_host_error),
        ConfigurationOperation::Preview(request) => edit::preview(*request)
            .await
            .map(ConfigurationValue::Preview)
            .map_err(map_host_error),
        ConfigurationOperation::Apply(request) => {
            let _lock = match MachineName::new(request.target.name()) {
                Ok(machine) => Some(
                    settings_lock::acquire(machine)
                        .await
                        .map_err(map_host_error)?,
                ),
                Err(_) => None,
            };
            edit::apply(*request)
                .await
                .map(ConfigurationValue::Apply)
                .map_err(map_host_error)
        }
    }
}

fn map_host_error(error: NspawnError) -> ConfigurationError {
    match error {
        NspawnError::PermissionDenied => {
            ConfigurationError::PermissionDenied("root privileges required".into())
        }
        NspawnError::Io(_, ref io) | NspawnError::GenericIo(ref io)
            if io.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            ConfigurationError::PermissionDenied(error.to_string())
        }
        NspawnError::Validation(message) | NspawnError::InvalidConfig(message) => {
            ConfigurationError::InvalidInput(message)
        }
        error => ConfigurationError::failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_failures_keep_permission_validation_and_io_distinct() {
        for error in [
            NspawnError::PermissionDenied,
            NspawnError::Io(
                "/etc/systemd/nspawn/test.nspawn".into(),
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            ),
            NspawnError::GenericIo(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        ] {
            assert!(matches!(
                map_host_error(error),
                ConfigurationError::PermissionDenied(_)
            ));
        }
        assert!(matches!(
            map_host_error(NspawnError::Validation("bad target".into())),
            ConfigurationError::InvalidInput(_)
        ));
        assert!(matches!(
            map_host_error(NspawnError::Io(
                "file".into(),
                std::io::Error::from(std::io::ErrorKind::NotFound)
            )),
            ConfigurationError::Failed(_)
        ));
    }
}
