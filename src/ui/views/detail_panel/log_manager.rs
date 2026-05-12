use ratatui::text::Line;
use std::collections::{HashSet, VecDeque};

pub struct LogBuffer {
    pub container_name: String,
    pub lines: VecDeque<Line<'static>>,
    pub offset_index: Vec<usize>,
    pub wrapped_height: usize,
    pub dirty: bool,
    pub stream: Option<tokio::task::JoinHandle<()>>,
}

pub struct LogManager {
    pub buffers: Vec<LogBuffer>,
    pub max_lines: usize,
}

impl LogManager {
    pub fn new(max_lines: usize) -> Self {
        Self {
            buffers: Vec::new(),
            max_lines: if max_lines == 0 { 5000 } else { max_lines },
        }
    }

    pub fn active_buffer(&self) -> Option<&LogBuffer> {
        if self.buffers.is_empty() {
            None
        } else {
            Some(&self.buffers[0])
        }
    }

    pub fn active_buffer_mut(&mut self) -> Option<&mut LogBuffer> {
        if self.buffers.is_empty() {
            None
        } else {
            Some(&mut self.buffers[0])
        }
    }

    pub fn get_or_create(&mut self, name: &str) -> &mut LogBuffer {
        if let Some(idx) = self.buffers.iter().position(|b| b.container_name == name) {
            if idx != 0 {
                // Abort the old active buffer's stream before switching
                if let Some(handle) = self.buffers[0].stream.take() {
                    handle.abort();
                }
                let mut buf = self.buffers.remove(idx);
                buf.dirty = true; // force sync_data_lengths to recalc panel.logs_len
                self.buffers.insert(0, buf);
            }
            &mut self.buffers[0]
        } else {
            // Abort the old active buffer's stream before switching to a new one
            if let Some(first) = self.buffers.first_mut() {
                if let Some(handle) = first.stream.take() {
                    handle.abort();
                }
            }
            let cap = self.max_lines;
            self.buffers.insert(
                0,
                LogBuffer {
                    container_name: name.to_string(),
                    lines: VecDeque::with_capacity(cap),
                    offset_index: Vec::with_capacity(cap),
                    wrapped_height: 0,
                    dirty: true,
                    stream: None,
                },
            );
            &mut self.buffers[0]
        }
    }

    pub fn remove_stale(&mut self, active_names: &HashSet<String>) {
        let mut i = 0;
        while i < self.buffers.len() {
            if !active_names.contains(&self.buffers[i].container_name) {
                let mut buf = self.buffers.remove(i);
                if let Some(handle) = buf.stream.take() {
                    handle.abort();
                }
            } else {
                i += 1;
            }
        }
    }

    pub fn cleanup_all(&mut self) {
        for buf in &mut self.buffers {
            if let Some(handle) = buf.stream.take() {
                handle.abort();
            }
        }
        self.buffers.clear();
    }
}
