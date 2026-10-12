//! Carve, recover parameter sets, group access units, and decode through a decoder port.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use rayon::prelude::*;

use crate::application::ports::{FrameDecoder, VideoDecoder};
use crate::container::{ContainerInfo, find_mdat};
use crate::domain::codec::Codec;
use crate::domain::annexb::to_annexb;
use crate::domain::frame::Frame;
use crate::domain::model::{
    AccessUnit, CarveAnalysis, CarveOptions, Extraction, FrameRecord, Options, Report, Selection,
};
use crate::nal::{self, NAL_IDR, NAL_PPS, NAL_SEI, NAL_SLICE, NAL_SPS, Nal};
use crate::report::Reporter;
use crate::sei::{X264Options, parse_x264_sei};
use crate::slice;
use crate::sps::{EncoderProfile, ParamSource, ParameterSets, synthesize, synthesize_profile};

pub fn extract_with_decoder(
    data: &[u8],
    input_name: &str,
    opts: &Options,
    reporter: &mut (dyn Reporter + Send),
    factory: &dyn VideoDecoder,
) -> Result<Extraction> {
    let analysis = analyze(
        data,
        &CarveOptions {
            width: opts.width,
            height: opts.height,
            with_audio: opts.with_audio,
            codec: opts.codec,
        },
        reporter,
        factory,
    )?;
    decode(
        data,
        &analysis,
        &Selection::Filtered {
            include_non_idr: opts.include_non_idr,
            include_partial: opts.include_partial,
            all_frames: opts.all_frames,
        },
        input_name,
        reporter,
        factory,
    )
}

pub fn analyze(
    data: &[u8],
    opts: &CarveOptions,
    reporter: &mut dyn Reporter,
    factory: &dyn VideoDecoder,
) -> Result<CarveAnalysis> {
    reporter.phase("finding mdat");
    let mdat = find_mdat(data)?;
    let container = ContainerInfo::read(data);
    reporter.finish_phase(&format!(
        "{} payload {}..{} ({})",
        container.kind,
        mdat.payload_start,
        mdat.payload_end,
        crate::report::human_bytes(mdat.payload_end.saturating_sub(mdat.payload_start)),
    ));

    let mb_count = opts.width.div_ceil(16) * opts.height.div_ceil(16);
    let codec = opts
        .codec
        .unwrap_or_else(|| nal::detect_codec(data, &mdat, mb_count));
    reporter.note(&format!(
        "{} bitstream{}",
        codec.label(),
        if opts.codec.is_some() { " (forced)" } else { "" }
    ));
    reporter.phase("walking NAL chain");
    let walk = nal::walk(codec, data, &mdat, mb_count, opts.with_audio, reporter);
    if walk.nals.is_empty() {
        bail!("no NAL units recovered from mdat");
    }
    reporter.finish_phase(&format!(
        "{} NALs, {}-byte length prefixes",
        walk.nals.len(),
        walk.nal_length_size
    ));
    if walk.skipped_regions > 0 {
        reporter.note(&format!(
            "stepped over {} non-video region(s) totalling {} (interleaved audio or another track)",
            walk.skipped_regions,
            crate::report::human_bytes(walk.skipped_bytes),
        ));
    }
    if let Some(off) = walk.truncation_offset {
        reporter.note(&format!(
            "chain broke at offset {off} ({:.2}% of file; file is truncated)",
            off as f64 / data.len().max(1) as f64 * 100.0
        ));
    } else {
        reporter.note("chain ran cleanly to the end of mdat");
    }

    if codec == Codec::H265 {
        return super::hevc::analyze(data, opts, &mdat, container, walk, reporter);
    }

    let x264_opts = walk
        .nals
        .iter()
        .filter(|n| n.nal_type == NAL_SEI && n.complete)
        .find_map(|n| parse_x264_sei(n.payload(data)));
    let x264_options_found = x264_opts.is_some();
    let x264_opts = x264_opts.unwrap_or_default();

    let access_units = group_access_units(
        &walk.nals,
        data,
        walk.nal_length_size,
        walk.truncation_offset.is_none(),
    );
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
        recover_parameter_sets(data, &walk.nals, &access_units, &x264_opts, opts, reporter, factory)?;
    reporter.finish_phase(&format!(
        "{:?}, qp {}, {} attempt(s)",
        params.source, params.pic_init_qp, qp_attempts
    ));

    let trusted_data_end = access_units
        .iter()
        .filter(|au| au.complete)
        .filter_map(|au| au.nal_indices.last())
        .map(|&i| walk.nals[i].payload_offset + walk.nals[i].declared_len as u64)
        .max()
        .unwrap_or(mdat.payload_start);

    Ok(CarveAnalysis {
        codec,
        container,
        width: opts.width,
        height: opts.height,
        with_audio: opts.with_audio,
        mdat_payload_start: mdat.payload_start,
        mdat_payload_end: mdat.payload_end,
        nal_length_size: walk.nal_length_size,
        nals: walk.nals,
        skipped: walk.skipped,
        truncation_offset: walk.truncation_offset,
        trusted_data_end,
        x264_opts,
        x264_options_found,
        params,
        qp_attempts,
        access_units,
        hevc: None,
    })
}

pub fn decode(
    data: &[u8],
    a: &CarveAnalysis,
    selection: &Selection,
    input_name: &str,
    reporter: &mut (dyn Reporter + Send),
    factory: &dyn VideoDecoder,
) -> Result<Extraction> {
    let (wanted, all_frames, include_partial): (Vec<&AccessUnit>, bool, bool) = match *selection {
        Selection::Filtered {
            include_non_idr,
            include_partial,
            all_frames,
        } => (
            a.access_units
                .iter()
                .filter(|au| {
                    let keep = all_frames || au.is_idr || (include_non_idr && au.is_intra);
                    keep && (au.complete || include_partial)
                })
                .collect(),
            all_frames,
            include_partial,
        ),
        Selection::Single(i) => {
            let Some(au) = a.access_units.get(i) else {
                bail!("access unit {i} out of range ({} total)", a.access_units.len());
            };
            (vec![au], false, !au.complete)
        }
    };

    let phase = if all_frames {
        "decoding frames (I + P)"
    } else {
        "decoding I-frames"
    };
    reporter.phase(phase);

    let total = wanted.len() as u64;
    let done = AtomicU64::new(0);
    let shared = Mutex::new(&mut *reporter);
    let tick = |au: &AccessUnit, n: u64| {
        let d = done.fetch_add(n, Ordering::Relaxed) + n;
        shared.lock().unwrap().progress(
            d,
            total,
            &format!("frame {d}/{total}  off {}", au.offset),
        );
    };

    // (frame, sps_profile) per wanted access unit, in order.
    let mut annexb = Vec::new();
    let outcomes: Vec<(Option<Frame>, Option<String>)> = match *selection {
        Selection::Single(i) if !wanted[0].is_intra => {
            let au = wanted[0];
            let gop: Vec<&AccessUnit> = a.access_units[au.gop..=i].iter().collect();
            annexb = gop_annexb(data, a, &gop);
            let mut frames = decode_gop(data, a, &gop, factory, true)?;
            tick(au, 1);
            vec![(frames.pop().flatten(), None)]
        }
        _ if all_frames => {
            let mut groups: Vec<Vec<&AccessUnit>> = Vec::new();
            for au in &wanted {
                match groups.last_mut() {
                    Some(g) if g[0].gop == au.gop => g.push(au),
                    _ => groups.push(vec![au]),
                }
            }
            let results: Vec<(Vec<u8>, Vec<Option<Frame>>)> = groups
                .par_iter()
                .map(|g| {
                    let frames = decode_gop(data, a, g, factory, include_partial)?;
                    tick(g[g.len() - 1], g.len() as u64);
                    Ok((gop_annexb(data, a, g), frames))
                })
                .collect::<Result<_>>()?;
            let mut out = Vec::with_capacity(wanted.len());
            for (stream, frames) in results {
                annexb.extend_from_slice(&stream);
                out.extend(frames.into_iter().map(|f| (f, None)));
            }
            out
        }
        _ if a.codec == Codec::H265 => {
            annexb = gop_annexb(data, a, &wanted);
            let frames = decode_hevc_pictures(data, a, &wanted, &annexb, factory);
            if let Some(last) = wanted.last() {
                tick(last, wanted.len() as u64);
            }
            let label = a.hevc.as_ref().and_then(|h| h.label.clone());
            frames.into_iter().map(|f| (f, label.clone())).collect()
        }
        _ => {
            annexb = gop_annexb(data, a, &wanted);
            wanted
                .par_iter()
                .map_init(
                    || factory.open(include_partial),
                    |decoder, au| {
                        let decoder = decoder.as_mut().map_err(|e| anyhow!("{e}"))?;
                        let r = decode_one_au(data, a, au, decoder, factory, include_partial)?;
                        tick(au, 1);
                        Ok(r)
                    },
                )
                .collect::<Result<_>>()?
        }
    };

    let mut frames = Vec::new();
    let mut records = Vec::with_capacity(wanted.len());
    for (index, (au, (decoded, sps_profile))) in wanted.iter().zip(outcomes).enumerate() {
        let record = FrameRecord {
            index,
            offset: au.offset,
            source_size: au.size,
            nal_type: au.nal_type,
            slice_count: au.nal_indices.len(),
            partial: !au.complete,
            decoded: decoded.is_some(),
            sps_profile,
        };
        records.push(record.clone());
        if let Some(f) = decoded {
            frames.push((record, f));
        }
    }
    reporter.finish_phase(&format!("{} of {} decoded", frames.len(), wanted.len()));

    let report = Report {
        input: input_name.to_string(),
        codec: a.codec,
        container: a.container.clone(),
        width: a.width,
        height: a.height,
        mdat_payload_start: a.mdat_payload_start,
        nal_length_size: a.nal_length_size,
        nal_count: a.nals.len(),
        truncation_offset: a.truncation_offset,
        skipped_regions: a.skipped.len(),
        skipped_bytes: a.skipped.iter().map(|&(_, len)| len).sum(),
        trusted_data_end: a.trusted_data_end,
        param_source: a.params.source,
        profile_idc: match &a.hevc {
            None => crate::sps::profile_for(&a.x264_opts),
            Some(h) => h.model.as_ref().map_or(0, |m| if m.bit_depth > 8 { 2 } else { 1 }),
        },
        pic_init_qp: a.params.pic_init_qp,
        qp_search_attempts: a.qp_attempts,
        x264_options_found: a.x264_options_found,
        x265_options_found: a.hevc.as_ref().is_some_and(|h| h.x265_options_found),
        access_units: a.access_units.len(),
        intra_access_units: a.access_units.iter().filter(|x| x.is_intra).count(),
        hevc_params: a.hevc.as_ref().and_then(|h| h.model.clone()),
        frames: records,
    };

    Ok(Extraction {
        report,
        frames,
        annexb,
    })
}

fn au_payloads<'a>(data: &'a [u8], a: &CarveAnalysis, au: &AccessUnit) -> Vec<&'a [u8]> {
    au.nal_indices
        .iter()
        .map(|&i| &a.nals[i])
        .filter(|n| au.complete || n.complete)
        .map(|n| n.payload(data))
        .collect()
}

/// Parameter sets as they go in front of a keyframe: SPS and PPS, preceded by the VPS for H.265.
fn param_units(a: &CarveAnalysis) -> Vec<&[u8]> {
    let mut units: Vec<&[u8]> = Vec::with_capacity(3);
    if let Some(h) = &a.hevc {
        units.push(&h.vps);
    }
    units.extend([a.params.sps.as_slice(), a.params.pps.as_slice()]);
    units
}

/// Parameter sets go in front of every IDR (every IRAP for H.265), as the stream is otherwise
/// parameter-set free.
fn gop_annexb(data: &[u8], a: &CarveAnalysis, aus: &[&AccessUnit]) -> Vec<u8> {
    let mut annexb = Vec::new();
    for au in aus {
        if au.is_idr {
            annexb.extend_from_slice(&to_annexb(&param_units(a)));
        }
        annexb.extend_from_slice(&to_annexb(&au_payloads(data, a, au)));
    }
    annexb
}

/// One frame (or `None`) per access unit. The whole run goes through the stream decoder; if that
/// fails or yields nothing, a sequential decoder takes it one access unit at a time.
fn decode_gop(
    data: &[u8],
    a: &CarveAnalysis,
    aus: &[&AccessUnit],
    factory: &dyn VideoDecoder,
    include_partial: bool,
) -> Result<Vec<Option<Frame>>> {
    let stream = gop_annexb(data, a, aus);
    if let Ok(df) = factory.decode_stream(a.codec, &stream, a.width as usize, a.height as usize)
        && !df.is_empty()
    {
        let mut it = df.into_iter();
        return Ok(aus.iter().map(|_| it.next()).collect());
    }
    if a.codec == Codec::H265 {
        // The per-access-unit decoder is H.264 only.
        return Ok(aus.iter().map(|_| None).collect());
    }

    let mut decoder = factory.open(include_partial)?;
    let mut out = Vec::with_capacity(aus.len());
    for au in aus {
        let mut units: Vec<&[u8]> = Vec::new();
        if au.is_idr {
            units.extend([a.params.sps.as_slice(), a.params.pps.as_slice()]);
        }
        units.extend(au_payloads(data, a, au));
        match decoder.decode_au(&to_annexb(&units)) {
            Ok(f) => out.push(f),
            Err(_) => {
                decoder = factory.open(include_partial)?;
                out.push(None);
            }
        }
    }
    Ok(out)
}

/// H.265 keyframes through the stream decoder: all at once when the picture count comes back
/// whole, otherwise one at a time, since a picture ffmpeg drops would shift every later one.
fn decode_hevc_pictures(
    data: &[u8],
    a: &CarveAnalysis,
    wanted: &[&AccessUnit],
    stream: &[u8],
    factory: &dyn VideoDecoder,
) -> Vec<Option<Frame>> {
    let (w, h) = (a.width as usize, a.height as usize);
    if let Ok(frames) = factory.decode_stream(Codec::H265, stream, w, h)
        && frames.len() == wanted.len()
    {
        return frames.into_iter().map(Some).collect();
    }
    wanted
        .par_iter()
        .map(|au| {
            if !au.is_intra {
                return None;
            }
            let mut units = param_units(a);
            units.extend(au_payloads(data, a, au));
            factory
                .decode_stream(Codec::H265, &to_annexb(&units), w, h)
                .ok()
                .and_then(|f| f.into_iter().next())
        })
        .collect()
}

fn decode_one_au(
    data: &[u8],
    a: &CarveAnalysis,
    au: &AccessUnit,
    decoder: &mut Box<dyn FrameDecoder>,
    factory: &dyn VideoDecoder,
    include_partial: bool,
) -> Result<(Option<Frame>, Option<String>)> {
    let params = &a.params;
    let x264_opts = &a.x264_opts;
    let payloads = au_payloads(data, a, au);
    let mut units: Vec<&[u8]> = vec![&params.sps, &params.pps];
    units.extend_from_slice(&payloads);
    let au_stream = to_annexb(&units);

    let mut decoded = match decoder.decode_au(&au_stream) {
        Ok(frame) => frame,
        Err(_) => {
            *decoder = factory.open(include_partial)?;
            None
        }
    };

    let mut frame_sps_profile = None;
    if decoded.is_none()
        && (params.source == ParamSource::Synthesized
            || params.source == ParamSource::SynthesizedHardware)
    {
        for variant in option_variants(x264_opts) {
            for qp in qp_candidates(x264_opts) {
                let candidate = synthesize(a.width, a.height, &variant, qp);
                let mut probe_units: Vec<&[u8]> = vec![&candidate.sps, &candidate.pps];
                probe_units.extend_from_slice(&payloads);
                let mut probe_dec = factory.open(false)?;
                if let Ok(Some(f)) = probe_dec.decode_au(&to_annexb(&probe_units)) {
                    decoded = Some(f);
                    frame_sps_profile = Some("x264".to_string());
                    break;
                }
            }
            if decoded.is_some() {
                break;
            }
        }

        if decoded.is_none() {
            let hw_qps = hardware_qp_candidates();
            let hw_profiles = hardware_profile_variants();
            let hw_base = X264Options {
                bframes: 0,
                b_pyramid: 0,
                weighted_pred: false,
                weighted_bipred: false,
                ..x264_opts.clone()
            };
            let hw_opts_variants = hardware_opts_variants(&hw_base);
            'hw_retry: for profile in &hw_profiles {
                for hw_opts in &hw_opts_variants {
                    for &qp in &hw_qps {
                        let candidate = synthesize_profile(a.width, a.height, hw_opts, qp, *profile);
                        let mut probe_units: Vec<&[u8]> = vec![&candidate.sps, &candidate.pps];
                        probe_units.extend_from_slice(&payloads);
                        let mut probe_dec = factory.open(false)?;
                        if let Ok(Some(f)) = probe_dec.decode_au(&to_annexb(&probe_units)) {
                            decoded = Some(f);
                            frame_sps_profile = Some(profile.label());
                            break 'hw_retry;
                        }
                    }
                }
            }
        }
    }
    Ok((decoded, frame_sps_profile))
}

/// A VCL NAL with `first_mb_in_slice == 0` starts a new picture. This is what makes the
/// multi-slice encodes group correctly without relying on access unit delimiters.
///
/// A picture is only trusted once the chain has moved on to the next one. Without that rule a
/// multi-slice frame whose last slice fell past the cut would look whole, because each surviving
/// slice is individually well formed - nothing but the arrival of the next picture proves the
/// previous one was fully received.
fn group_access_units(
    nals: &[Nal],
    data: &[u8],
    nal_length_size: usize,
    clean_end: bool,
) -> Vec<AccessUnit> {
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
                nal_type: n.nal_type,
                is_idr: n.nal_type == NAL_IDR,
                is_intra: header.is_intra(),
                complete: n.complete,
                size: (nal_length_size + n.available_len) as u64,
                gop: 0,
            });
        } else {
            let au = units.last_mut().unwrap();
            au.nal_indices.push(i);
            au.complete &= n.complete;
            au.size += (nal_length_size + n.available_len) as u64;
        }
    }

    let mut gop = 0;
    for (i, au) in units.iter_mut().enumerate() {
        if au.is_idr {
            gop = i;
        }
        au.gop = gop;
    }

    if !clean_end && let Some(last) = units.last_mut() {
        last.complete = false;
    }

    units
}

/// In-band parameter sets win when present. Otherwise synthesise, searching for `pic_init_qp`
/// (and a few encoder-flavour fallbacks) because the SEI cannot pin that field down.
///
/// Validation uses the first **two** complete IDR frames: a candidate that decodes Frame 0 but
/// not Frame 1 is rejected. Frame 0 always has `frame_num = 0` (all-zero bits), so it decodes
/// correctly even with a wrong `log2_max_frame_num_minus4` bit-width; Frame 1 has `frame_num > 0`
/// and reveals the mismatch. Requiring both frames prevents accepting a bad synthesised SPS that
/// only happens to work for the very first IDR.
fn recover_parameter_sets(
    data: &[u8],
    nals: &[Nal],
    access_units: &[AccessUnit],
    x264_opts: &X264Options,
    opts: &CarveOptions,
    reporter: &mut dyn Reporter,
    factory: &dyn VideoDecoder,
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

    // Collect up to two complete IDR frames to validate candidates against.
    // Frame 0: frame_num = 0 (all-zero bits) — passes even with wrong bit-width.
    // Frame 1: frame_num = 1 — reveals bit-width mismatches in the SPS.
    // Fall back to intra (non-IDR) frames when IDRs are scarce.
    let mut probe_aus: Vec<&AccessUnit> = access_units
        .iter()
        .filter(|a| a.is_idr && a.complete)
        .take(2)
        .collect();
    if probe_aus.is_empty() {
        // No IDR at all — try intra slices as a last resort.
        probe_aus = access_units
            .iter()
            .filter(|a| a.is_intra && a.complete)
            .take(2)
            .collect();
    }

    let mut attempts = 0usize;
    let variants = option_variants(x264_opts);
    let qp_lists: Vec<Vec<i32>> = variants.iter().map(qp_candidates).collect();
    let search_len: u64 = qp_lists.iter().map(|q| q.len() as u64).sum::<u64>().max(1);
    for (variant, qps) in variants.iter().zip(&qp_lists) {
        for &qp in qps {
            attempts += 1;
            reporter.progress(attempts as u64, search_len, &format!("trying qp {qp}"));
            let params = synthesize(opts.width, opts.height, variant, qp);
            if probe_aus.is_empty() {
                // Nothing to validate against; trust the first candidate.
                return Ok((params, attempts));
            }
            if decode_probe(&params, &probe_aus, nals, data, factory)? {
                return Ok((params, attempts));
            }
        }
    }

    // ── Hardware encoder sweep ────────────────────────────────────────────────
    // x264 candidates exhausted. This file is probably from a hardware encoder
    // (Android/Qualcomm, Apple AVFoundation, etc.) that uses a wider frame_num
    // field and different PPS knobs (refs, chroma_qp_offset, cabac).
    // Try each combination of SPS profile × PPS opts × QP until one decodes
    // both probe frames successfully.
    if !probe_aus.is_empty() {
        let hw_qps = hardware_qp_candidates();
        let hw_profiles = hardware_profile_variants();
        let hw_base = X264Options {
            bframes: 0,
            b_pyramid: 0,
            weighted_pred: false,
            weighted_bipred: false,
            ..x264_opts.clone()
        };
        let hw_opts_variants = hardware_opts_variants(&hw_base);
        let hw_total = (hw_profiles.len() * hw_opts_variants.len() * hw_qps.len()) as u64;
        reporter.note("x264 search exhausted; trying hardware encoder profiles");
        for profile in &hw_profiles {
            for hw_opts in &hw_opts_variants {
                for &qp in &hw_qps {
                    attempts += 1;
                    reporter.progress(
                        attempts as u64,
                        attempts as u64 + hw_total,
                        &format!("hw {} qp {qp}", profile.label()),
                    );
                    let params =
                        synthesize_profile(opts.width, opts.height, hw_opts, qp, *profile);
                    if decode_probe(&params, &probe_aus, nals, data, factory)? {
                        reporter.note(&format!(
                            "hardware profile matched: {} (qp {qp})",
                            profile.label()
                        ));
                        return Ok((params, attempts));
                    }
                }
            }
        }
    }

    bail!("could not find parameter sets that decode the probe frames; \
           try supplying --size or passing in-band SPS/PPS from a companion file")
}

/// Try to decode every access unit in `probe_aus` with the given `params`. Returns `true` only
/// when **all** supplied frames produce an output picture. A fresh decoder is used for each call
/// so that a prior failure's state cannot contaminate the probe.
///
/// Using multiple frames as the acceptance criterion prevents false positives where a wrong
/// `log2_max_frame_num_minus4` passes Frame 0 (frame_num = 0 ≡ all-zero bits in any width) but
/// would fail Frame 1 (frame_num = 1, bit pattern depends on the declared width).
fn decode_probe(
    params: &ParameterSets,
    probe_aus: &[&AccessUnit],
    nals: &[Nal],
    data: &[u8],
    factory: &dyn VideoDecoder,
) -> Result<bool> {
    // Single decoder instance shared across all probe frames: this also tests that the decoder
    // stays coherent across consecutive IDRs, catching any state-carryover issues.
    let mut decoder = factory.open(false)?;
    for au in probe_aus {
        let payloads: Vec<&[u8]> = au
            .nal_indices
            .iter()
            .map(|&i| nals[i].payload(data))
            .collect();
        let mut units: Vec<&[u8]> = vec![&params.sps, &params.pps];
        units.extend_from_slice(&payloads);
        match decoder.decode_au(&to_annexb(&units)) {
            Ok(Some(_)) => {} // frame decoded — continue to next probe
            Ok(None) | Err(_) => return Ok(false),
        }
    }
    Ok(true)
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

/// The ordered list of hardware encoder profiles to try when the x264 search fails.
///
/// Ordered by real-world prevalence — the most common mobile hardware encoders first so that
/// the search terminates quickly for the majority of forensic inputs:
///
/// 1. Android Qualcomm (LG, Samsung, Motorola, Google Pixel — dominant in Android ecosystem)
/// 2. Apple AVFoundation (iPhone, iPad)
/// 3. Generic sweep: 6 combinations of `log2_max_frame_num_minus4` × `poc_type` covering
///    MediaTek, Hisilicon, Exynos, and any other hardware that does not match the above exactly.
fn hardware_profile_variants() -> Vec<EncoderProfile> {
    vec![
        // Known named profiles first (fast path for common devices).
        EncoderProfile::AndroidQualcomm,   // log2_max_frame_num_minus4=10, poc_type=2
        EncoderProfile::AppleAVFoundation, // log2_max_frame_num_minus4=4,  poc_type=0
        // Generic sweep: poc_type 2 (no B-frames, most common) then poc_type 0.
        EncoderProfile::Generic { log2_max_frame_num_minus4: 6,  poc_type: 2 },
        EncoderProfile::Generic { log2_max_frame_num_minus4: 6,  poc_type: 0 },
        EncoderProfile::Generic { log2_max_frame_num_minus4: 8,  poc_type: 2 },
        EncoderProfile::Generic { log2_max_frame_num_minus4: 8,  poc_type: 0 },
        EncoderProfile::Generic { log2_max_frame_num_minus4: 10, poc_type: 0 },
        EncoderProfile::Generic { log2_max_frame_num_minus4: 12, poc_type: 2 },
        EncoderProfile::Generic { log2_max_frame_num_minus4: 12, poc_type: 0 },
        EncoderProfile::Generic { log2_max_frame_num_minus4: 14, poc_type: 2 },
        EncoderProfile::Generic { log2_max_frame_num_minus4: 14, poc_type: 0 },
    ]
}

/// QP candidates for the hardware sweep. Hardware encoders almost always leave `pic_init_qp`
/// at the PPS default of 26; a small window around it catches any that deviate.
fn hardware_qp_candidates() -> Vec<i32> {
    // Try 26 first (PPS default), then expand outward in the range typical for mobile encoders.
    let mut candidates = vec![26];
    for q in [24, 25, 27, 28, 20, 21, 22, 23, 29, 30] {
        candidates.push(q);
    }
    // Full sweep as a last resort.
    for q in 0..=51i32 {
        if !candidates.contains(&q) {
            candidates.push(q);
        }
    }
    candidates
}

/// PPS-level option variants for the hardware sweep.
///
/// Hardware encoders differ from x264 mainly in two PPS fields:
/// - `refs` (num_ref_idx_l0_default_active_minus1 + 1): commonly 1 or 2 on mobile, 3 on x264.
/// - `chroma_qp_offset`: commonly −1 or 0 on mobile; x264 defaults to 0.
///
/// CABAC is tried both on and off since some SoCs default to non-CABAC even at High profile.
/// The base opts (no B-frames) are always included first to preserve the fast common-case path.
fn hardware_opts_variants(base: &X264Options) -> Vec<X264Options> {
    let mut out = Vec::new();
    // Most common mobile PPS configurations first.
    // refs=2, chroma_qp_offset=-1 → matches many Qualcomm/MediaTek phones (e.g. LG M250).
    // refs=1, chroma_qp_offset=0  → matches simpler embedded encoders and some Apple modes.
    for &refs in &[2u32, 1, 3] {
        for &cqo in &[-1i32, 0, -2] {
            for &cabac in &[true, false] {
                let variant = X264Options {
                    refs,
                    chroma_qp_offset: cqo,
                    cabac,
                    transform_8x8: cabac, // High profile needs 8x8dct; Baseline/Main do not
                    ..base.clone()
                };
                // Deduplicate.
                if !out.iter().any(|o: &X264Options| {
                    o.cabac == variant.cabac
                        && o.refs == variant.refs
                        && o.chroma_qp_offset == variant.chroma_qp_offset
                        && o.transform_8x8 == variant.transform_8x8
                }) {
                    out.push(variant);
                }
            }
        }
    }
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
        assert!(
            c.contains(&26),
            "stitchable/ABR default must remain reachable"
        );
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
