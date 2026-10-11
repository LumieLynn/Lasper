//! Best-effort failure reporting to the caller's desktop notification service.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use zbus::zvariant::Value;

const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_BODY_CHARACTERS: usize = 1024;

pub(crate) async fn notify_launch_failure(target: &str, error: &str) -> Result<()> {
    let body = notification_body(target, error);
    tokio::time::timeout(NOTIFICATION_TIMEOUT, async {
        let connection = zbus::Connection::session().await?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await?;
        send_notification(&proxy, &body).await?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("desktop notification timed out")?
}

async fn send_notification(proxy: &zbus::Proxy<'_>, body: &str) -> zbus::Result<()> {
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    let _: u32 = proxy
        .call(
            "Notify",
            &(
                "Lasper",
                0u32,
                "dialog-error",
                "Lasper launch failed",
                body,
                Vec::<&str>::new(),
                hints,
                -1i32,
            ),
        )
        .await?;
    Ok(())
}

fn notification_body(target: &str, error: &str) -> String {
    let text = format!("{target}\n{error}");
    let mut characters = text
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'));
    let mut body = String::new();
    for character in characters.by_ref().take(MAX_BODY_CHARACTERS) {
        match character {
            '&' => body.push_str("&amp;"),
            '<' => body.push_str("&lt;"),
            '>' => body.push_str("&gt;"),
            c => body.push(c),
        }
    }
    if characters.next().is_some() {
        body.push_str("...");
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_text_escapes_markup_and_removes_terminal_control_bytes() {
        assert_eq!(
            notification_body("Lumie@desktop", "denied <bind> & failed\r\0"),
            "Lumie@desktop\ndenied &lt;bind&gt; &amp; failed"
        );
    }

    #[test]
    fn notification_text_is_bounded_without_splitting_unicode_characters() {
        let body = notification_body("user@machine", &"错".repeat(MAX_BODY_CHARACTERS));
        assert_eq!(body.chars().count(), MAX_BODY_CHARACTERS + 3);
        assert!(body.ends_with("..."));
        assert!(!notification_body("user@machine", "short error").ends_with("..."));
    }

    struct Notifications {
        received: tokio::sync::mpsc::UnboundedSender<(String, String, String)>,
    }

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Notifications {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            app_name: &str,
            replaces_id: u32,
            app_icon: &str,
            summary: &str,
            body: &str,
            actions: Vec<String>,
            hints: HashMap<String, zbus::zvariant::OwnedValue>,
            expire_timeout: i32,
        ) -> u32 {
            assert_eq!(replaces_id, 0);
            assert_eq!(app_icon, "dialog-error");
            assert!(actions.is_empty());
            assert!(hints.is_empty());
            assert_eq!(expire_timeout, -1);
            self.received
                .send((app_name.into(), summary.into(), body.into()))
                .unwrap();
            1
        }
    }

    #[tokio::test]
    #[ignore = "requires dbus-daemon; uses an isolated session bus"]
    async fn notify_uses_the_desktop_protocol_and_preserves_the_failure_text() {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let mut bus = tokio::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut address = String::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address),
        )
        .await
        .unwrap()
        .unwrap();
        let address = address.trim();
        let (received, mut messages) = tokio::sync::mpsc::unbounded_channel();
        let _server = zbus::connection::Builder::address(address)
            .unwrap()
            .name("org.freedesktop.Notifications")
            .unwrap()
            .serve_at("/org/freedesktop/Notifications", Notifications { received })
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address)
            .unwrap()
            .build()
            .await
            .unwrap();
        let proxy = zbus::Proxy::new(
            &client,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await
        .unwrap();
        let body = notification_body("user@desktop", "authorization denied");
        tokio::time::timeout(NOTIFICATION_TIMEOUT, send_notification(&proxy, &body))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            messages.try_recv().unwrap(),
            ("Lasper".into(), "Lasper launch failed".into(), body)
        );
        bus.kill().await.unwrap();
        bus.wait().await.unwrap();
    }
}
