//! Ties the pieces together: carve, recover parameter sets, group access units, decode.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::container::find_mdat;
use crate::decode::{FrameDecoder, Frame, OpenH264Backend, to_annexb};
use crate::nal::{self, NAL_IDR, NAL_PPS, NAL_SEI, NAL_SLICE, NAL_SPS, Nal};
use crate::report::{Reporter, Silent};
use crate::sei::{X264Options, parse_x264_sei};
use crate::slice;
use crate::sps::{ParamSource, ParameterSets, synthesize};

pub struct Options {
    pub width: u32,
    pub height: u32,
    pub include_non_idr: bool,
    pub include_partial: bool,
    pub with_audio: bool,
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
    pub nal_type: u8,
    pub slice_count: usize,
    pub partial: bool,
    pub decoded: bool,
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
    /// End of the last fully received picture. Everything before this is intact data; anything
    /// after it is either the frame straddling the cut or padding.
    pub trusted_data_end: u64,
    pub param_source: ParamSource,
    pub profile_idc: u32,
    pub pic_init_qp: i32,
    pub qp_search_attempts: usize,
    pub x264_options_found: bool,
    pub audio_gaps_skipped: usize,
    pub access_units: usize,
    pub intra_access_units: usize,
    pub frames: Vec<FrameRecord>,
}

pub struct Extraction {
    pub report: Report,
    pub frames: Vec<(FrameRecord, Frame)>,
    pub annexb: Vec<u8>,
}

pub fn extract(data: &[u8], input_name: &str, opts: &Options) -> Result<Extraction> {
    extract_with(data, input_name, opts, &mut Silent)
}

pub fn extract_with(
    data: &[u8],
    input_name: &str,
    opts: &Options,
    reporter: &mut dyn Reporter,
) -> Result<Extraction> {
    reporter.phase("finding mdat");
    let mdat = find_mdat(data)?;
    reporter.finish_phase(&format!(
        "payload {}..{} ({})",
        mdat.payload_start,
        mdat.payload_end,
        crate::report::human_bytes(mdat.payload_end.saturating_sub(mdat.payload_start)),
    ));

    let mb_count = opts.width.div_ceil(16) * opts.height.div_ceil(16);
    reporter.phase("walking NAL chain");
    let walk = nal::walk(data, &mdat, mb_count, opts.with_audio, reporter);
    if walk.nals.is_empty() {
        bail!("no NAL units recovered from mdat");
    }
    reporter.finish_phase(&format!(
        "{} NALs, {}-byte length prefixes",
        walk.nals.len(),
        walk.nal_length_size
    ));
    if !walk.audio_gaps.is_empty() {
        reporter.note(&format!(
            "skipped {} audio gap(s)",
            walk.audio_gaps.len()
        ));
    }
    if let Some(off) = walk.truncation_offset {
        reporter.note(&format!("chain broke at offset {off} (file is truncated)"));
    } else {
        reporter.note("chain ran cleanly to the end of mdat");
    }

    let x264_opts = walk
        .nals
        .iter()
        .filter(|n| n.nal_type == NAL_SEI && n.complete)
        .find_map(|n| parse_x264_sei(n.payload(data)));
    let x264_options_found = x264_opts.is_some();
    let x264_opts = x264_opts.unwrap_or_default();

    let access_units = group_access_units(&walk.nals, data, walk.truncation_offset.is_none());
    reporter.note(&format!(
        "{} access units, {} intra",
        access_units.len(),
        access_units.iter().filter(|a| a.is_intra).count()
    ));

    if x264_options_found {
        reporter.note(&format!(
            "x264 SEI: cabac={} ref={} bframes={} 8x8dct={} rc={}{}",
            x264_opts.cabac as u8,
            x264_opts.refs,
            x264_opts.bframes,
            x264_opts.transform_8x8 as u8,
            x264_opts.rc,
            x264_opts
                .crf
                .map(|c| format!(" crf={c}"))
                .unwrap_or_default(),
        ));
    } else {
        reporter.note("no x264 options SEI; using typical High/CABAC defaults");
    }
    reporter.phase("recovering parameter sets");
    let (params, qp_attempts) =
        recover_parameter_sets(data, &walk.nals, &access_units, &x264_opts, opts, reporter)?;
    reporter.finish_phase(&format!(
        "{:?}, qp {}, {} attempt(s)",
        params.source, params.pic_init_qp, qp_attempts
    ));

    let mut decoder = OpenH264Backend::new(opts.include_partial)?;
    let mut frames = Vec::new();
    let mut records = Vec::new();
    let mut annexb = to_annexb(&[&params.sps, &params.pps]);

    let wanted: Vec<&AccessUnit> = access_units
        .iter()
        .filter(|au| {
            let keep = au.is_idr || (opts.include_non_idr && au.is_intra);
            keep && (au.complete || opts.include_partial)
        })
        .collect();

    reporter.phase("decoding I-frames");
    let mut emitted = 0usize;
    for (i, au) in wanted.iter().enumerate() {

        // For a partial picture, hand the decoder only the slices that arrived whole. A slice cut
        // mid-CABAC makes openh264 discard the entire picture, whereas a picture that is merely
        // missing its trailing slices can be concealed, recovering the macroblock rows that did
        // survive.
        // For a partial picture, hand the decoder only the slices that arrived whole; a slice cut
        // mid-CABAC is worthless. openh264 still refuses to emit a picture that is missing
        // slices, so these currently come back undecoded and are reported as such rather than
        // being invented - see README.
        let payloads: Vec<&[u8]> = au
            .nal_indices
            .iter()
            .map(|&i| &walk.nals[i])
            .filter(|n| au.complete || n.complete)
            .map(|n| n.payload(data))
            .collect();
        let mut units: Vec<&[u8]> = vec![&params.sps, &params.pps];
        units.extend_from_slice(&payloads);
        let au_stream = to_annexb(&units);
        annexb.extend_from_slice(&to_annexb(&payloads));

        // A frame that fails to decode must never sink the whole extraction - the frame
        // straddling the cut is expected to fail, and a poisoned decoder would otherwise take
        // every later frame with it.
        let decoded = match decoder.decode_au(&au_stream) {
            Ok(frame) => frame,
            Err(_) => {
                decoder = OpenH264Backend::new(opts.include_partial)?;
                None
            }
        };
        let record = FrameRecord {
            index: emitted,
            offset: au.offset,
            nal_type: if au.is_idr { NAL_IDR } else { NAL_SLICE },
            slice_count: au.nal_indices.len(),
            partial: !au.complete,
            decoded: decoded.is_some(),
        };
        records.push(record.clone());
        if let Some(frame) = decoded {
            frames.push((record, frame));
        }
        emitted += 1;
        reporter.progress(
            (i + 1) as u64,
            wanted.len() as u64,
            &format!("frame {}/{}  off {}", i + 1, wanted.len(), au.offset),
        );
    }
    reporter.finish_phase(&format!("{} of {} decoded", frames.len(), wanted.len()));

    let report = Report {
        input: input_name.to_string(),
        width: opts.width,
        height: opts.height,
        mdat_payload_start: mdat.payload_start,
        nal_length_size: walk.nal_length_size,
        nal_count: walk.nals.len(),
        truncation_offset: walk.truncation_offset,
        trusted_data_end: access_units
            .iter()
            .filter(|au| au.complete)
            .filter_map(|au| au.nal_indices.last())
            .map(|&i| walk.nals[i].payload_offset + walk.nals[i].declared_len as u64)
            .max()
            .unwrap_or(mdat.payload_start),
        param_source: params.source,
        profile_idc: crate::sps::profile_for(&x264_opts),
        pic_init_qp: params.pic_init_qp,
        qp_search_attempts: qp_attempts,
        x264_options_found,
        audio_gaps_skipped: walk.audio_gaps.len(),
        access_units: access_units.len(),
        intra_access_units: access_units.iter().filter(|a| a.is_intra).count(),
        frames: records,
    };

    Ok(Extraction {
        report,
        frames,
        annexb,
    })
}

/// A VCL NAL with `first_mb_in_slice == 0` starts a new picture. This is what makes the
/// multi-slice encodes group correctly without relying on access unit delimiters.
///
/// A picture is only trusted once the chain has moved on to the next one. Without that rule a
/// multi-slice frame whose last slice fell past the cut would look whole, because each surviving
/// slice is individually well formed - nothing but the arrival of the next picture proves the
/// previous one was fully received.
fn group_access_units(nals: &[Nal], data: &[u8], clean_end: bool) -> Vec<AccessUnit> {
    let mut units: Vec<AccessUnit> = Vec::new();

    for (i, n) in nals.iter().enumerate() {
        if n.nal_type != NAL_SLICE && n.nal_type != NAL_IDR {
            continue;
        }
        let Some(header) = slice::parse(n.payload(data)) else {
            continue;
        };

        if header.first_mb_in_slice == 0 || units.is_empty() {
            units.push(AccessUnit {
                nal_indices: vec![i],
                offset: n.offset,
                is_idr: n.nal_type == NAL_IDR,
                is_intra: header.is_intra(),
                complete: n.complete,
            });
        } else {
            let au = units.last_mut().unwrap();
            au.nal_indices.push(i);
            au.complete &= n.complete;
        }
    }

    if !clean_end && let Some(last) = units.last_mut() {
        last.complete = false;
    }

    units
}

/// In-band parameter sets win when present. Otherwise synthesise, searching for `pic_init_qp`
/// (and a few encoder-flavour fallbacks) because the SEI cannot pin that field down.
fn recover_parameter_sets(
    data: &[u8],
    nals: &[Nal],
    access_units: &[AccessUnit],
    x264_opts: &X264Options,
    opts: &Options,
    reporter: &mut dyn Reporter,
) -> Result<(ParameterSets, usize)> {
    let in_band_sps = nals
        .iter()
        .find(|n| n.nal_type == NAL_SPS && n.complete)
        .map(|n| n.payload(data).to_vec());
    let in_band_pps = nals
        .iter()
        .find(|n| n.nal_type == NAL_PPS && n.complete)
        .map(|n| n.payload(data).to_vec());

    if let (Some(sps), Some(pps)) = (in_band_sps, in_band_pps) {
        return Ok((
            ParameterSets {
                sps,
                pps,
                source: ParamSource::InBand,
                pic_init_qp: -1,
            },
            0,
        ));
    }

    // A reference picture to validate candidates against: the first complete IDR.
    let probe_au = access_units
        .iter()
        .find(|a| a.is_idr && a.complete)
        .or_else(|| access_units.iter().find(|a| a.is_intra && a.complete));

    let mut attempts = 0usize;
    let variants = option_variants(x264_opts);
    let qp_lists: Vec<Vec<i32>> = variants.iter().map(qp_candidates).collect();
    let search_len: u64 = qp_lists.iter().map(|q| q.len() as u64).sum::<u64>().max(1);
    for (variant, qps) in variants.iter().zip(&qp_lists) {
        for &qp in qps {
            attempts += 1;
            reporter.progress(
                attempts as u64,
                search_len,
                &format!("trying qp {qp}"),
            );
            let params = synthesize(opts.width, opts.height, variant, qp);
            let Some(au) = probe_au else {
                // Nothing to validate against; trust the first candidate.
                return Ok((params, attempts));
            };

            let payloads: Vec<&[u8]> = au.nal_indices.iter().map(|&i| nals[i].payload(data)).collect();
            let mut units: Vec<&[u8]> = vec![&params.sps, &params.pps];
            units.extend_from_slice(&payloads);

            let mut decoder = OpenH264Backend::new(false)?;
            if let Ok(Some(_)) = decoder.decode_au(&to_annexb(&units)) {
                return Ok((params, attempts));
            }
        }
    }

    bail!("could not find parameter sets that decode the first intra frame")
}

/// CRF is a first guess, not a pin. x264 writes `pic_init_qp = round(crf)` only for plain CRF;
/// `--stitchable` and ABR leave the PPS default of 26 (see `x264_pps_init`). The Spring corpus
/// is all plain CRF, so treating the SEI value as exact made the tool fail on otherwise ordinary
/// x264 files.
fn qp_candidates(opts: &X264Options) -> Vec<i32> {
    let mut candidates = Vec::with_capacity(52);
    let mut push = |q: i32| {
        if (0..=51).contains(&q) && !candidates.contains(&q) {
            candidates.push(q);
        }
    };

    if let Some(qp) = opts.qp {
        push(qp);
    }
    if opts.rc == "crf"
        && let Some(crf) = opts.crf
    {
        push(crf.round() as i32);
    }
    push(26);
    for q in 19..=25 {
        push(q);
    }
    for q in 0..=51 {
        push(q);
    }
    candidates
}

/// If the SEI-derived (or default) knobs fail to decode, try the other common H.264 flavour:
/// the opposite entropy coder, 8x8 transform off, and no B-frames (POC type 2). Each variant
/// still runs the QP search; the first one is the SEI/default path so the corpus stays fast.
fn option_variants(base: &X264Options) -> Vec<X264Options> {
    let mut out = vec![base.clone()];
    let mut push = |next: X264Options| {
        let dup = out.iter().any(|o| {
            o.cabac == next.cabac
                && o.transform_8x8 == next.transform_8x8
                && o.bframes == next.bframes
                && o.refs == next.refs
                && o.b_pyramid == next.b_pyramid
        });
        if !dup {
            out.push(next);
        }
    };

    let mut alt = base.clone();
    alt.cabac = !base.cabac;
    push(alt);

    let mut alt = base.clone();
    alt.transform_8x8 = !base.transform_8x8;
    push(alt);

    let mut alt = base.clone();
    alt.bframes = 0;
    alt.b_pyramid = 0;
    push(alt);

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crf_is_a_hint_not_a_pin() {
        let opts = X264Options {
            rc: "crf".into(),
            crf: Some(23.0),
            ..X264Options::default()
        };
        let c = qp_candidates(&opts);
        assert_eq!(c[0], 23, "plain CRF still tries round(crf) first");
        assert!(c.contains(&26), "stitchable/ABR default must remain reachable");
        assert_eq!(c.len(), 52);
    }

    #[test]
    fn two_pass_still_starts_at_26() {
        let opts = X264Options {
            rc: "2pass".into(),
            ..X264Options::default()
        };
        let c = qp_candidates(&opts);
        assert_eq!(c[0], 26);
        assert_eq!(c.len(), 52);
    }
}
