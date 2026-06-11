//! Multithreaded BGRA/RGBA → NV12 (BT.709 limited range) conversion.
//!
//! Replaces the single-threaded `sws_scale` path for 8-bit RGB capture
//! formats: at 3440x1440 the sws conversion alone took tens of
//! milliseconds and capped the whole pipeline below 30 fps. This
//! converter splits the image into row bands processed in parallel and
//! uses the BT.709 matrix the encoder actually advertises (sws defaults
//! to BT.601, which subtly shifted colors).

use stargaze_core::capture::PixelFormat;

/// Byte offsets of (R, G, B) within a 4-byte pixel.
#[derive(Debug, Clone, Copy)]
struct ChannelOrder {
    r: usize,
    g: usize,
    b: usize,
}

/// Returns the channel order for an 8-bit RGB capture format, or `None`
/// if the format needs the generic sws fallback (NV12, 10-bit).
pub(crate) fn channel_order(format: PixelFormat) -> Option<(usize, usize, usize)> {
    match format {
        PixelFormat::Bgra8 => Some((2, 1, 0)),
        PixelFormat::Rgba8 => Some((0, 1, 2)),
        PixelFormat::Nv12 | PixelFormat::Bgra10 | PixelFormat::Rgba10 => None,
    }
}

/// BT.709 limited-range luma for one pixel.
#[inline]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    let y = (47 * r + 157 * g + 16 * b + 128) >> 8;
    u8::try_from(y + 16).unwrap_or(235)
}

/// BT.709 limited-range chroma (U, V) for averaged RGB values.
#[inline]
#[allow(clippy::many_single_char_names)] // r/g/b/u/v are the domain names
fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    let u = ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    (
        u8::try_from(u).unwrap_or(if u < 0 { 16 } else { 240 }),
        u8::try_from(v).unwrap_or(if v < 0 { 16 } else { 240 }),
    )
}

/// Converts one band of 2-pixel rows.
///
/// `src_rows` starts at the band's first source row; `y_band` and
/// `uv_band` are the matching destination slices.
#[allow(clippy::too_many_arguments)]
fn convert_band(
    src_rows: &[u8],
    src_stride: usize,
    width: usize,
    block_rows: usize,
    order: ChannelOrder,
    y_band: &mut [u8],
    y_stride: usize,
    uv_band: &mut [u8],
    uv_stride: usize,
) {
    for by in 0..block_rows {
        let row0 = &src_rows[by * 2 * src_stride..][..width * 4];
        let row1 = &src_rows[(by * 2 + 1) * src_stride..][..width * 4];
        let (y0, rest) = y_band[by * 2 * y_stride..].split_at_mut(y_stride);
        let y0 = &mut y0[..width];
        let y1 = &mut rest[..width];
        let uv = &mut uv_band[by * uv_stride..][..width];

        for bx in 0..width / 2 {
            let px = |row: &[u8], x: usize| -> (i32, i32, i32) {
                let p = &row[x * 4..x * 4 + 4];
                (
                    i32::from(p[order.r]),
                    i32::from(p[order.g]),
                    i32::from(p[order.b]),
                )
            };
            let (r00, g00, b00) = px(row0, bx * 2);
            let (r01, g01, b01) = px(row0, bx * 2 + 1);
            let (r10, g10, b10) = px(row1, bx * 2);
            let (r11, g11, b11) = px(row1, bx * 2 + 1);

            y0[bx * 2] = luma(r00, g00, b00);
            y0[bx * 2 + 1] = luma(r01, g01, b01);
            y1[bx * 2] = luma(r10, g10, b10);
            y1[bx * 2 + 1] = luma(r11, g11, b11);

            // Chroma from the 2x2 average (4:2:0 subsampling).
            let ravg = (r00 + r01 + r10 + r11 + 2) >> 2;
            let gavg = (g00 + g01 + g10 + g11 + 2) >> 2;
            let bavg = (b00 + b01 + b10 + b11 + 2) >> 2;
            let (u, v) = chroma(ravg, gavg, bavg);
            uv[bx * 2] = u;
            uv[bx * 2 + 1] = v;
        }
    }
}

/// Converts an 8-bit RGB image to NV12 planes, in parallel row bands.
///
/// `width` and `height` must be even (always true for video modes).
/// `y_plane` must hold `y_stride * height` bytes and `uv_plane` at least
/// `uv_stride * height / 2` bytes.
///
/// # Panics
///
/// Panics if the source or destination slices are smaller than the
/// dimensions imply.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_to_nv12(
    src: &[u8],
    src_stride: usize,
    width: usize,
    height: usize,
    rgb_order: (usize, usize, usize),
    y_plane: &mut [u8],
    y_stride: usize,
    uv_plane: &mut [u8],
    uv_stride: usize,
) {
    assert!(width.is_multiple_of(2) && height.is_multiple_of(2));
    let order = ChannelOrder {
        r: rgb_order.0,
        g: rgb_order.1,
        b: rgb_order.2,
    };

    let total_block_rows = height / 2;
    let threads = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .min(8)
        .min(total_block_rows);
    let per_band = total_block_rows.div_ceil(threads);

    // Carve disjoint destination bands so each thread owns its slices.
    let mut bands = Vec::with_capacity(threads);
    let mut y_rest: &mut [u8] = y_plane;
    let mut uv_rest: &mut [u8] = uv_plane;
    let mut start = 0usize;
    while start < total_block_rows {
        let rows = per_band.min(total_block_rows - start);
        // The last band keeps the remainder (strides may overrun the
        // nominal size on the final row, so don't over-split).
        let y_take = (rows * 2 * y_stride).min(y_rest.len());
        let uv_take = (rows * uv_stride).min(uv_rest.len());
        let (y_band, y_next) = y_rest.split_at_mut(y_take);
        let (uv_band, uv_next) = uv_rest.split_at_mut(uv_take);
        y_rest = y_next;
        uv_rest = uv_next;
        bands.push((start, rows, y_band, uv_band));
        start += rows;
    }

    std::thread::scope(|scope| {
        for (start, rows, y_band, uv_band) in bands {
            scope.spawn(move || {
                let src_rows = &src[start * 2 * src_stride..];
                convert_band(
                    src_rows, src_stride, width, rows, order, y_band, y_stride, uv_band, uv_stride,
                );
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Converts a uniform `width` x `height` image of one BGRA color and
    /// returns (Y, U, V) of the first pixel/block.
    fn convert_solid_bgra(b: u8, g: u8, r: u8, width: usize, height: usize) -> (u8, u8, u8) {
        let src: Vec<u8> = [b, g, r, 255].repeat(width * height);
        let mut y = vec![0u8; width * height];
        let mut uv = vec![0u8; width * height / 2];
        convert_to_nv12(
            &src,
            width * 4,
            width,
            height,
            channel_order(PixelFormat::Bgra8).unwrap(),
            &mut y,
            width,
            &mut uv,
            width,
        );
        assert!(y.iter().all(|&v| v == y[0]), "luma must be uniform");
        (y[0], uv[0], uv[1])
    }

    fn assert_near(actual: u8, expected: u8, what: &str) {
        assert!(
            (i16::from(actual) - i16::from(expected)).abs() <= 2,
            "{what}: got {actual}, expected ~{expected}"
        );
    }

    #[test]
    fn white_maps_to_y235_neutral_chroma() {
        let (y, u, v) = convert_solid_bgra(255, 255, 255, 8, 8);
        assert_near(y, 235, "white Y");
        assert_near(u, 128, "white U");
        assert_near(v, 128, "white V");
    }

    #[test]
    fn black_maps_to_y16_neutral_chroma() {
        let (y, u, v) = convert_solid_bgra(0, 0, 0, 8, 8);
        assert_near(y, 16, "black Y");
        assert_near(u, 128, "black U");
        assert_near(v, 128, "black V");
    }

    #[test]
    fn bt709_primaries() {
        // Reference values for BT.709 limited range.
        let (y, u, v) = convert_solid_bgra(0, 0, 255, 8, 8); // red
        assert_near(y, 63, "red Y");
        assert_near(u, 102, "red U");
        assert_near(v, 240, "red V");

        let (y, u, v) = convert_solid_bgra(0, 255, 0, 8, 8); // green
        assert_near(y, 173, "green Y");
        assert_near(u, 42, "green U");
        assert_near(v, 26, "green V");

        let (y, u, v) = convert_solid_bgra(255, 0, 0, 8, 8); // blue
        assert_near(y, 32, "blue Y");
        assert_near(u, 240, "blue U");
        assert_near(v, 118, "blue V");
    }

    #[test]
    fn rgba_order_matches_bgra() {
        let width = 4;
        let height = 4;
        let bgra: Vec<u8> = [10u8, 200, 60, 255].repeat(width * height);
        let rgba: Vec<u8> = [60u8, 200, 10, 255].repeat(width * height);

        let run = |src: &[u8], fmt: PixelFormat| -> (Vec<u8>, Vec<u8>) {
            let mut y = vec![0u8; width * height];
            let mut uv = vec![0u8; width * height / 2];
            convert_to_nv12(
                src,
                width * 4,
                width,
                height,
                channel_order(fmt).unwrap(),
                &mut y,
                width,
                &mut uv,
                width,
            );
            (y, uv)
        };

        assert_eq!(
            run(&bgra, PixelFormat::Bgra8),
            run(&rgba, PixelFormat::Rgba8)
        );
    }

    #[test]
    fn handles_source_stride_padding() {
        let width = 4;
        let height = 4;
        let stride = width * 4 + 16; // padded rows
        let mut src = vec![0u8; stride * height];
        for row in src.chunks_exact_mut(stride) {
            for px in row[..width * 4].chunks_exact_mut(4) {
                px.copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        let mut y = vec![0u8; width * height];
        let mut uv = vec![0u8; width * height / 2];
        convert_to_nv12(
            &src,
            stride,
            width,
            height,
            channel_order(PixelFormat::Bgra8).unwrap(),
            &mut y,
            width,
            &mut uv,
            width,
        );
        assert!(y.iter().all(|&v| (i16::from(v) - 235).abs() <= 2));
    }

    #[test]
    fn non_rgb_formats_need_fallback() {
        assert!(channel_order(PixelFormat::Nv12).is_none());
        assert!(channel_order(PixelFormat::Bgra10).is_none());
        assert!(channel_order(PixelFormat::Rgba10).is_none());
    }

    #[test]
    fn large_image_parallel_bands_are_consistent() {
        // Tall image to force multiple bands; gradient so band boundaries
        // would show errors.
        let width = 16;
        let height = 64;
        let mut src = vec![0u8; width * 4 * height];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            let v = u8::try_from((i * 7) % 256).unwrap();
            px.copy_from_slice(&[v, v, v, 255]);
        }
        let mut y = vec![0u8; width * height];
        let mut uv = vec![0u8; width * height / 2];
        convert_to_nv12(
            &src,
            width * 4,
            width,
            height,
            channel_order(PixelFormat::Bgra8).unwrap(),
            &mut y,
            width,
            &mut uv,
            width,
        );

        // Gray input: every Y must equal luma(v,v,v) of its own pixel.
        for (i, &yv) in y.iter().enumerate() {
            let v = i32::from(src[i * 4]);
            let expected = luma(v, v, v);
            assert!(
                (i16::from(yv) - i16::from(expected)).abs() <= 1,
                "pixel {i}: got {yv}, expected {expected}"
            );
        }
    }
}
