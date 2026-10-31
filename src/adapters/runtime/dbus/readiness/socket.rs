//! zbus transport after the connector sent the credential-bearing NUL byte.
//! All D-Bus authentication and messages still use zbus's implementation.

use std::io;
use std::os::fd::BorrowedFd;
use zbus::connection::socket::{Socket, Split, WriteHalf};

pub(super) struct GuestBusSocket(pub(super) tokio::net::UnixStream);

impl Socket for GuestBusSocket {
    type ReadHalf = tokio::net::unix::OwnedReadHalf;
    type WriteHalf = GuestBusWriter;

    fn split(self) -> Split<Self::ReadHalf, Self::WriteHalf> {
        let (read, write) = self.0.into_split();
        Split::new(
            read,
            GuestBusWriter {
                write,
                first_write: true,
            },
        )
    }
}

#[derive(Debug)]
pub(super) struct GuestBusWriter {
    write: tokio::net::unix::OwnedWriteHalf,
    first_write: bool,
}

#[async_trait::async_trait]
impl WriteHalf for GuestBusWriter {
    async fn sendmsg(&mut self, buffer: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
        let skipped = usize::from(self.first_write);
        if self.first_write && buffer.first() != Some(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected initial D-Bus NUL",
            ));
        }
        // Keep first_write unchanged if the underlying send is cancelled or
        // fails, so a retry cannot put a duplicate NUL on the authenticated stream.
        let remaining = &buffer[skipped..];
        let written = if remaining.is_empty() {
            0
        } else {
            let written = self.write.sendmsg(remaining, fds).await?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "guest bus write returned zero",
                ));
            }
            written
        };
        self.first_write = false;
        Ok(written + skipped)
    }

    async fn close(&mut self) -> io::Result<()> {
        WriteHalf::close(&mut self.write).await
    }

    fn can_pass_unix_fd(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn suppresses_only_the_connector_supplied_initial_nul() {
        let (client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let (_, mut writer) = GuestBusSocket(client).split().take();
        let authentication = b"\0AUTH EXTERNAL 30\r\n";
        assert_eq!(
            writer.sendmsg(authentication, &[]).await.unwrap(),
            authentication.len()
        );
        assert_eq!(writer.sendmsg(b"BEGIN\r\n", &[]).await.unwrap(), 7);
        let mut data = [0u8; 25];
        server.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"AUTH EXTERNAL 30\r\nBEGIN\r\n");
    }

    #[tokio::test]
    async fn accepts_an_initial_nul_written_as_a_separate_authentication_step() {
        let (client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let (_, mut writer) = GuestBusSocket(client).split().take();
        assert_eq!(writer.sendmsg(&[0], &[]).await.unwrap(), 1);
        assert_eq!(writer.sendmsg(b"AUTH\r\n", &[]).await.unwrap(), 6);
        let mut bytes = [0; 6];
        server.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"AUTH\r\n");
    }
}
