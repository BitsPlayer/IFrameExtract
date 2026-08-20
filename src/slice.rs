//! Just enough slice header to group access units and classify slices.
//!
//! `first_mb_in_slice`, `slice_type` and `pic_parameter_set_id` are the first three `ue(v)`
//! fields of the header, so they can be read before any parameter set has been recovered. That
//! is what lets access-unit grouping work on a file whose SPS is still unknown.

use crate::bits::{BitReader, unescape_rbsp};

#[derive(Debug, Clone, Copy)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type: u32,
}

impl SliceHeader {
    pub fn is_intra(&self) -> bool {
        // 2 and 7 are I, 4 and 9 are SI.
        matches!(self.slice_type % 5, 2 | 4)
    }
}

pub fn parse(nal_payload: &[u8]) -> Option<SliceHeader> {
    // Emulation prevention can appear anywhere; the header is tiny, so unescaping a prefix is
    // enough and keeps this cheap on large slices.
    let prefix_len = nal_payload.len().min(32);
    let rbsp = unescape_rbsp(&nal_payload[1..prefix_len]);
    let mut r = BitReader::new(&rbsp);

    Some(SliceHeader {
        first_mb_in_slice: r.ue()?,
        slice_type: r.ue()?,
    })
}
