use super::{
    JournalSessionHandle, JournalSessionRequest, SessionError, SessionPort, TerminalSessionHandle,
    TerminalSessionRequest,
};
use crate::domain::machine::MachineName;
use crate::domain::session::{SessionId, SessionSize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct SessionService {
    port: Arc<dyn SessionPort>,
    next_id: AtomicU64,
}

impl SessionService {
    pub fn new(port: Arc<dyn SessionPort>) -> Self {
        Self {
            port,
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn open_terminal(
        &self,
        machine: MachineName,
        size: SessionSize,
    ) -> Result<TerminalSessionHandle, SessionError> {
        self.port
            .open_terminal(TerminalSessionRequest {
                id: self.allocate_id(),
                machine,
                size,
            })
            .await
    }

    pub async fn open_journal(
        &self,
        machine: MachineName,
    ) -> Result<JournalSessionHandle, SessionError> {
        self.port
            .open_journal(JournalSessionRequest {
                id: self.allocate_id(),
                machine,
            })
            .await
    }

    fn allocate_id(&self) -> SessionId {
        loop {
            let value = self.next_id.fetch_add(1, Ordering::Relaxed);
            if let Ok(id) = SessionId::new(value) {
                return id;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{
        journal_session_channel, terminal_session_channel, JournalSessionRequest, SessionPort,
        TerminalSessionRequest,
    };
    use crate::domain::session::TerminalAttachmentKind;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct RecordingPort {
        ids: Mutex<Vec<SessionId>>,
    }

    #[async_trait::async_trait]
    impl SessionPort for RecordingPort {
        async fn open_terminal(
            &self,
            request: TerminalSessionRequest,
        ) -> Result<TerminalSessionHandle, SessionError> {
            self.ids.lock().push(request.id);
            Ok(terminal_session_channel(request.id, TerminalAttachmentKind::Login).0)
        }

        async fn open_journal(
            &self,
            request: JournalSessionRequest,
        ) -> Result<JournalSessionHandle, SessionError> {
            self.ids.lock().push(request.id);
            Ok(journal_session_channel(request.id).0)
        }
    }

    #[tokio::test]
    async fn service_assigns_distinct_ids_across_session_kinds() {
        let port = Arc::new(RecordingPort::default());
        let service = SessionService::new(port.clone());
        let machine = MachineName::new("test").unwrap();
        let _terminal = service
            .open_terminal(machine.clone(), SessionSize::new(80, 24).unwrap())
            .await
            .unwrap();
        let _journal = service.open_journal(machine).await.unwrap();

        let ids = port.ids.lock();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }
}
