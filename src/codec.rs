//! The two NAL grammars the carver understands.
//!
//! H.264 and H.265 share the length-prefixed sample layout, so the chain walker is the same for
//! both. What differs is how a unit is recognised: header layout, which types an encoder can emit,
//! and how a slice announces the start of a new picture.

use crate::hevc;
use crate::nal::{NAL_IDR, NAL_PPS, NAL_SEI, NAL_SLICE, NAL_SPS};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    H265,
}

/// NAL unit types x264 actually emits. Anything else is treated as a broken chain, which is what
/// makes `startcodes` and `random` padding fail to produce plausible successors.
const H264_ALLOWED_TYPES: [u8; 7] = [1, 5, 6, 7, 8, 9, 12];

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "H.265",
        }
    }

    /// Format name for ffmpeg's raw elementary stream demuxer.
    pub fn ffmpeg_format(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "hevc",
        }
    }

    pub fn annexb_extension(self) -> &'static str {
        match self {
            Codec::H264 => "264",
            Codec::H265 => "265",
        }
    }

    /// Bytes of NAL header. A unit shorter than this cannot even be classified.
    pub fn header_len(self) -> usize {
        match self {
            Codec::H264 => 1,
            Codec::H265 => 2,
        }
    }

    pub fn nal_type(self, payload: &[u8]) -> u8 {
        match self {
            Codec::H264 => payload[0] & 0x1f,
            Codec::H265 => hevc::nal_type(payload),
        }
    }

    pub fn is_vcl(self, nal_type: u8) -> bool {
        match self {
            Codec::H264 => nal_type == NAL_SLICE || nal_type == NAL_IDR,
            Codec::H265 => hevc::is_vcl(nal_type),
        }
    }

    /// A picture that can be decoded on its own: IDR for H.264, any IRAP for H.265.
    pub fn is_keyframe(self, nal_type: u8) -> bool {
        match self {
            Codec::H264 => nal_type == NAL_IDR,
            Codec::H265 => hevc::is_irap(nal_type),
        }
    }

    /// Header-level validation of a unit's first bytes.
    pub fn header_ok(self, payload: &[u8]) -> bool {
        match self {
            Codec::H264 => {
                let Some(&byte) = payload.first() else {
                    return false;
                };
                if byte & 0x80 != 0 {
                    // forbidden_zero_bit
                    return false;
                }
                let nal_type = byte & 0x1f;
                if !H264_ALLOWED_TYPES.contains(&nal_type) {
                    return false;
                }
                // The spec fixes nal_ref_idc for these types, so it is free validation against
                // garbage that happens to carry a plausible type.
                let ref_idc = (byte >> 5) & 0x03;
                match nal_type {
                    NAL_IDR | NAL_SPS | NAL_PPS => ref_idc != 0,
                    NAL_SEI | 9 | 12 => ref_idc == 0,
                    _ => true,
                }
            }
            Codec::H265 => hevc::header_ok(payload),
        }
    }

    /// A slice NAL is never a couple of bytes; this rejects the tiny units that `startcodes`
    /// padding synthesises.
    pub fn min_len(self, nal_type: u8) -> usize {
        match self {
            Codec::H264 if self.is_vcl(nal_type) => 8,
            Codec::H264 => 2,
            Codec::H265 if self.is_vcl(nal_type) => 8,
            Codec::H265 => 3,
        }
    }

    /// Slice NALs get a second opinion from their header: a fabricated unit rarely yields a
    /// plausible macroblock address or parameter set id.
    pub fn slice_header_ok(self, payload: &[u8], mb_count: u32) -> bool {
        match self {
            Codec::H264 => match crate::slice::parse(payload) {
                Some(h) => h.first_mb_in_slice < mb_count && h.slice_type <= 9,
                None => false,
            },
            Codec::H265 => hevc::slice::peek(payload).is_some_and(|h| h.pps_id <= 63),
        }
    }

    /// Whether a slice is the first of its picture. `None` when the header does not parse.
    pub fn starts_picture(self, payload: &[u8]) -> Option<bool> {
        match self {
            Codec::H264 => crate::slice::parse(payload).map(|h| h.first_mb_in_slice == 0),
            Codec::H265 => hevc::slice::peek(payload).map(|h| h.first_slice_segment_in_pic),
        }
    }
}
