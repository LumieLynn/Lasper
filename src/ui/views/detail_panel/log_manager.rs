use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

const LOG_STREAM_CHANNEL_CAPACITY: usize = 1024;

pub struct LogBuffer {
    pub lines: VecDeque<String>,
    pub dirty: bool,
    pub stream: Option<tokio::task::JoinHandle<()>>,
    pub log_rx: Option<mpsc::Receiver<String>>,
    /// Set by the spawned log-stream task when it hits a non-recoverable
    /// error (e.g. permission denied).  Checked by [`LogManager::start_stream`]
    /// to avoid restart loops.
    pub fatal_flag: Option<Arc<AtomicBool>>,
    pub stream_failed: bool,
}

pub struct LogManager {
    pub buffers: HashMap<String, LogBuffer>,
    pub active_name: Option<String>,
    pub max_lines: usize,
}

impl LogManager {
    pub fn new(max_lines: usize) -> Self {
        Self {
            buffers: HashMap::new(),
            active_name: None,
            max_lines: if max_lines == 0 { 5000 } else { max_lines },
        }
    }

    pub fn active_buffer(&self) -> Option<&LogBuffer> {
        self.active_name
            .as_ref()
            .and_then(|name| self.buffers.get(name))
    }

    pub fn active_buffer_mut(&mut self) -> Option<&mut LogBuffer> {
        self.active_name
            .as_ref()
            .and_then(|name| self.buffers.get_mut(name))
    }

    /// Ensure a buffer exists for `name`, mark it as the active one.
    /// Sets dirty when switching to a different buffer so the render cache
    /// is recomputed for the new active container.
    pub fn get_or_create(&mut self, name: &str) -> &mut LogBuffer {
        let switched = self.active_name.as_deref() != Some(name);
        self.active_name = Some(name.to_string());
        let cap = self.max_lines;
        let buf = self
            .buffers
            .entry(name.to_string())
            .or_insert_with(|| LogBuffer {
                lines: VecDeque::with_capacity(cap),
                dirty: true,
                stream: None,
                log_rx: None,
                fatal_flag: None,
                stream_failed: false,
            });
        if switched {
            buf.dirty = true;
        }
        buf
    }

    // stream management

    /// Returns true if there is a running log stream for `name`.
    pub fn stream_is_active(&self, name: &str) -> bool {
        self.buffers
            .get(name)
            .and_then(|b| b.stream.as_ref())
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    /// Create a channel for `name` and return the sender half together with
    /// a fatal flag.  The spawned log-stream task sets the flag on
    /// non-recoverable errors so we avoid restart loops.
    pub fn start_stream(&mut self, name: &str) -> Option<(mpsc::Sender<String>, Arc<AtomicBool>)> {
        let buf = self.buffers.get_mut(name)?;
        if buf
            .stream
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
        {
            return None; // already running
        }
        if buf.stream_failed {
            return None; // previous attempt failed permanently
        }
        buf.stream_failed = false;
        // A full channel applies backpressure to journalctl instead of
        // allowing a log burst to grow memory without bound.
        let (tx, rx) = mpsc::channel(LOG_STREAM_CHANNEL_CAPACITY);
        let fatal = Arc::new(AtomicBool::new(false));
        buf.fatal_flag = Some(Arc::clone(&fatal));
        buf.log_rx = Some(rx);
        Some((tx, fatal))
    }

    /// Store the JoinHandle returned by `spawn_log_stream`.
    pub fn attach_stream_handle(&mut self, name: &str, handle: tokio::task::JoinHandle<()>) {
        if let Some(buf) = self.buffers.get_mut(name) {
            buf.stream = Some(handle);
        }
    }

    /// Stop the log stream for `name`. Returns true if there was one.
    pub fn stop_stream(&mut self, name: &str) -> bool {
        match self.buffers.get_mut(name) {
            Some(buf) => {
                let had = buf.stream.is_some();
                if let Some(h) = buf.stream.take() {
                    h.abort();
                }
                buf.log_rx = None;
                buf.fatal_flag = None;
                buf.stream_failed = false;
                had
            }
            None => false,
        }
    }

    /// Drain all pending lines from per-buffer receivers into their buffers.
    /// Call once per frame, before rendering.
    pub fn drain_all(&mut self) {
        for buf in self.buffers.values_mut() {
            // Detect fatal stream failures signalled by the spawned task.
            if !buf.stream_failed {
                if let Some(ref flag) = buf.fatal_flag {
                    if flag.load(Ordering::Relaxed) {
                        buf.stream_failed = true;
                    }
                }
            }

            let Some(rx) = &mut buf.log_rx else { continue };
            let mut changed = false;
            while let Ok(line) = rx.try_recv() {
                buf.lines.push_back(line);
                changed = true;
            }
            if changed {
                while buf.lines.len() > self.max_lines {
                    buf.lines.pop_front();
                }
                buf.dirty = true;
            }
        }
    }

    /// Append a synthetic line (e.g. "[CONTAINER STOPPED]") to a buffer.
    pub fn push_line(&mut self, name: &str, text: impl Into<String>) {
        if let Some(buf) = self.buffers.get_mut(name) {
            buf.lines.push_back(text.into());
            while buf.lines.len() > self.max_lines {
                buf.lines.pop_front();
            }
            buf.dirty = true;
        }
    }

    // lifecycle

    pub fn remove_stale(&mut self, active_names: &HashSet<String>) {
        self.buffers.retain(|name, buf| {
            if !active_names.contains(name) {
                if let Some(handle) = buf.stream.take() {
                    handle.abort();
                }
                false
            } else {
                true
            }
        });
        if let Some(ref active) = self.active_name {
            if !self.buffers.contains_key(active) {
                self.active_name = self.buffers.keys().next().cloned();
            }
        }
    }

    pub fn cleanup_all(&mut self) {
        for buf in self.buffers.values_mut() {
            if let Some(handle) = buf.stream.take() {
                handle.abort();
            }
        }
        self.buffers.clear();
        self.active_name = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{LogManager, LOG_STREAM_CHANNEL_CAPACITY};
    use tokio::sync::mpsc::error::TrySendError;

    #[test]
    fn log_stream_channel_is_bounded() {
        let mut manager = LogManager::new(5000);
        manager.get_or_create("machine");
        let (tx, _) = manager.start_stream("machine").expect("stream channel");

        for _ in 0..LOG_STREAM_CHANNEL_CAPACITY {
            tx.try_send("line".to_string()).expect("channel capacity");
        }
        assert!(matches!(
            tx.try_send("overflow".to_string()),
            Err(TrySendError::Full(_))
        ));
    }

    #[test]
    fn draining_log_stream_preserves_line_cap() {
        let mut manager = LogManager::new(2);
        manager.get_or_create("machine");
        let (tx, _) = manager.start_stream("machine").expect("stream channel");
        for line in ["one", "two", "three"] {
            tx.try_send(line.to_string()).expect("channel capacity");
        }

        manager.drain_all();
        let buffer = manager.active_buffer().expect("buffer");
        assert_eq!(buffer.lines.len(), 2);
        assert_eq!(buffer.lines.front().map(String::as_str), Some("two"));
        assert_eq!(buffer.lines.back().map(String::as_str), Some("three"));
    }
}
