//! Translation from host/transport failures into lifecycle outcomes.

use crate::adapters::error::{is_permission_dbus_error_name, NspawnError};
use crate::adapters::system_operation::SystemOperationError;
use crate::application::image_lifecycle::{ImageControlOutcome, ImageRemovalRejection};
use crate::application::machine_lifecycle::{MachineControlOutcome, MachineRejection};

/// Map the typed system-operation adapter error at the image lifecycle
/// boundary.  The adapter reports source evidence; only this layer chooses
/// whether that evidence is a rejection, a not-attempted command, or an
/// unknown side effect.
pub(crate) fn map_system_operation_image_error(error: SystemOperationError) -> ImageControlOutcome {
    let reason = error.to_string();
    match error {
        SystemOperationError::InvalidTarget(_) => ImageControlOutcome::Rejected {
            rejection: ImageRemovalRejection::InvalidTarget,
            reason,
        },
        SystemOperationError::ProtectedImage(_) => ImageControlOutcome::Rejected {
            rejection: ImageRemovalRejection::Protected,
            reason,
        },
        SystemOperationError::PermissionDenied => ImageControlOutcome::Rejected {
            rejection: ImageRemovalRejection::PermissionDenied,
            reason,
        },
        SystemOperationError::Io { .. } => ImageControlOutcome::NotAttempted { reason },
        SystemOperationError::OutcomeUnknown(_) => ImageControlOutcome::OutcomeUnknown { reason },
        SystemOperationError::CommandFailed { .. } | SystemOperationError::Backend(_) => {
            ImageControlOutcome::Failed { reason }
        }
        SystemOperationError::Dbus(error) => map_image_dbus_error(error, reason),
    }
}

/// Map a typed system-operation failure for a machine lifecycle action.
pub(crate) fn map_system_operation_machine_error(
    error: SystemOperationError,
) -> MachineControlOutcome {
    let reason = error.to_string();
    match error {
        SystemOperationError::InvalidTarget(_) => MachineControlOutcome::Rejected {
            rejection: MachineRejection::InvalidTarget,
            reason,
        },
        SystemOperationError::PermissionDenied => MachineControlOutcome::Rejected {
            rejection: MachineRejection::PermissionDenied,
            reason,
        },
        SystemOperationError::Io { .. } => MachineControlOutcome::NotAttempted { reason },
        SystemOperationError::OutcomeUnknown(_) => MachineControlOutcome::OutcomeUnknown { reason },
        SystemOperationError::CommandFailed { .. } | SystemOperationError::Backend(_) => {
            MachineControlOutcome::Failed { reason }
        }
        SystemOperationError::ProtectedImage(_) => MachineControlOutcome::Rejected {
            rejection: MachineRejection::InvalidTarget,
            reason,
        },
        SystemOperationError::Dbus(error) => map_machine_dbus_error(error, reason),
    }
}

pub(crate) fn map_image_control_error(error: NspawnError) -> ImageControlOutcome {
    let reason = error.to_string();
    match error {
        NspawnError::ProtectedImage(_) => ImageControlOutcome::Rejected {
            rejection: ImageRemovalRejection::Protected,
            reason,
        },
        NspawnError::Validation(_) => ImageControlOutcome::Rejected {
            rejection: ImageRemovalRejection::InvalidTarget,
            reason,
        },
        NspawnError::ContainerAlreadyRunning(_) => ImageControlOutcome::Rejected {
            rejection: ImageRemovalRejection::AlreadyRunning,
            reason,
        },
        NspawnError::PermissionDenied => ImageControlOutcome::Rejected {
            rejection: ImageRemovalRejection::PermissionDenied,
            reason,
        },
        NspawnError::ContainerNotFound(_) => ImageControlOutcome::Rejected {
            rejection: ImageRemovalRejection::NotFound,
            reason,
        },
        NspawnError::Dbus(zbus::Error::MethodError(name, detail, _)) => {
            match classify_image_method_error(name.as_str()) {
                Some(rejection) => ImageControlOutcome::Rejected {
                    rejection,
                    reason: detail.unwrap_or(reason),
                },
                None => ImageControlOutcome::Failed { reason },
            }
        }
        NspawnError::Io(_, _)
        | NspawnError::GenericIo(_)
        | NspawnError::Dbus(_)
        | NspawnError::SystemOperationOutcomeUnknown(_) => {
            ImageControlOutcome::OutcomeUnknown { reason }
        }
        _ => ImageControlOutcome::Failed { reason },
    }
}

fn map_image_dbus_error(error: zbus::Error, reason: String) -> ImageControlOutcome {
    match error {
        zbus::Error::MethodError(name, detail, _) => {
            match classify_image_method_error(name.as_str()) {
                Some(rejection) => ImageControlOutcome::Rejected {
                    rejection,
                    reason: detail.unwrap_or(reason),
                },
                None => ImageControlOutcome::OutcomeUnknown { reason },
            }
        }
        zbus::Error::FDO(error) if matches!(error.as_ref(), zbus::fdo::Error::AccessDenied(_)) => {
            ImageControlOutcome::Rejected {
                rejection: ImageRemovalRejection::PermissionDenied,
                reason,
            }
        }
        _ => ImageControlOutcome::OutcomeUnknown { reason },
    }
}

fn map_machine_dbus_error(error: zbus::Error, reason: String) -> MachineControlOutcome {
    match error {
        zbus::Error::MethodError(name, detail, _) => {
            match classify_machine_method_error(name.as_str()) {
                Some(rejection) => MachineControlOutcome::Rejected {
                    rejection,
                    reason: detail.unwrap_or(reason),
                },
                None => MachineControlOutcome::OutcomeUnknown { reason },
            }
        }
        zbus::Error::FDO(error) if matches!(error.as_ref(), zbus::fdo::Error::AccessDenied(_)) => {
            MachineControlOutcome::Rejected {
                rejection: MachineRejection::PermissionDenied,
                reason,
            }
        }
        _ => MachineControlOutcome::OutcomeUnknown { reason },
    }
}

fn classify_image_method_error(name: &str) -> Option<ImageRemovalRejection> {
    match name {
        "org.freedesktop.machine1.NoSuchImage" | "System.Error.ENOENT" => {
            Some(ImageRemovalRejection::NotFound)
        }
        "System.Error.EBUSY" => Some(ImageRemovalRejection::Busy),
        name if is_permission_dbus_error_name(name) => {
            Some(ImageRemovalRejection::PermissionDenied)
        }
        name if is_invalid_argument_method_error(name) => {
            Some(ImageRemovalRejection::InvalidTarget)
        }
        _ => None,
    }
}

fn classify_machine_method_error(name: &str) -> Option<MachineRejection> {
    match name {
        "org.freedesktop.machine1.NoSuchMachine" | "System.Error.ENOENT" => {
            Some(MachineRejection::NotFound)
        }
        name if is_permission_dbus_error_name(name) => Some(MachineRejection::PermissionDenied),
        name if is_invalid_argument_method_error(name) => Some(MachineRejection::InvalidTarget),
        _ => None,
    }
}

fn is_invalid_argument_method_error(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.InvalidArgs" | "System.Error.EINVAL"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_method_errors_keep_semantic_rejections() {
        for (name, expected) in [
            (
                "org.freedesktop.machine1.NoSuchImage",
                ImageRemovalRejection::NotFound,
            ),
            ("System.Error.EBUSY", ImageRemovalRejection::Busy),
            (
                "org.freedesktop.DBus.Error.AccessDenied",
                ImageRemovalRejection::PermissionDenied,
            ),
            (
                "org.freedesktop.DBus.Error.InvalidArgs",
                ImageRemovalRejection::InvalidTarget,
            ),
        ] {
            assert_eq!(classify_image_method_error(name), Some(expected));
        }
        assert_eq!(
            classify_image_method_error("org.freedesktop.machine1.UnexpectedFailure"),
            None
        );
    }

    #[test]
    fn machine_method_errors_keep_semantic_rejections() {
        for (name, expected) in [
            (
                "org.freedesktop.machine1.NoSuchMachine",
                MachineRejection::NotFound,
            ),
            (
                "org.freedesktop.DBus.Error.AccessDenied",
                MachineRejection::PermissionDenied,
            ),
            (
                "org.freedesktop.DBus.Error.InvalidArgs",
                MachineRejection::InvalidTarget,
            ),
        ] {
            assert_eq!(classify_machine_method_error(name), Some(expected));
        }
        assert_eq!(
            classify_machine_method_error("org.freedesktop.machine1.UnexpectedFailure"),
            None
        );
    }

    #[test]
    fn typed_native_errors_map_without_transport_knowledge_in_services() {
        assert!(matches!(
            map_image_control_error(NspawnError::ContainerAlreadyRunning("test".into())),
            ImageControlOutcome::Rejected {
                rejection: ImageRemovalRejection::AlreadyRunning,
                ..
            }
        ));
        assert!(matches!(
            map_image_control_error(NspawnError::ProtectedImage(".host".into())),
            ImageControlOutcome::Rejected {
                rejection: ImageRemovalRejection::Protected,
                ..
            }
        ));
    }

    #[test]
    fn validation_messages_do_not_select_semantic_rejections() {
        for reason in ["busy target", "host image from an invalid source"] {
            assert!(matches!(
                map_image_control_error(NspawnError::Validation(reason.into())),
                ImageControlOutcome::Rejected {
                    rejection: ImageRemovalRejection::InvalidTarget,
                    ..
                }
            ));
        }
    }

    #[test]
    fn typed_command_output_does_not_turn_text_into_busy_rejection() {
        let error = SystemOperationError::CommandFailed {
            context: "machinectl".into(),
            command: "machinectl remove image".into(),
            output: "image is busy".into(),
        };
        assert!(matches!(
            map_system_operation_image_error(error),
            ImageControlOutcome::Failed { .. }
        ));
    }

    #[test]
    fn mutation_timeout_is_never_treated_as_not_attempted() {
        let error = SystemOperationError::OutcomeUnknown("deadline exceeded".into());
        assert!(matches!(
            map_system_operation_machine_error(error),
            MachineControlOutcome::OutcomeUnknown { .. }
        ));

        let error = SystemOperationError::OutcomeUnknown("deadline exceeded".into());
        assert!(matches!(
            map_system_operation_image_error(error),
            ImageControlOutcome::OutcomeUnknown { .. }
        ));
    }

    #[test]
    fn typed_dbus_name_selects_busy_rejection() {
        let error_name = zbus::names::ErrorName::try_from("System.Error.EBUSY")
            .expect("test error name")
            .to_owned()
            .into();
        let message = zbus::Message::method("/test", "Failure")
            .expect("test method message")
            .build(&())
            .expect("test message body");
        let error = SystemOperationError::Dbus(zbus::Error::MethodError(
            error_name,
            Some("resource is busy".into()),
            message,
        ));
        assert!(matches!(
            map_system_operation_image_error(error),
            ImageControlOutcome::Rejected {
                rejection: ImageRemovalRejection::Busy,
                ..
            }
        ));
    }
}
