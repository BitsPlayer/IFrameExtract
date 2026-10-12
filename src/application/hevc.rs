//! The H.265 half of analysis: group pictures and recover VPS/SPS/PPS. Decoding is shared with
//! H.264 in `extract::decode`.

use anyhow::{Result, bail};

use crate::domain::codec::Codec;
use crate::domain::container::{ContainerInfo, MdatRange};
use crate::domain::hevc::params::{self, HevcParams, SliceTypeFields};
use crate::domain::hevc::search::{self, Hints, Probe};
use crate::domain::hevc::slice::{self as hslice, SLICE_I};
use crate::domain::hevc::validate::Scratch;
use crate::domain::hevc::{self, NAL_DOLBY_RPU, NAL_PPS, NAL_SEI_PREFIX, NAL_SPS, NAL_VPS};
use crate::domain::model::{AccessUnit, CarveAnalysis, CarveOptions, HevcState};
use crate::domain::nal::{Nal, Walk};
use crate::domain::report::Reporter;
use crate::domain::sei::X264Options;
use crate::domain::sps::{ParamSource, ParameterSets};

/// A VCL NAL with `first_slice_segment_in_pic_flag` set starts a new picture. As with H.264, a
/// picture is only trusted once the chain has moved on to the next one.
fn group_pictures(
    nals: &[Nal],
    data: &[u8],
    nal_length_size: usize,
    clean_end: bool,
) -> Vec<AccessUnit> {
    let mut pics: Vec<AccessUnit> = Vec::new();
    for (i, n) in nals.iter().enumerate() {
        if !hevc::is_vcl(n.nal_type) {
            continue;
        }
        let payload = n.payload(data);
        // The second view of MV-HEVC spatial video rides along in the same samples; only the base
        // layer is decodable without the multi-layer extensions.
        if payload.len() < 2 || hevc::layer_id(payload) != 0 {
            continue;
        }
        let Some(peek) = hslice::peek(payload) else {
            continue;
        };
        let size = (nal_length_size + n.available_len) as u64;
        if peek.first_slice_segment_in_pic || pics.is_empty() {
            let irap = hevc::is_irap(n.nal_type);
            pics.push(AccessUnit {
                nal_indices: vec![i],
                offset: n.offset,
                nal_type: n.nal_type,
                is_idr: irap,
                is_intra: irap,
                complete: n.complete,
                size,
                gop: 0,
            });
        } else {
            let pic = pics.last_mut().unwrap();
            pic.nal_indices.push(i);
            pic.complete &= n.complete;
            pic.size += size;
        }
    }

    // A group may only start where decoding restarts cleanly. A CRA is a keyframe, but the RASL
    // pictures after it reference the group before; decoded as a group of its own they would be
    // discarded. IDR and BLA pictures drop leading references, so groups split there only.
    let mut gop = 0;
    for (i, pic) in pics.iter_mut().enumerate() {
        if hevc::is_idr(pic.nal_type) || (hevc::NAL_BLA_W_LP..=18).contains(&pic.nal_type) {
            gop = i;
        }
        pic.gop = gop;
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
    model: Option<HevcParams>,
    source: ParamSource,
    attempts: usize,
    label: Option<String>,
}

fn in_band(nals: &[Nal], data: &[u8]) -> Option<Recovered> {
    let find = |t: u8| {
        nals.iter()
            .find(|n| n.nal_type == t && n.complete && hevc::layer_id(n.payload(data)) == 0)
            .map(|n| n.payload(data).to_vec())
    };
    let (vps, sps, pps) = (find(NAL_VPS)?, find(NAL_SPS)?, find(NAL_PPS)?);
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

fn slice_payloads<'a>(pic: &AccessUnit, nals: &[Nal], data: &'a [u8]) -> Vec<&'a [u8]> {
    pic.nal_indices
        .iter()
        .map(|&i| &nals[i])
        .filter(|n| pic.complete || n.complete)
        .map(|n| n.payload(data))
        .collect()
}

/// Up to three complete IRAP pictures to prove candidates against, including a non-IDR one when
/// there is one: an IDR header carries no POC or reference picture set, so only a CRA pins those.
fn pick_probes(pics: &[AccessUnit]) -> Vec<usize> {
    let complete: Vec<usize> = (0..pics.len())
        .filter(|&i| pics[i].is_idr && pics[i].complete)
        .collect();
    let mut chosen: Vec<usize> = complete.iter().copied().take(2).collect();
    if let Some(&cra) = complete
        .iter()
        .find(|&&i| !hevc::is_idr(pics[i].nal_type) && !chosen.contains(&i))
    {
        chosen.push(cra);
    } else if let Some(&third) = complete.get(2) {
        chosen.push(third);
    }
    chosen
}

fn synthesise(
    pics: &[AccessUnit],
    nals: &[Nal],
    data: &[u8],
    opts: &CarveOptions,
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
            p.is_idr && p.complete && !probe_pics.contains(i) && {
                let pr: Vec<Probe> = slice_payloads(p, nals, data)
                    .into_iter()
                    .map(Probe::new)
                    .collect();
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

pub fn analyze(
    data: &[u8],
    opts: &CarveOptions,
    mdat: &MdatRange,
    container: ContainerInfo,
    walk: Walk,
    reporter: &mut dyn Reporter,
) -> Result<CarveAnalysis> {
    let nals = &walk.nals;
    let mut pics = group_pictures(
        nals,
        data,
        walk.nal_length_size,
        walk.truncation_offset.is_none(),
    );

    let x265 = nals
        .iter()
        .filter(|n| n.nal_type == NAL_SEI_PREFIX && n.complete)
        .find_map(|n| hevc::x265_options(n.payload(data)));
    if let Some(o) = &x265 {
        let short: String = o.chars().take(120).collect();
        reporter.note(&format!("x265 SEI: {short}..."));
    }
    let hints = Hints {
        x265: x265
            .as_deref()
            .map(|o| search::from_x265_options(o, opts.width, opts.height)),
        high_bit_depth: nals.iter().any(|n| n.nal_type == NAL_DOLBY_RPU),
    };
    if hints.high_bit_depth {
        reporter.note("Dolby Vision RPUs present; preferring 10-bit parameter sets");
    }

    reporter.phase("recovering parameter sets");
    let rec = match in_band(nals, data) {
        Some(r) => r,
        None => synthesise(&pics, nals, data, opts, &hints, reporter)?,
    };
    let init_qp = rec.model.as_ref().map_or(-1, |m| m.init_qp);
    reporter.finish_phase(&format!(
        "{:?}{}, init_qp {init_qp}, {} attempt(s)",
        rec.source,
        rec.label
            .as_deref()
            .map(|l| format!(" ({l})"))
            .unwrap_or_default(),
        rec.attempts,
    ));

    // Non-IRAP pictures can still be all-intra; their slice type is readable now that the PPS is.
    let fields = rec
        .model
        .as_ref()
        .map(SliceTypeFields::from)
        .or_else(|| params::parse_pps_slice_fields(&rec.pps))
        .unwrap_or_default();
    for pic in pics.iter_mut().filter(|p| !p.is_idr) {
        let first = nals[pic.nal_indices[0]].payload(data);
        pic.is_intra = hslice::first_segment_type(first, fields) == Some(SLICE_I);
    }
    reporter.note(&format!(
        "{} pictures, {} IRAP, {} intra",
        pics.len(),
        pics.iter().filter(|p| p.is_idr).count(),
        pics.iter().filter(|p| p.is_intra).count(),
    ));

    let trusted_data_end = pics
        .iter()
        .filter(|p| p.complete)
        .filter_map(|p| p.nal_indices.last())
        .map(|&i| nals[i].payload_offset + nals[i].declared_len as u64)
        .max()
        .unwrap_or(mdat.payload_start);

    Ok(CarveAnalysis {
        codec: Codec::H265,
        container,
        width: opts.width,
        height: opts.height,
        with_audio: opts.with_audio,
        mdat_payload_start: mdat.payload_start,
        mdat_payload_end: mdat.payload_end,
        nal_length_size: walk.nal_length_size,
        skipped: walk.skipped,
        truncation_offset: walk.truncation_offset,
        trusted_data_end,
        x264_opts: X264Options::default(),
        x264_options_found: false,
        params: ParameterSets {
            sps: rec.sps,
            pps: rec.pps,
            source: rec.source,
            pic_init_qp: init_qp,
        },
        qp_attempts: rec.attempts,
        access_units: pics,
        hevc: Some(HevcState {
            vps: rec.vps,
            model: rec.model,
            label: rec.label,
            x265_options_found: x265.is_some(),
        }),
        nals: walk.nals,
    })
}
