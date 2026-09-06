//! Embedded selected-user shell prompt and terminal bridge.

use crate::application::sessions::{
    InteractiveShellEnvironment, SessionService, ShellOpenError, ShellOpenIntent, ShellTarget,
    TerminalCommand, TerminalSessionEndpoint, TerminalSessionHandle, ValidatedGuestUserName,
    WaylandShellRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionLifecycle, SessionSize};
use std::sync::Arc;

const WAYLAND_FALLBACK_NOTICE: &str = "🪐 Continuing without Wayland...";
const USER_MAX_BYTES: usize = 32;

pub(super) async fn run_builtin_shell_prompt(
    endpoint: TerminalSessionEndpoint,
    service: Arc<SessionService>,
    machine: MachineName,
    initial_size: SessionSize,
) {
    let TerminalSessionEndpoint {
        mut commands,
        output,
        lifecycle,
        close,
        ..
    } = endpoint;
    let mut close = close;
    let prompt = format!("lasper shell {machine} user: ");
    if send_output(&output, prompt.clone().into_bytes())
        .await
        .is_err()
    {
        let _ = lifecycle.send(SessionLifecycle::Closed);
        return;
    }

    let mut line = Vec::new();
    let mut size = initial_size;
    let mut root_confirmation = None;
    loop {
        tokio::select! {
            _ = &mut close => {
                let _ = lifecycle.send(SessionLifecycle::Closed);
                return;
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    let _ = lifecycle.send(SessionLifecycle::Closed);
                    return;
                };
                match command {
                    TerminalCommand::Resize(next) => size = next,
                    TerminalCommand::Reply(_) => {}
                    TerminalCommand::Input(bytes) => {
                        for byte in bytes {
                            match byte {
                                b'\r' | b'\n' => {
                                    if line.is_empty() {
                                        let _ = send_output(&output, b"\r\n".to_vec()).await;
                                        let _ = send_output(&output, prompt.as_bytes().to_vec()).await;
                                        continue;
                                    }
                                    let value = String::from_utf8_lossy(&line).into_owned();
                                    line.clear();
                                    let user = match ValidatedGuestUserName::new(value) {
                                        Ok(user) => user,
                                        Err(error) => {
                                            root_confirmation = None;
                                            let message = format!("\r\nlasper: invalid guest user: {error}\r\n{prompt}");
                                            let _ = send_output(&output, message.into_bytes()).await;
                                            continue;
                                        }
                                    };
                                    if matches!(user.as_str(), "root" | "0")
                                        && root_confirmation.as_ref() != Some(&user)
                                    {
                                        root_confirmation = Some(user);
                                        let message = format!(
                                            "\r\nRoot grants full control inside this guest. Enter the account again to confirm.\r\n{prompt}"
                                        );
                                        let _ = send_output(&output, message.into_bytes()).await;
                                        continue;
                                    }
                                    root_confirmation = None;
                                    let _ = send_output(&output, b"\r\n".to_vec()).await;
                                    match open_shell(&service, machine.clone(), user, size).await {
                                        Ok((mut remote, fallback)) => {
                                            if fallback {
                                                let _ = send_output(&output, format!("{WAYLAND_FALLBACK_NOTICE}\r\n").into_bytes()).await;
                                            }
                                            match bridge_shell(
                                                &mut commands,
                                                &mut close,
                                                &output,
                                                &mut remote,
                                            ).await {
                                                BridgeResult::Closed => {
                                                    let _ = lifecycle.send(SessionLifecycle::Closed);
                                                    return;
                                                }
                                                BridgeResult::Finished(state) => {
                                                    let message = match state {
                                                        SessionLifecycle::Exited { .. } | SessionLifecycle::Closed => format!("\r\n{prompt}"),
                                                        SessionLifecycle::Failed(error) => format!("\r\nlasper: {error}\r\n{prompt}"),
                                                        SessionLifecycle::Running => prompt.clone(),
                                                    };
                                                    let _ = send_output(&output, message.into_bytes()).await;
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            let message = format!("lasper: {error}\r\n{prompt}");
                                            let _ = send_output(&output, message.into_bytes()).await;
                                        }
                                    }
                                }
                                0x04 if line.is_empty() => {
                                    let _ = lifecycle.send(SessionLifecycle::Closed);
                                    return;
                                }
                                0x03 => {
                                    line.clear();
                                    root_confirmation = None;
                                    let _ = send_output(&output, format!("^C\r\n{prompt}").into_bytes()).await;
                                }
                                0x08 | 0x7f if !line.is_empty() => {
                                    line.pop();
                                    let _ = send_output(&output, b"\x08 \x08".to_vec()).await;
                                }
                                byte if byte.is_ascii_graphic() && line.len() < USER_MAX_BYTES => {
                                    line.push(byte);
                                    let _ = send_output(&output, vec![byte]).await;
                                }
                                byte if byte.is_ascii_graphic() => {
                                    let _ = send_output(&output, b"\x07".to_vec()).await;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn open_shell(
    service: &SessionService,
    machine: MachineName,
    user: ValidatedGuestUserName,
    size: SessionSize,
) -> Result<(TerminalSessionHandle, bool), String> {
    let (wayland, selection_failure) = match service.automatic_wayland(&machine).await {
        Ok(wayland) => (wayland, None),
        Err(error) => (WaylandShellRequest::Disabled, Some(error)),
    };
    let intent = ShellOpenIntent::new(
        ShellTarget::new(machine, user),
        wayland.clone(),
        InteractiveShellEnvironment::embedded(),
        size,
    );
    match service.open_shell(intent.clone()).await {
        Ok(handle) => Ok((handle, selection_failure.is_some())),
        Err(ShellOpenError::WaylandPreparation(error)) if wayland.host_socket().is_some() => {
            service
                .open_shell(intent.with_wayland(WaylandShellRequest::Disabled))
                .await
                .map(|handle| (handle, true))
                .map_err(|fallback| {
                    format!(
                    "Wayland validation failed: {error}; terminal-only fallback failed: {fallback}"
                )
                })
        }
        Err(error) => match selection_failure {
            Some(selection) => Err(format!(
                "Wayland selection failed: {selection}; terminal-only fallback failed: {error}"
            )),
            None => Err(error.to_string()),
        },
    }
}

enum BridgeResult {
    Closed,
    Finished(SessionLifecycle),
}

async fn bridge_shell(
    commands: &mut tokio::sync::mpsc::Receiver<TerminalCommand>,
    close: &mut tokio::sync::oneshot::Receiver<()>,
    output: &tokio::sync::mpsc::Sender<Vec<u8>>,
    remote: &mut TerminalSessionHandle,
) -> BridgeResult {
    let Some(mut remote_output) = remote.take_output() else {
        return BridgeResult::Finished(SessionLifecycle::Failed(
            "selected-user shell output is unavailable".into(),
        ));
    };
    let remote_input = remote.input();
    let mut remote_wait = Box::pin(remote.wait());
    let mut output_open = true;
    let mut finished = None;
    loop {
        tokio::select! {
            _ = &mut *close => return BridgeResult::Closed,
            state = &mut remote_wait, if finished.is_none() => {
                finished = Some(state);
                if !output_open {
                    if let Some(state) = finished.take() {
                        return BridgeResult::Finished(state);
                    }
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { return BridgeResult::Closed; };
                if finished.is_none() {
                    match command {
                        TerminalCommand::Input(bytes) => { let _ = remote_input.send_input(bytes).await; }
                        TerminalCommand::Reply(bytes) => { let _ = remote_input.send_reply(bytes).await; }
                        TerminalCommand::Resize(size) => { let _ = remote_input.try_resize(size); }
                    }
                }
            }
            chunk = remote_output.recv(), if output_open => {
                match chunk {
                    Some(chunk) => {
                        tokio::select! {
                            result = output.send(chunk) => {
                                if result.is_err() {
                                    return BridgeResult::Closed;
                                }
                            }
                            _ = &mut *close => return BridgeResult::Closed,
                        }
                    }
                    None => {
                        output_open = false;
                        if let Some(state) = finished.take() {
                            return BridgeResult::Finished(state);
                        }
                    }
                }
            }
        }
    }
}

async fn send_output(
    output: &tokio::sync::mpsc::Sender<Vec<u8>>,
    bytes: Vec<u8>,
) -> Result<(), ()> {
    output.send(bytes).await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::session::{DirectSessionAdapter, DirectTerminalPolicy};
    use crate::application::sessions::{terminal_session_channel, SessionSendStatus};
    use crate::domain::session::{SessionId, TerminalAttachmentKind};

    fn prompt_service() -> Arc<SessionService> {
        Arc::new(SessionService::new(Arc::new(DirectSessionAdapter::new(
            DirectTerminalPolicy::LoginOnly,
            crate::adapters::session::MachineSessionTransport::Cli,
            crate::adapters::config::NspawnConfigStore::direct(),
        ))))
    }

    #[tokio::test]
    async fn prompt_is_rendered_before_opening_a_guest_session() {
        let (mut handle, endpoint) =
            terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login);
        let mut output = handle.take_output().unwrap();
        let task = tokio::spawn(run_builtin_shell_prompt(
            endpoint,
            prompt_service(),
            MachineName::new("demo").unwrap(),
            SessionSize::new(80, 24).unwrap(),
        ));

        let first = tokio::time::timeout(std::time::Duration::from_secs(1), output.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&first).contains("lasper shell demo user:"));

        handle.close();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn prompt_rejects_invalid_guest_names_without_opening() {
        let (mut handle, endpoint) =
            terminal_session_channel(SessionId::new(1).unwrap(), TerminalAttachmentKind::Login);
        let input = handle.input();
        let mut output = handle.take_output().unwrap();
        let task = tokio::spawn(run_builtin_shell_prompt(
            endpoint,
            prompt_service(),
            MachineName::new("demo").unwrap(),
            SessionSize::new(80, 24).unwrap(),
        ));

        let _ = output.recv().await.unwrap();
        assert_eq!(
            input.try_input(b"../root\r".to_vec()),
            SessionSendStatus::Queued
        );
        let mut response = String::new();
        while !response.contains("invalid guest user") {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(1), output.recv())
                .await
                .unwrap()
                .unwrap();
            response.push_str(&String::from_utf8_lossy(&chunk));
        }

        handle.close();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }
}
