//! Translation from host/transport failures into lifecycle outcomes.

use crate::application::image_lifecycle::{ImageControlOutcome, ImageRemovalRejection};
use crate::application::machine_lifecycle::{MachineControlOutcome, MachineRejection};
use crate::nspawn::errors::NspawnError;

pub(crate) fn map_image_control_error(error: NspawnError) -> ImageControlOutcome {
    let reason = error.to_string();
    match error {
        NspawnError::Validation(_) => ImageControlOutcome::Rejected {
            rejection: if reason.contains("host image") {
                ImageRemovalRejection::Protected
            } else if reason.to_ascii_lowercase().contains("busy") {
                ImageRemovalRejection::Busy
            } else {
                ImageRemovalRejection::InvalidTarget
            },
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
        NspawnError::Io(_, _) | NspawnError::GenericIo(_) | NspawnError::Dbus(_) => {
            ImageControlOutcome::OutcomeUnknown { reason }
        }
        _ => ImageControlOutcome::Failed { reason },
    }
}

pub(crate) fn map_machine_control_error(error: NspawnError) -> MachineControlOutcome {
    let reason = error.to_string();
    match error {
        NspawnError::Validation(_) | NspawnError::InvalidConfig(_) => {
            MachineControlOutcome::Rejected {
                rejection: MachineRejection::InvalidTarget,
                reason,
            }
        }
        NspawnError::ContainerNotFound(_) => MachineControlOutcome::Rejected {
            rejection: MachineRejection::NotFound,
            reason,
        },
        NspawnError::ContainerAlreadyRunning(_) => MachineControlOutcome::Rejected {
            rejection: MachineRejection::AlreadyRunning,
            reason,
        },
        NspawnError::ContainerNotRunning(_) => MachineControlOutcome::Rejected {
            rejection: MachineRejection::NotRunning,
            reason,
        },
        NspawnError::PermissionDenied => MachineControlOutcome::Rejected {
            rejection: MachineRejection::PermissionDenied,
            reason,
        },
        NspawnError::Dbus(zbus::Error::MethodError(name, detail, _)) => {
            match classify_machine_method_error(name.as_str()) {
                Some(rejection) => MachineControlOutcome::Rejected {
                    rejection,
                    reason: detail.unwrap_or(reason),
                },
                None => MachineControlOutcome::Failed { reason },
            }
        }
        NspawnError::Io(_, _) | NspawnError::GenericIo(_) | NspawnError::Dbus(_) => {
            MachineControlOutcome::OutcomeUnknown { reason }
        }
        _ => MachineControlOutcome::Failed { reason },
    }
}

fn classify_image_method_error(name: &str) -> Option<ImageRemovalRejection> {
    match name {
        "org.freedesktop.machine1.NoSuchImage" | "System.Error.ENOENT" => {
            Some(ImageRemovalRejection::NotFound)
        }
        "System.Error.EBUSY" => Some(ImageRemovalRejection::Busy),
        name if is_permission_method_error(name) => Some(ImageRemovalRejection::PermissionDenied),
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
        name if is_permission_method_error(name) => Some(MachineRejection::PermissionDenied),
        name if is_invalid_argument_method_error(name) => Some(MachineRejection::InvalidTarget),
        _ => None,
    }
}

fn is_permission_method_error(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.DBus.Error.AccessDenied"
            | "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired"
            | "org.freedesktop.PolicyKit1.Error.NotAuthorized"
            | "org.freedesktop.PolicyKit1.Error.AuthorizationFailed"
            | "org.freedesktop.PolicyKit1.Error.Failed"
            | "System.Error.EACCES"
            | "System.Error.EPERM"
    )
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
            map_machine_control_error(NspawnError::ContainerNotRunning("test".into())),
            MachineControlOutcome::Rejected {
                rejection: MachineRejection::NotRunning,
                ..
            }
        ));
    }
}
