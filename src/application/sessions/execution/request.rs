use super::{GuestCommand, GuestEnvironment, GuestTarget};

/// An account's login shell and an explicit executable have different argv
/// semantics. Neither request describes how the guest process is created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GuestProcess {
    LoginShell,
    Command(GuestCommand),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GuestExecutionRequest {
    target: GuestTarget,
    environment: GuestEnvironment,
    process: GuestProcess,
}

impl GuestExecutionRequest {
    pub(crate) fn login_shell(target: GuestTarget, environment: GuestEnvironment) -> Self {
        Self {
            target,
            environment,
            process: GuestProcess::LoginShell,
        }
    }

    pub(crate) fn command(
        target: GuestTarget,
        environment: GuestEnvironment,
        command: GuestCommand,
    ) -> Self {
        Self {
            target,
            environment,
            process: GuestProcess::Command(command),
        }
    }

    pub(crate) fn target(&self) -> &GuestTarget {
        &self.target
    }

    pub(crate) fn environment(&self) -> &GuestEnvironment {
        &self.environment
    }

    pub(crate) fn process(&self) -> &GuestProcess {
        &self.process
    }
}
