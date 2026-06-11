//! Network transport module — server side.
//!
//! Provides [`start_server_transport()`] which accepts a `QUIC` connection
//! from a client, performs session handshake, and streams encoded
//! packets as unreliable datagrams.

pub(crate) mod quic;
pub(crate) mod sender;

use stargaze_core::config::ServerConfig;
use stargaze_core::encode::EncodedPacket;
use stargaze_core::input::InputEvent;
use stargaze_core::transport::{STREAM_TYPE_AUDIO, STREAM_TYPE_VIDEO, TransportError};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

/// Handle to a running server transport session.
pub struct ServerTransport {
    /// Join handle for the transport task.
    task_handle: tokio::task::JoinHandle<()>,
    /// Address the QUIC endpoint is bound to.
    local_addr: std::net::SocketAddr,
}

impl ServerTransport {
    /// Returns the address the QUIC endpoint is bound to.
    #[must_use]
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// Waits for the transport task to complete.
    ///
    /// # Errors
    ///
    /// Returns `TransportError` if the transport task panicked or was aborted.
    pub async fn join(&mut self) -> Result<(), TransportError> {
        (&mut self.task_handle)
            .await
            .map_err(|e| TransportError::ConnectionError(format!("transport task panicked: {e}")))
    }

    /// Aborts the transport task. Await [`join`](Self::join) afterwards to
    /// make sure the task has finished and released its channel endpoints.
    pub fn abort(&self) {
        self.task_handle.abort();
    }
}

/// Starts the server transport.
///
/// Binds a `QUIC` endpoint, waits for a client connection, performs
/// session handshake, and starts streaming encoded video and audio packets.
///
/// # Arguments
///
/// * `config` — Server configuration (bind address, port, resolution, etc.)
/// * `video_packets` — Receiver for encoded video packets from the video encoder
/// * `audio_packets` — Receiver for encoded audio packets from the audio encoder
/// * `idr_tx` — Sender to signal the video encoder to produce IDR keyframes
/// * `input_tx` — Sender to forward client input events to the input injection pipeline
///
/// # Errors
///
/// Returns `TransportError` if `QUIC` endpoint setup fails.
pub fn start_server_transport(
    config: &ServerConfig,
    video_packets: mpsc::Receiver<EncodedPacket>,
    audio_packets: mpsc::Receiver<EncodedPacket>,
    idr_tx: watch::Sender<u64>,
    input_tx: mpsc::Sender<InputEvent>,
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
        if let Err(e) = run_server_loop(
            endpoint,
            config,
            video_packets,
            audio_packets,
            idr_tx,
            input_tx,
        )
        .await
        {
            error!("Server transport error: {e}");
        }
    });

    Ok(ServerTransport {
        task_handle,
        local_addr,
    })
}

/// Main server loop: accepts clients one at a time, runs a streaming
/// session for each, and goes back to accepting when the client
/// disconnects — so a new client can reconnect to the running session.
async fn run_server_loop(
    endpoint: quinn::Endpoint,
    config: ServerConfig,
    mut video_packets: mpsc::Receiver<EncodedPacket>,
    mut audio_packets: mpsc::Receiver<EncodedPacket>,
    idr_tx: watch::Sender<u64>,
    input_tx: mpsc::Sender<InputEvent>,
) -> Result<(), TransportError> {
    loop {
        let Some(incoming) = endpoint.accept().await else {
            info!("Endpoint closed, transport exiting");
            return Ok(());
        };

        let connection = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to accept connection: {e}");
                continue;
            }
        };

        info!(
            remote = %connection.remote_address(),
            "Client connected"
        );

        if let Err(e) = run_session(
            &config,
            &connection,
            &mut video_packets,
            &mut audio_packets,
            &idr_tx,
            &input_tx,
        )
        .await
        {
            warn!("Session ended: {e}");
        }

        connection.close(quinn::VarInt::from_u32(0), b"session over");
        info!("Client disconnected, waiting for a new connection");
    }
}

/// Runs one streaming session on an established connection: handshake,
/// then concurrent video/audio sending and control-message handling
/// until the connection closes.
async fn run_session(
    config: &ServerConfig,
    connection: &quinn::Connection,
    video_packets: &mut mpsc::Receiver<EncodedPacket>,
    audio_packets: &mut mpsc::Receiver<EncodedPacket>,
    idr_tx: &watch::Sender<u64>,
    input_tx: &mpsc::Sender<InputEvent>,
) -> Result<(), TransportError> {
    let (mut send_stream, mut recv_stream) = connection.accept_bi().await.map_err(|e| {
        TransportError::ConnectionError(format!("failed to accept control stream: {e}"))
    })?;

    let session_response =
        sender::handle_session_handshake(config, connection, &mut send_stream, &mut recv_stream)
            .await?;

    info!(
        "Session established: {}x{} @ {}fps, {} Mbps",
        session_response.0, session_response.1, session_response.2, session_response.3
    );

    // Packets that queued up while no client was connected are stale —
    // throw them away and force a fresh IDR so the new client can start
    // decoding immediately.
    while video_packets.try_recv().is_ok() {}
    while audio_packets.try_recv().is_ok() {}
    idr_tx.send_modify(|v| *v += 1);

    // Run the control listener and both senders until the connection dies:
    // the control listener returns when the client closes, and the senders
    // return on connection loss. Whichever finishes first ends the session
    // (select! drops the other futures; mpsc recv is cancel-safe).
    tokio::select! {
        result = sender::handle_control_messages(&mut recv_stream, idr_tx, input_tx) => {
            if let Err(e) = result {
                warn!("Control stream error: {e}");
            }
        }
        result = sender::send_packets(connection, video_packets, STREAM_TYPE_VIDEO) => {
            if let Err(e) = result {
                warn!("Video send error: {e}");
            }
        }
        result = sender::send_packets(connection, audio_packets, STREAM_TYPE_AUDIO) => {
            if let Err(e) = result {
                warn!("Audio send error: {e}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use quinn::rustls;
    use stargaze_core::transport::{
        ControlMessage, deserialize_control_message, serialize_control_message,
    };

    use super::*;

    /// Accepts any server certificate (test-only, mirrors the client's
    /// LAN-MVP behavior).
    #[derive(Debug)]
    struct SkipServerVerification;

    impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::ED25519,
            ]
        }
    }

    /// Connects a test client and performs the session handshake.
    /// Returns the endpoint (must stay alive) and the connection.
    async fn connect_and_handshake(
        addr: std::net::SocketAddr,
    ) -> (quinn::Endpoint, quinn::Connection) {
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();
        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap(),
        ));

        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(client_config);

        let connection = endpoint
            .connect(addr, "stargaze-server")
            .unwrap()
            .await
            .expect("client connection should succeed");

        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        let request = ControlMessage::SessionRequest {
            width: 1920,
            height: 1080,
            framerate: 60,
            codec: stargaze_core::config::Codec::H265,
        };
        send.write_all(&serialize_control_message(&request).unwrap())
            .await
            .unwrap();

        let mut len_buf = [0u8; 4];
        recv.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        recv.read_exact(&mut body).await.unwrap();
        let response = deserialize_control_message(&body).unwrap();
        assert!(
            matches!(response, ControlMessage::SessionResponse { .. }),
            "expected SessionResponse, got {response:?}"
        );

        (endpoint, connection)
    }

    /// Regression test: after a client disconnects, the server must go
    /// back to accepting so a new client can join the running session.
    #[tokio::test(flavor = "multi_thread")]
    async fn client_can_reconnect_after_disconnect() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();

        let config = ServerConfig {
            bind_address: "127.0.0.1".to_string(),
            port: 0, // OS-assigned port
            ..ServerConfig::default()
        };

        let (_video_tx, video_rx) = mpsc::channel::<EncodedPacket>(4);
        let (_audio_tx, audio_rx) = mpsc::channel::<EncodedPacket>(4);
        let (idr_tx, idr_rx) = tokio::sync::watch::channel(0u64);
        let (input_tx, _input_rx) = mpsc::channel::<InputEvent>(8);

        let transport = start_server_transport(&config, video_rx, audio_rx, idr_tx, input_tx)
            .expect("transport should start");
        let addr = transport.local_addr();

        let run = async {
            // First client connects, handshakes, and disconnects.
            let (endpoint1, conn1) = connect_and_handshake(addr).await;
            conn1.close(quinn::VarInt::from_u32(0), b"bye");
            endpoint1.wait_idle().await;

            // Second client must be able to connect and handshake.
            let (_endpoint2, conn2) = connect_and_handshake(addr).await;

            // Each session start forces an IDR so the joining client gets
            // a keyframe immediately.
            let mut idr_rx = idr_rx;
            while *idr_rx.borrow() < 2 {
                tokio::time::timeout(Duration::from_secs(1), idr_rx.changed())
                    .await
                    .expect("expected a second IDR request after reconnect")
                    .unwrap();
            }

            conn2.close(quinn::VarInt::from_u32(0), b"bye");
        };

        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("reconnect test timed out");
    }
}
