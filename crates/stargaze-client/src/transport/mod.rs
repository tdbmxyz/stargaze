//! Network transport module — client side.
//!
//! Provides [`connect()`] which establishes a `QUIC` connection to the server,
//! performs session handshake, and starts receiving video frames.

pub(crate) mod quic;
pub mod receiver;

use stargaze_core::config::{ClientConfig, Codec};
use stargaze_core::input::InputEvent;
use stargaze_core::transport::{ReassembledFrame, TransportError};
use tokio::sync::mpsc;
use tracing::{error, info};

pub use receiver::SessionParams;

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

/// Callback returning the current QUIC round-trip time estimate.
pub type RttProbe = Box<dyn Fn() -> std::time::Duration + Send>;

/// Receiver-side network counters, shared with the stats overlay.
///
/// Counted at reassembly time, so they reflect what actually arrives on
/// the wire — unlike render-side stats, which miss frames dropped under
/// decoder backpressure.
#[derive(Debug, Default)]
pub struct NetStats {
    /// Total video payload bytes received (complete frames).
    pub video_bytes: std::sync::atomic::AtomicU64,
    /// Total complete video frames received.
    pub video_frames: std::sync::atomic::AtomicU64,
    /// Video frames dropped because the decoder was behind.
    pub video_dropped: std::sync::atomic::AtomicU64,
}

/// # Errors
///
/// Returns `TransportError` if connection or handshake fails.
pub async fn connect(
    config: &ClientConfig,
    session_request: SessionRequest,
) -> Result<
    (
        ClientTransport,
        SessionParams,
        mpsc::Receiver<ReassembledFrame>,
        mpsc::Receiver<ReassembledFrame>,
        mpsc::Sender<InputEvent>,
        mpsc::Sender<()>,
        RttProbe,
        std::sync::Arc<NetStats>,
    ),
    TransportError,
> {
    let server_addr: std::net::SocketAddr = format!("{}:{}", config.server_address, config.port)
        .parse()
        .map_err(|e| TransportError::ConnectionError(format!("invalid server address: {e}")))?;

    let connection = quic::connect_to_server(server_addr).await?;
    info!(
        remote = %connection.remote_address(),
        "Connected to server"
    );

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

    let (video_tx, video_rx) = mpsc::channel::<ReassembledFrame>(2);
    let (audio_tx, audio_rx) = mpsc::channel::<ReassembledFrame>(16);
    let (input_tx, input_rx) = mpsc::channel::<InputEvent>(64);
    // Decoder → transport keyframe requests (sent after decode failures).
    let (idr_tx, idr_rx) = mpsc::channel::<()>(4);

    // Cloneable handle for RTT queries from the stats overlay.
    let rtt_conn = connection.clone();
    let rtt_probe: RttProbe = Box::new(move || rtt_conn.rtt());

    let net_stats = std::sync::Arc::new(NetStats::default());
    let net_stats_clone = std::sync::Arc::clone(&net_stats);

    let task_handle = tokio::spawn(async move {
        if let Err(e) = receiver::receive_loop(
            connection,
            send_stream,
            video_tx,
            audio_tx,
            input_rx,
            idr_rx,
            &net_stats_clone,
        )
        .await
        {
            error!("Client transport error: {e}");
        }
    });

    Ok((
        ClientTransport { task_handle },
        session_response,
        video_rx,
        audio_rx,
        input_tx,
        idr_tx,
        rtt_probe,
        net_stats,
    ))
}
