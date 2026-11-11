//! HDR10 static metadata and HEVC prefix SEI. No mastering values are invented.

use serde::{Deserialize, Serialize};

/// SMPTE ST 2086 mastering display; coordinates use HEVC's 1/50000 units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MasteringDisplay {
    /// Red, green, blue xy coordinates (RGB order, not HEVC's GBR wire order).
    pub primaries: [[u16; 2]; 3],
    /// White point xy coordinates.
    pub white_point: [u16; 2],
    /// Minimum luminance, in 1/10000 cd/m².
    pub min_luminance: u32,
    /// Maximum luminance, in 1/10000 cd/m².
    pub max_luminance: u32,
}

/// CTA-861 content light levels, in cd/m²; zero means unspecified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentLight {
    /// Maximum content light level.
    pub max_cll: u16,
    /// Maximum frame-average light level.
    pub max_fall: u16,
}

/// Optional source metadata, independent of the BT.2020/PQ signal encoding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::unsafe_derive_deserialize)] // The unsafe constructor reads an independent AVFrame; every numeric record is validated before use.
pub struct HdrMetadata {
    /// Actual mastering display metadata, if available.
    pub mastering: Option<MasteringDisplay>,
    /// Actual content light levels, if available.
    pub content_light: Option<ContentLight>,
}

impl HdrMetadata {
    /// Read an explicit source metadata document (not monitor calibration).
    ///
    /// # Errors
    /// Returns a TOML decoding error for malformed documents.
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
    /// Check metadata before sending it to a decoder or compositor.
    ///
    /// # Errors
    /// Returns a diagnostic for invalid chromaticities/luminance ranges.
    pub fn validate(self) -> Result<(), &'static str> {
        if let Some(m) = self.mastering
            && (m
                .primaries
                .into_iter()
                .chain([m.white_point])
                .any(|p| p[0] == 0 || p[1] == 0 || u32::from(p[0]) + u32::from(p[1]) > 50000)
                || m.max_luminance <= m.min_luminance
                || m.max_luminance > 100_000_000)
        {
            return Err("invalid HDR mastering primaries/luminance");
        }
        if let Some(c) = self.content_light
            && (c.max_cll > 10000
                || c.max_fall > 10000
                || (c.max_cll != 0 && c.max_fall > c.max_cll))
        {
            return Err("invalid HDR content light levels");
        }
        Ok(())
    }

    /// Encode HEVC prefix SEI (types 137/144), including emulation prevention.
    /// `FFmpeg` 7 NVENC does not emit these from `AVFrame` mastering side data.
    ///
    /// # Errors
    /// Returns an error for invalid metadata.
    pub fn hevc_sei(self) -> Result<Vec<u8>, &'static str> {
        self.validate()?;
        let mut rbsp = Vec::new();
        if let Some(m) = self.mastering {
            rbsp.extend([137, 24]);
            for p in [
                m.primaries[1],
                m.primaries[2],
                m.primaries[0],
                m.white_point,
            ] {
                for c in p {
                    rbsp.extend(c.to_be_bytes());
                }
            }
            rbsp.extend(m.max_luminance.to_be_bytes());
            rbsp.extend(m.min_luminance.to_be_bytes());
        }
        if let Some(c) = self.content_light {
            rbsp.extend([144, 4]);
            rbsp.extend(c.max_cll.to_be_bytes());
            rbsp.extend(c.max_fall.to_be_bytes());
        }
        if rbsp.is_empty() {
            return Ok(Vec::new());
        }
        rbsp.push(0x80); // rbsp_trailing_bits
        let mut nal = vec![0, 0, 0, 1, 39 << 1, 1];
        let mut zeros = 0;
        for byte in rbsp {
            if zeros == 2 && byte <= 3 {
                nal.push(3);
                zeros = 0;
            }
            nal.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        Ok(nal)
    }

    /// Read `FFmpeg`'s decoded static HDR side data, checking buffer lengths.
    /// Incomplete mastering metadata is not promoted to a full record.
    /// Call `validate` before using the decoded numeric values.
    ///
    /// # Safety
    /// `frame` must point to a live `AVFrame` and its side data must be valid.
    #[must_use]
    pub unsafe fn from_avframe(frame: *const ffmpeg_sys_next::AVFrame) -> Self {
        use ffmpeg_sys_next::AVFrameSideDataType as S;
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct AvMastering {
            primaries: [[ffmpeg_sys_next::AVRational; 2]; 3],
            white: [ffmpeg_sys_next::AVRational; 2],
            min: ffmpeg_sys_next::AVRational,
            max: ffmpeg_sys_next::AVRational,
            has_primaries: libc::c_int,
            has_luminance: libc::c_int,
        }
        fn scaled(q: ffmpeg_sys_next::AVRational, scale: u32) -> Option<u32> {
            if q.num < 0 || q.den <= 0 {
                return None;
            }
            let value = (u64::try_from(q.num).ok()? * u64::from(scale)
                + u64::try_from(q.den).ok()? / 2)
                / u64::try_from(q.den).ok()?;
            u32::try_from(value).ok()
        }
        fn parse_mastering(m: AvMastering) -> Option<MasteringDisplay> {
            let coord = |q| scaled(q, 50000).and_then(|v| u16::try_from(v).ok());
            let primary = |p: [ffmpeg_sys_next::AVRational; 2]| Some([coord(p[0])?, coord(p[1])?]);
            Some(MasteringDisplay {
                primaries: [
                    primary(m.primaries[0])?,
                    primary(m.primaries[1])?,
                    primary(m.primaries[2])?,
                ],
                white_point: primary(m.white)?,
                min_luminance: scaled(m.min, 10000)?,
                max_luminance: scaled(m.max, 10000)?,
            })
        }
        let mut result = Self::default();
        let side = unsafe {
            ffmpeg_sys_next::av_frame_get_side_data(
                frame,
                S::AV_FRAME_DATA_MASTERING_DISPLAY_METADATA,
            )
        };
        if !side.is_null() && unsafe { (*side).size } >= std::mem::size_of::<AvMastering>() {
            let m = unsafe { (*side).data.cast::<AvMastering>().read_unaligned() };
            if m.has_primaries != 0 && m.has_luminance != 0 {
                result.mastering = parse_mastering(m);
            }
        }
        let side = unsafe {
            ffmpeg_sys_next::av_frame_get_side_data(frame, S::AV_FRAME_DATA_CONTENT_LIGHT_LEVEL)
        };
        if !side.is_null() && unsafe { (*side).size } >= 8 {
            let data = unsafe { std::slice::from_raw_parts((*side).data, 8) };
            if let (Ok(max_cll), Ok(max_fall)) = (
                u16::try_from(u32::from_ne_bytes([data[0], data[1], data[2], data[3]])),
                u16::try_from(u32::from_ne_bytes([data[4], data[5], data[6], data[7]])),
            ) {
                result.content_light = Some(ContentLight { max_cll, max_fall });
            }
        }
        result
    }
}

/// Standard PQ inverse EOTF for generating diagnostic/test RGB code values.
#[must_use]
pub fn pq_from_nits(nits: f64) -> f64 {
    let p = (nits.clamp(0.0, 10000.0) / 10000.0).powf(2610.0 / 16384.0);
    ((3424.0 / 4096.0 + 2413.0 / 128.0 * p) / (1.0 + 2392.0 / 128.0 * p)).powf(2523.0 / 32.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pq_reference_white_and_highlights_are_absolute() {
        for (nits, expected) in [
            (0.0, 0.0),
            (100.0, 0.508_078),
            (203.0, 0.580_689),
            (1000.0, 0.751_827),
            (10000.0, 1.0),
        ] {
            assert!((pq_from_nits(nits) - expected).abs() < 0.00001);
        }
    }
    #[test]
    fn missing_metadata_does_not_invent_mastering_or_sei() {
        assert_eq!(HdrMetadata::default().hevc_sei().unwrap(), Vec::<u8>::new());
    }
    #[test]
    fn content_light_sei_escapes_start_codes() {
        let bytes = HdrMetadata {
            mastering: None,
            content_light: Some(ContentLight {
                max_cll: 0,
                max_fall: 0,
            }),
        }
        .hevc_sei()
        .unwrap();
        assert_eq!(&bytes[..6], &[0, 0, 0, 1, 78, 1]);
        assert!(bytes[6..].windows(3).all(|b| b != [0, 0, 1]));
    }
}
