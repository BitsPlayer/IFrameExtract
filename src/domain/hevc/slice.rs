//! HEVC slice segment headers.
//!
//! Two depths of parsing. `peek` reads only what precedes any parameter-set dependency and so
//! works before anything is recovered; `parse` reads the whole header against a candidate
//! parameter set, which is how a wrong candidate is first caught.

use super::params::{HevcParams, SliceTypeFields};
use crate::bits::{BitReader, unescape_rbsp};

pub const SLICE_B: u32 = 0;
pub const SLICE_P: u32 = 1;
pub const SLICE_I: u32 = 2;

/// Fields readable with no parameter set at all.
#[derive(Debug, Clone, Copy)]
pub struct SlicePeek {
    pub first_slice_segment_in_pic: bool,
    pub pps_id: u32,
}

pub fn peek(payload: &[u8]) -> Option<SlicePeek> {
    let nal_type = super::nal_type(payload);
    let prefix_len = payload.len().min(16);
    let rbsp = unescape_rbsp(payload.get(2..prefix_len)?);
    let mut r = BitReader::new(&rbsp);
    let first_slice_segment_in_pic = r.u(1)? == 1;
    if super::is_irap(nal_type) {
        r.u(1)?; // no_output_of_prior_pics_flag
    }
    let pps_id = r.ue()?;
    Some(SlicePeek {
        first_slice_segment_in_pic,
        pps_id,
    })
}

/// `slice_type` of the first segment of a picture. Later segments carry an address whose width
/// depends on the SPS, and dependent segments carry no type at all.
pub fn first_segment_type(payload: &[u8], f: SliceTypeFields) -> Option<u32> {
    let nal_type = super::nal_type(payload);
    let prefix_len = payload.len().min(32);
    let rbsp = unescape_rbsp(payload.get(2..prefix_len)?);
    let mut r = BitReader::new(&rbsp);
    if r.u(1)? != 1 {
        return None;
    }
    if super::is_irap(nal_type) {
        r.u(1)?;
    }
    r.ue()?;
    r.u(f.num_extra_slice_header_bits)?;
    let t = r.ue()?;
    (t <= 2).then_some(t)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub address: u32,
    pub dependent: bool,
    pub slice_type: u32,
    pub slice_qp: i32,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    /// Entry point offsets in bytes of the escaped NAL payload, each one the size of a substream.
    pub entry_points: Vec<u32>,
    /// Bit position in the unescaped payload (NAL header included) where slice data begins.
    pub data_bit: usize,
}

/// Parses a whole slice segment header against `p`. `rbsp` is the unescaped payload including
/// the two-byte NAL header. Returns `None` on anything the parameter set makes impossible.
pub fn parse(p: &HevcParams, nal_type: u8, rbsp: &[u8]) -> Option<SliceHeader> {
    let mut r = BitReader::new(rbsp);
    r.u(16)?;
    let first = r.u(1)? == 1;
    if super::is_irap(nal_type) {
        r.u(1)?;
    }
    if r.ue()? != 0 {
        return None; // we only ever synthesise pps 0
    }
    let pic_size_in_ctbs = p.width_in_ctbs() * p.height_in_ctbs();
    let mut dependent = false;
    let mut address = 0;
    if !first {
        if p.dependent_slice_segments {
            dependent = r.u(1)? == 1;
        }
        let bits = u32::BITS - (pic_size_in_ctbs - 1).leading_zeros();
        address = r.u(bits)?;
        if address == 0 || address >= pic_size_in_ctbs {
            return None;
        }
    }

    let mut slice_type = SLICE_I;
    let mut slice_qp = 0;
    let mut sao_luma = false;
    let mut sao_chroma = false;
    if !dependent {
        r.u(p.num_extra_slice_header_bits)?;
        slice_type = r.ue()?;
        if slice_type != SLICE_I {
            // Only intra slices are ever validated.
            return None;
        }
        if p.output_flag_present {
            r.u(1)?;
        }
        if !super::is_idr(nal_type) {
            r.u(p.log2_max_poc_lsb)?;
            let short_term_ref_pic_set_sps_flag = r.u(1)? == 1;
            if short_term_ref_pic_set_sps_flag {
                return None; // num_short_term_ref_pic_sets is always 0 in what we synthesise
            }
            // st_ref_pic_set(0): no inter-RPS prediction for index 0.
            let negative = r.ue()?;
            let positive = r.ue()?;
            if negative + positive > 16 {
                return None;
            }
            for _ in 0..negative + positive {
                if r.ue()? > 1 << 15 {
                    return None;
                }
                r.u(1)?;
            }
            if p.temporal_mvp {
                r.u(1)?;
            }
        }
        if p.sao {
            sao_luma = r.u(1)? == 1;
            sao_chroma = r.u(1)? == 1;
        }
        let qp_delta = r.se()?;
        slice_qp = p.init_qp + qp_delta;
        if !(-p.qp_bd_offset()..=51).contains(&slice_qp) {
            return None;
        }
        if p.slice_chroma_qp_offsets_present {
            let cb = r.se()?;
            let cr = r.se()?;
            if !(-12..=12).contains(&cb) || !(-12..=12).contains(&cr) {
                return None;
            }
        }
        let mut deblocking_disabled = p.deblocking_disabled;
        if p.deblocking_override_enabled && r.u(1)? == 1 {
            deblocking_disabled = r.u(1)? == 1;
            if !deblocking_disabled {
                let beta = r.se()?;
                let tc = r.se()?;
                if !(-6..=6).contains(&beta) || !(-6..=6).contains(&tc) {
                    return None;
                }
            }
        }
        if p.loop_filter_across_slices && (sao_luma || sao_chroma || !deblocking_disabled) {
            r.u(1)?;
        }
    }

    let mut entry_points = Vec::new();
    if p.entropy_coding_sync {
        let n = r.ue()?;
        // One substream per CTB row the segment touches, at most.
        let first_row = address / p.width_in_ctbs();
        if n >= p.height_in_ctbs() - first_row {
            return None;
        }
        if n > 0 {
            let len = r.ue()? + 1;
            if len > 32 {
                return None;
            }
            for _ in 0..n {
                entry_points.push(r.u(len)?.checked_add(1)?);
            }
        }
    }

    // byte_alignment(): a one, then zeros to the boundary.
    if r.u(1)? != 1 {
        return None;
    }
    while !r.pos().is_multiple_of(8) {
        if r.u(1)? != 0 {
            return None;
        }
    }

    Some(SliceHeader {
        address,
        dependent,
        slice_type,
        slice_qp,
        sao_luma,
        sao_chroma,
        entry_points,
        data_bit: r.pos(),
    })
}
