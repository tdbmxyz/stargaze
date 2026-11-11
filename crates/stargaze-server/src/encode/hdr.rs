//! Precision-preserving full-range BT.2020/PQ RGB → limited P010.
//! No transfer/gamut conversion: input MUST already be BT.2020/PQ.

use stargaze_core::capture::PixelFormat;

pub(super) fn pixel_bytes(format: PixelFormat) -> Option<usize> {
    match format {
        PixelFormat::Bgra10 | PixelFormat::Rgba10 => Some(4),
        PixelFormat::Rgba16 => Some(8),
        _ => None,
    }
}

#[allow(clippy::many_single_char_names, clippy::cast_precision_loss)] // Masked 10-bit components are exact in f32.
fn rgb(pixel: &[u8], format: PixelFormat) -> (f32, f32, f32) {
    match format {
        PixelFormat::Rgba16 => {
            let sample = |i| f32::from(u16::from_le_bytes([pixel[i], pixel[i + 1]])) / 65535.0;
            (sample(0), sample(2), sample(4))
        }
        PixelFormat::Bgra10 | PixelFormat::Rgba10 => {
            let word = u32::from_le_bytes(pixel[..4].try_into().expect("validated packed pixel"));
            let low = (word & 1023) as f32 / 1023.0;
            let g = ((word >> 10) & 1023) as f32 / 1023.0;
            let high = ((word >> 20) & 1023) as f32 / 1023.0;
            if format == PixelFormat::Bgra10 {
                (low, g, high)
            } else {
                (high, g, low)
            }
        }
        _ => unreachable!("validated HDR format"),
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn quantize(code: f32) -> [u8; 2] {
    ((code.round().clamp(0.0, 1023.0) as u16) << 6).to_le_bytes()
}

/// Converts row bands in parallel; padding in either source/destination is valid.
#[allow(clippy::too_many_arguments, clippy::many_single_char_names)]
pub(super) fn convert_to_p010(
    src: &[u8],
    src_stride: usize,
    width: usize,
    height: usize,
    format: PixelFormat,
    y: &mut [u8],
    y_stride: usize,
    uv: &mut [u8],
    uv_stride: usize,
) -> Result<(), &'static str> {
    let bpp = pixel_bytes(format).ok_or("HDR conversion requires packed 10-bit RGB or RGBA16")?;
    if width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || src_stride < width * bpp
        || src.len() < (height - 1) * src_stride + width * bpp
        || y_stride < width * 2
        || uv_stride < width * 2
        || y.len() < y_stride * height
        || uv.len() < uv_stride * height / 2
    {
        return Err("invalid HDR conversion dimensions/strides/buffer lengths");
    }
    let threads = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .min(8);
    let band_rows = (height / 2).div_ceil(threads) * 2;
    std::thread::scope(|scope| {
        for (band, (y_band, uv_band)) in y
            .chunks_mut(y_stride * band_rows)
            .zip(uv.chunks_mut(uv_stride * band_rows / 2))
            .enumerate()
        {
            let start = band * band_rows;
            let rows = (height - start).min(band_rows);
            scope.spawn(move || {
                for by in (0..rows).step_by(2) {
                    for bx in (0..width).step_by(2) {
                        let mut sum = (0.0_f32, 0.0_f32, 0.0_f32);
                        for dy in 0..2 {
                            for dx in 0..2 {
                                let at = (start + by + dy) * src_stride + (bx + dx) * bpp;
                                let (r, g, b) = rgb(&src[at..at + bpp], format);
                                let luma = 0.2627 * r + 0.6780 * g + 0.0593 * b;
                                let out = (by + dy) * y_stride + (bx + dx) * 2;
                                y_band[out..out + 2]
                                    .copy_from_slice(&quantize(64.0 + 876.0 * luma));
                                sum.0 += r;
                                sum.1 += g;
                                sum.2 += b;
                            }
                        }
                        let (r, g, b) = (sum.0 * 0.25, sum.1 * 0.25, sum.2 * 0.25);
                        let luma = 0.2627 * r + 0.6780 * g + 0.0593 * b;
                        let at = by / 2 * uv_stride + bx * 2;
                        uv_band[at..at + 2]
                            .copy_from_slice(&quantize(512.0 + 896.0 * (b - luma) / 1.8814));
                        uv_band[at + 2..at + 4]
                            .copy_from_slice(&quantize(512.0 + 896.0 * (r - luma) / 1.4746));
                    }
                }
            });
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt2020_limited_primaries_and_packed_channel_order() {
        for format in [PixelFormat::Bgra10, PixelFormat::Rgba10] {
            for ((r, g, b), expected) in [
                ((1023_u32, 0, 0), (294_u16, 387_u16, 960_u16)),
                ((0, 1023, 0), (658, 189, 100)),
                ((0, 0, 1023), (116, 960, 476)),
            ] {
                let packed = if format == PixelFormat::Bgra10 {
                    r | (g << 10) | (b << 20)
                } else {
                    b | (g << 10) | (r << 20)
                };
                let src = packed.to_le_bytes().repeat(4);
                let mut y = [0; 8];
                let mut uv = [0; 4];
                convert_to_p010(&src, 8, 2, 2, format, &mut y, 4, &mut uv, 4).unwrap();
                let got = (
                    u16::from_le_bytes([y[0], y[1]]) >> 6,
                    u16::from_le_bytes([uv[0], uv[1]]) >> 6,
                    u16::from_le_bytes([uv[2], uv[3]]) >> 6,
                );
                assert_eq!(got, expected);
            }
        }
    }

    #[test]
    fn hdr_gradient_retains_ten_bits_and_neutral_chroma() {
        let w = 2048;
        let src: Vec<u8> = (0..2)
            .flat_map(|_| {
                (0..w).flat_map(|x| {
                    let c = u32::try_from(x / 2).unwrap();
                    (c | (c << 10) | (c << 20)).to_le_bytes()
                })
            })
            .collect();
        let mut y = vec![0; w * 2 * 2];
        let mut uv = vec![0; w * 2];
        convert_to_p010(
            &src,
            w * 4,
            w,
            2,
            PixelFormat::Bgra10,
            &mut y,
            w * 2,
            &mut uv,
            w * 2,
        )
        .unwrap();
        let codes: Vec<u16> = y
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u16::from_le_bytes(*p) >> 6)
            .collect();
        assert_eq!(codes[0], 64);
        assert_eq!(codes[w - 1], 940);
        assert!(codes[..w].windows(2).all(|v| v[0] <= v[1]));
        assert!(codes[..w].windows(2).filter(|v| v[0] != v[1]).count() > 800);
        assert!(
            uv.as_chunks::<2>()
                .0
                .iter()
                .all(|p| u16::from_le_bytes(*p) == 512 << 6)
        );
    }
}
