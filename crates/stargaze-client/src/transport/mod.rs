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

/// Resolves the configured server address (IP literal or DNS hostname)
/// to a socket address, preferring IPv4 (the QUIC endpoint binds an
/// IPv4 wildcard by default; IPv6 results are used only when nothing
/// else resolves).
async fn resolve_server_addr(
    address: &str,
    port: u16,
) -> Result<std::net::SocketAddr, TransportError> {
    // IP literals (including bracketed IPv6, as the pre-DNS config
    // format required) skip the resolver entirely.
    let bare = address
        .strip_prefix('[')
        .and_then(|a| a.strip_suffix(']'))
        .unwrap_or(address);
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, port));
    }
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((address, port))
        .await
        .map_err(|e| {
            TransportError::ConnectionError(format!("cannot resolve server address {address}: {e}"))
        })?
        .collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| {
            TransportError::ConnectionError(format!("server address {address} resolved to nothing"))
        })
}

/// An established session connection: everything the client needs to
/// run decoders, forward input, and render until the session ends.
pub struct ConnectedSession {
    /// Handle to abort the transport receive task.
    pub transport: ClientTransport,
    /// Server-confirmed session parameters.
    pub session_params: SessionParams,
    /// Reassembled video frames.
    pub video_frames: mpsc::Receiver<ReassembledFrame>,
    /// Reassembled audio frames.
    pub audio_frames: mpsc::Receiver<ReassembledFrame>,
    /// Input events to forward to the server.
    pub input_tx: mpsc::Sender<InputEvent>,
    /// Keyframe requests from the decoder.
    pub idr_tx: mpsc::Sender<()>,
    /// RTT query handle for the stats overlay.
    pub rtt_probe: RttProbe,
    /// Network counters for the stats overlay.
    pub net_stats: std::sync::Arc<NetStats>,
    /// Connection handle for opening USB tunnel streams.
    pub usb_connection: quinn::Connection,
}

/// # Errors
///
/// Returns `TransportError` if connection or handshake fails.
pub async fn connect(
    config: &ClientConfig,
    session_request: SessionRequest,
) -> Result<ConnectedSession, TransportError> {
    let server_addr = resolve_server_addr(&config.server_address, config.port).await?;

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

    // Handle for opening extra streams (USB tunnels) on the same session.
    let usb_connection = connection.clone();

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

    Ok(ConnectedSession {
        transport: ClientTransport { task_handle },
        session_params: session_response,
        video_frames: video_rx,
        audio_frames: audio_rx,
        input_tx,
        idr_tx,
        rtt_probe,
        net_stats,
        usb_connection,
    })
}

#[cfg(test)]
mod tests {
    use super::resolve_server_addr;

    #[tokio::test]
    async fn resolves_ip_literals_without_dns() {
        let v4 = resolve_server_addr("192.168.1.10", 9000).await.unwrap();
        assert_eq!(v4.to_string(), "192.168.1.10:9000");

        // Bracketed IPv6, as the pre-DNS "addr:port".parse() format required.
        let v6 = resolve_server_addr("[::1]", 9000).await.unwrap();
        assert!(v6.is_ipv6());
        assert_eq!(v6.port(), 9000);

        // Bare IPv6 literals work too.
        let v6 = resolve_server_addr("fd00::1", 9000).await.unwrap();
        assert!(v6.is_ipv6());
    }
}
