# Network Transport Design — Sub-project 4

**Date:** 2026-03-31
**Status:** Draft
**Sub-project:** 4 of 9 (Network Transport)

## Overview

Transport encoded video (and later audio) packets from server to client over the network, with a control channel for session management and feedback. This sub-project takes `EncodedPacket` values from the encoding pipeline (Sub-project 3) on the server and delivers reassembled frame data to the client for decoding (Sub-project 5). It also provides a bidirectional control channel that later sub-projects use for input forwarding (Sub-project 8) and audio configuration (Sub-project 6).

Both server-side sending and client-side receiving are implemented in this sub-project, enabling end-to-end transport testing with synthetic data — no real capture, encoder, or GPU required.

## Approach

**QUIC via `quinn` + `rustls`.** A single QUIC connection multiplexes all data (video datagrams, audio datagrams, control messages) over one UDP port. QUIC provides built-in TLS encryption, congestion control, and multiplexing.

**Why QUIC over custom UDP + TCP:**

- **Security for free.** QUIC mandates TLS 1.3. Even for LAN MVP, this eliminates the "add encryption later" tech debt. Industry-tested crypto via `rustls` rather than hand-rolled AES.
- **Congestion control.** Quinn implements congestion control algorithms that adapt to network conditions. Raw UDP requires building this from scratch or risking packet storms.
- **Single port.** No need for separate video, audio, and control ports. One `address:port` pair handles everything via QUIC's multiplexing.
- **Unreliable datagrams.** QUIC RFC 9221 defines unreliable datagrams — fire-and-forget delivery like raw UDP, but within the encrypted QUIC connection. Ideal for low-latency A/V data.
- **Reliable streams.** Control messages use a reliable bidirectional QUIC stream with ordering and retransmission guarantees, replacing the need for a separate TCP channel.

**Why not raw UDP + TCP (Approach A):**

- Must implement encryption, congestion control, and connection management manually.
- Two listening ports (UDP data + TCP control) complicate firewall and NAT config.
- No industry-tested transport security.

**Trade-offs of QUIC:**

- Adds `quinn` + `rustls` + `rcgen` dependencies (~significant compile time increase).
- TLS handshake adds ~1 RTT to connection setup (negligible on LAN).
- QUIC congestion control may add latency under congestion — but quinn allows tuning, and on a LAN this is rarely an issue.
- QUIC datagram MTU (~1200 bytes) requires application-level fragmentation for video frames.

## Data Channel Design

### Three Logical Channels

All channels share a single QUIC connection on the server's configured `address:port`.

**1. Video datagrams** — Unreliable QUIC datagrams carrying fragments of encoded video frames. Each datagram is a self-contained fragment with a header + payload. Lost datagrams are not retransmitted — instead, the client requests a keyframe via the control channel.

**2. Audio datagrams** — Same mechanism as video, with a different stream type tag. Opus audio packets are typically small enough to fit in a single datagram (~200 bytes for 10ms at 128kbps).

**3. Control stream** — A single long-lived bidirectional QUIC reliable stream. Carries session handshake, IDR keyframe requests, keepalive pings, and (in future sub-projects) input events. Messages are length-prefixed and serialized with `postcard`.

### Datagram Packet Format

Each QUIC datagram contains exactly one fragment with this header:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
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
```

The header is serialized with `postcard` (compact binary, deterministic size for a given struct). The remainder of the datagram is the payload — a slice of the encoded frame data.

**Fragment sizing:** The maximum payload per datagram is `connection.max_datagram_size() - serialized_header_size`. Quinn reports the max datagram size from the QUIC transport parameters. Typically ~1200 bytes on the wire, yielding ~1180 bytes of payload after the header.

**Sending a frame (server):**
1. Receive `EncodedPacket` from encoder channel
2. Compute `fragment_count = ceil(data.len() / max_payload_size)`
3. For each fragment: serialize `DatagramHeader` + payload slice, call `connection.send_datagram()`
4. Increment `frame_index`

### Control Message Format

Messages on the control stream are length-prefixed:

```
[4 bytes LE: message length][postcard-serialized ControlMessage]
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Client → Server: request a streaming session.
    SessionRequest {
        width: u32,
        height: u32,
        framerate: u32,
        codec: Codec,
    },
    /// Server → Client: confirm session parameters.
    SessionResponse {
        width: u32,
        height: u32,
        framerate: u32,
        bitrate_mbps: u32,
        codec: Codec,
        max_datagram_size: u16,
    },
    /// Client → Server: request an IDR keyframe (after packet loss).
    IdrRequest,
    /// Bidirectional: keepalive with timestamp.
    Ping { timestamp_ms: u64 },
    /// Bidirectional: keepalive response.
    Pong { timestamp_ms: u64 },
}
```

Future sub-projects extend this enum with `InputEvent { ... }`, `AudioConfig { ... }`, etc. Using `postcard` with serde means adding variants is backward-compatible as long as new variants are appended (postcard uses varint enum discriminants).

## Connection Flow

### Server Startup
1. Check for existing TLS certificate at `~/.config/stargaze/cert.der` + `key.der`
2. If not found, generate a self-signed certificate using `rcgen` and save it
3. Configure quinn `ServerConfig` with the certificate
4. Bind QUIC endpoint to `bind_address:port`
5. Log "Listening on {address}:{port}" and wait for connections

### Client Connection
1. Create quinn `ClientConfig` with certificate verification disabled (LAN MVP)
2. Connect to `server_address:port`
3. Open a bidirectional QUIC stream (the control stream)
4. Send `SessionRequest` with desired resolution, framerate, codec
5. Read `SessionResponse` — server confirms parameters and reports `max_datagram_size`
6. Transition to receiving state: start reading datagrams + listening on control stream

### Session Lifecycle
1. **Handshake** — TCP-like exchange over reliable control stream (as above)
2. **Streaming** — Server sends video/audio datagrams continuously; client reassembles
3. **Feedback** — Client sends `IdrRequest` when it detects frame loss; `Ping`/`Pong` for keepalive
4. **Shutdown** — Either side closes the QUIC connection with `CONNECTION_CLOSE` and an application reason code. Ctrl+C on server triggers graceful shutdown.

## Frame Reassembly (Client)

The client maintains a **frame assembler** — a bounded map of in-progress frames keyed by `frame_index`. When all fragments of a frame arrive, the assembler emits a `ReassembledFrame` containing the concatenated payload data, pts, and keyframe flag. These are delivered to the caller (later: the video decoder) via an `mpsc` channel.

```rust
pub struct FrameAssembler {
    /// In-progress frames, keyed by frame_index.
    pending: HashMap<u32, PendingFrame>,
    /// Next frame_index expected for in-order delivery.
    next_frame: u32,
    /// Maximum number of pending incomplete frames before triggering IDR.
    max_pending: usize,
}

struct PendingFrame {
    /// Fragment slots (None = not yet received).
    fragments: Vec<Option<Vec<u8>>>,
    /// Number of fragments received so far.
    received_count: u16,
    /// Total fragments expected.
    fragment_count: u16,
    /// Presentation timestamp.
    pts: u64,
    /// Whether this is a keyframe.
    is_keyframe: bool,
}
```

**On receiving a datagram:**
1. Deserialize `DatagramHeader`
2. Look up or create `PendingFrame` for `frame_index`
3. Insert payload at `fragments[fragment_index]` (ignore duplicates)
4. If `received_count == fragment_count`, frame is complete
5. Deliver completed frames in order: advance `next_frame`, emit assembled frames
6. If `pending.len() > max_pending` or frame gap detected (newer complete frame but older frame still incomplete), the older frame is lost — request IDR

**IDR request flow:**
1. Client detects irrecoverable frame loss (incomplete frame with newer frames arriving)
2. Client sends `IdrRequest` on control stream
3. Server receives it, signals the encoder to produce a keyframe on the next frame
4. Client discards all pending incomplete frames, waits for the next keyframe to resume clean decoding

**Constants:**
- `max_pending`: 16 frames (generous buffer for LAN; at 60fps this is ~267ms of frames)
- IDR request rate limit: at most once per 500ms (avoid flooding the encoder with IDR requests during sustained loss)

## TLS Certificate Management

**Server:**
- On first run, generate a self-signed X.509 certificate using `rcgen`
- Subject: `stargaze-server` (informational only)
- Validity: 365 days
- Key type: ECDSA P-256 (fast, small)
- Store `cert.der` and `key.der` in `~/.config/stargaze/` (same directory as config files)
- On subsequent runs, load from disk. If expired or missing, regenerate.

**Client:**
- Create a custom `rustls::client::danger::ServerCertVerifier` that accepts any certificate
- This is safe for the LAN MVP where both machines are trusted
- Future improvement: fingerprint pinning (display server cert fingerprint, client verifies)

## Encoder IDR Feedback Path

The server needs a way to signal the encoder to produce a keyframe when the client requests one. This requires a feedback channel from the transport layer back to the encoder.

**Mechanism:** A `tokio::sync::watch` channel carrying an IDR request flag (or counter). The transport module increments it when receiving `IdrRequest`. The encoder thread checks it before each frame — if the counter has changed, it sets `AV_PICTURE_TYPE_I` on the next frame (via `ffmpeg-sys-next` FFI).

This is a small modification to the existing encoder from Sub-project 3:
- Add an `idr_watch: tokio::sync::watch::Receiver<u64>` parameter to `run_encode_loop()`
- Before encoding each frame, check if the IDR counter changed — if so, force the next frame to be an IDR
- The `start_encoder()` function creates the watch channel and returns the sender to the transport layer

## Module Structure

```
crates/stargaze-core/src/
├── lib.rs              # add `pub mod transport;`
├── transport.rs        # NEW — DatagramHeader, ControlMessage, TransportError, constants
├── encode.rs           # existing (no changes)
├── capture.rs          # existing (no changes)
├── config.rs           # existing (no changes)
└── error.rs            # existing (no changes)

crates/stargaze-server/src/
├── main.rs             # modified — wire transport after encoder
├── encode/             # existing, minor modification for IDR feedback
├── capture/            # existing (no changes)
└── transport/
    ├── mod.rs          # public API: TransportSession, start_transport()
    ├── quic.rs         # QUIC endpoint setup, cert generation/loading, connection accept
    └── sender.rs       # frame fragmentation + datagram sending, control message handling

crates/stargaze-client/src/
├── main.rs             # modified — connect to server, receive and log frames
└── transport/
    ├── mod.rs          # public API: ClientSession, connect()
    ├── quic.rs         # QUIC endpoint setup, connection to server
    └── receiver.rs     # datagram reassembly (FrameAssembler), control message handling
```

**Rationale:** Same pattern as capture and encode — shared types in `stargaze-core`, server-side implementation in `stargaze-server/transport/`, client-side in `stargaze-client/transport/`. Splitting `quic.rs` (QUIC/TLS setup) from `sender.rs`/`receiver.rs` (application logic) isolates the QUIC configuration from the streaming protocol.

## Dependencies

### New crate dependencies

**`stargaze-core`:**
- `postcard` (with `alloc` feature) — compact binary serde format for packet headers and control messages
- `serde` already present

**`stargaze-server`:**
- `quinn` — QUIC implementation
- `rustls` — TLS (transitive via quinn, but direct dependency for cert configuration)
- `rcgen` — self-signed certificate generation
- `postcard` — (via stargaze-core, but may need direct dep for serialization calls)

**`stargaze-client`:**
- `quinn` — QUIC implementation
- `rustls` — TLS (for custom cert verifier)
- `postcard` — (via stargaze-core)

### Existing dependencies used
- `tokio` — async runtime, UDP socket (via quinn), channels
- `serde` — serialization framework
- `tracing` — logging
- `anyhow` / `thiserror` — error handling

## Error Handling

```rust
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

    /// Frame reassembly failed.
    #[error("reassembly error: {0}")]
    ReassemblyError(String),
}
```

## Testing Strategy

### Unit tests (no network, run on every `cargo test`)

- **Datagram header serde round-trip:** Serialize and deserialize `DatagramHeader` with various field values, verify equality.
- **Control message serde round-trip:** Serialize and deserialize each `ControlMessage` variant, verify equality.
- **Frame assembler — complete reassembly:** Feed all fragments of a frame in order, verify assembled data matches original.
- **Frame assembler — out-of-order fragments:** Feed fragments in reverse/random order, verify correct reassembly.
- **Frame assembler — duplicate fragments:** Feed the same fragment twice, verify it's handled gracefully (ignored).
- **Frame assembler — gap detection:** Feed fragments for frame N+1 while frame N is incomplete, verify IDR is requested and stale frame discarded.
- **Fragmentation logic:** Fragment a large payload, verify fragment count and that concatenating all fragment payloads reconstructs the original.

### Integration test (localhost, run on every `cargo test`)

- Spin up server QUIC endpoint + client QUIC endpoint on `127.0.0.1:0` (OS-assigned port)
- Perform session handshake over control stream
- Server sends N synthetic `EncodedPacket` values (random data of varying sizes, some marked as keyframes)
- Client reassembles all frames via `FrameAssembler`
- Verify: all frames arrive, data matches byte-for-byte, pts and keyframe flags preserved, no IDR requests triggered (no loss on localhost)

### Ignored integration test

- Full pipeline: capture → encode → transport → client receive
- Requires Wayland + PipeWire + NVIDIA GPU + both binaries running
- Marked `#[ignore]`

## Performance Considerations

- **Zero-copy where possible.** Datagram payloads are slices of the `EncodedPacket.data` vec — no extra allocation per fragment on the send side. On the receive side, each fragment is a `Vec<u8>` that gets assembled into the final frame buffer.
- **Async everywhere.** Quinn is fully async (tokio). The transport tasks run as tokio tasks, not dedicated threads. This differs from the encoder (which uses a dedicated `std::thread` because FFmpeg is blocking).
- **Batched sends.** When fragmenting a large frame into many datagrams, send them all in a tight loop without yielding. Quinn internally batches UDP sends via GSO (Generic Segmentation Offload) when available.
- **Frame assembler bounded.** The `max_pending` limit prevents unbounded memory growth if frames arrive faster than they're consumed.
