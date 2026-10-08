//! Extraction inputs and results. No decoding and no filesystem.

use serde::Serialize;

use super::frame::Frame;
use super::sps::ParamSource;

pub struct Options {
    pub width: u32,
    pub height: u32,
    pub include_non_idr: bool,
    pub include_partial: bool,
    /// Enable two-pass audio-resync walking. Off by default; pass `--with-audio` for files that
    /// interleave an audio track in `mdat`.
    pub with_audio: bool,
    /// Also recover and decode P-frames (inter-coded pictures), recovering the full video stream.
    pub all_frames: bool,
}

/// One access unit: the run of VCL NALs belonging to a single picture.
#[derive(Debug)]
pub struct AccessUnit {
    pub nal_indices: Vec<usize>,
    pub offset: u64,
    pub is_idr: bool,
    pub is_intra: bool,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameRecord {
    pub index: usize,
    pub offset: u64,
    /// Bytes this picture occupies in the source file: every NAL's length prefix plus the payload
    /// bytes actually present. For a partial picture this is less than the container declares.
    pub source_size: u64,
    pub nal_type: u8,
    pub slice_count: usize,
    pub partial: bool,
    pub decoded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sps_profile: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub input: String,
    pub width: u32,
    pub height: u32,
    pub mdat_payload_start: u64,
    pub nal_length_size: usize,
    pub nal_count: usize,
    pub truncation_offset: Option<u64>,
    /// Runs inside `mdat` that are not part of the video chain - the interleaved chunks of another
    /// track, usually audio - which the walk stepped over.
    pub skipped_regions: usize,
    pub skipped_bytes: u64,
    /// End of the last fully received picture. Everything before this is intact data; anything
    /// after it is either the frame straddling the cut or padding.
    pub trusted_data_end: u64,
    pub param_source: ParamSource,
    pub profile_idc: u32,
    pub pic_init_qp: i32,
    pub qp_search_attempts: usize,
    pub x264_options_found: bool,
    pub access_units: usize,
    pub intra_access_units: usize,
    pub frames: Vec<FrameRecord>,
}

pub struct Extraction {
    pub report: Report,
    pub frames: Vec<(FrameRecord, Frame)>,
    pub annexb: Vec<u8>,
}
