//! Validate SPA metadata before either DMA-BUF or CPU pixels leave capture.

use stargaze_core::capture::PixelFormat;
use stargaze_core::color::ColorDescription;

/// Raw SPA enums, deliberately retained even when unknown/unsupported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SpaColor {
    pub range: u32,
    pub matrix: u32,
    pub transfer: u32,
    pub primaries: u32,
}

impl SpaColor {
    /// Current converters operate on full-range sRGB, not arbitrary RGB/YUV.
    /// Enum values are from spa/param/video/color.h, NOT CICP values.
    pub fn validate(
        self,
        format: PixelFormat,
        require_metadata: bool,
    ) -> Result<(ColorDescription, bool), &'static str> {
        if matches!(self.transfer, 14 | 15) {
            return Err(
                "PQ/HLG capture rejected: no native HDR presentation or HDR-to-SDR tone/gamut mapping",
            );
        }
        if self.primaries != 0 && self.primaries != 1 {
            return Err("capture primaries require unsupported gamut mapping");
        }
        if self.transfer != 0 && self.transfer != 7 {
            return Err("capture transfer is not sRGB; transfer conversion is not implemented");
        }
        if self.range > 1 || self.matrix > 1 {
            return Err(
                "capture requires unsupported range/matrix conversion (only full-range RGB supported)",
            );
        }
        if format == PixelFormat::Nv12 {
            return Err(
                "NV12 capture rejected: planar capture layout/color conversion is not implemented safely",
            );
        }
        let missing =
            self.range == 0 || self.matrix == 0 || self.transfer == 0 || self.primaries == 0;
        let ten_bit = matches!(format, PixelFormat::Bgra10 | PixelFormat::Rgba10);
        if missing && (require_metadata || ten_bit) {
            return Err(
                "capture color metadata missing: cannot infer HDR/SDR from bit depth; use a color-described SDR capture source",
            );
        }
        // Compatibility contract, not detection: old portals omit all color
        // fields. Preserve historical 8-bit SDR behavior with a warning.
        Ok((
            ColorDescription {
                bit_depth: if ten_bit { 10 } else { 8 },
                ..ColorDescription::SRGB
            },
            missing,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRGB: SpaColor = SpaColor {
        range: 1,
        matrix: 1,
        transfer: 7,
        primaries: 1,
    };

    #[test]
    fn legacy_rgb_is_explicitly_assumed_not_detected() {
        assert_eq!(
            SpaColor::default().validate(PixelFormat::Bgra8, false),
            Ok((ColorDescription::SRGB, true))
        );
        assert!(
            SpaColor::default()
                .validate(PixelFormat::Bgra8, true)
                .is_err()
        );
        assert!(
            SpaColor::default()
                .validate(PixelFormat::Bgra10, false)
                .is_err()
        );
        assert!(
            SpaColor::default()
                .validate(PixelFormat::Nv12, false)
                .is_err()
        );
    }

    #[test]
    fn ten_bit_sdr_is_not_hdr() {
        let (color, assumed) = SRGB.validate(PixelFormat::Rgba10, false).unwrap();
        assert_eq!(color.bit_depth, 10);
        assert!(color.is_supported_rgb());
        assert!(!assumed);
    }

    #[test]
    fn explicit_unsafe_metadata_is_never_overridden() {
        for bad in [
            SpaColor {
                transfer: 14,
                ..SRGB
            },
            SpaColor {
                transfer: 15,
                ..SRGB
            },
            SpaColor {
                transfer: 1,
                ..SRGB
            },
            SpaColor {
                transfer: 5,
                ..SRGB
            },
            SpaColor {
                primaries: 7,
                ..SRGB
            },
            SpaColor {
                primaries: 12,
                ..SRGB
            },
            SpaColor { range: 2, ..SRGB },
            SpaColor { matrix: 3, ..SRGB },
        ] {
            for format in [PixelFormat::Bgra8, PixelFormat::Rgba10] {
                assert!(bad.validate(format, false).is_err(), "{bad:?}");
            }
        }
    }

    #[test]
    fn metadata_transition_is_validated_again() {
        assert!(SRGB.validate(PixelFormat::Bgra8, true).is_ok());
        assert!(
            SpaColor {
                transfer: 14,
                ..SRGB
            }
            .validate(PixelFormat::Bgra8, true)
            .is_err()
        );
        assert!(SRGB.validate(PixelFormat::Bgra8, true).is_ok());
    }
}
