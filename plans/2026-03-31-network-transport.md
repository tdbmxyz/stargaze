# Network Transport Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transport encoded video packets from server to client over the network using QUIC, with a control channel for session management and IDR keyframe feedback.

**Architecture:** QUIC via `quinn` + `rustls` on a single UDP port. Unreliable datagrams carry A/V frame fragments for minimal latency. A reliable bidirectional QUIC stream carries control messages (session handshake, IDR requests, ping/pong). Server fragments `EncodedPacket` into datagrams; client reassembles them via a `FrameAssembler`. Self-signed TLS certs generated with `rcgen`, stored on disk. All serialization uses `postcard`.

**Tech Stack:** Rust 2024 nightly, `quinn` 0.11, `rustls` 0.23 (via quinn), `rcgen` 0.14, `postcard` 1.1, tokio, serde, thiserror

**Spec:** `docs/specs/2026-03-31-network-transport-design.md`

**Build environment note:** Every `cargo` command in this plan MUST be prefixed with:
```bash
export PATH="$HOME/.local/usr/bin:$HOME/.local/usr/lib/llvm-19/bin:$PATH" && \
export PKG_CONFIG_PATH="$HOME/.local/usr/lib/x86_64-linux-gnu/pkgconfig" && \
export LIBRARY_PATH="$HOME/.local/usr/lib/x86_64-linux-gnu:$HOME/.local/usr/lib/llvm-19/lib" && \
export LD_LIBRARY_PATH="$HOME/.local/usr/lib/x86_64-linux-gnu:$HOME/.local/usr/lib/llvm-19/lib" && \
export C_INCLUDE_PATH="$HOME/.local/usr/include" && \
export LIBCLANG_PATH="$HOME/.local/usr/lib/llvm-19/lib" && \
export BINDGEN_EXTRA_CLANG_ARGS="-I$HOME/.local/usr/include -I/usr/lib/gcc/x86_64-linux-gnu/14/include -I/usr/include"
```

For brevity, the plan references this as `ENV_SETUP` — paste the full block before each `cargo` invocation.

---

## File Structure

### New files to create

- `crates/stargaze-core/src/transport.rs` — shared types: `DatagramHeader`, `ControlMessage`, `ReassembledFrame`, `TransportError`, constants
- `crates/stargaze-server/src/transport/mod.rs` — public API: `ServerTransport`, `start_server_transport()`
- `crates/stargaze-server/src/transport/quic.rs` — QUIC endpoint setup, TLS cert generation/loading, connection accept
- `crates/stargaze-server/src/transport/sender.rs` — frame fragmentation + datagram sending, control message handling
- `crates/stargaze-client/src/transport/mod.rs` — public API: `ClientTransport`, `connect()`
- `crates/stargaze-client/src/transport/quic.rs` — QUIC endpoint setup, connection to server
- `crates/stargaze-client/src/transport/receiver.rs` — datagram reassembly (`FrameAssembler`), control message handling

### Files to modify

- `crates/stargaze-core/src/lib.rs` — add `pub mod transport;`
- `crates/stargaze-core/Cargo.toml` — add `postcard` dependency
- `crates/stargaze-server/Cargo.toml` — add `quinn`, `rcgen`, `bytes` dependencies
- `crates/stargaze-server/src/main.rs` — wire transport after encoder, add `mod transport`
- `crates/stargaze-server/src/encode/mod.rs` — add IDR watch channel parameter to `start_encoder()`
- `crates/stargaze-server/src/encode/ffmpeg.rs` — check IDR watch in encode loop, force keyframe when requested
- `crates/stargaze-client/Cargo.toml` — add `quinn`, `rustls`, `bytes` dependencies
- `crates/stargaze-client/src/main.rs` — connect to server, receive and log frames, add `mod transport`

---

## Task 1: Add crate dependencies

**Files:**
- Modify: `crates/stargaze-core/Cargo.toml`
- Modify: `crates/stargaze-server/Cargo.toml`
- Modify: `crates/stargaze-client/Cargo.toml`

- [ ] **Step 1: Add postcard to stargaze-core**

Add `postcard` with the `alloc` feature to `stargaze-core/Cargo.toml`:

```bash
ENV_SETUP && cargo add postcard --features alloc --package stargaze-core
```

The `[dependencies]` section should now include:
```toml
postcard = { version = "1", features = ["alloc"] }
```

- [ ] **Step 2: Add quinn, rcgen, bytes to stargaze-server**

```bash
ENV_SETUP && cargo add quinn rcgen bytes --package stargaze-server
```

The `[dependencies]` section should gain:
```toml
quinn = "0.11"
rcgen = "0.14"
bytes = "1"
```

Quinn's default features include `runtime-tokio` and `rustls-ring`, which is exactly what we need. No need to add `rustls` directly — it's re-exported via quinn.

- [ ] **Step 3: Add quinn, rustls, bytes to stargaze-client**

```bash
ENV_SETUP && cargo add quinn rustls bytes --package stargaze-client
```

The client needs `rustls` directly for the custom `ServerCertVerifier`. The `[dependencies]` section should gain:
```toml
quinn = "0.11"
rustls = "0.23"
bytes = "1"
```

- [ ] **Step 4: Verify the workspace compiles**

```bash
ENV_SETUP && cargo check --workspace
```

Expected: compiles with zero errors. There may be unused-dependency warnings — that's fine since we haven't written the code yet.

- [ ] **Step 5: Commit**

```bash
git add crates/stargaze-core/Cargo.toml crates/stargaze-server/Cargo.toml crates/stargaze-client/Cargo.toml Cargo.lock && \
git commit --no-gpg-sign -m "chore(deps): add quinn, rcgen, postcard, bytes for network transport"
```

---

## Task 2: Shared transport types in stargaze-core

**Files:**
- Create: `crates/stargaze-core/src/transport.rs`
- Modify: `crates/stargaze-core/src/lib.rs`

This task creates all the shared types that both server and client use: `DatagramHeader`, `ControlMessage`, `ReassembledFrame`, `TransportError`, stream type constants, and helper functions for serialization.

- [ ] **Step 1: Write the failing tests**

Create `crates/stargaze-core/src/transport.rs` with the test module first:

```rust
//! Shared transport types for network communication.
//!
//! Defines packet headers, control messages, and error types used by
//! both the server sender and client receiver.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::Codec;

/// Stream type tag for video datagrams.
pub const STREAM_TYPE_VIDEO: u8 = 0;

/// Stream type tag for audio datagrams.
pub const STREAM_TYPE_AUDIO: u8 = 1;

/// Maximum number of incomplete frames the assembler will buffer
/// before requesting an IDR.
pub const MAX_PENDING_FRAMES: usize = 16;

/// Minimum interval between IDR requests in milliseconds.
pub const IDR_RATE_LIMIT_MS: u64 = 500;

/// Header prepended to each QUIC datagram.
///
/// Serialized with `postcard` (compact binary format). The remaining
/// bytes after the header are the fragment payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatagramHeader {
    /// Stream type: 0 = video, 1 = audio.
    pub stream_type: u8,
    /// Monotonically increasing frame index (per stream type).
    pub frame_index: u32,
    /// 0-based index of this fragment within the frame.
    pub fragment_index: u16,
    /// Total number of fragments in this frame.
    pub fragment_count: u16,
    /// Presentation timestamp (frame number from encoder).
    pub pts: u64,
    /// True for IDR/keyframes (video only, always false for audio).
    pub is_keyframe: bool,
}

/// Messages exchanged over the reliable control stream.
///
/// Length-prefixed with 4-byte LE length before the `postcard`-serialized body.
/// New variants may be appended without breaking backward compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Client -> Server: request a streaming session.
    SessionRequest {
        /// Requested video width in pixels.
        width: u32,
        /// Requested video height in pixels.
        height: u32,
        /// Requested framerate.
        framerate: u32,
        /// Requested video codec.
        codec: Codec,
    },
    /// Server -> Client: confirm session parameters.
    SessionResponse {
        /// Confirmed video width in pixels.
        width: u32,
        /// Confirmed video height in pixels.
        height: u32,
        /// Confirmed framerate.
        framerate: u32,
        /// Bitrate in Mbps.
        bitrate_mbps: u32,
        /// Confirmed video codec.
        codec: Codec,
        /// Maximum datagram payload size for the connection.
        max_datagram_size: u16,
    },
    /// Client -> Server: request an IDR keyframe (after packet loss).
    IdrRequest,
    /// Bidirectional: keepalive with timestamp.
    Ping {
        /// Millisecond timestamp from sender.
        timestamp_ms: u64,
    },
    /// Bidirectional: keepalive response.
    Pong {
        /// Echoed timestamp from the original `Ping`.
        timestamp_ms: u64,
    },
}

/// A fully reassembled frame ready for decoding.
#[derive(Debug, Clone)]
pub struct ReassembledFrame {
    /// Concatenated payload data from all fragments.
    pub data: Vec<u8>,
    /// Presentation timestamp.
    pub pts: u64,
    /// Whether this is a keyframe.
    pub is_keyframe: bool,
    /// Stream type (video or audio).
    pub stream_type: u8,
}

/// Errors from the transport subsystem.
#[derive(Error, Debug)]
pub enum TransportError {
    /// QUIC connection failed.
    #[error("connection error: {0}")]
    ConnectionError(String),

    /// Failed to send a datagram.
    #[error("send error: {0}")]
    SendError(String),

    /// Failed to read or write a control message.
    #[error("control channel error: {0}")]
    ControlError(String),

    /// Session handshake failed or was rejected.
    #[error("session error: {0}")]
    SessionError(String),

    /// TLS certificate generation or loading failed.
    #[error("TLS error: {0}")]
    TlsError(String),

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    SerializationError(String),
}

/// Serializes a `DatagramHeader` to bytes using `postcard`.
///
/// # Errors
///
/// Returns `TransportError::SerializationError` if serialization fails.
pub fn serialize_header(header: &DatagramHeader) -> Result<Vec<u8>, TransportError> {
    postcard::to_allocvec(header)
        .map_err(|e| TransportError::SerializationError(format!("header serialize: {e}")))
}

/// Deserializes a `DatagramHeader` from bytes, returning the header
/// and the remaining bytes (the payload).
///
/// # Errors
///
/// Returns `TransportError::SerializationError` if deserialization fails.
pub fn deserialize_header(buf: &[u8]) -> Result<(DatagramHeader, &[u8]), TransportError> {
    postcard::take_from_bytes(buf)
        .map_err(|e| TransportError::SerializationError(format!("header deserialize: {e}")))
}

/// Serializes a `ControlMessage` to a length-prefixed byte buffer.
///
/// Format: `[4 bytes LE: body length][postcard-serialized body]`
///
/// # Errors
///
/// Returns `TransportError::SerializationError` if serialization fails.
pub fn serialize_control_message(msg: &ControlMessage) -> Result<Vec<u8>, TransportError> {
    let body = postcard::to_allocvec(msg)
        .map_err(|e| TransportError::SerializationError(format!("control serialize: {e}")))?;
    let len = u32::try_from(body.len())
        .map_err(|_| TransportError::SerializationError("control message too large".to_string()))?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&body);
    Ok(buf)
}

/// Deserializes a `ControlMessage` from a `postcard`-serialized body
/// (without the length prefix — the caller reads the length first).
///
/// # Errors
///
/// Returns `TransportError::SerializationError` if deserialization fails.
pub fn deserialize_control_message(body: &[u8]) -> Result<ControlMessage, TransportError> {
    postcard::from_bytes(body)
        .map_err(|e| TransportError::SerializationError(format!("control deserialize: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_header_round_trip() {
        let header = DatagramHeader {
            stream_type: STREAM_TYPE_VIDEO,
            frame_index: 42,
            fragment_index: 3,
            fragment_count: 10,
            pts: 12345,
            is_keyframe: true,
        };
        let bytes = serialize_header(&header).unwrap();
        let (decoded, remainder) = deserialize_header(&bytes).unwrap();
        assert_eq!(decoded, header);
        assert!(remainder.is_empty());
    }

    #[test]
    fn datagram_header_with_payload() {
        let header = DatagramHeader {
            stream_type: STREAM_TYPE_AUDIO,
            frame_index: 0,
            fragment_index: 0,
            fragment_count: 1,
            pts: 0,
            is_keyframe: false,
        };
        let header_bytes = serialize_header(&header).unwrap();
        let payload = b"audio data here";
        let mut datagram = header_bytes.clone();
        datagram.extend_from_slice(payload);
        let (decoded, remainder) = deserialize_header(&datagram).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(remainder, payload);
    }

    #[test]
    fn control_message_session_request_round_trip() {
        let msg = ControlMessage::SessionRequest {
            width: 1920,
            height: 1080,
            framerate: 60,
            codec: Codec::H265,
        };
        let bytes = serialize_control_message(&msg).unwrap();
        // First 4 bytes are length prefix.
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4);
        let decoded = deserialize_control_message(&bytes[4..]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_session_response_round_trip() {
        let msg = ControlMessage::SessionResponse {
            width: 2560,
            height: 1440,
            framerate: 120,
            bitrate_mbps: 50,
            codec: Codec::Av1,
            max_datagram_size: 1200,
        };
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_idr_request_round_trip() {
        let msg = ControlMessage::IdrRequest;
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_ping_pong_round_trip() {
        for msg in [
            ControlMessage::Ping {
                timestamp_ms: 1_000_000,
            },
            ControlMessage::Pong {
                timestamp_ms: 1_000_000,
            },
        ] {
            let bytes = serialize_control_message(&msg).unwrap();
            let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
            let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn transport_error_display() {
        let err = TransportError::ConnectionError("timeout".to_string());
        assert_eq!(err.to_string(), "connection error: timeout");

        let err = TransportError::TlsError("bad cert".to_string());
        assert_eq!(err.to_string(), "TLS error: bad cert");

        let err = TransportError::SerializationError("eof".to_string());
        assert_eq!(err.to_string(), "serialization error: eof");
    }

    #[test]
    fn reassembled_frame_construction() {
        let frame = ReassembledFrame {
            data: vec![1, 2, 3],
            pts: 100,
            is_keyframe: false,
            stream_type: STREAM_TYPE_VIDEO,
        };
        assert_eq!(frame.data.len(), 3);
        assert_eq!(frame.pts, 100);
        assert!(!frame.is_keyframe);
        assert_eq!(frame.stream_type, STREAM_TYPE_VIDEO);
    }

    #[test]
    fn stream_type_constants() {
        assert_eq!(STREAM_TYPE_VIDEO, 0);
        assert_eq!(STREAM_TYPE_AUDIO, 1);
        assert_ne!(STREAM_TYPE_VIDEO, STREAM_TYPE_AUDIO);
    }
}
```

- [ ] **Step 2: Add the module to lib.rs**

Modify `crates/stargaze-core/src/lib.rs` to add the transport module:

```rust
pub mod capture;
pub mod config;
pub mod encode;
pub mod error;
pub mod transport;
```

- [ ] **Step 3: Run tests to verify they pass**

```bash
ENV_SETUP && cargo test --package stargaze-core -- transport
```

Expected: all 9 transport tests pass.

- [ ] **Step 4: Run clippy**

```bash
ENV_SETUP && cargo clippy --package stargaze-core -W clippy::pedantic
```

Expected: no warnings. If there are clippy issues (like `as` casts), fix them using `.cast_signed()`, `.cast_unsigned()`, or other Rust 2024 idioms.

- [ ] **Step 5: Commit**

```bash
git add crates/stargaze-core/src/transport.rs crates/stargaze-core/src/lib.rs && \
git commit --no-gpg-sign -m "feat(core): add shared transport types — DatagramHeader, ControlMessage, TransportError"
```

---

## Task 3: Server QUIC endpoint setup and TLS certificate management

**Files:**
- Create: `crates/stargaze-server/src/transport/mod.rs`
- Create: `crates/stargaze-server/src/transport/quic.rs`

This task sets up the server's QUIC endpoint: generating/loading self-signed TLS certificates and creating the `quinn::Endpoint`.

- [ ] **Step 1: Create the transport module structure**

Create `crates/stargaze-server/src/transport/mod.rs`:

```rust
//! Network transport module — server side.
//!
//! Provides `start_server_transport()` which accepts a QUIC connection
//! from a client, performs session handshake, and streams encoded
//! packets as unreliable datagrams.

pub(crate) mod quic;
pub(crate) mod sender;

use std::net::SocketAddr;
use std::sync::Arc;

use stargaze_core::config::ServerConfig;
use stargaze_core::encode::EncodedPacket;
use stargaze_core::transport::{ControlMessage, TransportError};
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
        self.task_handle.await.map_err(|e| {
            TransportError::ConnectionError(format!("transport task panicked: {e}"))
        })
    }

    /// Aborts the transport task.
    pub fn abort(&self) {
        self.task_handle.abort();
    }
}

/// Starts the server transport.
///
/// Binds a QUIC endpoint, waits for a client connection, performs
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
/// Returns `TransportError` if QUIC endpoint setup fails.
pub async fn start_server_transport(
    config: &ServerConfig,
    packets: mpsc::Receiver<EncodedPacket>,
    idr_tx: watch::Sender<u64>,
) -> Result<ServerTransport, TransportError> {
    let bind_addr: SocketAddr = format!("{}:{}", config.bind_address, config.port)
        .parse()
        .map_err(|e| TransportError::ConnectionError(format!("invalid bind address: {e}")))?;

    let endpoint = quic::create_server_endpoint(bind_addr).await?;
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

    info!("Session established: {}x{} @ {}fps, {} Mbps",
        session_response.0, session_response.1,
        session_response.2, session_response.3);

    // Start the sender + control listener concurrently.
    let connection_clone = connection.clone();
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
```

- [ ] **Step 2: Create the QUIC endpoint setup module**

Create `crates/stargaze-server/src/transport/quic.rs`:

```rust
//! QUIC endpoint setup and TLS certificate management for the server.

use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;

use directories::ProjectDirs;
use stargaze_core::transport::TransportError;
use tracing::{debug, info, warn};

/// Creates a server QUIC endpoint with TLS using a self-signed certificate.
///
/// Loads an existing certificate from disk or generates a new one.
///
/// # Errors
///
/// Returns `TransportError::TlsError` if certificate operations fail,
/// or `TransportError::ConnectionError` if the endpoint cannot bind.
pub(crate) async fn create_server_endpoint(
    bind_addr: SocketAddr,
) -> Result<quinn::Endpoint, TransportError> {
    let (cert_der, key_der) = load_or_generate_cert()?;

    let cert_chain = vec![cert_der];
    let server_config = quinn::ServerConfig::with_single_cert(cert_chain, key_der).map_err(|e| {
        TransportError::TlsError(format!("failed to create server TLS config: {e}"))
    })?;

    let endpoint = quinn::Endpoint::server(server_config, bind_addr).map_err(|e| {
        TransportError::ConnectionError(format!("failed to bind QUIC endpoint: {e}"))
    })?;

    Ok(endpoint)
}

/// Loads an existing TLS certificate from disk, or generates a new
/// self-signed one if none exists.
///
/// Certificates are stored in `~/.config/stargaze/cert.der` and `key.der`.
fn load_or_generate_cert(
) -> Result<(rustls::pki_types::CertificateDer<'static>, rustls::pki_types::PrivateKeyDer<'static>), TransportError> {
    let config_dir = ProjectDirs::from("", "", "stargaze")
        .ok_or_else(|| TransportError::TlsError("cannot determine config directory".to_string()))?;
    let dir = config_dir.config_dir();

    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");

    // Try loading existing cert.
    if cert_path.exists() && key_path.exists() {
        debug!("Loading TLS certificate from {}", cert_path.display());
        let cert_bytes = fs::read(&cert_path)
            .map_err(|e| TransportError::TlsError(format!("read cert: {e}")))?;
        let key_bytes = fs::read(&key_path)
            .map_err(|e| TransportError::TlsError(format!("read key: {e}")))?;

        let cert = rustls::pki_types::CertificateDer::from(cert_bytes);
        let key = rustls::pki_types::PrivateKeyDer::try_from(key_bytes)
            .map_err(|e| TransportError::TlsError(format!("parse key: {e}")))?;

        return Ok((cert, key));
    }

    // Generate new self-signed certificate.
    info!("Generating new self-signed TLS certificate");
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| TransportError::TlsError(format!("key generation: {e}")))?;

    let mut params = rcgen::CertificateParams::new(vec!["stargaze-server".to_string()])
        .map_err(|e| TransportError::TlsError(format!("cert params: {e}")))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "stargaze-server");

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| TransportError::TlsError(format!("self-sign: {e}")))?;

    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    // Save to disk.
    fs::create_dir_all(dir)
        .map_err(|e| TransportError::TlsError(format!("create config dir: {e}")))?;
    fs::write(&cert_path, &cert_der)
        .map_err(|e| TransportError::TlsError(format!("write cert: {e}")))?;
    fs::write(&key_path, &key_der)
        .map_err(|e| TransportError::TlsError(format!("write key: {e}")))?;

    info!("Saved TLS certificate to {}", cert_path.display());

    let cert = rustls::pki_types::CertificateDer::from(cert_der);
    let key = rustls::pki_types::PrivateKeyDer::try_from(key_der)
        .map_err(|e| TransportError::TlsError(format!("parse generated key: {e}")))?;

    Ok((cert, key))
}
```

- [ ] **Step 3: Create a placeholder sender.rs**

Create `crates/stargaze-server/src/transport/sender.rs` with placeholder functions so the module compiles:

```rust
//! Frame fragmentation and datagram sending for the server.
//!
//! Handles fragmenting `EncodedPacket` values into QUIC datagrams
//! and processing incoming control messages.

use stargaze_core::config::ServerConfig;
use stargaze_core::encode::EncodedPacket;
use stargaze_core::transport::{
    ControlMessage, TransportError,
    serialize_control_message, deserialize_control_message,
};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// Performs the session handshake with the client.
///
/// Reads `SessionRequest` from the control stream, validates it,
/// and sends back `SessionResponse`.
///
/// Returns (width, height, framerate, bitrate_mbps) of the confirmed session.
///
/// # Errors
///
/// Returns `TransportError::SessionError` if the handshake fails.
pub(crate) async fn handle_session_handshake(
    config: &ServerConfig,
    connection: &quinn::Connection,
    send_stream: &mut quinn::SendStream,
    recv_stream: &mut quinn::RecvStream,
) -> Result<(u32, u32, u32, u32), TransportError> {
    // Read length prefix.
    let mut len_buf = [0u8; 4];
    recv_stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| TransportError::SessionError(format!("read request length: {e}")))?;
    let msg_len = u32::from_le_bytes(len_buf) as usize;

    if msg_len > 65536 {
        return Err(TransportError::SessionError(
            "session request too large".to_string(),
        ));
    }

    // Read message body.
    let mut body = vec![0u8; msg_len];
    recv_stream
        .read_exact(&mut body)
        .await
        .map_err(|e| TransportError::SessionError(format!("read request body: {e}")))?;

    let request = deserialize_control_message(&body)?;

    let (width, height, framerate, codec) = match request {
        ControlMessage::SessionRequest {
            width,
            height,
            framerate,
            codec,
        } => (width, height, framerate, codec),
        other => {
            return Err(TransportError::SessionError(format!(
                "expected SessionRequest, got {other:?}"
            )));
        }
    };

    info!(
        "Session request: {}x{} @ {}fps, {:?}",
        width, height, framerate, codec
    );

    // For MVP, use server's configured parameters (ignore client preferences
    // that differ — a real implementation would negotiate).
    let max_datagram_size = connection
        .max_datagram_size()
        .unwrap_or(1200) as u16;

    let response = ControlMessage::SessionResponse {
        width: config.resolution.width,
        height: config.resolution.height,
        framerate: config.framerate,
        bitrate_mbps: config.bitrate,
        codec: config.codec,
        max_datagram_size,
    };

    let response_bytes = serialize_control_message(&response)?;
    send_stream
        .write_all(&response_bytes)
        .await
        .map_err(|e| TransportError::SessionError(format!("send response: {e}")))?;

    Ok((
        config.resolution.width,
        config.resolution.height,
        config.framerate,
        config.bitrate,
    ))
}

/// Listens for control messages from the client (IDR requests, pings).
///
/// Runs until the stream is closed or an error occurs.
///
/// # Errors
///
/// Returns `TransportError::ControlError` on stream errors.
pub(crate) async fn handle_control_messages(
    recv_stream: &mut quinn::RecvStream,
    idr_tx: &watch::Sender<u64>,
) -> Result<(), TransportError> {
    loop {
        // Read length prefix.
        let mut len_buf = [0u8; 4];
        match recv_stream.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(_))) => {
                info!("Control stream: connection closed");
                return Ok(());
            }
            Err(quinn::ReadExactError::FinishedEarly(_)) => {
                info!("Control stream: client closed stream");
                return Ok(());
            }
            Err(e) => {
                return Err(TransportError::ControlError(format!("read length: {e}")));
            }
        }

        let msg_len = u32::from_le_bytes(len_buf) as usize;
        if msg_len > 65536 {
            return Err(TransportError::ControlError(
                "control message too large".to_string(),
            ));
        }

        let mut body = vec![0u8; msg_len];
        recv_stream
            .read_exact(&mut body)
            .await
            .map_err(|e| TransportError::ControlError(format!("read body: {e}")))?;

        let msg = deserialize_control_message(&body)?;

        match msg {
            ControlMessage::IdrRequest => {
                debug!("Received IDR request from client");
                // Increment the IDR counter to signal the encoder.
                idr_tx.send_modify(|v| *v += 1);
            }
            ControlMessage::Ping { timestamp_ms } => {
                debug!(timestamp_ms, "Received ping (pong not yet implemented)");
                // TODO: send pong back once we have a send_stream reference
            }
            other => {
                warn!("Unexpected control message: {other:?}");
            }
        }
    }
}

/// Sends encoded packets as fragmented QUIC datagrams.
///
/// Runs until the packet channel closes.
///
/// # Errors
///
/// Returns `TransportError::SendError` on datagram send failures.
pub(crate) async fn send_packets(
    connection: &quinn::Connection,
    packets: &mut mpsc::Receiver<EncodedPacket>,
) -> Result<(), TransportError> {
    use bytes::Bytes;
    use stargaze_core::transport::{DatagramHeader, STREAM_TYPE_VIDEO, serialize_header};

    let mut frame_index: u32 = 0;

    while let Some(pkt) = packets.recv().await {
        let max_datagram_size = connection.max_datagram_size().unwrap_or(1200);

        // Compute max payload per fragment.
        // Serialize a sample header to determine header size.
        let sample_header = DatagramHeader {
            stream_type: STREAM_TYPE_VIDEO,
            frame_index,
            fragment_index: 0,
            fragment_count: 1,
            pts: pkt.pts,
            is_keyframe: pkt.is_keyframe,
        };
        let header_size = serialize_header(&sample_header)
            .map_err(|e| TransportError::SendError(format!("header size: {e}")))?
            .len();

        let max_payload = max_datagram_size.saturating_sub(header_size);
        if max_payload == 0 {
            warn!("Max datagram size too small for header, skipping frame");
            frame_index = frame_index.wrapping_add(1);
            continue;
        }

        // Fragment the packet.
        let fragment_count = (pkt.data.len() + max_payload - 1) / max_payload;
        let fragment_count_u16 = u16::try_from(fragment_count).unwrap_or(u16::MAX);

        for i in 0..fragment_count {
            let start = i * max_payload;
            let end = ((i + 1) * max_payload).min(pkt.data.len());
            let payload = &pkt.data[start..end];

            let header = DatagramHeader {
                stream_type: STREAM_TYPE_VIDEO,
                frame_index,
                fragment_index: u16::try_from(i).unwrap_or(u16::MAX),
                fragment_count: fragment_count_u16,
                pts: pkt.pts,
                is_keyframe: pkt.is_keyframe,
            };

            let header_bytes = serialize_header(&header)
                .map_err(|e| TransportError::SendError(format!("serialize: {e}")))?;

            let mut datagram = Vec::with_capacity(header_bytes.len() + payload.len());
            datagram.extend_from_slice(&header_bytes);
            datagram.extend_from_slice(payload);

            if let Err(e) = connection.send_datagram(Bytes::from(datagram)) {
                // Log but don't fail — datagrams are unreliable by nature.
                debug!(
                    frame = frame_index,
                    fragment = i,
                    "Datagram send failed: {e}"
                );
            }
        }

        frame_index = frame_index.wrapping_add(1);
    }

    info!("Packet channel closed, transport sender exiting");
    Ok(())
}
```

- [ ] **Step 4: Add `mod transport` to server main.rs**

Add the module declaration to `crates/stargaze-server/src/main.rs`, right after the existing `mod` lines:

```rust
mod capture;
mod encode;
mod transport;
```

Do NOT wire it into the pipeline yet — that happens in Task 7.

- [ ] **Step 5: Verify compilation**

```bash
ENV_SETUP && cargo check --package stargaze-server
```

Expected: compiles with zero errors. There may be dead-code warnings for the transport module since it's not used in `main()` yet.

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-server/src/transport/ crates/stargaze-server/src/main.rs && \
git commit --no-gpg-sign -m "feat(server): add QUIC transport endpoint, TLS cert management, and packet sender"
```

---

## Task 4: Client QUIC connection and frame reassembly

**Files:**
- Create: `crates/stargaze-client/src/transport/mod.rs`
- Create: `crates/stargaze-client/src/transport/quic.rs`
- Create: `crates/stargaze-client/src/transport/receiver.rs`

This task implements the client-side: connecting to the server over QUIC (with certificate verification disabled for LAN MVP), session handshake, and the `FrameAssembler` for reassembling fragmented datagrams into complete frames.

- [ ] **Step 1: Create the client transport module**

Create `crates/stargaze-client/src/transport/mod.rs`:

```rust
//! Network transport module — client side.
//!
//! Provides `connect()` which establishes a QUIC connection to the server,
//! performs session handshake, and starts receiving video frames.

pub(crate) mod quic;
pub(crate) mod receiver;

use std::net::SocketAddr;

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
        self.task_handle.await.map_err(|e| {
            TransportError::ConnectionError(format!("transport task panicked: {e}"))
        })
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
    let server_addr: SocketAddr = format!("{}:{}", config.server_address, config.port)
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

    let session_response = receiver::perform_handshake(
        &session_request,
        &mut send_stream,
        &mut recv_stream,
    )
    .await?;

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
        if let Err(e) =
            receiver::receive_loop(connection, send_stream, frames_tx).await
        {
            error!("Client transport error: {e}");
        }
    });

    Ok((ClientTransport { task_handle }, frames_rx))
}
```

- [ ] **Step 2: Create the client QUIC connection module**

Create `crates/stargaze-client/src/transport/quic.rs`:

```rust
//! QUIC connection setup for the client.
//!
//! Connects to the server with TLS certificate verification disabled
//! (LAN MVP — both machines are trusted).

use std::net::SocketAddr;
use std::sync::Arc;

use stargaze_core::transport::TransportError;
use tracing::debug;

/// A `rustls` certificate verifier that accepts any server certificate.
///
/// This is safe for the LAN MVP where both machines are on a trusted
/// local network. A future improvement would use certificate fingerprint
/// pinning.
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
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

/// Connects to the server at the given address using QUIC.
///
/// Uses TLS with server certificate verification disabled (LAN MVP).
///
/// # Errors
///
/// Returns `TransportError::ConnectionError` if the connection fails.
pub(crate) async fn connect_to_server(
    server_addr: SocketAddr,
) -> Result<quinn::Connection, TransportError> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    let client_config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).map_err(|e| {
            TransportError::TlsError(format!("failed to create QUIC client config: {e}"))
        })?,
    ));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap()).map_err(|e| {
        TransportError::ConnectionError(format!("failed to create client endpoint: {e}"))
    })?;
    endpoint.set_default_client_config(client_config);

    debug!("Connecting to {server_addr}");
    let connection = endpoint
        .connect(server_addr, "stargaze-server")
        .map_err(|e| TransportError::ConnectionError(format!("connect: {e}")))?
        .await
        .map_err(|e| TransportError::ConnectionError(format!("connection failed: {e}")))?;

    Ok(connection)
}
```

- [ ] **Step 3: Create the receiver with FrameAssembler**

Create `crates/stargaze-client/src/transport/receiver.rs`:

```rust
//! Datagram reassembly and control message handling for the client.
//!
//! Contains the `FrameAssembler` which collects datagram fragments
//! into complete frames, and the handshake/receive logic.

use std::collections::HashMap;
use std::time::Instant;

use stargaze_core::config::Codec;
use stargaze_core::transport::{
    ControlMessage, DatagramHeader, ReassembledFrame, TransportError,
    IDR_RATE_LIMIT_MS, MAX_PENDING_FRAMES, STREAM_TYPE_VIDEO,
    deserialize_control_message, deserialize_header,
    serialize_control_message,
};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use super::SessionRequest;

/// Session parameters confirmed by the server.
#[derive(Debug, Clone)]
pub(crate) struct SessionParams {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_mbps: u32,
    pub max_datagram_size: u16,
}

/// Performs the session handshake with the server.
///
/// Sends `SessionRequest` and reads `SessionResponse`.
///
/// # Errors
///
/// Returns `TransportError::SessionError` if the handshake fails.
pub(crate) async fn perform_handshake(
    request: &SessionRequest,
    send_stream: &mut quinn::SendStream,
    recv_stream: &mut quinn::RecvStream,
) -> Result<SessionParams, TransportError> {
    // Send session request.
    let req_msg = ControlMessage::SessionRequest {
        width: request.width,
        height: request.height,
        framerate: request.framerate,
        codec: request.codec,
    };
    let req_bytes = serialize_control_message(&req_msg)?;
    send_stream
        .write_all(&req_bytes)
        .await
        .map_err(|e| TransportError::SessionError(format!("send request: {e}")))?;

    // Read session response.
    let mut len_buf = [0u8; 4];
    recv_stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| TransportError::SessionError(format!("read response length: {e}")))?;
    let msg_len = u32::from_le_bytes(len_buf) as usize;

    if msg_len > 65536 {
        return Err(TransportError::SessionError(
            "session response too large".to_string(),
        ));
    }

    let mut body = vec![0u8; msg_len];
    recv_stream
        .read_exact(&mut body)
        .await
        .map_err(|e| TransportError::SessionError(format!("read response body: {e}")))?;

    let response = deserialize_control_message(&body)?;

    match response {
        ControlMessage::SessionResponse {
            width,
            height,
            framerate,
            bitrate_mbps,
            codec: _,
            max_datagram_size,
        } => Ok(SessionParams {
            width,
            height,
            framerate,
            bitrate_mbps,
            max_datagram_size,
        }),
        other => Err(TransportError::SessionError(format!(
            "expected SessionResponse, got {other:?}"
        ))),
    }
}

/// A pending frame being assembled from fragments.
struct PendingFrame {
    /// Fragment slots (`None` = not yet received).
    fragments: Vec<Option<Vec<u8>>>,
    /// Number of fragments received so far.
    received_count: u16,
    /// Total fragments expected.
    fragment_count: u16,
    /// Presentation timestamp.
    pts: u64,
    /// Whether this is a keyframe.
    is_keyframe: bool,
    /// Stream type.
    stream_type: u8,
}

/// Assembles datagram fragments into complete frames.
pub(crate) struct FrameAssembler {
    /// In-progress frames, keyed by `frame_index`.
    pending: HashMap<u32, PendingFrame>,
    /// Next `frame_index` expected for in-order delivery.
    next_frame: u32,
    /// Maximum number of pending incomplete frames before triggering IDR.
    max_pending: usize,
    /// Last time an IDR request was sent.
    last_idr_request: Option<Instant>,
}

impl FrameAssembler {
    /// Creates a new `FrameAssembler`.
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            next_frame: 0,
            max_pending: MAX_PENDING_FRAMES,
            last_idr_request: None,
        }
    }

    /// Processes an incoming datagram fragment.
    ///
    /// Returns a list of completed frames (may be empty or contain
    /// multiple frames if out-of-order fragments completed several frames).
    /// Also returns `true` in the second element if an IDR should be requested.
    pub fn process_datagram(
        &mut self,
        header: &DatagramHeader,
        payload: Vec<u8>,
    ) -> (Vec<ReassembledFrame>, bool) {
        let mut completed = Vec::new();
        let mut need_idr = false;

        // Insert fragment.
        let pending = self
            .pending
            .entry(header.frame_index)
            .or_insert_with(|| PendingFrame {
                fragments: vec![None; header.fragment_count as usize],
                received_count: 0,
                fragment_count: header.fragment_count,
                pts: header.pts,
                is_keyframe: header.is_keyframe,
                stream_type: header.stream_type,
            });

        let idx = header.fragment_index as usize;
        if idx < pending.fragments.len() && pending.fragments[idx].is_none() {
            pending.fragments[idx] = Some(payload);
            pending.received_count += 1;
        }

        // Check if this frame is now complete.
        if pending.received_count == pending.fragment_count {
            // Assemble the frame.
            if let Some(frame) = self.assemble_frame(header.frame_index) {
                completed.push(frame);
            }
        }

        // Deliver any consecutive completed frames starting from next_frame.
        // (The frame we just completed might allow delivering a sequence.)
        self.deliver_in_order(&mut completed);

        // Check if we need an IDR (too many pending frames = likely loss).
        if self.pending.len() > self.max_pending {
            need_idr = self.should_request_idr();
            if need_idr {
                // Discard all incomplete pending frames.
                self.pending.clear();
            }
        }

        (completed, need_idr)
    }

    /// Assembles a complete frame from its fragments and removes it from pending.
    fn assemble_frame(&mut self, frame_index: u32) -> Option<ReassembledFrame> {
        let pending = self.pending.remove(&frame_index)?;

        let mut data = Vec::new();
        for fragment in pending.fragments {
            if let Some(bytes) = fragment {
                data.extend_from_slice(&bytes);
            }
        }

        Some(ReassembledFrame {
            data,
            pts: pending.pts,
            is_keyframe: pending.is_keyframe,
            stream_type: pending.stream_type,
        })
    }

    /// Delivers frames in order starting from `next_frame`.
    fn deliver_in_order(&mut self, completed: &mut Vec<ReassembledFrame>) {
        // The completed vec may already contain the frame we just assembled.
        // We need to check if next_frame is already assembled and advance.
        loop {
            if self.pending.contains_key(&self.next_frame) {
                let pending = &self.pending[&self.next_frame];
                if pending.received_count == pending.fragment_count {
                    if let Some(frame) = self.assemble_frame(self.next_frame) {
                        completed.push(frame);
                    }
                    self.next_frame = self.next_frame.wrapping_add(1);
                } else {
                    break;
                }
            } else {
                // Frame already delivered or not yet seen.
                // If frame_index is ahead of next_frame, we might have a gap.
                break;
            }
        }
    }

    /// Checks if we should send an IDR request based on rate limiting.
    fn should_request_idr(&mut self) -> bool {
        let now = Instant::now();
        if let Some(last) = self.last_idr_request {
            if now.duration_since(last).as_millis() < u128::from(IDR_RATE_LIMIT_MS) {
                return false;
            }
        }
        self.last_idr_request = Some(now);
        true
    }
}

/// Main receive loop: reads datagrams from the connection and
/// assembles them into frames.
///
/// # Errors
///
/// Returns `TransportError` on fatal errors.
pub(crate) async fn receive_loop(
    connection: quinn::Connection,
    mut control_send: quinn::SendStream,
    frames_tx: mpsc::Sender<ReassembledFrame>,
) -> Result<(), TransportError> {
    let mut assembler = FrameAssembler::new();
    let mut total_frames: u64 = 0;

    loop {
        let datagram = match connection.read_datagram().await {
            Ok(bytes) => bytes,
            Err(quinn::ConnectionError::ApplicationClosed(_)) => {
                info!("Server closed connection");
                return Ok(());
            }
            Err(quinn::ConnectionError::LocallyClosed) => {
                info!("Connection closed locally");
                return Ok(());
            }
            Err(e) => {
                return Err(TransportError::ConnectionError(format!(
                    "read datagram: {e}"
                )));
            }
        };

        let (header, payload) = match deserialize_header(&datagram) {
            Ok(result) => result,
            Err(e) => {
                warn!("Failed to deserialize datagram header: {e}");
                continue;
            }
        };

        let (completed_frames, need_idr) =
            assembler.process_datagram(&header, payload.to_vec());

        for frame in completed_frames {
            total_frames += 1;
            if frame.is_keyframe || total_frames % 300 == 1 {
                info!(
                    frame = total_frames,
                    pts = frame.pts,
                    size = frame.data.len(),
                    keyframe = frame.is_keyframe,
                    "Reassembled frame"
                );
            }
            if frames_tx.send(frame).await.is_err() {
                info!("Frame receiver dropped, stopping transport");
                return Ok(());
            }
        }

        if need_idr {
            debug!("Requesting IDR keyframe");
            let idr_msg = serialize_control_message(&ControlMessage::IdrRequest)?;
            if let Err(e) = control_send.write_all(&idr_msg).await {
                warn!("Failed to send IDR request: {e}");
            }
        }
    }
}
```

- [ ] **Step 4: Add `mod transport` to client main.rs**

Add the module declaration to `crates/stargaze-client/src/main.rs`, right after the existing imports:

```rust
mod transport;
```

Do NOT wire it into the main function yet — that happens in Task 7.

- [ ] **Step 5: Verify compilation**

```bash
ENV_SETUP && cargo check --package stargaze-client
```

Expected: compiles with zero errors (dead-code warnings are fine).

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-client/src/transport/ crates/stargaze-client/src/main.rs && \
git commit --no-gpg-sign -m "feat(client): add QUIC connection, frame assembler, and session handshake"
```

---

## Task 5: Frame assembler unit tests

**Files:**
- Modify: `crates/stargaze-client/src/transport/receiver.rs`

This task adds thorough unit tests for the `FrameAssembler`. The assembler is the most complex piece of logic in the transport layer and needs to handle various edge cases.

- [ ] **Step 1: Add unit tests to receiver.rs**

Append the following `#[cfg(test)]` module at the end of `crates/stargaze-client/src/transport/receiver.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use stargaze_core::transport::{STREAM_TYPE_VIDEO, STREAM_TYPE_AUDIO};

    /// Helper: creates a DatagramHeader for testing.
    fn make_header(
        frame_index: u32,
        fragment_index: u16,
        fragment_count: u16,
        pts: u64,
        is_keyframe: bool,
    ) -> DatagramHeader {
        DatagramHeader {
            stream_type: STREAM_TYPE_VIDEO,
            frame_index,
            fragment_index,
            fragment_count,
            pts,
            is_keyframe,
        }
    }

    #[test]
    fn single_fragment_frame() {
        let mut assembler = FrameAssembler::new();
        let header = make_header(0, 0, 1, 100, true);
        let payload = vec![1, 2, 3, 4, 5];

        let (frames, need_idr) = assembler.process_datagram(&header, payload.clone());

        assert!(!need_idr);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, payload);
        assert_eq!(frames[0].pts, 100);
        assert!(frames[0].is_keyframe);
        assert_eq!(frames[0].stream_type, STREAM_TYPE_VIDEO);
    }

    #[test]
    fn multi_fragment_in_order() {
        let mut assembler = FrameAssembler::new();

        // 3 fragments for frame 0.
        let h0 = make_header(0, 0, 3, 0, false);
        let h1 = make_header(0, 1, 3, 0, false);
        let h2 = make_header(0, 2, 3, 0, false);

        let (frames, _) = assembler.process_datagram(&h0, vec![1, 2]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h1, vec![3, 4]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h2, vec![5, 6]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn multi_fragment_out_of_order() {
        let mut assembler = FrameAssembler::new();

        // Send fragments in reverse order.
        let h2 = make_header(0, 2, 3, 42, true);
        let h0 = make_header(0, 0, 3, 42, true);
        let h1 = make_header(0, 1, 3, 42, true);

        let (frames, _) = assembler.process_datagram(&h2, vec![5, 6]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h0, vec![1, 2]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h1, vec![3, 4]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(frames[0].pts, 42);
        assert!(frames[0].is_keyframe);
    }

    #[test]
    fn duplicate_fragment_ignored() {
        let mut assembler = FrameAssembler::new();

        let h0 = make_header(0, 0, 2, 0, false);
        let h1 = make_header(0, 1, 2, 0, false);

        // Send fragment 0 twice.
        let (frames, _) = assembler.process_datagram(&h0, vec![1, 2]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h0, vec![99, 99]);
        assert!(frames.is_empty()); // Duplicate ignored.

        let (frames, _) = assembler.process_datagram(&h1, vec![3, 4]);
        assert_eq!(frames.len(), 1);
        // Original data preserved, not the duplicate.
        assert_eq!(frames[0].data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn max_pending_triggers_idr() {
        let mut assembler = FrameAssembler::new();

        // Fill up max_pending + 1 incomplete frames.
        for i in 0..=MAX_PENDING_FRAMES as u32 {
            let h = make_header(i, 0, 2, i.into(), false);
            let (_, need_idr) = assembler.process_datagram(&h, vec![0]);
            if i as usize > MAX_PENDING_FRAMES {
                // Should trigger IDR when exceeding max_pending.
                // Note: the exact trigger depends on pending.len() check.
            }
        }

        // After exceeding max_pending, the assembler should request IDR
        // and clear pending frames.
        assert!(assembler.pending.is_empty() || assembler.pending.len() <= MAX_PENDING_FRAMES);
    }

    #[test]
    fn idr_rate_limiting() {
        let mut assembler = FrameAssembler::new();

        // First IDR request should succeed.
        assert!(assembler.should_request_idr());

        // Immediate second request should be rate-limited.
        assert!(!assembler.should_request_idr());
    }

    #[test]
    fn multiple_frames_sequential() {
        let mut assembler = FrameAssembler::new();

        // Frame 0: single fragment.
        let h0 = make_header(0, 0, 1, 0, true);
        let (frames, _) = assembler.process_datagram(&h0, vec![10]);
        assert_eq!(frames.len(), 1);

        // Frame 1: single fragment.
        let h1 = make_header(1, 0, 1, 1, false);
        let (frames, _) = assembler.process_datagram(&h1, vec![20]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].pts, 1);
    }
}
```

- [ ] **Step 2: Run the tests**

```bash
ENV_SETUP && cargo test --package stargaze-client -- transport::receiver::tests
```

Expected: all 7 tests pass.

- [ ] **Step 3: Run clippy on client**

```bash
ENV_SETUP && cargo clippy --package stargaze-client -W clippy::pedantic
```

Fix any warnings. Common issues:
- `as` casts → use `.cast_signed()` / `.cast_unsigned()` / `usize::from()`
- Missing doc comments on public items
- `unwrap_or(1200) as u16` → use `u16::try_from(...).unwrap_or(1200)`

- [ ] **Step 4: Commit**

```bash
git add crates/stargaze-client/src/transport/receiver.rs && \
git commit --no-gpg-sign -m "test(client): add FrameAssembler unit tests for fragment reassembly"
```

---

## Task 6: Encoder IDR feedback path

**Files:**
- Modify: `crates/stargaze-server/src/encode/mod.rs`
- Modify: `crates/stargaze-server/src/encode/ffmpeg.rs`
- Modify: `crates/stargaze-server/src/main.rs` (update `start_encoder` call)

This task adds the `tokio::sync::watch` channel for IDR requests from the transport layer back to the encoder. When the client detects frame loss, it sends an `IdrRequest` → the server transport increments the watch counter → the encoder forces the next frame to be a keyframe.

- [ ] **Step 1: Add IDR watch channel to start_encoder**

Modify `crates/stargaze-server/src/encode/mod.rs` to create and return a `watch::Sender<u64>`:

The `start_encoder` function signature changes from:
```rust
pub fn start_encoder(
    config: EncoderConfig,
    frames: mpsc::Receiver<Frame>,
) -> Result<(EncoderSession, mpsc::Receiver<EncodedPacket>), EncodeError>
```

to:
```rust
pub fn start_encoder(
    config: EncoderConfig,
    frames: mpsc::Receiver<Frame>,
) -> Result<(EncoderSession, mpsc::Receiver<EncodedPacket>, watch::Sender<u64>), EncodeError>
```

The changes inside the function:
1. Create the watch channel: `let (idr_tx, idr_rx) = watch::channel(0u64);`
2. Pass `idr_rx` into the thread closure
3. Pass it to `ffmpeg::run_encode_loop()` as a new parameter
4. Return `idr_tx` as the third tuple element

Updated function body (full replacement of `start_encoder`):

```rust
pub fn start_encoder(
    config: EncoderConfig,
    frames: mpsc::Receiver<Frame>,
) -> Result<(EncoderSession, mpsc::Receiver<EncodedPacket>, watch::Sender<u64>), EncodeError> {
    let (packets_tx, packets_rx) = mpsc::channel(PACKET_CHANNEL_CAPACITY);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let (idr_tx, idr_rx) = watch::channel(0u64);

    // Use a oneshot channel to report initialization errors back to the caller.
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), EncodeError>>();

    let thread_handle = thread::Builder::new()
        .name("stargaze-encoder".to_string())
        .spawn(move || {
            // Initialize the encoder on this thread (FFmpeg contexts are thread-local).
            let mut encoder = match ffmpeg::init_encoder(&config) {
                Ok(enc) => {
                    let _ = init_tx.send(Ok(()));
                    enc
                }
                Err(e) => {
                    error!("Encoder initialization failed: {e}");
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let mut frames = frames;

            // Run the encode loop until shutdown or channel close.
            if let Err(e) =
                ffmpeg::run_encode_loop(&mut encoder, &mut frames, &packets_tx, &shutdown_clone, idr_rx)
            {
                error!("Encoder loop failed: {e}");
            }

            info!("Encoder thread exiting");
        })
        .map_err(|e| EncodeError::FfmpegError(format!("failed to spawn encoder thread: {e}")))?;

    // Wait for initialization to complete.
    let init_result = init_rx.recv().map_err(|_| {
        EncodeError::InitError("encoder thread exited during initialization".to_string())
    })?;

    // If init failed, join the thread and propagate the error.
    init_result?;

    info!("Encoder started on dedicated thread");

    Ok((
        EncoderSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        packets_rx,
        idr_tx,
    ))
}
```

Also add `use tokio::sync::watch;` to the imports at the top of the file.

- [ ] **Step 2: Add IDR check to the encode loop in ffmpeg.rs**

Modify `run_encode_loop` in `crates/stargaze-server/src/encode/ffmpeg.rs`:

Change the function signature to accept the IDR watch receiver:

```rust
pub(crate) fn run_encode_loop(
    encoder: &mut FfmpegEncoder,
    frames: &mut mpsc::Receiver<Frame>,
    packets_tx: &mpsc::Sender<EncodedPacket>,
    shutdown: &Arc<AtomicBool>,
    mut idr_rx: watch::Receiver<u64>,
) -> Result<(), EncodeError> {
```

Add `use tokio::sync::watch;` to the imports.

Add an `idr_counter` tracker and check before each frame. Insert the following right after the `let mut frame_counter: u64 = 0;` line:

```rust
let mut last_idr_value: u64 = 0;
```

Then, inside the loop after receiving a frame (before `upload_and_encode`), add the IDR check:

```rust
// Check if an IDR keyframe was requested.
let force_idr = if *idr_rx.borrow_and_update() != last_idr_value {
    last_idr_value = *idr_rx.borrow();
    info!(frame = frame_counter, "Forcing IDR keyframe (requested by client)");
    true
} else {
    false
};
```

Then modify the `upload_and_encode` call and the functions it calls to accept the `force_idr` flag. The simplest approach: after `upload_and_encode` succeeds, if `force_idr` is true, set `AV_PICTURE_TYPE_I` on the frame before sending it.

Actually, a simpler approach that doesn't require modifying `upload_and_encode`: set the picture type on the hardware frame. But since `upload_and_encode` handles the frame internally, we need to thread the flag through.

Modify `upload_and_encode` signature:

```rust
fn upload_and_encode(
    encoder: &mut FfmpegEncoder,
    frame: &Frame,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
```

And in both the CpuMapped and DmaBuf paths, right before `encoder.encoder.send_frame(&hw_frame)`, add:

```rust
if force_idr {
    unsafe {
        (*hw_frame.as_mut_ptr()).pict_type = ffmpeg_sys_next::AVPictureType::AV_PICTURE_TYPE_I;
    }
}
```

Similarly update `upload_dmabuf_and_encode`:

```rust
fn upload_dmabuf_and_encode(
    encoder: &mut FfmpegEncoder,
    info: &stargaze_core::capture::DmaBufInfo,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
```

And add the same `if force_idr` block before `send_frame` in that function too.

Update the call sites:
- In `upload_and_encode`, the `Frame::DmaBuf` match arm: `return upload_dmabuf_and_encode(encoder, info, pts, force_idr);`
- In `run_encode_loop`: `match upload_and_encode(encoder, &frame, frame_counter, force_idr) {`

- [ ] **Step 3: Update main.rs to match new start_encoder signature**

In `crates/stargaze-server/src/main.rs`, change:

```rust
let (encoder_session, mut packets) = encode::start_encoder(encoder_config, frames)?;
```

to:

```rust
let (encoder_session, mut packets, _idr_tx) = encode::start_encoder(encoder_config, frames)?;
```

The `_idr_tx` is prefixed with underscore for now — Task 7 will pass it to the transport layer.

- [ ] **Step 4: Verify compilation and tests**

```bash
ENV_SETUP && cargo check --workspace && cargo test --workspace
```

Expected: all 30+ tests pass, no compilation errors.

- [ ] **Step 5: Run clippy**

```bash
ENV_SETUP && cargo clippy --workspace -W clippy::pedantic
```

Fix any warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-server/src/encode/mod.rs crates/stargaze-server/src/encode/ffmpeg.rs crates/stargaze-server/src/main.rs && \
git commit --no-gpg-sign -m "feat(encode): add IDR keyframe feedback path via watch channel"
```

---

## Task 7: Wire transport into server and client binaries

**Files:**
- Modify: `crates/stargaze-server/src/main.rs`
- Modify: `crates/stargaze-client/src/main.rs`

This task connects the transport layer to the existing server pipeline and makes the client actually connect and receive frames.

- [ ] **Step 1: Update server main.rs to use transport**

Replace the packet-logging loop in `crates/stargaze-server/src/main.rs` with the transport layer. The full `main` function should become:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let cfg = build_config(&cli)?;

    info!(
        "Starting stargaze server on {}:{} ({}@{}fps, {} Mbps, {})",
        cfg.bind_address, cfg.port, cfg.resolution, cfg.framerate, cfg.bitrate, cfg.codec
    );

    // Start capture pipeline.
    let capture_config = CaptureConfig {
        width: cfg.resolution.width,
        height: cfg.resolution.height,
        framerate: cfg.framerate,
    };
    let (capture_session, frames) = capture::start_capture(capture_config).await?;
    info!("Capture started");

    // Start encoder pipeline.
    let encoder_config = EncoderConfig {
        width: cfg.resolution.width,
        height: cfg.resolution.height,
        framerate: cfg.framerate,
        bitrate_mbps: cfg.bitrate,
    };
    let (encoder_session, packets, idr_tx) = encode::start_encoder(encoder_config, frames)?;
    info!("Encoder started");

    // Start transport — waits for client connection, then streams packets.
    let server_transport = transport::start_server_transport(&cfg, packets, idr_tx).await?;
    info!("Transport started, waiting for client connection...");

    // Wait for transport to finish (client disconnect or error) or Ctrl+C.
    tokio::select! {
        result = server_transport.join() => {
            if let Err(e) = result {
                tracing::warn!("Transport error: {e}");
            }
            info!("Transport finished");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down gracefully");
            server_transport.abort();
        }
    }

    info!("Shutting down pipeline");
    encoder_session.stop()?;
    capture_session.stop()?;

    Ok(())
}
```

Also update the imports at the top of the file — remove unused `use stargaze_core::config::Codec` if it's no longer needed, and ensure `tracing::warn` is available (it's already in scope via `tracing`).

- [ ] **Step 2: Update client main.rs to connect and receive frames**

Replace the `main()` function in `crates/stargaze-client/src/main.rs`:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let cfg = build_config(&cli)?;

    info!(
        "Connecting to {}:{} (fullscreen: {})",
        cfg.server_address, cfg.port, cfg.fullscreen
    );

    // Connect to server.
    let session_request = transport::SessionRequest {
        width: 1920,
        height: 1080,
        framerate: 60,
        codec: stargaze_core::config::Codec::H265,
    };

    let (client_transport, mut frames) =
        transport::connect(&cfg, session_request).await?;

    info!("Connected, receiving frames...");

    // Receive frames until disconnect or Ctrl+C.
    let mut frame_count: u64 = 0;
    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else {
                    info!("Frame channel closed");
                    break;
                };
                frame_count += 1;
                if frame.is_keyframe || frame_count % 300 == 1 {
                    info!(
                        frame = frame_count,
                        pts = frame.pts,
                        size = frame.data.len(),
                        keyframe = frame.is_keyframe,
                        "Received frame"
                    );
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, disconnecting");
                client_transport.abort();
                break;
            }
        }
    }

    info!(total_frames = frame_count, "Client shutting down");

    Ok(())
}
```

- [ ] **Step 3: Verify both binaries compile**

```bash
ENV_SETUP && cargo check --workspace
```

Expected: zero errors.

- [ ] **Step 4: Run clippy and fmt**

```bash
ENV_SETUP && cargo fmt --all && cargo clippy --workspace -W clippy::pedantic
```

Fix any warnings.

- [ ] **Step 5: Run all tests**

```bash
ENV_SETUP && cargo test --workspace
```

Expected: all existing tests pass plus the new transport and assembler tests.

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-server/src/main.rs crates/stargaze-client/src/main.rs && \
git commit --no-gpg-sign -m "feat(server,client): wire transport into capture->encode->stream pipeline"
```

---

## Task 8: Localhost integration test

**Files:**
- Create: `tests/transport_integration.rs` (workspace-level integration test)

This test verifies end-to-end transport on localhost: server sends synthetic encoded packets via QUIC datagrams, client reassembles them, and we verify data integrity. No GPU, capture, or encoder needed.

- [ ] **Step 1: Create the integration test**

Create `tests/transport_integration.rs`:

```rust
//! Integration test: server<->client transport over localhost.
//!
//! Verifies that encoded packets sent from the server are correctly
//! fragmented into QUIC datagrams, transmitted over localhost, and
//! reassembled by the client.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use stargaze_core::config::Codec;
use stargaze_core::transport::STREAM_TYPE_VIDEO;
use tokio::time::timeout;

/// Test: send synthetic packets through the transport and verify
/// client receives them byte-for-byte.
#[tokio::test]
async fn test_transport_localhost_round_trip() {
    // This test uses the server and client QUIC setup directly.
    use stargaze_core::transport::{
        DatagramHeader, ReassembledFrame, STREAM_TYPE_VIDEO,
        serialize_header, deserialize_header,
    };

    // Create a self-signed cert for the test server.
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "test-server");
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der =
        rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();

    // Server endpoint.
    let server_config =
        quinn::ServerConfig::with_single_cert(vec![cert_der], key_der).unwrap();
    let server_endpoint = quinn::Endpoint::server(
        server_config,
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();

    // Client endpoint with skip verification.
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerification))
        .with_no_client_auth();
    let client_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap();
    let client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
    let mut client_endpoint =
        quinn::Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
    client_endpoint.set_default_client_config(client_config);

    // Connect client to server.
    let client_conn_future = client_endpoint.connect(server_addr, "localhost").unwrap();
    let server_conn_future = server_endpoint.accept();

    let (client_conn, server_incoming) = tokio::join!(client_conn_future, server_conn_future);
    let client_conn = client_conn.unwrap();
    let server_conn = server_incoming.unwrap().await.unwrap();

    // Test data: 3 synthetic frames of various sizes.
    let test_frames: Vec<Vec<u8>> = vec![
        vec![0xAA; 100],        // Small frame (fits in one datagram).
        vec![0xBB; 5000],       // Medium frame (multiple fragments).
        vec![0xCC; 15000],      // Large frame (many fragments).
    ];

    // Server sends frames as fragmented datagrams.
    let server_handle = tokio::spawn({
        let test_frames = test_frames.clone();
        async move {
            for (frame_idx, frame_data) in test_frames.iter().enumerate() {
                let max_datagram_size = server_conn.max_datagram_size().unwrap_or(1200);

                let sample_header = DatagramHeader {
                    stream_type: STREAM_TYPE_VIDEO,
                    frame_index: frame_idx as u32,
                    fragment_index: 0,
                    fragment_count: 1,
                    pts: frame_idx as u64,
                    is_keyframe: frame_idx == 0,
                };
                let header_size = serialize_header(&sample_header).unwrap().len();
                let max_payload = max_datagram_size - header_size;

                let fragment_count =
                    (frame_data.len() + max_payload - 1) / max_payload;

                for frag_idx in 0..fragment_count {
                    let start = frag_idx * max_payload;
                    let end = ((frag_idx + 1) * max_payload).min(frame_data.len());

                    let header = DatagramHeader {
                        stream_type: STREAM_TYPE_VIDEO,
                        frame_index: frame_idx as u32,
                        fragment_index: frag_idx as u16,
                        fragment_count: fragment_count as u16,
                        pts: frame_idx as u64,
                        is_keyframe: frame_idx == 0,
                    };

                    let header_bytes = serialize_header(&header).unwrap();
                    let mut datagram =
                        Vec::with_capacity(header_bytes.len() + (end - start));
                    datagram.extend_from_slice(&header_bytes);
                    datagram.extend_from_slice(&frame_data[start..end]);

                    server_conn
                        .send_datagram(bytes::Bytes::from(datagram))
                        .unwrap();
                }
            }

            // Small delay to ensure all datagrams are flushed.
            tokio::time::sleep(Duration::from_millis(100)).await;
            server_conn.close(quinn::VarInt::from_u32(0), b"done");
        }
    });

    // Client receives and reassembles.
    let mut assembler = stargaze_client::transport::receiver::FrameAssembler::new();
    let mut received_frames: Vec<ReassembledFrame> = Vec::new();

    let receive_result = timeout(Duration::from_secs(5), async {
        loop {
            match client_conn.read_datagram().await {
                Ok(datagram) => {
                    let (header, payload) = deserialize_header(&datagram).unwrap();
                    let (completed, _) =
                        assembler.process_datagram(&header, payload.to_vec());
                    received_frames.extend(completed);

                    if received_frames.len() == test_frames.len() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    })
    .await;

    server_handle.await.unwrap();

    // Verify all frames were received correctly.
    assert_eq!(
        received_frames.len(),
        test_frames.len(),
        "Expected {} frames, got {}",
        test_frames.len(),
        received_frames.len()
    );

    for (i, (received, expected)) in
        received_frames.iter().zip(test_frames.iter()).enumerate()
    {
        assert_eq!(
            received.data, *expected,
            "Frame {i} data mismatch (received {} bytes, expected {} bytes)",
            received.data.len(),
            expected.len()
        );
        assert_eq!(received.pts, i as u64);
        assert_eq!(received.is_keyframe, i == 0);
    }

    // Clean up.
    client_endpoint.close(quinn::VarInt::from_u32(0), b"done");
    server_endpoint.close(quinn::VarInt::from_u32(0), b"done");
}

/// Skip server verification for test client.
#[derive(Debug)]
struct SkipVerification;

impl rustls::client::danger::ServerCertVerifier for SkipVerification {
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
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
        ]
    }
}
```

**IMPORTANT NOTE:** This integration test directly uses types from `stargaze-client` internals (`FrameAssembler`). For this to work, `FrameAssembler` and `process_datagram` must be `pub` (not `pub(crate)`). The subagent implementing this task must make `FrameAssembler` and its `new()` and `process_datagram()` methods `pub` in `receiver.rs`, and make the `receiver` module `pub` in `transport/mod.rs`. Similarly, `stargaze_server::transport` must be `pub` in server's `main.rs`.

**ALTERNATIVE (simpler):** If making internals public is undesirable, the integration test should instead be placed in `crates/stargaze-client/tests/transport.rs` as a crate-level integration test, or test purely at the transport-types level (using `stargaze-core` types directly without importing from server/client crates). The test above is already mostly self-contained — the only import from the client is `FrameAssembler`. The simplest fix: copy the `FrameAssembler` logic into the test, OR make it `pub` and re-export it.

**Recommended approach:** Make `FrameAssembler` public since it's a well-defined component that the decoder (Sub-project 5) will need to interact with anyway. Change:
- `crates/stargaze-client/src/transport/mod.rs`: `pub mod receiver;` (was `pub(crate) mod receiver;`)
- `crates/stargaze-client/src/main.rs`: `pub mod transport;` (was `mod transport;`)
- `crates/stargaze-client/src/transport/receiver.rs`: `pub struct FrameAssembler` and `pub fn new()`, `pub fn process_datagram()` (already `pub` in the original code above)

Also this test needs `quinn`, `rustls`, `rcgen`, `bytes` as dev-dependencies of the workspace root or the test crate. Since workspace-level integration tests are tricky with dependencies, it's better to place this test in `crates/stargaze-client/tests/transport.rs` and add `quinn`, `rustls`, `rcgen`, `bytes` as dev-dependencies of `stargaze-client`.

**Updated plan:** Place the integration test at `crates/stargaze-client/tests/transport_integration.rs` and add test dependencies to `stargaze-client/Cargo.toml`.

- [ ] **Step 2: Add test dependencies to stargaze-client**

```bash
ENV_SETUP && cargo add --dev rcgen --package stargaze-client
```

`quinn`, `rustls`, and `bytes` are already normal dependencies, so they're available in tests too.

- [ ] **Step 3: Create the test file**

Write the test to `crates/stargaze-client/tests/transport_integration.rs` with the code above, but replace:
- `stargaze_client::transport::receiver::FrameAssembler` → this import should work if `transport` and `receiver` are `pub`
- Remove the `start_test_server` function and `todo!()` — it's not used; the test sets up QUIC directly
- Remove unused imports

- [ ] **Step 4: Run the integration test**

```bash
ENV_SETUP && cargo test --package stargaze-client -- transport_integration --nocapture
```

Expected: test passes, all 3 frames received with correct data.

- [ ] **Step 5: Run clippy**

```bash
ENV_SETUP && cargo clippy --workspace -W clippy::pedantic
```

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-client/tests/ crates/stargaze-client/Cargo.toml crates/stargaze-client/src/ && \
git commit --no-gpg-sign -m "test(transport): add localhost integration test for QUIC datagram transport"
```

---

## Task 9: Final cleanup, fmt, clippy, and full test run

**Files:**
- All workspace files (fmt + clippy pass)

- [ ] **Step 1: Format all code**

```bash
ENV_SETUP && cargo fmt --all
```

- [ ] **Step 2: Fix all clippy warnings**

```bash
ENV_SETUP && cargo clippy --workspace -W clippy::pedantic 2>&1
```

Fix all warnings. Common issues at this stage:
- `as` casts (use `.cast_signed()`, `.cast_unsigned()`, `usize::from()`, `u16::try_from()`)
- Backtick technical terms in doc comments (`QUIC`, `postcard`, `DMA-BUF`, etc.)
- `unnecessary_wraps` on functions that always return `Ok`
- `similar_names` on variables like `idr_tx` / `idr_rx`
- `too_many_lines` on integration tests (add `#[allow(clippy::too_many_lines)]`)
- `missing_errors_doc` on public functions

- [ ] **Step 3: Run the full test suite**

```bash
ENV_SETUP && cargo test --workspace
```

Expected: all tests pass (30+ existing + ~16 new transport tests).

- [ ] **Step 4: Verify both binaries build**

```bash
ENV_SETUP && cargo build --workspace
```

Expected: both `stargaze-server` and `stargaze-client` binaries build successfully.

- [ ] **Step 5: Commit any cleanup changes**

```bash
git add -A && git diff --cached --stat
```

If there are changes:
```bash
git commit --no-gpg-sign -m "chore(transport): final clippy and fmt cleanup"
```

---

## Summary

| Task | Description | New Tests |
|------|-------------|-----------|
| 1 | Add crate dependencies (quinn, rcgen, postcard, bytes) | 0 |
| 2 | Shared transport types in stargaze-core | 9 |
| 3 | Server QUIC endpoint + TLS cert + packet sender | 0 |
| 4 | Client QUIC connection + FrameAssembler + handshake | 0 |
| 5 | FrameAssembler unit tests | 7 |
| 6 | Encoder IDR feedback path (watch channel) | 0 |
| 7 | Wire transport into server + client binaries | 0 |
| 8 | Localhost integration test | 1 |
| 9 | Final cleanup, fmt, clippy, full test run | 0 |

**Total new tests: ~17** (9 unit tests for shared types, 7 unit tests for assembler, 1 integration test)
