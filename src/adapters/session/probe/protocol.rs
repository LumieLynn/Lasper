use std::time::Duration;

use crate::application::sessions::{SessionError, TerminalSessionHandle};

const PROBE_DEADLINE: Duration = Duration::from_secs(5);
const MAX_PROBE_LINE_BYTES: usize = 1024;

struct CompleteFrame<'a> {
    body: &'a [u8],
    result: &'a [u8],
    end: usize,
}

pub(crate) struct ProbeProtocol {
    pub(crate) magic: &'static str,
    pub(crate) label: &'static str,
    pub(crate) maximum_output: usize,
}

impl ProbeProtocol {
    pub(crate) async fn collect(
        &self,
        handle: &mut TerminalSessionHandle,
    ) -> Result<Vec<u8>, SessionError> {
        let result = self.collect_output(handle).await;
        handle.close();
        result
    }

    async fn collect_output(
        &self,
        handle: &mut TerminalSessionHandle,
    ) -> Result<Vec<u8>, SessionError> {
        let mut output = handle
            .take_output()
            .ok_or_else(|| self.error("output is unavailable"))?;
        let deadline = tokio::time::Instant::now() + PROBE_DEADLINE;
        let mut bytes = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, output.recv()).await {
                Ok(Some(chunk)) => {
                    let remaining = self.maximum_output - bytes.len();
                    bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                    if let Some(frame) = self.frame(&bytes)? {
                        bytes.truncate(frame.end);
                        return Ok(bytes);
                    }
                }
                Ok(None) => return Ok(bytes),
                Err(_) => return Err(self.error("timed out")),
            }
        }
    }

    /// The first complete header starts the frame; its first RESULT line ends
    /// it. The limit applies through that boundary, independently of PTY chunk
    /// sizes. PAM/machinectl output after it is not part of the protocol.
    fn frame<'a>(&self, bytes: &'a [u8]) -> Result<Option<CompleteFrame<'a>>, SessionError> {
        let bytes = &bytes[..bytes.len().min(self.maximum_output)];
        let magic = self.magic.as_bytes();
        let header_end = bytes
            .windows(magic.len())
            .enumerate()
            .find_map(|(offset, window)| {
                if window != magic {
                    return None;
                }
                let end = offset + magic.len();
                if bytes[end..].starts_with(b"\r\n") {
                    Some(end + 2)
                } else if bytes[end..].starts_with(b"\n") {
                    Some(end + 1)
                } else {
                    None
                }
            });
        let Some(header_end) = header_end else {
            if bytes.len() == self.maximum_output {
                return Err(self.error("output exceeded its limit"));
            }
            return Ok(None);
        };
        let body = &bytes[header_end..];
        let mut offset = 0;
        while let Some(length) = body[offset..].iter().position(|byte| *byte == b'\n') {
            let line = &body[offset..offset + length];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.len() > MAX_PROBE_LINE_BYTES {
                return Err(self.error("line exceeded its limit"));
            }
            if let Some(result) = line.strip_prefix(b"RESULT=") {
                return Ok(Some(CompleteFrame {
                    body: &body[..offset],
                    result,
                    end: header_end + offset + length + 1,
                }));
            }
            offset += length + 1;
        }
        if body.len().saturating_sub(offset) > MAX_PROBE_LINE_BYTES {
            return Err(self.error("line exceeded its limit"));
        }
        if bytes.len() == self.maximum_output {
            return Err(self.error("output exceeded its limit"));
        }
        Ok(None)
    }

    pub(crate) fn parse<'a>(&'a self, bytes: &'a [u8]) -> Result<ProbeFrame<'a>, SessionError> {
        let Some(complete) = self.frame(bytes)? else {
            let reason = if bytes
                .windows(self.magic.len())
                .any(|window| window == self.magic.as_bytes())
            {
                "protocol frame has no complete RESULT"
            } else {
                "protocol marker is missing"
            };
            return Err(self.error(format!(
                "result was incomplete ({reason}; captured output: {})",
                escaped(bytes, 512, true),
            )));
        };
        let mut frame = ProbeFrame {
            protocol: self,
            fields: Vec::new(),
        };
        for raw_line in complete.body.split(|byte| *byte == b'\n') {
            let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if line.is_empty() {
                continue;
            }
            let Some(separator) = line.iter().position(|byte| *byte == b'=') else {
                return Err(self.error("returned a malformed field"));
            };
            let (key, value) = (&line[..separator], &line[separator + 1..]);
            if frame.fields.iter().any(|(existing, _)| *existing == key) {
                return Err(self.error(format!("repeated {}", escaped(key, 96, false))));
            }
            frame.fields.push((key, value));
        }
        match complete.result {
            b"READY" => Ok(frame),
            b"ERROR" => {
                let reason = frame.required(b"ERROR")?;
                frame.finish()?;
                Err(self.error(format!("failed: {}", escaped(reason, 512, false))))
            }
            _ => Err(self.error("returned an invalid RESULT")),
        }
    }

    fn error(&self, detail: impl std::fmt::Display) -> SessionError {
        SessionError::new(format!("{} {detail}", self.label))
    }
}

pub(crate) struct ProbeFrame<'a> {
    protocol: &'a ProbeProtocol,
    fields: Vec<(&'a [u8], &'a [u8])>,
}

impl<'a> ProbeFrame<'a> {
    fn take(&mut self, key: &[u8]) -> Option<&'a [u8]> {
        let index = self.fields.iter().position(|(field, _)| *field == key)?;
        Some(self.fields.swap_remove(index).1)
    }

    pub(crate) fn required(&mut self, key: &[u8]) -> Result<&'a [u8], SessionError> {
        self.take(key)
            .ok_or_else(|| self.error(format!("is missing {}", escaped(key, 96, false))))
    }

    pub(crate) fn u32(&mut self, key: &[u8]) -> Result<u32, SessionError> {
        u32::try_from(self.u64(key)?).map_err(|_| self.invalid(key))
    }

    pub(crate) fn u64(&mut self, key: &[u8]) -> Result<u64, SessionError> {
        self.optional_u64(key)?
            .ok_or_else(|| self.error(format!("is missing {}", escaped(key, 96, false))))
    }

    pub(crate) fn optional_u64(&mut self, key: &[u8]) -> Result<Option<u64>, SessionError> {
        let Some(value) = self.take(key) else {
            return Ok(None);
        };
        if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
            return Err(self.invalid(key));
        }
        std::str::from_utf8(value)
            .ok()
            .and_then(|value| value.parse().ok())
            .map(Some)
            .ok_or_else(|| self.invalid(key))
    }

    pub(crate) fn finish(self) -> Result<(), SessionError> {
        if let Some((key, _)) = self.fields.first() {
            return Err(self.error(format!(
                "returned an unknown field: {}",
                escaped(key, 96, false)
            )));
        }
        Ok(())
    }

    pub(crate) fn error(&self, detail: impl std::fmt::Display) -> SessionError {
        self.protocol.error(detail)
    }

    fn invalid(&self, key: &[u8]) -> SessionError {
        self.error(format!("returned an invalid {}", escaped(key, 96, false)))
    }
}

fn escaped(bytes: &[u8], maximum: usize, tail: bool) -> String {
    if bytes.is_empty() {
        return "<empty>".into();
    }
    let start = if tail {
        bytes.len().saturating_sub(maximum)
    } else {
        0
    };
    let mut result = String::new();
    if start > 0 {
        result.push_str("...");
    }
    for byte in bytes[start..].iter().take(maximum) {
        result.extend(std::ascii::escape_default(*byte).map(char::from));
    }
    if !tail && bytes.len() > maximum {
        result.push_str("...");
    }
    result
}
