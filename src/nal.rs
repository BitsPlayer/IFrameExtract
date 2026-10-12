//! AVCC length-prefixed NAL chain walking.
//!
//! Without a `moov` there is no sample table, but the AVCC layout is a self-describing chain:
//! each sample is `[length][NAL bytes]`. Walking it from the start of `mdat` reconstructs every
//! frame boundary exactly, and the chain stops validating precisely at the truncation point.

use crate::codec::Codec;
use crate::container::MdatRange;
use crate::report::{Reporter, Silent};

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
}

/// A coded frame cannot plausibly be many times its own uncompressed size. Garbage length
/// prefixes read out of padding are typically hundreds of megabytes, so this upper bound is what
/// stops a fabricated unit from being mistaken for the frame that straddles the cut.
fn max_nal_len(mb_count: u32) -> usize {
    // 3 KB per macroblock is eight times the raw 4:2:0 size of one, so real frames never approach
    // it while garbage almost always exceeds it.
    mb_count as usize * 3072
}

fn len_ok(codec: Codec, nal_type: u8, len: usize, mb_count: u32) -> bool {
    len <= max_nal_len(mb_count) && len >= codec.min_len(nal_type)
}

/// Header-level check for a unit whose tail is missing: the slice header still has to make sense.
fn partial_unit_ok(codec: Codec, data: &[u8], payload: u64, available: usize, mb_count: u32) -> bool {
    let unit = &data[payload as usize..payload as usize + available];
    if available < codec.header_len() || !codec.header_ok(unit) {
        return false;
    }
    !codec.is_vcl(codec.nal_type(unit)) || codec.slice_header_ok(unit, mb_count)
}

/// Full validation of one unit whose bytes are all present.
fn unit_ok(codec: Codec, data: &[u8], payload: u64, len: usize, mb_count: u32) -> bool {
    let unit = &data[payload as usize..payload as usize + len];
    if len < codec.header_len() || !codec.header_ok(unit) {
        return false;
    }
    let nal_type = codec.nal_type(unit);
    len_ok(codec, nal_type, len, mb_count)
        && (!codec.is_vcl(nal_type) || codec.slice_header_ok(unit, mb_count))
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
    codec: Codec,
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
        if !unit_ok(codec, data, payload, len as usize, mb_count) {
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
fn starts_picture(codec: Codec, data: &[u8], payload: u64, len: u64) -> bool {
    let unit = &data[payload as usize..(payload + len) as usize];
    !codec.is_vcl(codec.nal_type(unit)) || codec.starts_picture(unit) == Some(true)
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
    codec: Codec,
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
            && unit_ok(codec, data, payload, len as usize, mb_count)
            && starts_picture(codec, data, payload, len)
        {
            let (units, _) =
                run_chain(codec, data, end, pos, length_size, mb_count, RESYNC_LOOKAHEAD);
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
    codec: Codec,
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
            Some((payload, len)) if unit_ok(codec, data, payload, len as usize, mb_count) => {
                if codec.is_vcl(codec.nal_type(&data[payload as usize..])) {
                    slices += 1;
                }
                total += 1;
                cur = payload + len;
            }
            _ => match resync(codec, data, end, cur + 1, length_size, mb_count, PROBE_GAP) {
                Some(next) => cur = next,
                None => break,
            },
        }
    }
    (slices, total)
}

/// Lower bound of the 95% Wilson score interval for `k` successes in `n` trials.
fn wilson_lower_bound(k: usize, n: usize) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let (k, n, z) = (k as f64, n as f64, 1.96f64);
    let p = k / n;
    let centre = p + z * z / (2.0 * n);
    let margin = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt();
    (centre - margin) / (1.0 + z * z / n)
}

/// Decides which NAL grammar the `mdat` payload is written in.
///
/// Nothing outside `moov` names the codec, so the bitstream has to. Each grammar is probed the same
/// way the walker would start - straight from the top of `mdat`, or from the first point the chain
/// picks up when another track's chunk comes first - and the one most confidently made of coded
/// slices wins.
///
/// A real chain is almost all slices over dozens of units. Read with the wrong grammar or width,
/// the same bytes give either a low share - H.264's one-byte header lets audio pass as a mix of
/// tiny "slices" and parameter sets - or a high share over only a unit or two. Neither raw counts
/// nor byte coverage separate those from a truncated file's few real pictures, so the score is
/// the lower confidence bound of the slice share, which rewards both purity and sample size.
pub fn detect_codec(data: &[u8], mdat: &MdatRange, mb_count: u32) -> Codec {
    let start = mdat.payload_start;
    let end = mdat.payload_end;
    let score = |codec: Codec| {
        [4usize, 2, 1, 3]
            .into_iter()
            .filter_map(|ls| {
                let at = if run_chain(codec, data, end, start, ls, mb_count, PLAIN_UNITS).0
                    >= PLAIN_UNITS
                {
                    start
                } else {
                    resync(codec, data, end, start, ls, mb_count, GAP_CEILING)?
                };
                let (slices, units) = slice_share(codec, data, end, at, ls, mb_count);
                Some(wilson_lower_bound(slices, units))
            })
            .fold(0.0, f64::max)
    };
    if score(Codec::H265) > score(Codec::H264) {
        Codec::H265
    } else {
        Codec::H264
    }
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

#[allow(clippy::too_many_arguments)]
fn skip_non_video(
    codec: Codec,
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
    if let Some(next) = resync(
        codec,
        data,
        mdat.payload_end,
        at + 1,
        length_size,
        mb_count,
        limit,
    ) {
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
    codec: Codec,
    data: &[u8],
    mdat: &MdatRange,
    mb_count: u32,
    with_audio: bool,
    reporter: &mut dyn Reporter,
) -> Walk {
    let plain = walk_once(codec, data, mdat, mb_count, reporter, false);
    if !with_audio || plain.truncation_offset.is_none() {
        return plain;
    }
    let skipped = walk_once(codec, data, mdat, mb_count, &mut Silent, true);
    if skipped.nals.len() >= plain.nals.len().saturating_mul(2).max(1) {
        skipped
    } else {
        plain
    }
}

fn walk_once(
    codec: Codec,
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
        .map(|ls| (run_chain(codec, data, end, start, ls, mb_count, 64).0, ls == 4, ls))
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
                let at = resync(codec, data, end, start, ls, mb_count, GAP_CEILING)?;
                let (slices, total) = slice_share(codec, data, end, at, ls, mb_count);
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
    let mut skipped_regions = 0usize;
    let mut skipped_bytes = 0u64;
    let mut widest_gap = 0u64;
    let mut last_report = start;

    if first > start {
        // Counted, but deliberately not treated as evidence of the interleave scale: the chain is
        // joined part-way through this chunk, so its length says nothing about a whole one.
        skipped_regions += 1;
        skipped_bytes += first - start;
    }

    loop {
        if cur + length_size as u64 > end {
            if cur != end {
                // A complete mdat cannot have been cut here, so a stray tail is another track's.
                if allow_resync && mdat.complete {
                    skipped_regions += 1;
                    skipped_bytes += end - cur;
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
                codec,
                data,
                mdat,
                cur,
                length_size,
                mb_count,
                allow_resync,
                widest_gap,
            ) {
                Skip::Resumed(next) => {
                    skipped_regions += 1;
                    skipped_bytes += next - cur;
                    widest_gap = widest_gap.max(next - cur);
                    cur = next;
                    continue;
                }
                Skip::TrailingChunk => {
                    skipped_regions += 1;
                    skipped_bytes += end - cur;
                    break;
                }
                Skip::Truncated => {
                    truncation_offset = Some(cur);
                    break;
                }
            }
        }

        let available = (end - payload).min(len) as usize;
        let straddles = payload + len > end;

        // A straddling unit cannot be fully validated - only its header is present - so it is
        // checked as far as its surviving bytes allow.
        let valid = if straddles {
            len <= max_nal_len(mb_count) as u64
                && partial_unit_ok(codec, data, payload, available, mb_count)
        } else {
            unit_ok(codec, data, payload, len as usize, mb_count)
        };
        if !valid {
            match skip_non_video(
                codec,
                data,
                mdat,
                cur,
                length_size,
                mb_count,
                allow_resync,
                widest_gap,
            ) {
                Skip::Resumed(next) => {
                    skipped_regions += 1;
                    skipped_bytes += next - cur;
                    widest_gap = widest_gap.max(next - cur);
                    cur = next;
                    continue;
                }
                Skip::TrailingChunk => {
                    skipped_regions += 1;
                    skipped_bytes += end - cur;
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
            nal_type: codec.nal_type(&data[payload as usize..]),
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
        skipped_regions,
        skipped_bytes,
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
        walk(Codec::H264, data, &mdat, MB_COUNT, true, &mut Silent)
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

    /// `[len][NAL]` for an H.265 picture: first_slice_segment_in_pic_flag set, pps id 0.
    fn hevc_unit(nal_type: u8, payload_len: usize) -> Vec<u8> {
        let mut v = (payload_len as u32).to_be_bytes().to_vec();
        v.push(nal_type << 1);
        v.push(1); // layer 0, temporal id 0
        // first_slice_segment_in_pic_flag, no_output_of_prior_pics_flag, ue(0) pps id
        v.push(0b1010_0000);
        v.resize(4 + payload_len, 0x10);
        v
    }

    #[test]
    fn hevc_chain_is_detected_and_walked_through_another_track() {
        let mut data = Vec::new();
        data.extend(hevc_unit(32, 24)); // VPS
        data.extend(hevc_unit(33, 40)); // SPS
        data.extend(hevc_unit(34, 8)); // PPS
        // Detection is statistical, so give it a stream of realistic length rather than a handful
        // of units.
        for i in 0..11 {
            data.extend(hevc_unit(if i == 0 { 20 } else { 1 }, 64));
            data.extend(other_track(500));
        }
        data.extend(hevc_unit(1, 64));
        let mdat = MdatRange {
            payload_start: 0,
            payload_end: data.len() as u64,
            complete: true,
        };

        assert_eq!(detect_codec(&data, &mdat, MB_COUNT), Codec::H265);
        let walk = walk(Codec::H265, &data, &mdat, MB_COUNT, true, &mut Silent);
        assert_eq!(walk.nals.len(), 15);
        assert_eq!(walk.skipped_regions, 11);
        assert!(walk.truncation_offset.is_none());
        assert_eq!(walk.nals[3].nal_type, 20);
    }

    #[test]
    fn h264_chain_is_not_mistaken_for_hevc() {
        let mut data = Vec::new();
        for _ in 0..6 {
            data.extend(idr(64));
        }
        let mdat = MdatRange {
            payload_start: 0,
            payload_end: data.len() as u64,
            complete: true,
        };
        assert_eq!(detect_codec(&data, &mdat, MB_COUNT), Codec::H264);
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
