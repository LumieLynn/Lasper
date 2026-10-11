//! Source-preserving view of one nspawn settings document.
//!
//! Configuration pages share this layer when they need physical source
//! locations or systemd-style logical lines. It deliberately does not assign
//! ownership or interpret page-specific settings.

use std::path::PathBuf;

use crate::adapters::config::nspawn_file::parse_nspawn_bind_fields;

pub(super) struct NspawnDocument<'a> {
    content: &'a str,
    lines: Vec<PhysicalLine<'a>>,
}

impl<'a> NspawnDocument<'a> {
    pub(super) fn new(content: &'a str) -> Self {
        Self {
            content,
            lines: physical_lines(content),
        }
    }

    pub(super) const fn content(&self) -> &'a str {
        self.content
    }

    pub(super) fn lines(&self) -> &[PhysicalLine<'a>] {
        &self.lines
    }

    pub(super) fn logical_lines(&self, mut visit: impl FnMut(&str, usize)) {
        let mut logical = String::new();
        let mut first_line = 1;
        // Match conf-parser.c: ignore comment lines even within a
        // continuation and replace an unescaped final backslash with a space.
        for (index, line) in self
            .content
            .strip_prefix('\u{feff}')
            .unwrap_or(self.content)
            .lines()
            .enumerate()
        {
            if line.trim_start().starts_with(['#', ';']) {
                continue;
            }
            if logical.is_empty() {
                first_line = index + 1;
            }
            logical.push_str(line);
            if has_continuation(line) {
                logical.pop();
                logical.push(' ');
                continue;
            }
            visit(&logical, first_line);
            logical.clear();
        }
        if !logical.is_empty() {
            visit(&logical, first_line);
        }
    }

    pub(super) fn bind_destinations(&self) -> Vec<(usize, PathBuf)> {
        let mut in_files = false;
        let mut destinations = Vec::new();
        self.logical_lines(|line, number| {
            let line = line.trim();
            if line.starts_with('[') {
                in_files = line.eq_ignore_ascii_case("[Files]");
                return;
            }
            if !in_files {
                return;
            }
            let Some((key, value)) = line.split_once('=') else {
                return;
            };
            if !matches!(key.trim(), "Bind" | "BindReadOnly") {
                return;
            }
            let Some(fields) = parse_nspawn_bind_fields(value.trim()) else {
                return;
            };
            if fields.is_empty() || fields.len() > 3 || fields[0].is_empty() {
                return;
            }
            let destination = fields
                .get(1)
                .filter(|destination| !destination.is_empty())
                .unwrap_or(&fields[0]);
            destinations.push((number, destination.into()));
        });
        destinations
    }

    pub(super) fn preferred_line_ending(&self) -> &'static str {
        match self.content.find('\n') {
            Some(index) if self.content.as_bytes().get(index.wrapping_sub(1)) == Some(&b'\r') => {
                "\r\n"
            }
            _ => "\n",
        }
    }
}

pub(super) struct PhysicalLine<'a> {
    number: usize,
    start: usize,
    end: usize,
    body: &'a str,
    ending: &'a str,
}

impl PhysicalLine<'_> {
    pub(super) const fn number(&self) -> usize {
        self.number
    }

    pub(super) const fn body(&self) -> &str {
        self.body
    }

    pub(super) const fn ending(&self) -> &str {
        self.ending
    }

    pub(super) const fn start(&self) -> usize {
        self.start
    }

    pub(super) const fn end(&self) -> usize {
        self.end
    }

    pub(super) fn has_continuation(&self) -> bool {
        has_continuation(self.body)
    }
}

fn physical_lines(content: &str) -> Vec<PhysicalLine<'_>> {
    let mut offset = 0;
    content
        .split_inclusive('\n')
        .enumerate()
        .map(|(index, raw)| {
            let (body, ending) = if let Some(body) = raw.strip_suffix("\r\n") {
                (body, "\r\n")
            } else if let Some(body) = raw.strip_suffix('\n') {
                (body, "\n")
            } else {
                (raw, "")
            };
            let line = PhysicalLine {
                number: index + 1,
                start: offset,
                end: offset + raw.len(),
                body,
                ending,
            };
            offset += raw.len();
            line
        })
        .collect()
}

fn has_continuation(line: &str) -> bool {
    line.bytes().rev().take_while(|byte| *byte == b'\\').count() % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_physical_lines_while_visiting_systemd_logical_lines() {
        let source = "\u{feff}[Files]\r\nBind=/one:\\\r\n# ignored\r\n /two\r\n";
        let document = NspawnDocument::new(source);
        assert_eq!(document.lines().len(), 4);
        assert_eq!(document.lines()[1].body(), "Bind=/one:\\");
        assert_eq!(document.lines()[1].ending(), "\r\n");
        assert!(document.lines()[1].has_continuation());
        assert_eq!(document.preferred_line_ending(), "\r\n");

        let mut logical = Vec::new();
        document.logical_lines(|line, number| logical.push((number, line.to_owned())));
        assert_eq!(
            logical,
            [(1, "[Files]".into()), (2, "Bind=/one:  /two".into())]
        );
    }

    #[test]
    fn bind_destinations_follow_files_sections_and_empty_targets() {
        let document = NspawnDocument::new(
            "[Exec]\nBind=/ignored\n[Files]\nBind=/one\nBindReadOnly=/two:/target:idmap\nBind=/three::idmap\n",
        );
        assert_eq!(
            document.bind_destinations(),
            [
                (4, PathBuf::from("/one")),
                (5, PathBuf::from("/target")),
                (6, PathBuf::from("/three")),
            ]
        );
    }
}
