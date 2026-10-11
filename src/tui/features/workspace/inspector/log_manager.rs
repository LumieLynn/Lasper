use crate::application::sessions::JournalSessionHandle;
use crate::domain::session::SessionLifecycle;
use std::collections::{HashMap, HashSet, VecDeque};

pub struct LogBuffer {
    pub lines: VecDeque<String>,
    pub dirty: bool,
    pub stream: Option<JournalSessionHandle>,
    pub stream_failed: bool,
}

pub struct LogManager {
    buffers: HashMap<String, LogBuffer>,
    max_lines: usize,
}

impl LogManager {
    pub fn new(max_lines: usize) -> Self {
        Self {
            buffers: HashMap::new(),
            max_lines: if max_lines == 0 { 5000 } else { max_lines },
        }
    }

    pub(crate) fn buffer_mut(&mut self, name: &str) -> Option<&mut LogBuffer> {
        self.buffers.get_mut(name)
    }

    pub fn get_or_create(&mut self, name: &str) -> &mut LogBuffer {
        let capacity = self.max_lines;
        self.buffers
            .entry(name.to_string())
            .or_insert_with(|| LogBuffer {
                lines: VecDeque::with_capacity(capacity),
                dirty: true,
                stream: None,
                stream_failed: false,
            })
    }

    pub fn stream_is_active(&self, name: &str) -> bool {
        self.buffers
            .get(name)
            .and_then(|buffer| buffer.stream.as_ref())
            .is_some_and(|session| session.lifecycle().is_running())
    }

    pub fn can_start_stream(&self, name: &str) -> bool {
        self.buffers
            .get(name)
            .is_some_and(|buffer| !buffer.stream_failed && !self.stream_is_active(name))
    }

    pub fn attach_stream(&mut self, name: &str, stream: JournalSessionHandle) {
        if let Some(buffer) = self.buffers.get_mut(name) {
            log::debug!("attached journal session {} for {name}", stream.id().get());
            buffer.stream = Some(stream);
            buffer.stream_failed = false;
        }
    }

    pub fn mark_stream_failed(&mut self, name: &str) {
        if let Some(buffer) = self.buffers.get_mut(name) {
            buffer.stream_failed = true;
        }
    }

    pub fn stop_stream(&mut self, name: &str) -> bool {
        let Some(buffer) = self.buffers.get_mut(name) else {
            return false;
        };
        let Some(mut stream) = buffer.stream.take() else {
            return false;
        };
        log::debug!("closing journal session {} for {name}", stream.id().get());
        stream.close();
        buffer.stream_failed = false;
        true
    }

    pub fn drain_all(&mut self) {
        for buffer in self.buffers.values_mut() {
            let Some(stream) = &mut buffer.stream else {
                continue;
            };
            let mut changed = false;
            while let Ok(line) = stream.try_recv() {
                buffer.lines.push_back(line);
                changed = true;
            }

            let failure = match stream.lifecycle() {
                SessionLifecycle::Failed(message) => Some(message),
                SessionLifecycle::Exited {
                    success: false,
                    code,
                } => Some(match code {
                    Some(code) => format!("Log stream exited with status {code}"),
                    None => "Log stream exited unsuccessfully".to_string(),
                }),
                _ => None,
            };
            if let Some(message) = failure {
                buffer.stream_failed = true;
                if buffer.lines.back().map(String::as_str) != Some(message.as_str()) {
                    buffer.lines.push_back(message);
                    changed = true;
                }
            }
            if changed {
                trim_lines(&mut buffer.lines, self.max_lines);
                buffer.dirty = true;
            }
        }
    }

    pub fn push_line(&mut self, name: &str, text: impl Into<String>) {
        if let Some(buffer) = self.buffers.get_mut(name) {
            buffer.lines.push_back(text.into());
            trim_lines(&mut buffer.lines, self.max_lines);
            buffer.dirty = true;
        }
    }

    pub fn remove_stale(&mut self, active_names: &HashSet<String>) {
        self.buffers.retain(|name, buffer| {
            if active_names.contains(name) {
                return true;
            }
            if let Some(mut stream) = buffer.stream.take() {
                stream.close();
            }
            false
        });
    }

    pub fn cleanup_all(&mut self) {
        for buffer in self.buffers.values_mut() {
            if let Some(mut stream) = buffer.stream.take() {
                stream.close();
            }
        }
        self.buffers.clear();
    }
}

fn trim_lines(lines: &mut VecDeque<String>, maximum: usize) {
    while lines.len() > maximum {
        lines.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::sessions::{journal_session_channel, JOURNAL_OUTPUT_CAPACITY};
    use crate::domain::session::SessionId;
    use tokio::sync::mpsc::error::TrySendError;

    #[test]
    fn log_stream_channel_is_bounded() {
        let mut manager = LogManager::new(5000);
        manager.get_or_create("machine");
        let (handle, endpoint) = journal_session_channel(SessionId::new(1).unwrap());
        manager.attach_stream("machine", handle);

        for _ in 0..JOURNAL_OUTPUT_CAPACITY {
            endpoint
                .output
                .try_send("line".to_string())
                .expect("channel capacity");
        }
        assert!(matches!(
            endpoint.output.try_send("overflow".to_string()),
            Err(TrySendError::Full(_))
        ));
    }

    #[test]
    fn draining_log_stream_preserves_line_cap() {
        let mut manager = LogManager::new(2);
        manager.get_or_create("machine");
        let (handle, endpoint) = journal_session_channel(SessionId::new(1).unwrap());
        manager.attach_stream("machine", handle);
        for line in ["one", "two", "three"] {
            endpoint.output.try_send(line.to_string()).unwrap();
        }

        manager.drain_all();
        let buffer = manager.buffers.get("machine").expect("buffer");
        assert_eq!(buffer.lines.len(), 2);
        assert_eq!(buffer.lines.front().map(String::as_str), Some("two"));
        assert_eq!(buffer.lines.back().map(String::as_str), Some("three"));
    }

    #[test]
    fn stopping_a_stream_requests_session_close() {
        let mut manager = LogManager::new(10);
        manager.get_or_create("machine");
        let (handle, mut endpoint) = journal_session_channel(SessionId::new(1).unwrap());
        manager.attach_stream("machine", handle);

        assert!(manager.stop_stream("machine"));
        assert!(endpoint.close.try_recv().is_ok());
    }
}
