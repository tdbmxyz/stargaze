//! Network transport module — client side.
//!
//! Provides [`connect()`] which establishes a `QUIC` connection to the server,
//! performs session handshake, and starts receiving video frames.

pub(crate) mod quic;
pub mod receiver;

use stargaze_core::config::{ClientConfig, Codec};
use stargaze_core::transport::{ReassembledFrame, TransportError};
use tokio::sync::mpsc;
use tracing::{error, info};

/// Handle to a running client transport session.
pub struct ClientTransport {
    /// Join handle for the transport task.
    task_handle: tokio::task::JoinHandle<()>,
}

impl ClientTransport {
    /// Waits for the transport task to complete.
    ///
    /// # Errors
    ///
    /// Returns `TransportError` if the transport task panicked.
    pub async fn join(self) -> Result<(), TransportError> {
        self.task_handle
            .await
            .map_err(|e| TransportError::ConnectionError(format!("transport task panicked: {e}")))
    }

    /// Aborts the transport task.
    pub fn abort(&self) {
        self.task_handle.abort();
    }
}

/// Session parameters requested by the client.
pub struct SessionRequest {
    /// Desired video width.
    pub width: u32,
    /// Desired video height.
    pub height: u32,
    /// Desired framerate.
    pub framerate: u32,
    /// Desired codec.
    pub codec: Codec,
}

/// Connects to the server and starts receiving frames.
///
/// Returns a `ClientTransport` handle and an `mpsc::Receiver` for
/// reassembled frames.
///
/// # Errors
///
/// Returns `TransportError` if connection or handshake fails.
pub async fn connect(
    config: &ClientConfig,
    session_request: SessionRequest,
) -> Result<(ClientTransport, mpsc::Receiver<ReassembledFrame>), TransportError> {
    let server_addr: std::net::SocketAddr = format!("{}:{}", config.server_address, config.port)
        .parse()
        .map_err(|e| TransportError::ConnectionError(format!("invalid server address: {e}")))?;

    let connection = quic::connect_to_server(server_addr).await?;
    info!(
        remote = %connection.remote_address(),
        "Connected to server"
    );

    // Open control stream and perform handshake.
    let (mut send_stream, mut recv_stream) = connection.open_bi().await.map_err(|e| {
        TransportError::ConnectionError(format!("failed to open control stream: {e}"))
    })?;

    let session_response =
        receiver::perform_handshake(&session_request, &mut send_stream, &mut recv_stream).await?;

    info!(
        "Session established: {}x{} @ {}fps, {} Mbps, max_datagram={}",
        session_response.width,
        session_response.height,
        session_response.framerate,
        session_response.bitrate_mbps,
        session_response.max_datagram_size,
    );

    // Create frame delivery channel.
    let (frames_tx, frames_rx) = mpsc::channel::<ReassembledFrame>(16);

    let task_handle = tokio::spawn(async move {
        if let Err(e) = receiver::receive_loop(connection, send_stream, frames_tx).await {
            error!("Client transport error: {e}");
        }
    });

    Ok((ClientTransport { task_handle }, frames_rx))
}
