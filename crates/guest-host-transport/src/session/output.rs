use lasper_guest_protocol::{ExitFrame, ExitReport, OutputStream, PayloadCodecError};

#[derive(Debug)]
pub struct CapturedStream {
    bytes: Vec<u8>,
    truncated: bool,
    complete: bool,
}

impl CapturedStream {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn complete(&self) -> bool {
        self.complete
    }
}

#[derive(Debug)]
pub struct SessionOutput {
    stdout: CapturedStream,
    stderr: CapturedStream,
}

impl SessionOutput {
    pub fn stdout(&self) -> &CapturedStream {
        &self.stdout
    }

    pub fn stderr(&self) -> &CapturedStream {
        &self.stderr
    }
}

#[derive(Debug)]
pub struct SessionExit {
    report: ExitReport,
    output: Option<SessionOutput>,
}

impl SessionExit {
    pub fn report(&self) -> ExitReport {
        self.report
    }

    pub fn output(&self) -> Option<&SessionOutput> {
        self.output.as_ref()
    }

    pub(super) fn decode(frame: ExitFrame<'_>) -> Result<Self, PayloadCodecError> {
        let output = frame
            .output()
            .map(|output| {
                Ok(SessionOutput {
                    stdout: capture(output.stdout())?,
                    stderr: capture(output.stderr())?,
                })
            })
            .transpose()?;
        Ok(Self {
            report: frame.report(),
            output,
        })
    }
}

fn capture(stream: &OutputStream<'_>) -> Result<CapturedStream, PayloadCodecError> {
    let mut bytes = vec![0; stream.len()];
    stream.write_into(&mut bytes)?;
    Ok(CapturedStream {
        bytes,
        truncated: stream.truncated(),
        complete: stream.complete(),
    })
}
