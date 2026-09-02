# Surround audio (5.1 / 7.1)

Stargaze can capture, encode, transport, decode, and play back multichannel
audio (mono, stereo, 5.1, 7.1) end to end. Stereo remains the default; surround
is opt-in on the server.

## Enabling

Server config (`~/.config/stargaze/server.toml`) or `--audio-channels`:

```toml
audio_channels = 6   # 1 = mono, 2 = stereo (default), 6 = 5.1, 8 = 7.1
```

The client needs no configuration — the server advertises the channel count in
the `SessionResponse` handshake and the client builds its decoder and audio
device to match.

> **Real vs upmixed surround.** The server captures the monitor of the default
> PipeWire sink. If that sink is genuinely 5.1/7.1 (an AV receiver, or a virtual
> surround sink), setting `audio_channels = 6/8` passes the discrete channels
> through. If the sink is stereo, PipeWire's channel mixer upmixes to the
> requested layout — valid, but not true surround. To get discrete surround,
> point applications at a real multichannel sink and set it as the default.

## Channel model

We control both ends of the wire, so we do **not** use Opus channel mapping
family 1 (RFC 7845) or its fixed Vorbis channel order. Instead both sides agree
on the **SPA/WAV interleave order**, which is also SDL's playback order — so no
channel is ever reordered between PipeWire capture and SDL playback:

| Count | Order                                    |
|-------|------------------------------------------|
| 1     | MONO                                     |
| 2     | FL FR                                    |
| 6     | FL FR FC LFE RL RR                       |
| 8     | FL FR FC LFE RL RR SL SR                 |

The Opus multistream encoder/decoder use a custom mapping table (see
`opus_channel_layout` in `stargaze-core/src/audio.rs`) that couples the natural
L/R pairs into stereo Opus streams and carries FC and LFE as mono streams:

| Count | streams | coupled | mapping (input-channel → encoded-channel) |
|-------|---------|---------|-------------------------------------------|
| 1     | 1       | 0       | [0]                                        |
| 2     | 1       | 1       | [0, 1]                                     |
| 6     | 4       | 2       | [0, 1, 4, 5, 2, 3]                         |
| 8     | 5       | 3       | [0, 1, 6, 7, 2, 3, 4, 5]                   |

Encoder and decoder are constructed from the identical layout, so the table is
never transmitted — only the channel count is.

## Pipeline

```
capture (PipeWire, N ch f32, SPA positions set)
  → Opus MSEncoder (N ch, custom mapping)         [server]
  → QUIC datagrams
  → Opus MSDecoder (N ch, same mapping)           [client]
  → SDL2 audio device (N ch)
```

- **Capture** (`audio/pipewire_audio.rs`): the format pod requests `channels =
  N` with explicit SPA channel positions for the layout, so PipeWire's mixer
  routes/up/downmixes correctly. The per-buffer interleave logic is already
  channel-agnostic.
- **Encode** (`encode/opus_enc.rs`): `opus::MSEncoder`. The 10 ms framing ring
  buffer works for any channel count (`480 * N` f32 per frame). Max packet size
  scales with the number of Opus streams.
- **Transport**: `SessionResponse` gained a trailing `audio_channels: u16`
  field. Old clients ignore it (postcard drops trailing bytes) and stay stereo;
  new clients reading an old server fall back to 2 via
  `deserialize_session_response_compat`.
- **Decode** (`decode/opus_dec.rs`): `opus::MSDecoder` built from the same
  layout.
- **Playback** (`render/audio.rs`): SDL2 audio device opened with N channels;
  the queue-backlog byte math scales with channel count.

## Compatibility

Cross-version streaming stays intact **as long as the server is left at the
default stereo**. Enabling `audio_channels = 6/8` requires the client to also be
≥ the surround release, because an older client cannot decode multichannel Opus
and would fall back to a stereo decoder over a 6/8-channel stream. The default
(2) keeps every existing deployment unchanged.
</content>
</invoke>
