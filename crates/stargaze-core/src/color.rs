//! Color descriptions are independent of pixel storage and output HDR state.

use serde::{Deserialize, Serialize};

/// CICP color description plus component precision (not an HDR flag).
///
/// Numeric codes follow H.273 / HEVC VUI, not SPA's different enum values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorDescription {
    /// CICP color primaries (1 = BT.709, 9 = BT.2020).
    pub primaries: u8,
    /// CICP transfer characteristic (13 = sRGB, 16 = PQ, 18 = HLG).
    pub transfer: u8,
    /// CICP matrix coefficients (0 = RGB, 1 = BT.709, 9 = BT.2020 NCL).
    pub matrix: u8,
    /// Full sample range, rather than video/limited range.
    pub full_range: bool,
    /// Significant bits per component.
    pub bit_depth: u8,
}

impl Default for ColorDescription {
    fn default() -> Self {
        Self::SDR_STREAM
    }
}

impl ColorDescription {
    /// Full-range sRGB desktop pixels, with BT.709/sRGB primaries.
    pub const SRGB: Self = Self {
        primaries: 1,
        transfer: 13,
        matrix: 0,
        full_range: true,
        bit_depth: 8,
    };

    /// sRGB code values encoded with the limited-range BT.709 YUV matrix.
    /// No opto-electrical transfer is applied during matrix conversion.
    pub const SDR_STREAM: Self = Self {
        matrix: 1,
        full_range: false,
        ..Self::SRGB
    };

    /// Historical Stargaze VUI: sRGB code values mislabeled as BT.709 transfer.
    /// Accepted only for SDR peer compatibility, without changing code values.
    pub const LEGACY_SDR: Self = Self {
        transfer: 1,
        ..Self::SDR_STREAM
    };

    /// HDR10's limited-range BT.2020/PQ, 10-bit YUV description.
    /// Describing this mode does not imply pipeline support.
    pub const HDR10: Self = Self {
        primaries: 9,
        transfer: 16,
        matrix: 9,
        full_range: false,
        bit_depth: 10,
    };

    /// Full-range BT.2020/PQ RGB source, with 10-bit component precision.
    pub const HDR10_RGB: Self = Self {
        matrix: 0,
        full_range: true,
        ..Self::HDR10
    };

    /// Whether this is the supported native HDR10 stream description.
    #[must_use]
    pub fn is_hdr10(self) -> bool {
        self == Self::HDR10
    }

    /// Select the stream matching source color, without tone or gamut mapping.
    #[must_use]
    pub fn stream(self) -> Option<Self> {
        if self.is_supported_hdr_rgb() {
            Some(Self::HDR10)
        } else if self.is_supported_rgb() {
            Some(Self::SDR_STREAM)
        } else {
            None
        }
    }

    /// Whether source RGB is full-range BT.2020/PQ at adequate precision.
    #[must_use]
    pub fn is_supported_hdr_rgb(self) -> bool {
        Self {
            bit_depth: 10,
            ..self
        } == Self::HDR10_RGB
            && matches!(self.bit_depth, 10 | 16)
    }

    /// Whether the implemented renderers support this stream description.
    #[must_use]
    pub fn is_supported_stream(self) -> bool {
        self == Self::SDR_STREAM || self == Self::LEGACY_SDR || self == Self::HDR10
    }

    /// Whether source RGB can use the SDR converters without tone/gamut mapping.
    #[must_use]
    pub fn is_supported_rgb(self) -> bool {
        Self {
            bit_depth: 8,
            ..self
        } == Self::SRGB
            && matches!(self.bit_depth, 8 | 10)
    }
}

/// End-to-end client capabilities, not just decoder capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorCapabilities {
    /// Can present the SDR stream as an SDR/sRGB surface.
    pub sdr: bool,
    /// Can decode AND present HDR10 with compositor-visible color signaling.
    pub hdr10: bool,
}

impl ColorCapabilities {
    /// Capabilities for legacy peers and unmanaged/SDR-only presentation.
    pub const SDR_ONLY: Self = Self {
        sdr: true,
        hdr10: false,
    };

    /// Native Wayland HDR presentation plus SDR support.
    pub const HDR10_AND_SDR: Self = Self {
        sdr: true,
        hdr10: true,
    };

    /// Validate a selected mode; never silently relabel HDR as SDR.
    ///
    /// # Errors
    /// Returns a diagnostic if the complete implemented pipeline cannot present it.
    pub fn validate(self, color: ColorDescription) -> Result<(), &'static str> {
        if !color.is_supported_stream() {
            return Err("unsupported stream color; no implicit tone/gamut mapping");
        }
        if color.is_hdr10() {
            return if self.hdr10 {
                Ok(())
            } else {
                Err(
                    "HDR10 source requires a Main10 decoder and color-managed 10-bit Wayland presentation; peer is SDR-only",
                )
            };
        }
        if !self.sdr {
            return Err("peer does not support the server's SDR stream");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_is_not_dynamic_range() {
        assert!(
            ColorDescription {
                bit_depth: 10,
                ..ColorDescription::SRGB
            }
            .is_supported_rgb()
        );
        assert!(
            !ColorDescription {
                bit_depth: 8,
                ..ColorDescription::HDR10
            }
            .is_supported_stream()
        );
    }

    #[test]
    fn hdr_display_does_not_select_hdr_stream() {
        for hdr10 in [false, true] {
            let caps = ColorCapabilities { sdr: true, hdr10 };
            assert!(caps.validate(ColorDescription::SDR_STREAM).is_ok());
            assert_eq!(caps.validate(ColorDescription::HDR10).is_ok(), hdr10);
        }
    }

    #[test]
    fn range_and_colorimetry_must_match() {
        let sdr = ColorDescription::SDR_STREAM;
        for bad in [
            ColorDescription {
                full_range: true,
                ..sdr
            },
            ColorDescription {
                primaries: 9,
                ..sdr
            },
            ColorDescription {
                transfer: 18,
                ..sdr
            },
            ColorDescription { matrix: 6, ..sdr },
            ColorDescription {
                bit_depth: 10,
                ..sdr
            },
        ] {
            assert!(ColorCapabilities::SDR_ONLY.validate(bad).is_err());
        }
        assert!(
            ColorCapabilities::SDR_ONLY
                .validate(ColorDescription::LEGACY_SDR)
                .is_ok()
        );
    }
}
