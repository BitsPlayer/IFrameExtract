//! Ties the pieces together: carve, recover parameter sets, group access units, decode.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::container::find_mdat;
use crate::decode::{FrameDecoder, Frame, OpenH264Backend, to_annexb};
use crate::nal::{self, NAL_IDR, NAL_PPS, NAL_SEI, NAL_SLICE, NAL_SPS, Nal};
use crate::sei::{X264Options, parse_x264_sei};
use crate::slice;
use crate::sps::{ParamSource, ParameterSets, synthesize};

pub struct Options {
    pub width: u32,
    pub height: u32,
    pub include_non_idr: bool,
    pub include_partial: bool,
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
    let mdat = find_mdat(data)?;
    let mb_count = opts.width.div_ceil(16) * opts.height.div_ceil(16);
    let walk = nal::walk(data, &mdat, mb_count);
    if walk.nals.is_empty() {
        bail!("no NAL units recovered from mdat");
    }

    let x264_opts = walk
        .nals
        .iter()
        .filter(|n| n.nal_type == NAL_SEI && n.complete)
        .find_map(|n| parse_x264_sei(n.payload(data)));
    let x264_options_found = x264_opts.is_some();
    let x264_opts = x264_opts.unwrap_or_default();

    let access_units = group_access_units(&walk.nals, data, walk.truncation_offset.is_none());
    let (params, qp_attempts) = recover_parameter_sets(data, &walk.nals, &access_units, &x264_opts, opts)?;

    let mut decoder = OpenH264Backend::new(opts.include_partial)?;
    let mut frames = Vec::new();
    let mut records = Vec::new();
    let mut annexb = to_annexb(&[&params.sps, &params.pps]);

    let mut emitted = 0usize;
    for au in &access_units {
        let wanted = au.is_idr || (opts.include_non_idr && au.is_intra);
        if !wanted {
            continue;
        }
        if !au.complete && !opts.include_partial {
            continue;
        }

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
    }

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
/// when the SEI does not pin it down.
fn recover_parameter_sets(
    data: &[u8],
    nals: &[Nal],
    access_units: &[AccessUnit],
    x264_opts: &X264Options,
    opts: &Options,
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

    for (attempt, qp) in qp_candidates(x264_opts).into_iter().enumerate() {
        let params = synthesize(opts.width, opts.height, x264_opts, qp);
        let Some(au) = probe_au else {
            // Nothing to validate against; trust the first candidate.
            return Ok((params, attempt + 1));
        };

        let payloads: Vec<&[u8]> = au.nal_indices.iter().map(|&i| nals[i].payload(data)).collect();
        let mut units: Vec<&[u8]> = vec![&params.sps, &params.pps];
        units.extend_from_slice(&payloads);

        let mut decoder = OpenH264Backend::new(false)?;
        if let Ok(Some(_)) = decoder.decode_au(&to_annexb(&units)) {
            return Ok((params, attempt + 1));
        }
    }

    bail!("could not find a pic_init_qp that decodes the first intra frame")
}

/// `rc=crf` pins the value exactly (verified across the corpus: pic_init_qp == round(crf)).
/// Otherwise 26 covers every CBR encode, and two-pass lands in the low twenties.
fn qp_candidates(opts: &X264Options) -> Vec<i32> {
    if let Some(qp) = opts.qp {
        return vec![qp];
    }
    if opts.rc == "crf"
        && let Some(crf) = opts.crf
    {
        return vec![crf.round() as i32];
    }

    let mut candidates = vec![26];
    candidates.extend(19..=25);
    candidates.extend((0..=51).filter(|q| !(19..=26).contains(q)));
    candidates
}
