# Fix Client Lag and Gray Frame Corruption

## Issues to Address

1. **Severe lag / unusable performance**: The client can't keep up with 144fps
   3440x1440 software H.265 decode. The pipeline backs up, causing input lag
   and sluggish rendering.

2. **Intermittent gray screen**: QUIC datagram losses cause the decoder to
   lose reference frames ("Could not find ref with POC"). The decoder
   produces corrupt (gray) output until the next keyframe arrives (~2s).

## Important Notes

- The decoder already outputs YUV420P, but we run `sws_scale` from YUV420P
  to YUV420P — a pure waste.  The only work needed is stride padding removal.
- The render loop uses `present_vsync()`, which blocks `canvas.present()` for
  up to 16ms on a 60Hz monitor.  Since the render loop is also the SDL event
  pump (input), this adds 16ms of input latency per frame.
- The decoded frame channel is `std::sync::mpsc::channel` (unbounded).  If the
  decoder produces faster than the renderer consumes, decoded frames pile up
  in memory without bound.
- The video_tx channel (transport → decoder) is `tokio::mpsc::channel(16)`.
  If the decoder falls behind, 16 reassembled frames queue up, then the
  transport blocks on `send().await`, stalling datagram processing and losing
  more frames.
- IDR requests are triggered when pending *incomplete* video frames exceed 16.
  This doesn't detect the case where frames are delivered but the decoder
  can't keep up, or where individual fragments are lost causing reference
  frame gaps.

## Implementation Strategy

### 1. Remove redundant sws_scale

The decoder outputs YUV420P (confirmed in logs: `decoded_format=YUV420P`).
The scaler converts YUV420P → YUV420P which is an identity transform with
overhead.  Skip the scaler when the decoded format is already YUV420P and
strip stride padding directly from the decoded frame's plane data.

### 2. Remove present_vsync from renderer

Replace `.present_vsync()` with plain `.build()` on the canvas.  This unblocks
the render/event loop so input events are processed immediately and decoded
frames are consumed as fast as they arrive.  Without vsync there may be
tearing, but latency is the priority for a game streaming client.

### 3. Drop stale frames in the decoder pipeline

Change the transport → decoder channel from `channel(16)` to `channel(1)`.
In the decoder loop, after receiving a frame, drain and discard any queued
frames keeping only the latest (but always keep keyframes).  This ensures the
decoder works on the most recent data rather than falling behind.

### 4. Detect frame loss and request IDR sooner

In the frame assembler, when `deliver_in_order` skips past a frame_index gap
(i.e., a frame was never completed), immediately trigger an IDR request.
This covers the case where fragments are lost but the pending count stays
below the threshold.

## Tests

- `cargo test --workspace` must still pass.
- Manual: verify reduced lag and faster recovery from gray frames.
