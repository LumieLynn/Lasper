//! Retention for the deployment observer; full diagnostics belong to the job.

use crate::tui::foundation::widgets::display::text::{TextEntryIndex, WrappedEntries};
use std::collections::VecDeque;

const MAX_ENTRIES: usize = 5000;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 16 * 1024;

#[derive(Default)]
pub(super) struct DeploymentLogs {
    lines: VecDeque<String>,
    bytes: usize,
    first: usize,
    index: TextEntryIndex,
}

impl DeploymentLogs {
    pub(super) fn push(&mut self, mut line: String) {
        if line.len() > MAX_ENTRY_BYTES {
            const NOTICE: &str = " [truncated in view]";
            line.truncate(line.floor_char_boundary(MAX_ENTRY_BYTES - NOTICE.len()));
            line.push_str(NOTICE);
            line.shrink_to_fit();
        }
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.lines.len() > MAX_ENTRIES || self.bytes > MAX_BYTES {
            if let Some(line) = self.lines.pop_front() {
                self.bytes -= line.len();
                self.first += 1;
            }
        }
    }

    pub(super) fn text(&mut self) -> WrappedEntries<'_> {
        WrappedEntries::new(&self.lines, self.first, &mut self.index)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.lines.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_retains_bounded_entries_and_truncates_at_utf8_boundaries() {
        let mut logs = DeploymentLogs::default();
        for _ in 0..MAX_ENTRIES + 2 {
            logs.push("entry".into());
        }
        assert_eq!(logs.lines.len(), MAX_ENTRIES);
        assert_eq!(logs.first, 2);
        for _ in 0..500 {
            logs.push("中".repeat(MAX_ENTRY_BYTES));
        }
        assert!(logs.bytes <= MAX_BYTES);
        assert!(logs.lines.iter().all(|line| line.len() <= MAX_ENTRY_BYTES));
        assert!(logs.lines.back().unwrap().ends_with(" [truncated in view]"));
        assert!(logs.lines.back().unwrap().capacity() <= MAX_ENTRY_BYTES);
    }
}
