use std::collections::{HashMap, HashSet, VecDeque};
use tokio::sync::mpsc;

pub struct LogBuffer {
    pub lines: VecDeque<String>,
    pub dirty: bool,
    pub stream: Option<tokio::task::JoinHandle<()>>,
    pub log_rx: Option<mpsc::UnboundedReceiver<String>>,
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

    /// Create a channel for `name` and return the sender half.
    /// The caller passes this sender to `spawn_log_stream`.
    pub fn start_stream(&mut self, name: &str) -> Option<mpsc::UnboundedSender<String>> {
        let buf = self.buffers.get_mut(name)?;
        if buf
            .stream
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
        {
            return None; // already running
        }
        let (tx, rx) = mpsc::unbounded_channel();
        buf.log_rx = Some(rx);
        Some(tx)
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
                had
            }
            None => false,
        }
    }

    /// Drain all pending lines from per-buffer receivers into their buffers.
    /// Call once per frame, before rendering.
    pub fn drain_all(&mut self) {
        for buf in self.buffers.values_mut() {
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
