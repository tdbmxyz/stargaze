//! Lightweight LAN lifecycle probes, independent of streaming sessions.
//!
//! Status uses TCP on the same numeric port as the UDP/QUIC stream. No
//! response means unreachable, not necessarily that the server is stopped.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

const MAGIC: &[u8; 18] = b"STARGAZE-STATUS/1\n";
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CLIENTS: usize = 32;

/// The lifecycle state advertised by a responding server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ServerStatus {
    /// The streaming pipeline is being initialized.
    Starting = 1,
    /// The streaming transport is ready (possibly already serving a client).
    Started = 2,
    /// Graceful pipeline shutdown is in progress.
    Stopping = 3,
}

impl ServerStatus {
    fn from_byte(byte: u8) -> io::Result<Self> {
        match byte {
            1 => Ok(Self::Starting),
            2 => Ok(Self::Started),
            3 => Ok(Self::Stopping),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown server status",
            )),
        }
    }
}

/// Probes a host without opening a streaming session.
///
/// DNS, connection, and the complete exchange share a one-second timeout.
///
/// # Errors
///
/// Returns an error for unreachable hosts, timeouts, or incompatible responses.
/// An error is not proof that the server process is stopped.
pub async fn probe(address: &str, port: u16) -> io::Result<ServerStatus> {
    probe_with_timeout(address, port, PROBE_TIMEOUT).await
}

async fn probe_with_timeout(
    address: &str,
    port: u16,
    timeout: Duration,
) -> io::Result<ServerStatus> {
    tokio::time::timeout(timeout, async {
        let mut stream = TcpStream::connect((address, port)).await?;
        stream.write_all(MAGIC).await?;
        let mut response = [0; MAGIC.len() + 1];
        stream.read_exact(&mut response).await?;
        if &response[..MAGIC.len()] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid status response",
            ));
        }
        ServerStatus::from_byte(response[MAGIC.len()])
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "status probe timed out"))?
}

/// A bounded lifecycle listener, stopped automatically when dropped.
///
/// It owns no capture, encoding, or streaming resources. Slow or malformed
/// clients cannot hold more than 32 slots, each with a one-second deadline.
pub struct StatusListener {
    state: watch::Sender<ServerStatus>,
    task: JoinHandle<()>,
    local_addr: SocketAddr,
}

impl StatusListener {
    /// Binds the TCP status port and initially advertises `Starting`.
    ///
    /// # Errors
    ///
    /// Returns an error if the address cannot be bound or inspected.
    pub async fn bind(address: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(address).await?;
        let local_addr = listener.local_addr()?;
        let (state, state_rx) = watch::channel(ServerStatus::Starting);
        let task = tokio::spawn(serve(listener, state_rx));
        Ok(Self {
            state,
            task,
            local_addr,
        })
    }

    /// Changes the advertised lifecycle state.
    pub fn set_status(&self, status: ServerStatus) {
        self.state.send_replace(status);
    }

    /// Returns the bound TCP address.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for StatusListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(listener: TcpListener, state: watch::Receiver<ServerStatus>) {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(client) => client,
                    Err(e) => {
                        tracing::warn!("Status listener stopped: {e}");
                        return;
                    }
                };
                if clients.len() < MAX_CLIENTS {
                    let state = state.clone();
                    clients.spawn(async move {
                        // Individual failures are expected (probes cancelled,
                        // unrelated port scans); do not spam lifecycle logs.
                        let _ = tokio::time::timeout(PROBE_TIMEOUT, reply(stream, state)).await;
                    });
                }
            }
            _ = clients.join_next(), if !clients.is_empty() => {}
        }
    }
}

async fn reply(mut stream: TcpStream, state: watch::Receiver<ServerStatus>) -> io::Result<()> {
    let mut request = [0; MAGIC.len()];
    stream.read_exact(&mut request).await?;
    if &request != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid status request",
        ));
    }
    let mut response = [0; MAGIC.len() + 1];
    response[..MAGIC.len()].copy_from_slice(MAGIC);
    response[MAGIC.len()] = *state.borrow() as u8;
    stream.write_all(&response).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_round_trips_and_supports_concurrent_probes() {
        let listener = StatusListener::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let port = listener.local_addr().port();
        for status in [
            ServerStatus::Starting,
            ServerStatus::Started,
            ServerStatus::Stopping,
        ] {
            listener.set_status(status);
            let (first, second) = tokio::join!(probe("127.0.0.1", port), probe("127.0.0.1", port));
            assert_eq!(first.unwrap(), status);
            assert_eq!(second.unwrap(), status);
        }
    }

    #[tokio::test]
    async fn malformed_requests_do_not_break_listener() {
        let listener = StatusListener::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let mut client = TcpStream::connect(listener.local_addr()).await.unwrap();
        client.write_all(&[0; MAGIC.len()]).await.unwrap();
        let mut response = [0; 1];
        assert_eq!(client.read(&mut response).await.unwrap(), 0);
        assert_eq!(
            probe("127.0.0.1", listener.local_addr().port())
                .await
                .unwrap(),
            ServerStatus::Starting
        );
    }

    #[tokio::test]
    async fn malformed_and_unknown_responses_are_rejected() {
        for response in [[0; MAGIC.len() + 1], {
            let mut response = [0; MAGIC.len() + 1];
            response[..MAGIC.len()].copy_from_slice(MAGIC);
            response[MAGIC.len()] = 255;
            response
        }] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; MAGIC.len()];
                stream.read_exact(&mut request).await.unwrap();
                stream.write_all(&response).await.unwrap();
            });
            assert_eq!(
                probe("127.0.0.1", port).await.unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn slow_client_does_not_block_other_probes_and_is_expired() {
        let listener = StatusListener::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let mut slow = TcpStream::connect(listener.local_addr()).await.unwrap();
        // An incomplete request must not monopolize the listener.
        slow.write_all(&MAGIC[..1]).await.unwrap();
        assert_eq!(
            probe("127.0.0.1", listener.local_addr().port())
                .await
                .unwrap(),
            ServerStatus::Starting
        );
        let mut response = [0; 1];
        let closed = tokio::time::timeout(Duration::from_secs(2), slow.read(&mut response))
            .await
            .unwrap();
        // Depending on the OS, closing with unread request bytes is EOF or reset.
        assert!(matches!(closed, Ok(0)) || closed.is_err());
    }

    #[tokio::test]
    async fn truncated_response_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; MAGIC.len()];
            stream.read_exact(&mut request).await.unwrap();
            stream.write_all(MAGIC).await.unwrap();
            // Close without the state byte.
        });
        assert_eq!(
            probe("127.0.0.1", port).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn silent_peer_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let error = probe_with_timeout("127.0.0.1", port, Duration::from_millis(30))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn dropping_listener_releases_port() {
        let listener = StatusListener::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let address = listener.local_addr();
        assert!(probe("127.0.0.1", address.port()).await.is_ok());
        drop(listener);
        // Task abort is asynchronous; wait until its socket is released.
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(rebound) = TcpListener::bind(address).await {
                    drop(rebound);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(probe("127.0.0.1", address.port()).await.is_err());
    }
}
