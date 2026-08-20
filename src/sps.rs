//! SPS/PPS recovery.
//!
//! The parameter sets normally live in `avcC` inside `moov`, which a truncated file has lost.
//! Two paths: reuse in-band sets when the encoder repeated them, otherwise synthesise them from
//! the known resolution plus the x264 option SEI.
//!
//! The synthesised values below were calibrated against the real `avcC` of all 66 intact corpus
//! files: `log2_max_frame_num` is 4 everywhere regardless of keyint, `poc_type` is 0 (lsb 6)
//! unless there are no B-frames, in which case it is 2, scaling matrices are never present and
//! `gaps_in_frame_num_value_allowed` is always 0.

use crate::bits::{BitWriter, escape_rbsp};
use crate::sei::X264Options;

#[derive(Debug, Clone)]
pub struct ParameterSets {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub source: ParamSource,
    pub pic_init_qp: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamSource {
    /// SPS/PPS were repeated in-band by the encoder.
    InBand,
    /// Reconstructed from resolution + x264 SEI options.
    Synthesized,
}

pub const PROFILE_BASELINE: u32 = 66;
pub const PROFILE_MAIN: u32 = 77;
pub const PROFILE_HIGH: u32 = 100;

pub fn profile_for(opts: &X264Options) -> u32 {
    if opts.transform_8x8 {
        PROFILE_HIGH
    } else if !opts.cabac && opts.bframes == 0 {
        PROFILE_BASELINE
    } else {
        PROFILE_MAIN
    }
}

/// Level is not needed to parse the bitstream, only to size the DPB. Pick one high enough that
/// the decoder never rejects the stream.
const LEVEL_IDC: u32 = 51;

pub fn synthesize(width: u32, height: u32, opts: &X264Options, pic_init_qp: i32) -> ParameterSets {
    let profile = profile_for(opts);
    let mb_w = width.div_ceil(16);
    let mb_h = height.div_ceil(16);
    // CropUnitX/Y are 2/2 for 4:2:0 with frame_mbs_only_flag = 1.
    let crop_right = (mb_w * 16 - width) / 2;
    let crop_bottom = (mb_h * 16 - height) / 2;

    let mut w = BitWriter::new();
    w.u(8, 0x67); // nal_ref_idc = 3, nal_unit_type = 7
    w.u(8, profile);
    // constraint_set flags: mark Constrained Baseline when that is what we claim.
    w.u(8, if profile == PROFILE_BASELINE { 0x40 } else { 0x00 });
    w.u(8, LEVEL_IDC);
    w.ue(0); // seq_parameter_set_id

    if profile == PROFILE_HIGH {
        w.ue(1); // chroma_format_idc = 4:2:0
        w.ue(0); // bit_depth_luma_minus8
        w.ue(0); // bit_depth_chroma_minus8
        w.flag(false); // qpprime_y_zero_transform_bypass_flag
        w.flag(false); // seq_scaling_matrix_present_flag
    }

    w.ue(0); // log2_max_frame_num_minus4 -> 4
    if opts.bframes > 0 {
        w.ue(0); // pic_order_cnt_type
        w.ue(2); // log2_max_pic_order_cnt_lsb_minus4 -> 6
    } else {
        w.ue(2); // pic_order_cnt_type = 2
    }
    w.ue(max_num_ref_frames(opts));
    w.flag(false); // gaps_in_frame_num_value_allowed_flag
    w.ue(mb_w - 1);
    w.ue(mb_h - 1);
    w.flag(true); // frame_mbs_only_flag
    w.flag(true); // direct_8x8_inference_flag

    let cropping = crop_right > 0 || crop_bottom > 0;
    w.flag(cropping);
    if cropping {
        w.ue(0); // left
        w.ue(crop_right);
        w.ue(0); // top
        w.ue(crop_bottom);
    }
    w.flag(false); // vui_parameters_present_flag

    let sps = escape_rbsp(&w.finish());

    let mut p = BitWriter::new();
    p.u(8, 0x68); // nal_ref_idc = 3, nal_unit_type = 8
    p.ue(0); // pic_parameter_set_id
    p.ue(0); // seq_parameter_set_id
    p.flag(opts.cabac); // entropy_coding_mode_flag
    p.flag(false); // bottom_field_pic_order_in_frame_present_flag
    p.ue(0); // num_slice_groups_minus1
    p.ue(opts.refs.saturating_sub(1)); // num_ref_idx_l0_default_active_minus1
    p.ue(0); // num_ref_idx_l1_default_active_minus1
    p.flag(opts.weighted_pred);
    p.u(2, if opts.weighted_bipred { 2 } else { 0 });
    p.se(pic_init_qp - 26);
    p.se(0); // pic_init_qs_minus26
    p.se(opts.chroma_qp_offset);
    p.flag(true); // deblocking_filter_control_present_flag
    p.flag(opts.constrained_intra);
    p.flag(false); // redundant_pic_cnt_present_flag
    if profile == PROFILE_HIGH {
        p.flag(opts.transform_8x8);
        p.flag(false); // pic_scaling_matrix_present_flag
        p.se(opts.chroma_qp_offset); // second_chroma_qp_index_offset
    }

    let pps = escape_rbsp(&p.finish());

    ParameterSets {
        sps,
        pps,
        source: ParamSource::Synthesized,
        pic_init_qp,
    }
}

/// Matches x264's `x264_sps_init`: DPB size is max(ref, 1+reorder, pyramid?4:1), then one slot
/// is dropped for a strict B-pyramid. The old `refs+1` rule only agreed with that formula for
/// the corpus's `ref=3` / normal-pyramid encodes.
fn max_num_ref_frames(opts: &X264Options) -> u32 {
    if opts.bframes == 0 {
        return opts.refs.min(16);
    }
    let pyramid = opts.b_pyramid;
    let num_reorder = if pyramid > 0 { 2 } else { 1 };
    let mut n = opts
        .refs
        .max(1 + num_reorder)
        .max(if pyramid > 0 { 4 } else { 1 })
        .min(16);
    if pyramid == 1 {
        n = n.saturating_sub(1);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(refs: u32, bframes: u32, b_pyramid: u32) -> X264Options {
        X264Options {
            refs,
            bframes,
            b_pyramid,
            ..X264Options::default()
        }
    }

    #[test]
    fn dpb_matches_x264_for_corpus_and_high_ref() {
        // Corpus: ref=3, bframes=3, normal pyramid → max(3, 3, 4) = 4.
        assert_eq!(max_num_ref_frames(&opts(3, 3, 2)), 4);
        // test.mp4: ref=5, bframes=3, normal pyramid → max(5, 3, 4) = 5, not refs+1=6.
        assert_eq!(max_num_ref_frames(&opts(5, 3, 2)), 5);
        assert_eq!(max_num_ref_frames(&opts(3, 0, 0)), 3);
        assert_eq!(max_num_ref_frames(&opts(4, 3, 1)), 3); // strict pyramid drops one
    }
}
