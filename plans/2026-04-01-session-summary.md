# Stargaze Session Summary — 2026-04-01

## Project State

**Stargaze** is a Rust-native low-latency desktop/game streaming system (server + client). Sub-projects 1–4 of 9 are complete. The codebase is 4,337 lines of Rust across 3 workspace crates.

### Completed Sub-projects

| # | Sub-project | Commits | Status |
|---|-------------|---------|--------|
| 1 | Scaffolding | `416743b`..`395603d` | Done — workspace, CLI, config, error types |
| 2 | Video Capture | `a8b227e`..`8a868b0` | Done — PipeWire + portal, DMA-BUF + CPU fallback |
| 3 | Video Encoding | `ab70b43`..`704f8f6` | Done — FFmpeg NVENC H.265, CUDA hw frames |
| 4 | Network Transport | `df38ade`..`3682389` | Done — QUIC via quinn, datagrams, frame assembly |

### Remaining Sub-projects

| # | Sub-project | Status |
|---|-------------|--------|
| 5 | Video Decoding + Rendering (Client) | Not started |
| 6 | Audio Capture + Encoding (Server) | Not started |
| 7 | Audio Decoding + Playback (Client) | Not started |
| 8 | Input Forwarding (Client→Server) | Not started |
| 9 | Mic Forwarding (rsonance integration) | Not started |

### Uncommitted Changes

- **`build.rs`**: Removed `--static` flag from `pkg-config` and `~/.local/usr/` path — system packages use dynamic linking now
- **`main.rs`**: Added 2 GPU-only NVENC tests (`test_nvenc_encode_synthetic_frames`, `test_nvenc_idr_request`), changed `init_tracing()` to use `try_init()`
- **`Dockerfile` / `devcontainer.json`**: Updated with all system deps installed natively + CUDA toolkit + `--gpus all`

### Test Status

- **47 pass, 0 fail, 3 ignored** (regular `cargo test --workspace`)
- **2 GPU tests pass** when run with `--ignored test_nvenc` (requires NVIDIA GPU)
- **1 full-pipeline test** remains ignored (requires Wayland + PipeWire + GPU)
- Zero clippy warnings (`cargo clippy -- -W clippy::pedantic`)
- Clean formatting (`cargo fmt --check`)

## Architecture

### Crate Structure

```
crates/
├── stargaze-core/     # Shared types: config, capture, encode, transport, error
├── stargaze-server/   # Server binary: capture → encode → transport
│   ├── capture/       # PipeWire screen capture via xdg-desktop-portal
│   ├── encode/        # FFmpeg NVENC H.265 encoder
│   └── transport/     # QUIC server endpoint, frame fragmentation, sender
└── stargaze-client/   # Client binary: transport → (decode → render, not yet)
    └── transport/     # QUIC client, frame assembler, receiver
```

### Key Design Decisions

- **QUIC (quinn + rustls)** for all transport — single port multiplexes control stream + A/V datagrams
- **Unreliable datagrams** for video packets (lowest latency), reliable stream for control
- **Self-signed certs + skip verification** for LAN MVP
- **H.265 only** for MVP (no AV1, no H.264)
- **CPU upload path** for MVP — both DMA-BUF and CpuMapped frames go through `av_hwframe_transfer_data()`
- **IDR-on-loss** strategy — client requests IDR via watch channel when frame assembly detects gaps
- **postcard** for all serialization (compact binary, serde-compatible)
- **Frame fragmentation** — server splits encoded frames into MTU-sized datagrams, client reassembles

### Data Flow

```
[Server]
Screen → PipeWire capture → Frame channel → NVENC encode → Packet channel → QUIC datagrams
                                                                              ↑
                                                                    IDR watch channel
                                                                              ↑
[Client]                                                             Control stream
QUIC datagrams → FrameAssembler → ReassembledFrame channel → (decoder, not yet)
```

## Environment

### Devcontainer

- **Base**: `rust:1.94-slim-trixie` (Rust nightly 1.94)
- **GPU**: NVIDIA RTX 2080 (NVENC-capable), CUDA 13.2 toolkit installed
- **System packages**: All installed natively via Dockerfile — `libpipewire-0.3-dev`, `libclang-dev`, all FFmpeg dev packages, `build-essential`
- **No `~/.local/usr/` workaround needed** — the devcontainer was rebuilt with system packages
- **Only env var needed**: `export LIBCLANG_PATH="/usr/lib/llvm-19/lib"` before cargo commands
- **No Wayland compositor or PipeWire daemon** — only dev libraries. Full pipeline test can't run.
- **No passwordless sudo** for `vscode` user

### Build Notes

- `build.rs` in `stargaze-server` queries `pkg-config --libs` (dynamic, not static) for FFmpeg transitive deps
- `ffmpeg-sys-next` handles FFmpeg library discovery; `build.rs` adds any extras
- `rustfmt` and `clippy` components may need reinstalling after toolchain updates: `rustup component add rustfmt clippy`

## Key Files

| File | Purpose |
|------|---------|
| `docs/specs/2026-03-31-stargaze-architecture-design.md` | Master architecture + all 9 sub-projects |
| `docs/specs/2026-03-31-video-capture-design.md` | Video capture design (Sub-project 2) |
| `docs/specs/2026-03-31-video-encoding-design.md` | Video encoding design (Sub-project 3) |
| `docs/specs/2026-03-31-network-transport-design.md` | Network transport design (Sub-project 4) |
| `plans/2026-03-31-scaffolding.md` | Scaffolding plan (6 tasks, all done) |
| `plans/2026-03-31-video-capture.md` | Video capture plan (7 tasks, all done) |
| `plans/2026-03-31-video-encoding.md` | Video encoding plan (6 tasks, all done) |
| `plans/2026-03-31-network-transport.md` | Network transport plan (9 tasks, all done) |
| `AGENTS.md` | Project guidelines for AI agents |

## Technical Discoveries

### Crate APIs (verified by implementation)

- **ashpd 0.13**: `select_sources()` takes positional args, `SourceType::Monitor` directly, `start()` returns streams
- **pipewire 0.9**: `MainLoopBox`/`ContextBox`/`StreamBox`, `add_local_listener_with_user_data()`, `pw_main_loop_quit()` via raw pointer
- **ffmpeg-next 7.1**: `set_bit_rate(usize)`, `Packet::empty()`, `Error::Other { errno: EAGAIN }`, `Rational(num, den)`
- **quinn 0.11**: `Endpoint::server()`, `connection.send_datagram(Bytes)`, `read_datagram()`, `max_datagram_size() → Option<usize>`
- **rcgen 0.14**: `generate_simple_self_signed()` for ECDSA P-256 certs
- **postcard 1.1**: `to_allocvec()`, `from_bytes()`, `take_from_bytes()` for header+payload

### Clippy Pedantic Compliance

- Use `&raw mut ptr` for raw pointer borrows
- Use `.cast_signed()` / `.cast_unsigned()` for integer casts
- Use `.is_multiple_of()` instead of `% n == 0`
- Backtick technical terms in doc comments for `clippy::doc_markdown`
- `thiserror` v2: fields named `source` auto-detect; use `reason` to avoid conflicts

## Next Steps

The next sub-project is **#5: Video Decoding + Rendering (Client)**. This involves:
1. Design spec for client-side FFmpeg/VAAPI decode
2. Implementation plan
3. Receiving `ReassembledFrame` from transport → decoding H.265 → rendering to Wayland window
4. End-to-end test: server encodes test pattern → QUIC → client decodes and displays

The client already has the transport layer receiving frames. The decoder needs to handle the H.265 NAL units from `ReassembledFrame.data` and output displayable frames.
