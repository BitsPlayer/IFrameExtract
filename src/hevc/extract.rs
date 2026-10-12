//! The H.265 half of extraction: group pictures, recover VPS/SPS/PPS, decode with ffmpeg.

use anyhow::{Result, bail};

use super::params::{self, HevcParams, SliceTypeFields};
use super::search::{self, Hints, Probe};
use super::slice::{self as hslice, SLICE_I};
use super::validate::Scratch;
use crate::codec::Codec;
use crate::container::{ContainerInfo, MdatRange};
use crate::decode::{Frame, decode_annexb_ffmpeg, to_annexb};
use crate::extract::{Extraction, FrameRecord, Options, Report};
use crate::nal::{Nal, Walk};
use crate::report::Reporter;
use crate::sps::ParamSource;

/// One picture: its base-layer VCL NALs.
struct Picture {
    nal_indices: Vec<usize>,
    offset: u64,
    nal_type: u8,
    is_irap: bool,
    is_intra: bool,
    complete: bool,
}

/// A VCL NAL with `first_slice_segment_in_pic_flag` set starts a new picture. As with H.264, a
/// picture is only trusted once the chain has moved on to the next one.
fn group_pictures(nals: &[Nal], data: &[u8], clean_end: bool) -> Vec<Picture> {
    let mut pics: Vec<Picture> = Vec::new();
    for (i, n) in nals.iter().enumerate() {
        if !super::is_vcl(n.nal_type) {
            continue;
        }
        let payload = n.payload(data);
        // The second view of MV-HEVC spatial video rides along in the same samples; only the base
        // layer is decodable without the multi-layer extensions.
        if payload.len() < 2 || super::layer_id(payload) != 0 {
            continue;
        }
        let Some(peek) = hslice::peek(payload) else {
            continue;
        };
        if peek.first_slice_segment_in_pic || pics.is_empty() {
            let irap = super::is_irap(n.nal_type);
            pics.push(Picture {
                nal_indices: vec![i],
                offset: n.offset,
                nal_type: n.nal_type,
                is_irap: irap,
                is_intra: irap,
                complete: n.complete,
            });
        } else {
            let pic = pics.last_mut().unwrap();
            pic.nal_indices.push(i);
            pic.complete &= n.complete;
        }
    }
    if !clean_end && let Some(last) = pics.last_mut() {
        last.complete = false;
    }
    pics
}

struct Recovered {
    vps: Vec<u8>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// The model behind the parameter sets, when we have one: always for synthesised sets, and for
    /// in-band ones the validator can represent.
    model: Option<HevcParams>,
    source: ParamSource,
    attempts: usize,
    label: Option<String>,
}

fn in_band(nals: &[Nal], data: &[u8]) -> Option<Recovered> {
    let find = |t: u8| {
        nals.iter()
            .find(|n| n.nal_type == t && n.complete && super::layer_id(n.payload(data)) == 0)
            .map(|n| n.payload(data).to_vec())
    };
    let (vps, sps, pps) = (
        find(super::NAL_VPS)?,
        find(super::NAL_SPS)?,
        find(super::NAL_PPS)?,
    );
    let model = params::parse(&sps, &pps);
    Some(Recovered {
        vps,
        sps,
        pps,
        model,
        source: ParamSource::InBand,
        attempts: 0,
        label: None,
    })
}

fn slice_payloads<'a>(pic: &Picture, nals: &[Nal], data: &'a [u8]) -> Vec<&'a [u8]> {
    pic.nal_indices
        .iter()
        .map(|&i| &nals[i])
        .filter(|n| pic.complete || n.complete)
        .map(|n| n.payload(data))
        .collect()
}

/// Up to three complete IRAP pictures to prove candidates against, including a non-IDR one when
/// there is one: an IDR header carries no POC or reference picture set, so only a CRA pins those.
fn pick_probes(pics: &[Picture]) -> Vec<usize> {
    let complete: Vec<usize> = (0..pics.len())
        .filter(|&i| pics[i].is_irap && pics[i].complete)
        .collect();
    let mut chosen: Vec<usize> = complete.iter().copied().take(2).collect();
    if let Some(&cra) = complete
        .iter()
        .find(|&&i| !super::is_idr(pics[i].nal_type) && !chosen.contains(&i))
    {
        chosen.push(cra);
    } else if let Some(&third) = complete.get(2) {
        chosen.push(third);
    }
    chosen
}

fn synthesise(
    pics: &[Picture],
    nals: &[Nal],
    data: &[u8],
    opts: &Options,
    hints: &Hints,
    reporter: &mut dyn Reporter,
) -> Result<Recovered> {
    let mut probe_pics = pick_probes(pics);
    if probe_pics.is_empty() {
        bail!("no complete IRAP picture to recover HEVC parameter sets from");
    }
    let mut attempts = 0;
    // Each round proves a candidate against the probes, then checks every other keyframe. One
    // that disagrees joins the probes: a field no probe exercised (a transform depth only a 64x64
    // block reaches, say) is then pinned down too.
    for _round in 0..4 {
        let mut payloads: Vec<&[u8]> = probe_pics
            .iter()
            .flat_map(|&i| slice_payloads(&pics[i], nals, data))
            .collect();
        // Cheapest first: a wrong candidate is usually rejected by the smallest slice.
        payloads.sort_by_key(|p| p.len());
        let probes: Vec<Probe> = payloads.into_iter().map(Probe::new).collect();
        let Some(found) = search::search(opts.width, opts.height, &probes, hints, reporter) else {
            bail!(
                "could not find HEVC parameter sets that parse the keyframes ({attempts} candidates \
                 tried); the encoder uses features this tool cannot synthesise"
            );
        };
        attempts += found.attempts;

        let mut scratch = Scratch::default();
        let disagreeing = pics.iter().enumerate().find(|(i, p)| {
            p.is_irap && p.complete && !probe_pics.contains(i) && {
                let pl = slice_payloads(p, nals, data);
                let pr: Vec<Probe> = pl.into_iter().map(Probe::new).collect();
                !search::validates(&found.params, &pr, &mut scratch)
            }
        });
        match disagreeing {
            None => {
                let p = found.params;
                return Ok(Recovered {
                    vps: p.vps(),
                    sps: p.sps(),
                    pps: p.pps(),
                    model: Some(p),
                    source: ParamSource::Synthesized,
                    attempts,
                    label: Some(found.label),
                });
            }
            Some((i, _)) => {
                reporter.note(&format!(
                    "candidate from {} does not parse keyframe at offset {}; widening the probe set",
                    found.label, pics[i].offset
                ));
                probe_pics.push(i);
            }
        }
    }
    bail!("HEVC parameter search did not converge on a set that parses every keyframe")
}

pub fn extract(
    data: &[u8],
    input_name: &str,
    opts: &Options,
    mdat: &MdatRange,
    container: ContainerInfo,
    walk: &Walk,
    reporter: &mut dyn Reporter,
) -> Result<Extraction> {
    let nals = &walk.nals;
    let mut pics = group_pictures(nals, data, walk.truncation_offset.is_none());

    let x265 = nals
        .iter()
        .filter(|n| n.nal_type == super::NAL_SEI_PREFIX && n.complete)
        .find_map(|n| super::x265_options(n.payload(data)));
    if let Some(o) = &x265 {
        let short: String = o.chars().take(120).collect();
        reporter.note(&format!("x265 SEI: {short}..."));
    }
    let hints = Hints {
        x265: x265
            .as_deref()
            .map(|o| search::from_x265_options(o, opts.width, opts.height)),
        high_bit_depth: nals.iter().any(|n| n.nal_type == super::NAL_DOLBY_RPU),
    };
    if hints.high_bit_depth {
        reporter.note("Dolby Vision RPUs present; preferring 10-bit parameter sets");
    }

    reporter.phase("recovering parameter sets");
    let rec = match in_band(nals, data) {
        Some(r) => r,
        None => synthesise(&pics, nals, data, opts, &hints, reporter)?,
    };
    reporter.finish_phase(&format!(
        "{:?}{}, init_qp {}, {} attempt(s)",
        rec.source,
        rec.label
            .as_deref()
            .map(|l| format!(" ({l})"))
            .unwrap_or_default(),
        rec.model.as_ref().map_or(-1, |m| m.init_qp),
        rec.attempts,
    ));

    // Non-IRAP pictures can still be all-intra; their slice type is readable now that the PPS is.
    let fields = rec
        .model
        .as_ref()
        .map(SliceTypeFields::from)
        .or_else(|| params::parse_pps_slice_fields(&rec.pps))
        .unwrap_or_default();
    for pic in pics.iter_mut().filter(|p| !p.is_irap) {
        let first = nals[pic.nal_indices[0]].payload(data);
        pic.is_intra = hslice::first_segment_type(first, fields) == Some(SLICE_I);
    }
    reporter.note(&format!(
        "{} pictures, {} IRAP, {} intra",
        pics.len(),
        pics.iter().filter(|p| p.is_irap).count(),
        pics.iter().filter(|p| p.is_intra).count(),
    ));

    let wanted: Vec<&Picture> = pics
        .iter()
        .filter(|p| {
            let keep = opts.all_frames || p.is_irap || (opts.include_non_idr && p.is_intra);
            keep && (p.complete || opts.include_partial)
        })
        .collect();

    let mut annexb = Vec::new();
    let mut per_picture: Vec<Vec<u8>> = Vec::new();
    for pic in &wanted {
        let mut units: Vec<&[u8]> = vec![&rec.vps, &rec.sps, &rec.pps];
        units.extend(slice_payloads(pic, nals, data));
        let standalone = to_annexb(&units);
        if pic.is_irap {
            annexb.extend_from_slice(&standalone);
        } else {
            annexb.extend_from_slice(&to_annexb(&units[3..]));
        }
        per_picture.push(standalone);
    }

    reporter.phase(if opts.all_frames {
        "decoding frames (ffmpeg)"
    } else {
        "decoding keyframes (ffmpeg)"
    });
    let (w, h) = (opts.width as usize, opts.height as usize);
    let passthrough = ["-fps_mode", "passthrough"];
    let mut decoded: Vec<Option<Frame>> =
        match decode_annexb_ffmpeg(&annexb, "hevc", &passthrough, w, h) {
            Ok(frames) if frames.len() == wanted.len() || opts.all_frames => {
                let mut it = frames.into_iter();
                wanted.iter().map(|_| it.next()).collect()
            }
            // A picture ffmpeg could not output shifts every later one, so fall back to decoding
            // each on its own, which only works for pictures that need no reference.
            _ => Vec::new(),
        };
    if decoded.is_empty() {
        for (i, (pic, stream)) in wanted.iter().zip(&per_picture).enumerate() {
            let frame = if pic.is_intra {
                decode_annexb_ffmpeg(stream, "hevc", &passthrough, w, h)
                    .ok()
                    .and_then(|f| f.into_iter().next())
            } else {
                None
            };
            decoded.push(frame);
            reporter.progress(
                (i + 1) as u64,
                wanted.len() as u64,
                &format!("off {}", pic.offset),
            );
        }
    }

    let mut frames = Vec::new();
    let mut records = Vec::new();
    for (i, (pic, frame)) in wanted.iter().zip(decoded).enumerate() {
        let record = FrameRecord {
            index: i,
            offset: pic.offset,
            source_size: pic
                .nal_indices
                .iter()
                .map(|&n| (walk.nal_length_size + nals[n].available_len) as u64)
                .sum(),
            nal_type: pic.nal_type,
            slice_count: pic.nal_indices.len(),
            partial: !pic.complete,
            decoded: frame.is_some(),
            sps_profile: rec.label.clone(),
        };
        records.push(record.clone());
        if let Some(f) = frame {
            frames.push((record, f));
        }
    }
    reporter.finish_phase(&format!("{} of {} decoded", frames.len(), wanted.len()));

    let report = Report {
        input: input_name.to_string(),
        codec: Codec::H265,
        container,
        width: opts.width,
        height: opts.height,
        mdat_payload_start: mdat.payload_start,
        nal_length_size: walk.nal_length_size,
        nal_count: nals.len(),
        truncation_offset: walk.truncation_offset,
        skipped_regions: walk.skipped_regions,
        skipped_bytes: walk.skipped_bytes,
        trusted_data_end: pics
            .iter()
            .filter(|p| p.complete)
            .filter_map(|p| p.nal_indices.last())
            .map(|&i| nals[i].payload_offset + nals[i].declared_len as u64)
            .max()
            .unwrap_or(mdat.payload_start),
        param_source: rec.source,
        profile_idc: rec
            .model
            .as_ref()
            .map_or(0, |m| if m.bit_depth > 8 { 2 } else { 1 }),
        pic_init_qp: rec.model.as_ref().map_or(-1, |m| m.init_qp),
        qp_search_attempts: rec.attempts,
        x264_options_found: false,
        x265_options_found: x265.is_some(),
        access_units: pics.len(),
        intra_access_units: pics.iter().filter(|p| p.is_intra).count(),
        hevc_params: rec.model.clone(),
        frames: records,
    };
    Ok(Extraction {
        report,
        frames,
        annexb,
    })
}
