//! Native login/shell creation and the login machine-removal subscription.

use crate::adapters::error::{NspawnError, Result};
use crate::application::sessions::execution::{GuestExecutionRequest, GuestProcess};
use crate::domain::machine::MachineName;
use std::os::fd::OwnedFd;
use thiserror::Error;

use super::DbusBackend;

#[derive(Debug, Error)]
pub(crate) enum GuestSessionOpenError {
    #[error("guest session was not submitted: {0}")]
    NotAttempted(NspawnError),
    #[error("guest session was rejected: {0}")]
    Rejected(NspawnError),
    #[error("guest session submission outcome is unknown: {0}")]
    OutcomeUnknown(NspawnError),
}

impl GuestSessionOpenError {
    fn from_submitted(error: NspawnError) -> Self {
        match &error {
            NspawnError::Dbus(zbus::Error::MethodError(name, ..))
                if matches!(
                    name.as_str(),
                    "org.freedesktop.DBus.Error.AccessDenied"
                        | "org.freedesktop.DBus.Error.AuthFailed"
                        | "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired"
                        | "org.freedesktop.DBus.Error.InvalidArgs"
                        | "org.freedesktop.DBus.Error.UnknownMethod"
                        | "org.freedesktop.machine1.NoSuchMachine"
                ) =>
            {
                Self::Rejected(error)
            }
            NspawnError::Dbus(zbus::Error::FDO(fdo))
                if matches!(fdo.as_ref(), zbus::fdo::Error::AccessDenied(_)) =>
            {
                Self::Rejected(error)
            }
            // A general error can follow process creation (for example while
            // constructing the PTY reply). It is not proof of rejection.
            _ => Self::OutcomeUnknown(error),
        }
    }
}

fn guest_process_arguments(request: &GuestExecutionRequest) -> (&str, Vec<String>) {
    match request.process() {
        GuestProcess::LoginShell => ("", Vec::new()),
        GuestProcess::Command(command) => (command.program(), command.argv()),
    }
}

impl DbusBackend {
    pub(crate) async fn open_machine_login(&self, machine: &MachineName) -> Result<OwnedFd> {
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let (fd, _path) = self
            .mutation_with_deadline(
                generation,
                "OpenMachineLogin",
                proxy.open_machine_login(machine.as_str()),
            )
            .await?;
        Ok(fd.into())
    }

    /// Execute the selected guest process through machine1. Session ownership
    /// and display preparation belong to the session adapter.
    pub(crate) async fn open_guest_session(
        &self,
        request: &GuestExecutionRequest,
        deadline: tokio::time::Instant,
    ) -> std::result::Result<OwnedFd, GuestSessionOpenError> {
        let (path, args) = guest_process_arguments(request);
        let proxy = tokio::time::timeout_at(deadline, self.manager_proxy()).await;
        let (generation, proxy) = proxy.ok().flatten().ok_or_else(|| {
            GuestSessionOpenError::NotAttempted(NspawnError::Dbus(zbus::Error::Failure(
                "machine1 connection unavailable before guest session submission".into(),
            )))
        })?;
        if tokio::time::Instant::now() >= deadline {
            return Err(GuestSessionOpenError::NotAttempted(
                NspawnError::Validation(
                    "guest execution deadline expired before submission".into(),
                ),
            ));
        }
        let (fd, _path) = self
            .mutation_until(
                generation,
                "OpenMachineShell",
                deadline,
                proxy.open_machine_shell(
                    request.target().machine().as_str(),
                    request.target().user().as_str(),
                    path,
                    args,
                    request.environment().assignments(),
                ),
            )
            .await
            .map_err(GuestSessionOpenError::from_submitted)?;
        Ok(fd.into())
    }

    /// Subscribe before opening the PTY so an intervening machine removal
    /// cannot leave a login forwarder waiting for a getty forever.
    pub(crate) async fn watch_login_machine(
        &self,
        machine: &MachineName,
    ) -> Result<tokio::sync::oneshot::Receiver<crate::domain::session::SessionLifecycle>> {
        use crate::domain::session::SessionLifecycle;
        use futures_util::StreamExt;
        let (generation, proxy) = self
            .manager_proxy()
            .await
            .ok_or_else(|| NspawnError::Dbus(zbus::Error::Failure("No connection".into())))?;
        let mut removed = self
            .query_with_deadline(
                generation,
                "login machine removal subscription",
                proxy.receive_machine_removed(),
            )
            .await?;
        let machine = machine.clone();
        let (mut tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => return,
                    event = removed.next() => match event {
                        Some(event) => {
                            if event.args().is_ok_and(|args| args.machine() == machine.as_str()) {
                                let _ = tx.send(SessionLifecycle::Exited { success: true, code: None });
                                return;
                            }
                        }
                        None => {
                            let _ = tx.send(SessionLifecycle::Failed("machine removal signal stream closed".into()));
                            return;
                        }
                    }
                }
            }
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::ValidatedGuestUserName;

    #[test]
    fn explicit_method_rejection_and_missing_reply_have_different_outcomes() {
        for (name, rejected) in [
            ("org.freedesktop.DBus.Error.AccessDenied", true),
            ("org.freedesktop.machine1.NoSuchMachine", true),
            ("org.freedesktop.DBus.Error.Failed", false),
            ("org.freedesktop.DBus.Error.NoReply", false),
            ("org.freedesktop.DBus.Error.Timeout", false),
            ("org.freedesktop.DBus.Error.Disconnected", false),
        ] {
            let error = NspawnError::Dbus(zbus::Error::MethodError(
                name.try_into().unwrap(),
                Some("test".into()),
                zbus::Message::method_call("/test", "Test")
                    .unwrap()
                    .build(&())
                    .unwrap(),
            ));
            assert_eq!(
                matches!(
                    GuestSessionOpenError::from_submitted(error),
                    GuestSessionOpenError::Rejected(_)
                ),
                rejected,
                "{name}"
            );
        }
    }

    #[test]
    fn machine1_session_bodies_match_the_upstream_xml_signatures() {
        let shell = zbus::Message::method_call("/org/freedesktop/machine1", "OpenMachineShell")
            .unwrap()
            .build(&(
                "demo",
                "alice",
                "",
                Vec::<String>::new(),
                vec!["WAYLAND_DISPLAY=/run/lasper/wayland/1000/wayland-0"],
            ))
            .unwrap();
        assert_eq!(shell.body().signature().to_string_no_parens(), "sssasas");

        let login = zbus::Message::method_call("/org/freedesktop/machine1", "OpenMachineLogin")
            .unwrap()
            .build(&("demo",))
            .unwrap();
        assert_eq!(login.body().signature().to_string_no_parens(), "s");
    }

    #[test]
    fn selected_user_command_maps_to_dbus_path_and_complete_argv() {
        use crate::application::sessions::execution::GuestEnvironment;
        use crate::application::sessions::GuestTarget;
        let target = GuestTarget::new(
            MachineName::new("demo").unwrap(),
            ValidatedGuestUserName::new("alice").unwrap(),
        );
        let environment = GuestEnvironment::shell(Default::default(), None, None).unwrap();
        let request = GuestExecutionRequest::command(
            target.clone(),
            environment.clone(),
            crate::application::sessions::GuestCommand::new(
                "/usr/bin/kitty",
                vec!["--class".into(), "a b".into()],
            )
            .unwrap(),
        );

        let (path, args) = guest_process_arguments(&request);
        assert_eq!(path, "/usr/bin/kitty");
        assert_eq!(args, ["/usr/bin/kitty", "--class", "a b"]);

        let default_shell = GuestExecutionRequest::login_shell(target, environment);
        assert_eq!(guest_process_arguments(&default_shell), ("", Vec::new()));
    }
}
