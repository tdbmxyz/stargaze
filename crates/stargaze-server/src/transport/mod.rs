//! Network transport module — server side.
//!
//! Provides [`start_server_transport()`] which accepts a `QUIC` connection
//! from a client, performs session handshake, and streams encoded
//! packets as unreliable datagrams.

pub(crate) mod quic;
pub(crate) mod sender;

use stargaze_core::config::ServerConfig;
use stargaze_core::encode::EncodedPacket;
use stargaze_core::transport::TransportError;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

/// Handle to a running server transport session.
pub struct ServerTransport {
    /// Join handle for the transport task.
    task_handle: tokio::task::JoinHandle<()>,
}

impl ServerTransport {
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

/// Starts the server transport.
///
/// Binds a `QUIC` endpoint, waits for a client connection, performs
/// session handshake, and starts streaming encoded packets.
///
/// # Arguments
///
/// * `config` — Server configuration (bind address, port, resolution, etc.)
/// * `packets` — Receiver for encoded packets from the encoder
/// * `idr_tx` — Sender to signal the encoder to produce IDR keyframes
///
/// # Errors
///
/// Returns `TransportError` if `QUIC` endpoint setup fails.
pub fn start_server_transport(
    config: &ServerConfig,
    packets: mpsc::Receiver<EncodedPacket>,
    idr_tx: watch::Sender<u64>,
) -> Result<ServerTransport, TransportError> {
    let bind_addr: std::net::SocketAddr = format!("{}:{}", config.bind_address, config.port)
        .parse()
        .map_err(|e| TransportError::ConnectionError(format!("invalid bind address: {e}")))?;

    let endpoint = quic::create_server_endpoint(bind_addr)?;
    let local_addr = endpoint
        .local_addr()
        .map_err(|e| TransportError::ConnectionError(format!("local addr: {e}")))?;
    info!("QUIC server listening on {local_addr}");

    let config = config.clone();
    let task_handle = tokio::spawn(async move {
        if let Err(e) = run_server_loop(endpoint, config, packets, idr_tx).await {
            error!("Server transport error: {e}");
        }
    });

    Ok(ServerTransport { task_handle })
}

/// Main server loop: accept connections and stream packets.
async fn run_server_loop(
    endpoint: quinn::Endpoint,
    config: ServerConfig,
    mut packets: mpsc::Receiver<EncodedPacket>,
    idr_tx: watch::Sender<u64>,
) -> Result<(), TransportError> {
    // Accept one connection (MVP: single client).
    let incoming = endpoint.accept().await.ok_or_else(|| {
        TransportError::ConnectionError("endpoint closed before accepting".to_string())
    })?;

    let connection = incoming.await.map_err(|e| {
        TransportError::ConnectionError(format!("failed to accept connection: {e}"))
    })?;

    info!(
        remote = %connection.remote_address(),
        "Client connected"
    );

    // Perform session handshake.
    let (mut send_stream, mut recv_stream) = connection.accept_bi().await.map_err(|e| {
        TransportError::ConnectionError(format!("failed to accept control stream: {e}"))
    })?;

    let session_response =
        sender::handle_session_handshake(&config, &connection, &mut send_stream, &mut recv_stream)
            .await?;

    info!(
        "Session established: {}x{} @ {}fps, {} Mbps",
        session_response.0, session_response.1, session_response.2, session_response.3
    );

    // Start the sender + control listener concurrently.
    let control_handle = tokio::spawn(async move {
        if let Err(e) = sender::handle_control_messages(&mut recv_stream, &idr_tx).await {
            warn!("Control stream error: {e}");
        }
    });

    let send_result = sender::send_packets(&connection, &mut packets).await;

    // Clean up.
    control_handle.abort();
    connection.close(quinn::VarInt::from_u32(0), b"server shutdown");
    endpoint.close(quinn::VarInt::from_u32(0), b"server shutdown");

    send_result
}
