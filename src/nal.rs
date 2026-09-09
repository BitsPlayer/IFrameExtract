//! AVCC length-prefixed NAL chain walking.
//!
//! Without a `moov` there is no sample table, but the AVCC layout is a self-describing chain:
//! each sample is `[length][NAL bytes]`. Walking it from the start of `mdat` reconstructs every
//! frame boundary exactly, and the chain stops validating precisely at the truncation point.
//!
//! When `with_audio` is set, the walker additionally handles interleaved audio chunks: on a chain
//! break it scans forward up to `MAX_GAP` bytes looking for a run of `MIN_RUN` consecutive valid
//! NAL units. Finding one means the gap was audio data; not finding one means the file is
//! genuinely truncated at that point.

use crate::container::MdatRange;
use crate::report::Reporter;

/// NAL unit types x264 actually emits. Anything else is treated as a broken chain, which is what
/// makes `startcodes` and `random` padding fail to produce plausible successors.
const ALLOWED_TYPES: [u8; 7] = [1, 5, 6, 7, 8, 9, 12];

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// Maximum bytes to scan past a chain break when looking for the next video chunk.
/// Audio chunks are typically 8–16 KB; 128 KB is generous enough to cover any reasonable
/// interleaving interval while staying well clear of large padding blocks.
const MAX_GAP: u64 = 128 * 1024;

/// Consecutive valid NAL units required before trusting a resync point.
/// A single VCL NAL unit validated with slice_header_ok already verifies forbidden_zero_bit,
/// allowed NAL types, ref_idc constraints, length bounds, and Exp-Golomb slice header fields
/// (first_mb_in_slice < mb_count, slice_type <= 9). This multi-layer check has an accidental
/// pass probability < 0.01% on AAC data, while allowing resync on single-frame video chunks.
const MIN_RUN: usize = 1;

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
    /// Offset at which the chain stopped validating due to real truncation.
    /// `None` means the chain ran cleanly to the end of `mdat` (or all breaks were audio gaps).
    pub truncation_offset: Option<u64>,
    /// Byte ranges that were skipped as interleaved audio. Each entry is `(gap_start, gap_end)`
    /// where `gap_end` is the offset of the first NAL unit in the next video chunk.
    /// Empty when `with_audio` is false or the file contains no audio.
    pub audio_gaps: Vec<(u64, u64)>,
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

/// Walks the AVCC chain from `start` and returns how many consecutive units validated.
/// Used both for prefix-width probing and for the resync minimum-run check.
fn probe_from(data: &[u8], start: u64, end: u64, length_size: usize, mb_count: u32) -> usize {
    let mut cur = start;
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

/// Probes from the start of `mdat` — used to pick the right length-prefix width.
fn probe(data: &[u8], mdat: &MdatRange, length_size: usize, mb_count: u32) -> usize {
    probe_from(data, mdat.payload_start, mdat.payload_end, length_size, mb_count)
}

/// Scans forward from `gap_start` to find the next valid AVCC chain restart.
///
/// Returns `Some(offset)` when `MIN_RUN` consecutive valid NAL units are found beginning at
/// `offset`, meaning the gap between `gap_start` and `offset` is an audio (or other non-video)
/// chunk. Returns `None` if no valid chain restart is found within `MAX_GAP` bytes, meaning the
/// break is real truncation.
fn resync(data: &[u8], gap_start: u64, end: u64, length_size: usize, mb_count: u32) -> Option<u64> {
    let scan_end = (gap_start + MAX_GAP).min(end);

    let mut p = gap_start;
    while p + length_size as u64 <= scan_end {
        if probe_from(data, p, end, length_size, mb_count) >= MIN_RUN {
            return Some(p);
        }
        p += 1;
    }
    None
}

pub fn walk(
    data: &[u8],
    mdat: &MdatRange,
    mb_count: u32,
    with_audio: bool,
    reporter: &mut dyn Reporter,
) -> Walk {
    // 4 is universal for MP4, but probe so the tool does not silently misparse an odd file.
    let length_size = [4usize, 2, 1, 3]
        .into_iter()
        .max_by_key(|&ls| probe(data, mdat, ls, mb_count))
        .unwrap();

    let mut nals = Vec::new();
    let mut cur = mdat.payload_start;
    let start = mdat.payload_start;
    let end = mdat.payload_end;
    let span = end.saturating_sub(start).max(1);
    let mut truncation_offset = None;
    let mut audio_gaps: Vec<(u64, u64)> = Vec::new();
    let mut last_report = start;

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
            if with_audio {
                if let Some(restart) = resync(data, cur, end, length_size, mb_count) {
                    audio_gaps.push((cur, restart));
                    cur = restart;
                    continue;
                }
            }
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
            if with_audio {
                if let Some(restart) = resync(data, cur, end, length_size, mb_count) {
                    audio_gaps.push((cur, restart));
                    cur = restart;
                    continue;
                }
            }
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
        if cur - last_report >= 512 * 1024 {
            reporter.progress(
                cur.saturating_sub(start),
                span,
                &format!("{} NALs, {}", nals.len(), crate::report::human_bytes(cur.saturating_sub(start))),
            );
            last_report = cur;
        }
    }

    // Confirm-by-successor: if the walk stopped on real truncation (not an audio gap), the last
    // accepted unit was sized by a length prefix we can no longer fully trust — with enough
    // padding appended, a unit that straddles the real cut still "fits" in the file and swallows
    // garbage. Demote it to incomplete so it is only ever emitted as a partial frame.
    // NALs immediately before audio gaps are NOT demoted: their length prefix was valid and all
    // their bytes were present — the break was in the subsequent (audio) data, not in them.
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
        audio_gaps,
    }
}
