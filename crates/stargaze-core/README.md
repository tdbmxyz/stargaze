# stargaze-core

Shared library for the [Stargaze](../../README.md) streaming system. Everything the server and client must agree on lives here — most importantly the wire protocol.

This crate is intentionally free of heavy native dependencies (no FFmpeg, SDL, PipeWire): it compiles fast and keeps the protocol definitions in one place.

## Modules

| Module | Contents |
|---|---|
| `config` | `ServerConfig` / `ClientConfig` (TOML + CLI override model), `Resolution`, `Codec`, `CursorConfig`, `MicForwardConfig`, default config paths (`~/.config/stargaze/{server,client}.toml`) |
| `transport` | The wire protocol: `DatagramHeader` (per-fragment header on QUIC datagrams), `ControlMessage` (handshake, input, `IdrRequest`, ping/pong on the reliable stream), `ReassembledFrame`, serialization via `postcard`, shared constants (`MAX_PENDING_FRAMES`, `IDR_RATE_LIMIT_MS`, MTU/buffer sizes) |
| `input` | `InputEvent` — keyboard scancodes, relative mouse motion, buttons, wheel, gamepad axes/buttons — shared between SDL capture (client) and uinput injection (server) |
| `capture` | `Frame`, `PixelFormat`, `DmaBufInfo` — capture-side frame descriptions handed to the encoder |
| `encode` / `decode` | `EncoderConfig` / `DecoderConfig`, `EncodedPacket`, `DecodedFrame`, error types |
| `audio` | Audio capture/encoder/decoder configs, `AudioFrame`, error types |
| `mic_forward` | Spawning/stopping the `rsonance` subprocess used for mic forwarding |
| `error` | Top-level error type re-exports |

## Protocol notes

- **Media datagrams**: each QUIC datagram is a `postcard`-encoded `DatagramHeader` followed by a fragment payload. Frames are fragmented to the connection MTU; `frame_index` is monotonically increasing per stream type (video = 0, audio = 1).
- **Control stream**: length-prefixed (`u32` LE) `postcard`-encoded `ControlMessage`s on a reliable bidirectional QUIC stream. New variants may be appended to the enum without breaking compatibility — never reorder existing ones.
- Changing anything in `transport` changes the wire format: server and client must be rebuilt together.

## Tests

```bash
cargo test -p stargaze-core
```

Round-trip serialization tests cover every `ControlMessage` variant and the datagram header; config tests cover TOML parsing, defaults, and partial files.
