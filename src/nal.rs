//! AVCC length-prefixed NAL chain walking.
//!
//! Without a `moov` there is no sample table, but the AVCC layout is a self-describing chain:
//! each sample is `[length][NAL bytes]`. Walking it from the start of `mdat` reconstructs every
//! frame boundary exactly, and the chain stops validating precisely at the truncation point.

use crate::container::MdatRange;

/// NAL unit types x264 actually emits. Anything else is treated as a broken chain, which is what
/// makes `startcodes` and `random` padding fail to produce plausible successors.
const ALLOWED_TYPES: [u8; 7] = [1, 5, 6, 7, 8, 9, 12];

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

#[derive(Debug, Clone)]
pub struct Nal {
    /// Offset of the length prefix. This is the value the corpus packet index records as `pos`.
    pub offset: u64,
    pub payload_offset: u64,
    /// Length declared by the prefix.
    pub declared_len: usize,
    /// Bytes actually present. Less than `declared_len` only for a straddling unit.
    pub available_len: usize,
    pub nal_type: u8,
    /// False when the unit straddles the cut, or when the chain failed immediately after it and
    /// its payload may therefore have swallowed padding.
    pub complete: bool,
}

impl Nal {
    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        let start = self.payload_offset as usize;
        &data[start..start + self.available_len]
    }
}

#[derive(Debug)]
pub struct Walk {
    pub nals: Vec<Nal>,
    pub nal_length_size: usize,
    /// Offset at which the chain stopped validating. `None` means it ran cleanly to the end of
    /// `mdat`, i.e. the file is intact.
    pub truncation_offset: Option<u64>,
}

fn header_ok(byte: u8) -> bool {
    if byte & 0x80 != 0 {
        // forbidden_zero_bit
        return false;
    }
    let nal_type = byte & 0x1f;
    if !ALLOWED_TYPES.contains(&nal_type) {
        return false;
    }
    // The spec fixes nal_ref_idc for these types, so it is free validation against garbage that
    // happens to carry a plausible type.
    let ref_idc = (byte >> 5) & 0x03;
    match nal_type {
        NAL_IDR | NAL_SPS | NAL_PPS => ref_idc != 0,
        NAL_SEI | 9 | 12 => ref_idc == 0,
        _ => true,
    }
}

/// Slice NALs get a second opinion from their header: a fabricated unit rarely yields a
/// macroblock address inside the picture and a valid slice_type.
fn slice_header_ok(payload: &[u8], mb_count: u32) -> bool {
    match crate::slice::parse(payload) {
        Some(h) => h.first_mb_in_slice < mb_count && h.slice_type <= 9,
        None => false,
    }
}

/// A coded frame cannot plausibly be many times its own uncompressed size. Garbage length
/// prefixes read out of padding are typically hundreds of megabytes, so this upper bound is what
/// stops a fabricated unit from being mistaken for the frame that straddles the cut.
fn max_nal_len(mb_count: u32) -> usize {
    // 3 KB per macroblock is eight times the raw 4:2:0 size of one, so real frames never approach
    // it while garbage almost always exceeds it.
    mb_count as usize * 3072
}

fn len_ok(nal_type: u8, len: usize, mb_count: u32) -> bool {
    if len > max_nal_len(mb_count) {
        return false;
    }
    match nal_type {
        // A slice NAL is never a couple of bytes; this rejects the 1-byte units that
        // `startcodes` padding synthesises.
        NAL_SLICE | NAL_IDR => len >= 8,
        _ => len >= 2,
    }
}

/// Header-level check for a unit whose tail is missing: the slice header still has to make sense.
fn slice_or_other_ok(data: &[u8], payload: u64, available: usize, mb_count: u32) -> bool {
    let nal_type = data[payload as usize] & 0x1f;
    if nal_type != NAL_SLICE && nal_type != NAL_IDR {
        return true;
    }
    let slice = &data[payload as usize..payload as usize + available];
    slice_header_ok(slice, mb_count)
}

/// Full validation of one unit whose bytes are all present.
fn unit_ok(data: &[u8], payload: u64, len: usize, mb_count: u32) -> bool {
    let b = data[payload as usize];
    if !header_ok(b) || !len_ok(b & 0x1f, len, mb_count) {
        return false;
    }
    let nal_type = b & 0x1f;
    if nal_type == NAL_SLICE || nal_type == NAL_IDR {
        let slice = &data[payload as usize..payload as usize + len];
        if !slice_header_ok(slice, mb_count) {
            return false;
        }
    }
    true
}

/// Walks the chain with a given prefix width and returns how many units validated.
fn probe(data: &[u8], mdat: &MdatRange, length_size: usize, mb_count: u32) -> usize {
    let mut cur = mdat.payload_start;
    let end = mdat.payload_end;
    let mut count = 0usize;

    while count < 64 {
        if cur + length_size as u64 > end {
            break;
        }
        let mut len: u64 = 0;
        for i in 0..length_size {
            len = (len << 8) | data[cur as usize + i] as u64;
        }
        let payload = cur + length_size as u64;
        if len == 0 || payload >= end || payload + len > end {
            break;
        }
        if !unit_ok(data, payload, len as usize, mb_count) {
            break;
        }
        count += 1;
        cur = payload + len;
    }
    count
}

pub fn walk(data: &[u8], mdat: &MdatRange, mb_count: u32) -> Walk {
    // 4 is universal for MP4, but probe so the tool does not silently misparse an odd file.
    let length_size = [4usize, 2, 1, 3]
        .into_iter()
        .max_by_key(|&ls| probe(data, mdat, ls, mb_count))
        .unwrap();

    let mut nals = Vec::new();
    let mut cur = mdat.payload_start;
    let end = mdat.payload_end;
    let mut truncation_offset = None;

    loop {
        if cur + length_size as u64 > end {
            if cur != end {
                truncation_offset = Some(cur);
            }
            break;
        }
        let mut len: u64 = 0;
        for i in 0..length_size {
            len = (len << 8) | data[cur as usize + i] as u64;
        }
        let payload = cur + length_size as u64;

        if len == 0 || payload >= end {
            truncation_offset = Some(cur);
            break;
        }

        let available = (end - payload).min(len) as usize;
        let b = data[payload as usize];
        let straddles = payload + len > end;

        // A straddling unit cannot be fully validated - only its header is present - so it is
        // checked as far as its surviving bytes allow.
        let valid = if straddles {
            header_ok(b)
                && len <= max_nal_len(mb_count) as u64
                && slice_or_other_ok(data, payload, available, mb_count)
        } else {
            unit_ok(data, payload, len as usize, mb_count)
        };
        if !valid {
            truncation_offset = Some(cur);
            break;
        }
        nals.push(Nal {
            offset: cur,
            payload_offset: payload,
            declared_len: len as usize,
            available_len: available,
            nal_type: b & 0x1f,
            complete: !straddles,
        });

        if straddles {
            truncation_offset = Some(cur);
            break;
        }
        cur = payload + len;
    }

    // Confirm-by-successor: if the walk stopped on an invalid successor rather than at the end of
    // mdat, the last accepted unit was sized by a length prefix we can no longer trust — with
    // enough padding appended, a unit that straddles the real cut still "fits" in the file and
    // swallows garbage. Demote it to incomplete so it is only ever emitted as a partial frame.
    if truncation_offset.is_some()
        && let Some(last) = nals.last_mut()
        && last.complete
    {
        last.complete = false;
    }

    Walk {
        nals,
        nal_length_size: length_size,
        truncation_offset,
    }
}
