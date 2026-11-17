use std::{future::Future, io};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub async fn copy(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
) -> io::Result<()> {
    tokio::io::copy(reader, writer).await?;
    writer.flush().await
}

pub async fn pipes<F>(
    stdout: &mut (impl AsyncRead + Unpin),
    stderr: &mut (impl AsyncRead + Unpin),
    host_stdout: &mut (impl AsyncWrite + Unpin),
    host_stderr: &mut (impl AsyncWrite + Unpin),
    close: impl Fn() -> F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    let forward = async {
        let result = copy(stdout, host_stdout).await;
        if result.is_err() {
            close().await;
        }
        result
    };
    let diagnostic = async {
        let result = copy(stderr, host_stderr).await;
        if result.is_err() {
            close().await;
        }
        result
    };
    // Request cleanup on either sink failure while the other stream keeps
    // draining. Waiting for both copies before closing can deadlock a writer.
    let (out, err) = tokio::join!(forward, diagnostic);
    out?;
    err?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/output.rs"]
mod tests;
