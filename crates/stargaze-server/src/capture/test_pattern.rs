//! Explicit native HDR diagnostic source. This does not claim desktop capture.

use super::{CaptureConfig, CaptureFormat, CaptureSession, FRAME_CHANNEL_CAPACITY};
use stargaze_core::{
    capture::{CaptureError, CapturedFrame, Frame, PixelFormat},
    color::ColorDescription,
    config::Resolution,
    hdr::{ContentLight, HdrMetadata, MasteringDisplay, pq_from_nits},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{mpsc, oneshot};

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pattern(width: u32, height: u32) -> Vec<u8> {
    let luminances = [
        0.0, 0.1, 1.0, 10.0, 100.0, 160.0, 203.0, 400.0, 1000.0, 4000.0, 10000.0,
    ];
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let (r, g, b) = if y < height * 2 / 3 {
                let band =
                    (x as usize * luminances.len() / width as usize).min(luminances.len() - 1);
                let code = (pq_from_nits(luminances[band]) * 1023.0).round() as u32;
                (code, code, code)
            } else if y < height * 5 / 6 {
                let code = x * 1023 / (width - 1);
                (code, code, code)
            } else {
                let code = (pq_from_nits(203.0) * 1023.0).round() as u32;
                match x * 6 / width {
                    0 => (code, 0, 0),
                    1 => (0, code, 0),
                    2 => (0, 0, code),
                    3 => (code, code, 0),
                    4 => (0, code, code),
                    _ => (code, 0, code),
                }
            };
            pixels.extend((r | (g << 10) | (b << 20)).to_le_bytes());
        }
    }
    pixels
}

pub(super) fn start(
    config: CaptureConfig,
    framerate: u32,
) -> Result<
    (
        CaptureSession,
        mpsc::Receiver<CapturedFrame>,
        oneshot::Receiver<CaptureFormat>,
    ),
    CaptureError,
> {
    if config.width < 2
        || config.height < 2
        || !config.width.is_multiple_of(2)
        || !config.height.is_multiple_of(2)
        || framerate == 0
        || framerate > 1000
    {
        return Err(CaptureError::NegotiationError(
            "HDR test pattern requires even dimensions >=2 and 1..1000 fps".into(),
        ));
    }
    let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
    let (format_tx, format_rx) = oneshot::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    let thread_handle = std::thread::Builder::new().name("stargaze-hdr-pattern".into()).spawn(move || {
        let pixels = pattern(config.width, config.height);
        let metadata = HdrMetadata {
            mastering: Some(MasteringDisplay { primaries: [[35400,14600],[8500,39850],[6550,2300]], white_point: [15635,16450],
                min_luminance: 0, max_luminance: 100_000_000 }),
            content_light: Some(ContentLight { max_cll: 10000, max_fall: 10000 }),
        };
        let _ = format_tx.send(CaptureFormat { resolution: Resolution { width: config.width, height: config.height }, color: ColorDescription::HDR10_RGB });
        tracing::info!(?metadata, "HDR10 diagnostic source: PQ absolute luminance bars, 10-bit gradient, BT.2020 primaries; NOT desktop capture");
        let period = std::time::Duration::from_secs_f64(1.0 / f64::from(framerate));
        let mut deadline = std::time::Instant::now();
        while !flag.load(Ordering::Relaxed) {
            let mut frame = CapturedFrame::from(Frame::CpuMapped { data: pixels.clone(), width: config.width, height: config.height,
                stride: config.width * 4, format: PixelFormat::Bgra10 });
            frame.color = ColorDescription::HDR10_RGB;
            frame.hdr_metadata = metadata;
            if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(frame) { break; }
            deadline += period;
            std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
            if deadline < std::time::Instant::now() { deadline = std::time::Instant::now(); }
        }
    }).map_err(|e| CaptureError::PipeWireError(format!("HDR pattern thread: {e}")))?;
    Ok((
        CaptureSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        rx,
        format_rx,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_source_contains_real_pq_reference_white_and_precision() {
        let data = pattern(2048, 24);
        let word = |x: usize, y: usize| {
            u32::from_le_bytes(
                data[(y * 2048 + x) * 4..(y * 2048 + x + 1) * 4]
                    .try_into()
                    .unwrap(),
            ) & 1023
        };
        assert_eq!(word(0, 0), 0);
        assert_eq!(word(2047, 0), 1023);
        assert!(word(2048 * 6 / 11 + 20, 0).abs_diff(594) <= 1); // 203 nits
        assert_eq!(word(0, 18), 0);
        assert_eq!(word(2047, 18), 1023);
    }
}
