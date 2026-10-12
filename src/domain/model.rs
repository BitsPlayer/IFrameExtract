//! Extraction inputs and results. No decoding and no filesystem.

use serde::Serialize;

use super::codec::Codec;
use super::container::ContainerInfo;
use super::frame::Frame;
use super::hevc::params::HevcParams;
use super::nal::Nal;
use super::sei::X264Options;
use super::sps::{ParamSource, ParameterSets};

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
    /// Force a codec instead of detecting it from the bitstream.
    pub codec: Option<Codec>,
}

/// The inputs analysis needs: everything in `Options` that affects carving and parameter recovery.
#[derive(Debug, Clone)]
pub struct CarveOptions {
    pub width: u32,
    pub height: u32,
    pub with_audio: bool,
    /// Force a codec instead of detecting it from the bitstream.
    pub codec: Option<Codec>,
}

/// Which access units `decode` should decode.
#[derive(Debug, Clone)]
pub enum Selection {
    Filtered {
        include_non_idr: bool,
        include_partial: bool,
        all_frames: bool,
    },
    /// An index into `CarveAnalysis::access_units`.
    Single(usize),
}

/// Everything recovered without decoding a picture: the NAL chain, access units and parameter sets.
#[derive(Debug, Clone)]
pub struct CarveAnalysis {
    pub codec: Codec,
    pub container: ContainerInfo,
    pub width: u32,
    pub height: u32,
    pub with_audio: bool,
    pub mdat_payload_start: u64,
    pub mdat_payload_end: u64,
    pub nal_length_size: usize,
    pub nals: Vec<Nal>,
    /// Stepped-over `(offset, len)` regions inside `mdat`.
    pub skipped: Vec<(u64, u64)>,
    pub truncation_offset: Option<u64>,
    pub trusted_data_end: u64,
    pub x264_opts: X264Options,
    pub x264_options_found: bool,
    /// SPS and PPS the pictures decode with. For H.265 the VPS is in `hevc`.
    pub params: ParameterSets,
    pub qp_attempts: usize,
    pub access_units: Vec<AccessUnit>,
    /// H.265 only.
    pub hevc: Option<HevcState>,
}

/// What H.265 recovery found beyond the SPS/PPS pair.
#[derive(Debug, Clone)]
pub struct HevcState {
    pub vps: Vec<u8>,
    /// The model behind the parameter sets: always for synthesised ones, and for in-band ones the
    /// validator can represent.
    pub model: Option<HevcParams>,
    /// Which known encoder (or the sweep) the synthesised set came from.
    pub label: Option<String>,
    pub x265_options_found: bool,
}

/// One access unit: the run of VCL NALs belonging to a single picture.
#[derive(Debug, Clone)]
pub struct AccessUnit {
    pub nal_indices: Vec<usize>,
    pub offset: u64,
    /// Type of the first VCL NAL.
    pub nal_type: u8,
    /// A picture that decodes on its own: IDR for H.264, any IRAP for H.265.
    pub is_idr: bool,
    pub is_intra: bool,
    pub complete: bool,
    /// Bytes this picture occupies in the source file, as `FrameRecord::source_size`.
    pub size: u64,
    /// Index into the access unit list of the IDR starting this GOP. Pictures before the first
    /// IDR share group 0, i.e. they are grouped with the start of the list.
    pub gop: usize,
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
    pub codec: Codec,
    pub container: ContainerInfo,
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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub x265_options_found: bool,
    pub access_units: usize,
    pub intra_access_units: usize,
    /// The H.265 parameter set the frames were decoded with, when it was synthesised or could be
    /// read back from the in-band one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hevc_params: Option<HevcParams>,
    pub frames: Vec<FrameRecord>,
}

pub struct Extraction {
    pub report: Report,
    pub frames: Vec<(FrameRecord, Frame)>,
    pub annexb: Vec<u8>,
}
