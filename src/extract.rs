//! Ties the pieces together: carve, recover parameter sets, group access units, decode.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::container::find_mdat;
use crate::decode::{Frame, FrameDecoder, OpenH264Backend, to_annexb};
use crate::nal::{self, NAL_IDR, NAL_PPS, NAL_SEI, NAL_SLICE, NAL_SPS, Nal};
use crate::report::{Reporter, Silent};
use crate::sei::{X264Options, parse_x264_sei};
use crate::slice;
use crate::sps::{EncoderProfile, ParamSource, ParameterSets, synthesize, synthesize_profile};

pub struct Options {
    pub width: u32,
    pub height: u32,
    pub include_non_idr: bool,
    pub include_partial: bool,
    /// Enable two-pass audio-resync walking. Off by default; pass `--with-audio` for files that
    /// interleave an audio track in `mdat`.
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
    /// Bytes this picture occupies in the source file: every NAL's length prefix plus the payload
    /// bytes actually present. For a partial picture this is less than the container declares.
    pub source_size: u64,
    pub nal_type: u8,
    pub slice_count: usize,
    pub partial: bool,
    pub decoded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sps_profile: Option<String>,
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
    /// Runs inside `mdat` that are not part of the video chain - the interleaved chunks of another
    /// track, usually audio - which the walk stepped over.
    pub skipped_regions: usize,
    pub skipped_bytes: u64,
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

        let mut decoded = match decoder.decode_au(&au_stream) {
            Ok(frame) => frame,
            Err(_) => {
                decoder = OpenH264Backend::new(opts.include_partial)?;
                None
            }
        };

        let mut frame_sps_profile = None;
        if decoded.is_none()
            && (params.source == ParamSource::Synthesized
                || params.source == ParamSource::SynthesizedHardware)
        {
            for variant in option_variants(&x264_opts) {
                for qp in qp_candidates(&x264_opts) {
                    let candidate = synthesize(opts.width, opts.height, &variant, qp);
                    let mut probe_units: Vec<&[u8]> = vec![&candidate.sps, &candidate.pps];
                    probe_units.extend_from_slice(&payloads);
                    let mut probe_dec = OpenH264Backend::new(false)?;
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
                    ..x264_opts.clone()
                };
                let hw_opts_variants = hardware_opts_variants(&hw_base);
                'hw_retry: for profile in &hw_profiles {
                    for hw_opts in &hw_opts_variants {
                        for &qp in &hw_qps {
                            let candidate =
                                synthesize_profile(opts.width, opts.height, hw_opts, qp, *profile);
                            let mut probe_units: Vec<&[u8]> = vec![&candidate.sps, &candidate.pps];
                            probe_units.extend_from_slice(&payloads);
                            let mut probe_dec = OpenH264Backend::new(false)?;
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
        let source_size: u64 = au
            .nal_indices
            .iter()
            .map(|&i| (walk.nal_length_size + walk.nals[i].available_len) as u64)
            .sum();
        let record = FrameRecord {
            index: emitted,
            offset: au.offset,
            source_size,
            nal_type: if au.is_idr { NAL_IDR } else { NAL_SLICE },
            slice_count: au.nal_indices.len(),
            partial: !au.complete,
            decoded: decoded.is_some(),
            sps_profile: frame_sps_profile,
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
        skipped_regions: walk.skipped_regions,
        skipped_bytes: walk.skipped_bytes,
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
            if decode_probe(&params, &probe_aus, nals, data)? {
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
        let hw_base = X264Options { bframes: 0, b_pyramid: 0, ..x264_opts.clone() };
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
                    if decode_probe(&params, &probe_aus, nals, data)? {
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
) -> Result<bool> {
    // Single decoder instance shared across all probe frames: this also tests that the decoder
    // stays coherent across consecutive IDRs, catching any state-carryover issues.
    let mut decoder = OpenH264Backend::new(false)?;
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
