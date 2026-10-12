//! HEVC parameter sets: the fields that matter for decoding, and writers that turn them into
//! VPS/SPS/PPS NAL units.
//!
//! Only the subset a recovered stream can need is modelled. Scaling lists, PCM, tiles, long-term
//! references and SPS-level reference picture sets are always off in what we synthesise; none of
//! x265's presets nor the hardware encoders surveyed use them.

use crate::bits::{BitReader, BitWriter, escape_rbsp, unescape_rbsp};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HevcParams {
    /// Display size. The coded size is rounded up to the minimum coding block and cropped back
    /// with a conformance window.
    pub width: u32,
    pub height: u32,
    /// Alignment of the coded size, as log2. Software encoders round up to the minimum coding
    /// block; hardware often rounds to 16 or to a whole CTB (1080 rows coded as 1088).
    pub coded_align_log2: u32,
    pub bit_depth: u32,

    pub log2_min_cb: u32,
    pub log2_ctb: u32,
    pub log2_min_tb: u32,
    pub log2_max_tb: u32,
    pub max_transform_depth_inter: u32,
    pub max_transform_depth_intra: u32,
    /// Scaling lists on, with the spec's default matrices. Apple's encoder does this; it changes
    /// dequantisation only, so no parse can detect it.
    pub scaling_list: bool,
    pub amp: bool,
    pub sao: bool,
    pub strong_intra_smoothing: bool,
    pub temporal_mvp: bool,
    pub log2_max_poc_lsb: u32,
    pub max_dec_pic_buffering: u32,
    pub num_reorder_pics: u32,

    pub dependent_slice_segments: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_l0_default: u32,
    pub num_ref_idx_l1_default: u32,
    /// `26 + init_qp_minus26`.
    pub init_qp: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip: bool,
    /// `diff_cu_qp_delta_depth` when `cu_qp_delta_enabled_flag` is set.
    pub cu_qp_delta_depth: Option<u32>,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass: bool,
    pub entropy_coding_sync: bool,
    pub loop_filter_across_slices: bool,
    pub deblocking_override_enabled: bool,
    pub deblocking_disabled: bool,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level: u32,
}

impl HevcParams {
    /// x265's `--preset medium` defaults, which is what ffmpeg's libx265 produces unless told
    /// otherwise. Every other candidate is a variation of this one.
    pub fn x265_default(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            coded_align_log2: 3,
            bit_depth: 8,
            log2_min_cb: 3,
            log2_ctb: 6,
            log2_min_tb: 2,
            log2_max_tb: 5,
            max_transform_depth_inter: 0,
            max_transform_depth_intra: 0,
            scaling_list: false,
            amp: false,
            sao: true,
            strong_intra_smoothing: true,
            temporal_mvp: true,
            log2_max_poc_lsb: 8,
            max_dec_pic_buffering: 5,
            num_reorder_pics: 2,
            dependent_slice_segments: false,
            output_flag_present: false,
            num_extra_slice_header_bits: 0,
            sign_data_hiding: true,
            cabac_init_present: false,
            num_ref_idx_l0_default: 1,
            num_ref_idx_l1_default: 1,
            init_qp: 26,
            constrained_intra_pred: false,
            transform_skip: false,
            cu_qp_delta_depth: Some(1),
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            slice_chroma_qp_offsets_present: false,
            weighted_pred: true,
            weighted_bipred: false,
            transquant_bypass: false,
            entropy_coding_sync: true,
            loop_filter_across_slices: true,
            deblocking_override_enabled: false,
            deblocking_disabled: false,
            lists_modification_present: false,
            log2_parallel_merge_level: 2,
        }
    }

    fn align(&self) -> u32 {
        1 << self.coded_align_log2.max(self.log2_min_cb)
    }

    pub fn coded_width(&self) -> u32 {
        self.width.next_multiple_of(self.align())
    }

    pub fn coded_height(&self) -> u32 {
        self.height.next_multiple_of(self.align())
    }

    pub fn width_in_ctbs(&self) -> u32 {
        self.coded_width().div_ceil(1 << self.log2_ctb)
    }

    pub fn height_in_ctbs(&self) -> u32 {
        self.coded_height().div_ceil(1 << self.log2_ctb)
    }

    pub fn qp_bd_offset(&self) -> i32 {
        6 * (self.bit_depth as i32 - 8)
    }

    /// Sanity of the block-size hierarchy; the validator assumes it.
    pub fn is_consistent(&self) -> bool {
        (3..=6).contains(&self.log2_min_cb)
            && (self.log2_min_cb..=6).contains(&self.log2_ctb)
            && self.log2_ctb >= 4
            && self.log2_min_tb == 2
            && self.log2_min_tb < self.log2_min_cb
            && (self.log2_min_tb..=5.min(self.log2_ctb)).contains(&self.log2_max_tb)
            && self.max_transform_depth_intra <= self.log2_ctb - self.log2_min_tb
            && self
                .cu_qp_delta_depth
                .is_none_or(|d| d <= self.log2_ctb - self.log2_min_cb)
    }

    fn profile_idc(&self) -> u32 {
        if self.bit_depth > 8 { 2 } else { 1 }
    }

    fn write_header(w: &mut BitWriter, nal_type: u32) {
        w.u(1, 0); // forbidden_zero_bit
        w.u(6, nal_type);
        w.u(6, 0); // nuh_layer_id
        w.u(3, 1); // nuh_temporal_id_plus1
    }

    fn write_ptl(&self, w: &mut BitWriter) {
        w.u(2, 0); // general_profile_space
        w.flag(false); // general_tier_flag
        let profile = self.profile_idc();
        w.u(5, profile);
        // Main10 decoders accept Main, so a Main stream claims both.
        let compat = if profile == 1 {
            (1u32 << 30) | (1 << 29)
        } else {
            1 << 29
        };
        w.u(32, compat);
        w.flag(true); // progressive_source
        w.flag(false); // interlaced_source
        w.flag(false); // non_packed_constraint
        w.flag(true); // frame_only_constraint
        w.u(32, 0); // 43 reserved bits + general_inbld_flag
        w.u(12, 0);
        // Level 6.2: high enough that no decoder rejects the stream for size.
        w.u(8, 186);
    }

    pub fn vps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        Self::write_header(&mut w, super::NAL_VPS as u32);
        w.u(4, 0); // vps_video_parameter_set_id
        w.flag(true); // vps_base_layer_internal_flag
        w.flag(true); // vps_base_layer_available_flag
        w.u(6, 0); // vps_max_layers_minus1
        w.u(3, 0); // vps_max_sub_layers_minus1
        w.flag(true); // vps_temporal_id_nesting_flag
        w.u(16, 0xffff);
        self.write_ptl(&mut w);
        w.flag(true); // vps_sub_layer_ordering_info_present_flag
        w.ue(self.max_dec_pic_buffering - 1);
        w.ue(self.num_reorder_pics);
        w.ue(0); // vps_max_latency_increase_plus1
        w.u(6, 0); // vps_max_layer_id
        w.ue(0); // vps_num_layer_sets_minus1
        w.flag(false); // vps_timing_info_present_flag
        w.flag(false); // vps_extension_flag
        escape_rbsp(&w.finish())
    }

    pub fn sps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        Self::write_header(&mut w, super::NAL_SPS as u32);
        w.u(4, 0); // sps_video_parameter_set_id
        w.u(3, 0); // sps_max_sub_layers_minus1
        w.flag(true); // sps_temporal_id_nesting_flag
        self.write_ptl(&mut w);
        w.ue(0); // sps_seq_parameter_set_id
        w.ue(1); // chroma_format_idc = 4:2:0
        w.ue(self.coded_width());
        w.ue(self.coded_height());
        let crop_right = (self.coded_width() - self.width) / 2;
        let crop_bottom = (self.coded_height() - self.height) / 2;
        let crop = crop_right > 0 || crop_bottom > 0;
        w.flag(crop);
        if crop {
            w.ue(0);
            w.ue(crop_right);
            w.ue(0);
            w.ue(crop_bottom);
        }
        w.ue(self.bit_depth - 8); // luma
        w.ue(self.bit_depth - 8); // chroma
        w.ue(self.log2_max_poc_lsb - 4);
        w.flag(true); // sps_sub_layer_ordering_info_present_flag
        w.ue(self.max_dec_pic_buffering - 1);
        w.ue(self.num_reorder_pics);
        w.ue(0); // sps_max_latency_increase_plus1
        w.ue(self.log2_min_cb - 3);
        w.ue(self.log2_ctb - self.log2_min_cb);
        w.ue(self.log2_min_tb - 2);
        w.ue(self.log2_max_tb - self.log2_min_tb);
        w.ue(self.max_transform_depth_inter);
        w.ue(self.max_transform_depth_intra);
        w.flag(self.scaling_list); // scaling_list_enabled_flag
        if self.scaling_list {
            w.flag(false); // sps_scaling_list_data_present_flag: default matrices
        }
        w.flag(self.amp);
        w.flag(self.sao);
        w.flag(false); // pcm_enabled_flag
        w.ue(0); // num_short_term_ref_pic_sets
        w.flag(false); // long_term_ref_pics_present_flag
        w.flag(self.temporal_mvp);
        w.flag(self.strong_intra_smoothing);
        w.flag(false); // vui_parameters_present_flag
        w.flag(false); // sps_extension_present_flag
        escape_rbsp(&w.finish())
    }

    pub fn pps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        Self::write_header(&mut w, super::NAL_PPS as u32);
        w.ue(0); // pps_pic_parameter_set_id
        w.ue(0); // pps_seq_parameter_set_id
        w.flag(self.dependent_slice_segments);
        w.flag(self.output_flag_present);
        w.u(3, self.num_extra_slice_header_bits);
        w.flag(self.sign_data_hiding);
        w.flag(self.cabac_init_present);
        w.ue(self.num_ref_idx_l0_default - 1);
        w.ue(self.num_ref_idx_l1_default - 1);
        w.se(self.init_qp - 26);
        w.flag(self.constrained_intra_pred);
        w.flag(self.transform_skip);
        w.flag(self.cu_qp_delta_depth.is_some());
        if let Some(d) = self.cu_qp_delta_depth {
            w.ue(d);
        }
        w.se(self.cb_qp_offset);
        w.se(self.cr_qp_offset);
        w.flag(self.slice_chroma_qp_offsets_present);
        w.flag(self.weighted_pred);
        w.flag(self.weighted_bipred);
        w.flag(self.transquant_bypass);
        w.flag(false); // tiles_enabled_flag
        w.flag(self.entropy_coding_sync);
        w.flag(self.loop_filter_across_slices);
        let deblocking_control = self.deblocking_override_enabled || self.deblocking_disabled;
        w.flag(deblocking_control);
        if deblocking_control {
            w.flag(self.deblocking_override_enabled);
            w.flag(self.deblocking_disabled);
            if !self.deblocking_disabled {
                w.se(0); // pps_beta_offset_div2
                w.se(0); // pps_tc_offset_div2
            }
        }
        w.flag(false); // pps_scaling_list_data_present_flag
        w.flag(self.lists_modification_present);
        w.ue(self.log2_parallel_merge_level - 2);
        w.flag(false); // slice_segment_header_extension_present_flag
        w.flag(false); // pps_extension_present_flag
        escape_rbsp(&w.finish())
    }
}

fn skip_ptl(r: &mut BitReader, max_sub_layers_minus1: u32) -> Option<()> {
    r.u(32)?; // profile space, tier, idc, first 24 compat flags
    r.u(32)?; // last 8 compat flags, 4 source flags, 20 reserved
    r.u(24)?; // 23 reserved + inbld
    r.u(8)?; // general_level_idc
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.u(1)? == 1;
        level_present[i] = r.u(1)? == 1;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.u(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.u(32)?;
            r.u(32)?;
            r.u(24)?;
        }
        if level_present[i] {
            r.u(8)?;
        }
    }
    Some(())
}

/// Reads an SPS and PPS into `HevcParams`. `None` when they use something the validator does not
/// model (PCM, tiles, scaling list data, SPS-level reference picture sets, range extensions);
/// such streams are still decoded with their own parameter sets, just not re-validated.
pub fn parse(sps: &[u8], pps: &[u8]) -> Option<HevcParams> {
    let rbsp = unescape_rbsp(sps.get(2..)?);
    let mut r = BitReader::new(&rbsp);
    r.u(4)?;
    let max_sub_layers_minus1 = r.u(3)?;
    r.u(1)?;
    skip_ptl(&mut r, max_sub_layers_minus1)?;
    r.ue()?; // sps_seq_parameter_set_id
    if r.ue()? != 1 {
        return None; // only 4:2:0
    }
    let coded_w = r.ue()?;
    let coded_h = r.ue()?;
    let (mut crop_x, mut crop_y) = (0, 0);
    if r.u(1)? == 1 {
        crop_x += r.ue()?;
        crop_x += r.ue()?;
        crop_y += r.ue()?;
        crop_y += r.ue()?;
    }
    let bit_depth = r.ue()? + 8;
    if r.ue()? + 8 != bit_depth {
        return None;
    }
    let log2_max_poc_lsb = r.ue()? + 4;
    let ordering_all = r.u(1)? == 1;
    let (mut dpb, mut reorder) = (1, 0);
    for _ in if ordering_all {
        0
    } else {
        max_sub_layers_minus1
    }..=max_sub_layers_minus1
    {
        dpb = r.ue()? + 1;
        reorder = r.ue()?;
        r.ue()?;
    }
    let log2_min_cb = r.ue()? + 3;
    let log2_ctb = log2_min_cb + r.ue()?;
    let log2_min_tb = r.ue()? + 2;
    let log2_max_tb = log2_min_tb + r.ue()?;
    let max_transform_depth_inter = r.ue()?;
    let max_transform_depth_intra = r.ue()?;
    let scaling_list = r.u(1)? == 1;
    if scaling_list && r.u(1)? == 1 {
        return None; // sps_scaling_list_data_present_flag
    }
    let amp = r.u(1)? == 1;
    let sao = r.u(1)? == 1;
    if r.u(1)? == 1 {
        return None; // pcm_enabled_flag
    }
    if r.ue()? != 0 {
        return None; // num_short_term_ref_pic_sets
    }
    if r.u(1)? == 1 {
        return None; // long_term_ref_pics_present_flag
    }
    let temporal_mvp = r.u(1)? == 1;
    let strong_intra_smoothing = r.u(1)? == 1;

    let rbsp = unescape_rbsp(pps.get(2..)?);
    let mut r = BitReader::new(&rbsp);
    r.ue()?;
    r.ue()?;
    let dependent_slice_segments = r.u(1)? == 1;
    let output_flag_present = r.u(1)? == 1;
    let num_extra_slice_header_bits = r.u(3)?;
    let sign_data_hiding = r.u(1)? == 1;
    let cabac_init_present = r.u(1)? == 1;
    let num_ref_idx_l0_default = r.ue()? + 1;
    let num_ref_idx_l1_default = r.ue()? + 1;
    let init_qp = 26 + r.se()?;
    let constrained_intra_pred = r.u(1)? == 1;
    let transform_skip = r.u(1)? == 1;
    let cu_qp_delta_depth = if r.u(1)? == 1 { Some(r.ue()?) } else { None };
    let cb_qp_offset = r.se()?;
    let cr_qp_offset = r.se()?;
    let slice_chroma_qp_offsets_present = r.u(1)? == 1;
    let weighted_pred = r.u(1)? == 1;
    let weighted_bipred = r.u(1)? == 1;
    let transquant_bypass = r.u(1)? == 1;
    if r.u(1)? == 1 {
        return None; // tiles_enabled_flag
    }
    let entropy_coding_sync = r.u(1)? == 1;
    let loop_filter_across_slices = r.u(1)? == 1;
    let (mut deblocking_override_enabled, mut deblocking_disabled) = (false, false);
    if r.u(1)? == 1 {
        deblocking_override_enabled = r.u(1)? == 1;
        deblocking_disabled = r.u(1)? == 1;
        if !deblocking_disabled {
            r.se()?;
            r.se()?;
        }
    }
    if r.u(1)? == 1 {
        return None; // pps_scaling_list_data_present_flag
    }
    let lists_modification_present = r.u(1)? == 1;
    let log2_parallel_merge_level = r.ue()? + 2;
    if r.u(1)? == 1 {
        return None; // slice_segment_header_extension_present_flag
    }
    if r.u(1)? == 1 && r.u(1)? == 1 {
        return None; // pps_range_extension_flag
    }

    let mut params = HevcParams {
        width: coded_w.checked_sub(2 * crop_x)?,
        height: coded_h.checked_sub(2 * crop_y)?,
        coded_align_log2: log2_min_cb,
        bit_depth,
        log2_min_cb,
        log2_ctb,
        log2_min_tb,
        log2_max_tb,
        max_transform_depth_inter,
        max_transform_depth_intra,
        scaling_list,
        amp,
        sao,
        strong_intra_smoothing,
        temporal_mvp,
        log2_max_poc_lsb,
        max_dec_pic_buffering: dpb,
        num_reorder_pics: reorder,
        dependent_slice_segments,
        output_flag_present,
        num_extra_slice_header_bits,
        sign_data_hiding,
        cabac_init_present,
        num_ref_idx_l0_default,
        num_ref_idx_l1_default,
        init_qp,
        constrained_intra_pred,
        transform_skip,
        cu_qp_delta_depth,
        cb_qp_offset,
        cr_qp_offset,
        slice_chroma_qp_offsets_present,
        weighted_pred,
        weighted_bipred,
        transquant_bypass,
        entropy_coding_sync,
        loop_filter_across_slices,
        deblocking_override_enabled,
        deblocking_disabled,
        lists_modification_present,
        log2_parallel_merge_level,
    };
    // The coded size must be one our own writer can reproduce from the display size, or the model
    // cannot represent the stream.
    (log2_min_cb..=6).find_map(|a| {
        params.coded_align_log2 = a;
        (params.coded_width() == coded_w && params.coded_height() == coded_h)
            .then(|| params.clone())
    })
}

/// The PPS fields that come before `slice_type` in a slice header, which is all that is needed to
/// classify a slice once the parameter sets are known.
#[derive(Debug, Clone, Copy, Default)]
pub struct SliceTypeFields {
    pub dependent_slice_segments: bool,
    pub num_extra_slice_header_bits: u32,
}

impl From<&HevcParams> for SliceTypeFields {
    fn from(p: &HevcParams) -> Self {
        Self {
            dependent_slice_segments: p.dependent_slice_segments,
            num_extra_slice_header_bits: p.num_extra_slice_header_bits,
        }
    }
}

/// Reads the leading fields of an in-band PPS.
pub fn parse_pps_slice_fields(pps: &[u8]) -> Option<SliceTypeFields> {
    let rbsp = unescape_rbsp(pps.get(2..)?);
    let mut r = BitReader::new(&rbsp);
    r.ue()?; // pps_pic_parameter_set_id
    r.ue()?; // pps_seq_parameter_set_id
    let dependent_slice_segments = r.u(1)? == 1;
    r.u(1)?; // output_flag_present_flag
    let num_extra_slice_header_bits = r.u(3)?;
    Some(SliceTypeFields {
        dependent_slice_segments,
        num_extra_slice_header_bits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What we write must read back as the same model, or the validator would be checking a
    /// different parameter set from the one the decoder receives.
    #[test]
    fn written_parameter_sets_parse_back_identically() {
        let base = HevcParams::x265_default(1366, 768);
        let variants = [
            base.clone(),
            HevcParams {
                bit_depth: 10,
                log2_ctb: 5,
                log2_min_cb: 4,
                coded_align_log2: 4,
                entropy_coding_sync: false,
                cu_qp_delta_depth: None,
                init_qp: 31,
                transform_skip: true,
                deblocking_override_enabled: true,
                slice_chroma_qp_offsets_present: true,
                ..base.clone()
            },
            HevcParams {
                width: 1920,
                height: 1080,
                // 16, 32 and 64 all code 1080 rows as 1088; the parser reports the smallest.
                coded_align_log2: 4,
                init_qp: 20,
                sao: false,
                cb_qp_offset: -2,
                cr_qp_offset: 1,
                ..base
            },
        ];
        for p in variants {
            assert!(p.is_consistent());
            assert_eq!(parse(&p.sps(), &p.pps()).as_ref(), Some(&p));
        }
    }

    #[test]
    fn hardware_style_1088_coding_is_cropped_back() {
        let p = HevcParams {
            width: 1920,
            height: 1080,
            coded_align_log2: 4,
            ..HevcParams::x265_default(1920, 1080)
        };
        assert_eq!((p.coded_width(), p.coded_height()), (1920, 1088));
        assert_eq!(p.height_in_ctbs(), 17);
    }
}
