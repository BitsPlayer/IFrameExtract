//! AVCC length-prefixed NAL chain walking.
//!
//! Without a `moov` there is no sample table, but the AVCC layout is a self-describing chain:
//! each sample is `[length][NAL bytes]`. Walking it from the start of `mdat` reconstructs every
//! frame boundary exactly, and the chain stops validating precisely at the truncation point.

use crate::container::MdatRange;
use crate::report::{Reporter, Silent};

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
    /// Offset at which the chain stopped validating and could not be picked up again. `None`
    /// means it ran cleanly to the end of `mdat`, i.e. the file is intact.
    pub truncation_offset: Option<u64>,
    /// Runs of bytes inside `mdat` that are not part of the video chain - in practice the
    /// interleaved chunks of another track, usually audio.
    pub skipped_regions: usize,
    pub skipped_bytes: u64,
    /// The stepped-over regions as `(offset, len)`; `skipped_regions` and `skipped_bytes` are
    /// their count and total.
    pub skipped: Vec<(u64, u64)>,
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

/// How far to scan for the chain to resume before concluding the file is truncated, until the
/// file has shown what its own interleave looks like.
///
/// Chunk size is set by the other track's bitrate over the interleave interval, which nothing in a
/// truncated file records - notably not the frame size, since two files at one resolution differ
/// by two orders of magnitude here while one resolution step leaves chunks unchanged.
const GAP_CEILING: u64 = 256 * 1024;

/// Headroom over the widest gap crossed so far. Interleave is regular, so once a file has shown
/// its chunk scale, a gap many times wider is padding rather than another chunk.
const GAP_HEADROOM: u64 = 8;

const GAP_FLOOR: u64 = 64 * 1024;

/// How far the next gap may run, given the widest crossed so far. Before any has been crossed
/// there is nothing to go on and the ceiling stands.
fn gap_limit(widest: u64) -> u64 {
    match widest {
        0 => GAP_CEILING,
        w => w.saturating_mul(GAP_HEADROOM).clamp(GAP_FLOOR, GAP_CEILING),
    }
}

/// A coded picture never exceeds its own uncompressed size, so a resync candidate declaring more
/// than a raw 4:2:0 frame is a length prefix read out of non-video bytes. The chain itself keeps
/// the looser `max_nal_len`; this tighter ceiling only governs where it may resume, where there is
/// no predecessor to corroborate the unit and a single bad choice derails the rest of the walk.
fn max_resync_len(mb_count: u32) -> u64 {
    // 384 bytes is one macroblock raw: a 16x16 luma block plus its two 8x8 chroma blocks.
    mb_count as u64 * 384
}

/// Reads the length prefix at `pos`, returning the payload offset and length when a whole unit
/// fits inside `end`.
fn unit_at(data: &[u8], end: u64, pos: u64, length_size: usize) -> Option<(u64, u64)> {
    if pos + length_size as u64 > end {
        return None;
    }
    let mut len: u64 = 0;
    for i in 0..length_size {
        len = (len << 8) | data[pos as usize + i] as u64;
    }
    let payload = pos + length_size as u64;
    if len == 0 || payload >= end || payload + len > end {
        return None;
    }
    Some((payload, len))
}

/// Follows the chain from `pos` for at most `want` units, returning how many validated and the
/// offset they end at. Only fully-present units count, so it stops at the first gap.
fn run_chain(
    data: &[u8],
    end: u64,
    pos: u64,
    length_size: usize,
    mb_count: u32,
    want: usize,
) -> (usize, u64) {
    let mut cur = pos;
    let mut count = 0usize;

    while count < want {
        let Some((payload, len)) = unit_at(data, end, cur, length_size) else {
            break;
        };
        if !unit_ok(data, payload, len as usize, mb_count) {
            break;
        }
        count += 1;
        cur = payload + len;
    }
    (count, cur)
}

/// Scans forward for the next offset where the chain credibly resumes.
///
/// A non-video chunk cannot be measured directly: raw AAC in MP4 carries neither a syncword nor a
/// length, and the sample table that does record its size lives in the `moov` this tool refuses to
/// read. Its end is therefore found the only way left - by locating where video starts again.
/// True when a unit could be the first NAL of a chunk.
///
/// A chunk holds whole samples and an AVC sample is a whole picture, so the chain always resumes
/// on a picture boundary. A slice that claims to resume mid-picture is therefore a length prefix
/// read out of the other track's bytes, however well its header validates.
fn starts_picture(data: &[u8], payload: u64, len: u64) -> bool {
    let nal_type = data[payload as usize] & 0x1f;
    if nal_type != NAL_SLICE && nal_type != NAL_IDR {
        return true;
    }
    let slice = &data[payload as usize..(payload + len) as usize];
    crate::slice::parse(slice).is_some_and(|h| h.first_mb_in_slice == 0)
}

/// Consecutive units that make a resync candidate decisive on the spot.
const RESYNC_LOOKAHEAD: usize = 4;

/// Finds where the chain resumes after a chunk of another track.
///
/// The earliest candidate wins, because the next chunk begins as soon as this one ends - unless a
/// later one is decisive, running `RESYNC_LOOKAHEAD` units without a break. That override is what
/// keeps a large frame size from resyncing onto a fabricated header: at 4K the length ceiling
/// admits almost any prefix, so the leading chunk of another track offers plenty of candidates
/// before the real one. It is deliberately not a preference for the longest run: where a chunk
/// holds a single NAL, a fabricated unit whose length happens to span the gap chains one further
/// than the genuine chunk start does.
fn resync(
    data: &[u8],
    end: u64,
    from: u64,
    length_size: usize,
    mb_count: u32,
    limit: u64,
) -> Option<u64> {
    let stop = (from.saturating_add(limit)).min(end);
    let ceiling = max_resync_len(mb_count);
    let mut earliest = None;
    let mut pos = from;

    while pos < stop {
        if let Some((payload, len)) = unit_at(data, end, pos, length_size)
            && len <= ceiling
            && unit_ok(data, payload, len as usize, mb_count)
            && starts_picture(data, payload, len)
        {
            let (units, _) = run_chain(data, end, pos, length_size, mb_count, RESYNC_LOOKAHEAD);
            if units >= RESYNC_LOOKAHEAD {
                return Some(pos);
            }
            earliest = earliest.or(Some(pos));
        }
        pos += 1;
    }
    earliest
}

/// How far a width probe may scan for the chain to resume. Smaller than `MAX_GAP` because the
/// probe runs for every candidate width before anything is known about the file.
const PROBE_GAP: u64 = 64 * 1024;

/// Units a width probe inspects before judging.
const PROBE_UNITS: usize = 64;

/// Consecutive units from the top of `mdat` that settle the prefix width outright.
const PLAIN_UNITS: usize = 4;

/// Walks a bounded prefix from `pos`, crossing interleave gaps, and reports how many of the units
/// it found were coded slices.
///
/// A real chain is almost all slices, and those are the only units whose contents get validated at
/// all. A wrong width collects whatever happens to pass the one-byte header test instead, which
/// skews hard towards the parameter-set and delimiter types.
fn slice_share(
    data: &[u8],
    end: u64,
    pos: u64,
    length_size: usize,
    mb_count: u32,
) -> (usize, usize) {
    let mut cur = pos;
    let mut slices = 0usize;
    let mut total = 0usize;

    while total < PROBE_UNITS {
        match unit_at(data, end, cur, length_size) {
            Some((payload, len)) if unit_ok(data, payload, len as usize, mb_count) => {
                let nal_type = data[payload as usize] & 0x1f;
                if nal_type == NAL_SLICE || nal_type == NAL_IDR {
                    slices += 1;
                }
                total += 1;
                cur = payload + len;
            }
            _ => match resync(data, end, cur + 1, length_size, mb_count, PROBE_GAP) {
                Some(next) => cur = next,
                None => break,
            },
        }
    }
    (slices, total)
}

/// What a break in the chain turned out to be.
enum Skip {
    /// The chain resumes here; the bytes in between belong to another track.
    Resumed(u64),
    /// The rest of `mdat` is another track's data and `mdat` itself is intact.
    TrailingChunk,
    /// The chain cannot be picked up again: this is the truncation point.
    Truncated,
}

fn skip_non_video(
    data: &[u8],
    mdat: &MdatRange,
    at: u64,
    length_size: usize,
    mb_count: u32,
    allow_resync: bool,
    widest_gap: u64,
) -> Skip {
    if !allow_resync {
        return Skip::Truncated;
    }
    let limit = gap_limit(widest_gap);
    if let Some(next) = resync(data, mdat.payload_end, at + 1, length_size, mb_count, limit) {
        return Skip::Resumed(next);
    }
    // Nothing resumed. When `mdat` is intact and the unparsed run is short enough to be one more
    // interleaved chunk, the file simply ends on another track rather than mid-picture; a run
    // longer than a plausible chunk is padding after a real cut.
    if mdat.complete && mdat.payload_end.saturating_sub(at) <= limit {
        Skip::TrailingChunk
    } else {
        Skip::Truncated
    }
}

/// Walks the chain, skipping interleaved non-video chunks only when doing so demonstrably pays.
///
/// Gap skipping is what lets a file with audio be read at all, but it is also what a truncated
/// file's padding invites: fabricated units that drag the chain past the real cut. The two are
/// told apart by result rather than by heuristic - on an interleaved file skipping unlocks the
/// whole video, while in padding it scrapes together a handful of extra units.
pub fn walk(
    data: &[u8],
    mdat: &MdatRange,
    mb_count: u32,
    with_audio: bool,
    reporter: &mut dyn Reporter,
) -> Walk {
    let plain = walk_once(data, mdat, mb_count, reporter, false);
    if !with_audio || plain.truncation_offset.is_none() {
        return plain;
    }
    let skipped = walk_once(data, mdat, mb_count, &mut Silent, true);
    if skipped.nals.len() >= plain.nals.len().saturating_mul(2).max(1) {
        skipped
    } else {
        plain
    }
}

fn walk_once(
    data: &[u8],
    mdat: &MdatRange,
    mb_count: u32,
    reporter: &mut dyn Reporter,
    allow_resync: bool,
) -> Walk {
    let start = mdat.payload_start;
    let end = mdat.payload_end;

    // 4 is universal for MP4, but probe so the tool does not silently misparse an odd file. A
    // width that runs straight off the top of mdat settles it: only the real one chains there.
    let (plain_units, _, plain_width) = [4usize, 2, 1, 3]
        .into_iter()
        .map(|ls| (run_chain(data, end, start, ls, mb_count, 64).0, ls == 4, ls))
        .max_by_key(|&(units, is_four, _)| (units, is_four))
        .unwrap();

    let (length_size, first) = if !allow_resync || plain_units >= PLAIN_UNITS {
        (plain_width, start)
    } else {
        // Nothing chains from the top, so mdat does not open with video - the file interleaves
        // another track, and its first chunk is not ours. Each width is then judged from wherever
        // it first picks up, by how much of what it goes on to find is actually coded slices.
        [4usize, 2, 1, 3]
            .into_iter()
            .filter_map(|ls| {
                let at = resync(data, end, start, ls, mb_count, GAP_CEILING)?;
                let (slices, total) = slice_share(data, end, at, ls, mb_count);
                (total > 0).then(|| (ls, at, slices * 1000 / total))
            })
            .max_by_key(|&(ls, _, share)| (share, ls == 4))
            .map(|(ls, at, _)| (ls, at))
            .unwrap_or((4, start))
    };

    let mut nals = Vec::new();
    let mut cur = first;
    let span = end.saturating_sub(start).max(1);
    let mut truncation_offset = None;
    let mut skipped: Vec<(u64, u64)> = Vec::new();
    let mut widest_gap = 0u64;
    let mut last_report = start;

    if first > start {
        // Counted, but deliberately not treated as evidence of the interleave scale: the chain is
        // joined part-way through this chunk, so its length says nothing about a whole one.
        skipped.push((start, first - start));
    }

    loop {
        if cur + length_size as u64 > end {
            if cur != end {
                // A complete mdat cannot have been cut here, so a stray tail is another track's.
                if allow_resync && mdat.complete {
                    skipped.push((cur, end - cur));
                } else {
                    truncation_offset = Some(cur);
                }
            }
            break;
        }
        let mut len: u64 = 0;
        for i in 0..length_size {
            len = (len << 8) | data[cur as usize + i] as u64;
        }
        let payload = cur + length_size as u64;

        if len == 0 || payload >= end {
            match skip_non_video(
                data,
                mdat,
                cur,
                length_size,
                mb_count,
                allow_resync,
                widest_gap,
            ) {
                Skip::Resumed(next) => {
                    skipped.push((cur, next - cur));
                    widest_gap = widest_gap.max(next - cur);
                    cur = next;
                    continue;
                }
                Skip::TrailingChunk => {
                    skipped.push((cur, end - cur));
                    break;
                }
                Skip::Truncated => {
                    truncation_offset = Some(cur);
                    break;
                }
            }
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
            match skip_non_video(
                data,
                mdat,
                cur,
                length_size,
                mb_count,
                allow_resync,
                widest_gap,
            ) {
                Skip::Resumed(next) => {
                    skipped.push((cur, next - cur));
                    widest_gap = widest_gap.max(next - cur);
                    cur = next;
                    continue;
                }
                Skip::TrailingChunk => {
                    skipped.push((cur, end - cur));
                    break;
                }
                Skip::Truncated => {
                    truncation_offset = Some(cur);
                    break;
                }
            }
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
                &format!(
                    "{} NALs, {}",
                    nals.len(),
                    crate::report::human_bytes(cur.saturating_sub(start))
                ),
            );
            last_report = cur;
        }
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
        skipped_regions: skipped.len(),
        skipped_bytes: skipped.iter().map(|&(_, len)| len).sum(),
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB_COUNT: u32 = 920;

    /// `[len][NAL]` for an IDR whose slice header reads first_mb_in_slice = 0, slice_type = 7.
    fn idr(payload_len: usize) -> Vec<u8> {
        let mut v = (payload_len as u32).to_be_bytes().to_vec();
        v.push(0x65); // nal_ref_idc 3, nal_type 5
        v.push(0x88); // ue(0) then ue(7)
        v.resize(4 + payload_len, 0x10);
        v
    }

    /// Bytes no length prefix can be read out of: the top bit set fails `header_ok`, and read as a
    /// length they overrun `mdat`.
    fn other_track(len: usize) -> Vec<u8> {
        vec![0xFF; len]
    }

    fn walk_bytes(data: &[u8], complete: bool) -> Walk {
        let mdat = MdatRange {
            payload_start: 0,
            payload_end: data.len() as u64,
            complete,
        };
        walk(data, &mdat, MB_COUNT, true, &mut Silent)
    }

    #[test]
    fn steps_over_chunks_of_another_track() {
        let mut data = Vec::new();
        for _ in 0..3 {
            data.extend(idr(64));
            data.extend(other_track(600));
        }
        data.extend(idr(64));

        let walk = walk_bytes(&data, true);
        assert_eq!(walk.nals.len(), 4, "every video chunk should be recovered");
        assert_eq!(walk.skipped_regions, 3);
        assert_eq!(walk.skipped_bytes, 1800);
        assert!(
            walk.truncation_offset.is_none(),
            "an intact file that merely interleaves must not look truncated"
        );
    }

    #[test]
    fn finds_a_chain_that_does_not_start_at_the_top_of_mdat() {
        let mut data = other_track(451);
        data.extend(idr(64));
        data.extend(other_track(600));
        data.extend(idr(64));

        let walk = walk_bytes(&data, true);
        assert_eq!(walk.nals.len(), 2);
        assert_eq!(walk.nals[0].offset, 451, "first chunk is not ours");
        assert!(walk.truncation_offset.is_none());
    }

    #[test]
    fn a_trailing_chunk_is_not_a_truncation() {
        let mut data = Vec::new();
        for _ in 0..3 {
            data.extend(idr(64));
            data.extend(other_track(600));
        }

        let walk = walk_bytes(&data, true);
        assert_eq!(walk.nals.len(), 3);
        assert_eq!(walk.skipped_regions, 3, "the tail counts as stepped over");
        assert!(
            walk.truncation_offset.is_none(),
            "an intact mdat that ends on another track's chunk is not truncated"
        );

        // The same bytes in an mdat that was itself cut short really are a truncation.
        let walk = walk_bytes(&data, false);
        assert_eq!(walk.truncation_offset, Some(1404));
    }

    #[test]
    fn a_decisive_run_outranks_an_earlier_lone_candidate() {
        // A large frame size makes the resync length ceiling permissive enough that the other
        // track's bytes throw up candidates of their own. One that leads nowhere must lose to the
        // chunk that actually chains, however much earlier it appears.
        let mut decoy = (900_000u32).to_be_bytes().to_vec();
        decoy.push(0x68); // nal_ref_idc 3, nal_type 8 (PPS) - no slice header to validate
        decoy.push(0x00);

        let mut data = other_track(400);
        data.extend(&decoy);
        data.extend(other_track(400));
        let chunk_at = data.len() as u64;
        for _ in 0..6 {
            data.extend(idr(64));
        }

        let walk = walk_bytes(&data, true);
        assert_eq!(
            walk.nals.first().map(|n| n.offset),
            Some(chunk_at),
            "the run of real units should win over the earlier decoy"
        );
        assert_eq!(walk.nals.len(), 6);
    }

    #[test]
    fn a_gap_far_wider_than_the_file_s_own_interleave_is_padding() {
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend(idr(64));
            data.extend(other_track(600));
        }
        // Far too wide to be another chunk in a file whose chunks have all been 600 bytes.
        data.extend(other_track(100_000));
        data.extend(idr(64));

        let walk = walk_bytes(&data, true);
        assert_eq!(walk.nals.len(), 4, "only the chunks before the outlier gap");
        assert_eq!(
            walk.truncation_offset,
            Some(2072),
            "a gap out of scale with the established interleave ends the walk"
        );
    }

    #[test]
    fn a_slice_resuming_mid_picture_is_not_a_chunk_start() {
        // first_mb_in_slice = 2: ue(2) is `011`, so this cannot be the first NAL of a sample.
        let mut mid = (64u32).to_be_bytes().to_vec();
        mid.push(0x61);
        mid.push(0b0110_0100);
        mid.resize(68, 0x10);

        let mut data = idr(64);
        data.extend(other_track(200));
        data.extend(&mid);
        data.extend(other_track(200));
        data.extend(idr(64));

        let walk = walk_bytes(&data, true);
        assert!(
            walk.nals.iter().all(|n| n.offset != 272),
            "a slice that resumes mid-picture must not be taken for a chunk start"
        );
    }
}
