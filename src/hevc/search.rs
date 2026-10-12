//! Finding a parameter set for a stream whose VPS/SPS/PPS died with `moov`.
//!
//! Candidates are proven, not scored: one is accepted only when the validator parses every probe
//! slice to its stop bit. The search is ordered so that common encoders are found at once - x265's
//! presets first, each with its `init_qp` - and falls back to a staged sweep for anything else.
//! The sweep first finds the slice-header layouts the probes admit, which is cheap and prunes
//! hard, then tries the slice-data fields within each in order of how common they are.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::params::HevcParams;
use super::slice;
use super::validate::{Outcome, Scratch, validate_slice};
use crate::bits::unescape_rbsp;
use crate::report::Reporter;

pub struct Probe<'a> {
    pub payload: &'a [u8],
    /// Unescaped prefix, enough to parse any slice header we can represent.
    header_rbsp: Vec<u8>,
}

impl<'a> Probe<'a> {
    pub fn new(payload: &'a [u8]) -> Self {
        Self {
            payload,
            header_rbsp: unescape_rbsp(&payload[..payload.len().min(1024)]),
        }
    }

    fn header(&self, p: &HevcParams) -> Option<slice::SliceHeader> {
        slice::parse(p, super::nal_type(self.payload), &self.header_rbsp)
    }

    fn header_ok(&self, p: &HevcParams) -> bool {
        self.header(p).is_some()
    }
}

/// What the stream itself suggests about the encoder.
#[derive(Default, Clone)]
pub struct Hints {
    /// From an in-band x265 options SEI.
    pub x265: Option<HevcParams>,
    /// Dolby Vision RPUs are present, so the stream is almost certainly 10-bit.
    pub high_bit_depth: bool,
}

pub struct Found {
    pub params: HevcParams,
    pub attempts: usize,
    pub label: String,
}

/// Accepts `p` only if every probe slice parses to its stop bit (dependent segments, which the
/// validator does not cover, neither confirm nor reject).
pub fn validates(p: &HevcParams, probes: &[Probe], scratch: &mut Scratch) -> bool {
    if !p.is_consistent() || !probes.iter().all(|pr| pr.header_ok(p)) {
        return false;
    }
    let mut confirmed = false;
    for pr in probes {
        match validate_slice(p, pr.payload, scratch) {
            Ok(Outcome::Valid) => confirmed = true,
            Ok(Outcome::Skipped) => {}
            Err(_) => return false,
        }
    }
    confirmed
}

/// `init_qp` candidates, nearest the PPS default first. x265 always writes 26; hardware encoders
/// tend to put their configured QP here instead.
fn qp_order() -> Vec<i32> {
    let mut v = vec![26];
    for d in 1..=26 {
        if 26 + d <= 51 {
            v.push(26 + d);
        }
        v.push(26 - d);
    }
    v
}

/// Encoders whose parameter sets have been observed: x265's presets, which differ in exactly the
/// fields that matter to parsing, and Intel's VAAPI/QSV hardware encoder.
fn known_profiles(width: u32, height: u32, hints: &Hints) -> Vec<(String, HevcParams)> {
    let mut out = Vec::new();
    if let Some(p) = &hints.x265 {
        out.push(("x265 (options SEI)".to_string(), p.clone()));
    }
    let medium = HevcParams::x265_default(width, height);
    out.push(("x265 medium".into(), medium.clone()));
    out.push((
        "x265 slower".into(),
        HevcParams {
            max_transform_depth_intra: 2,
            max_transform_depth_inter: 2,
            amp: true,
            weighted_bipred: true,
            ..medium.clone()
        },
    ));
    out.push((
        "x265 superfast".into(),
        HevcParams {
            log2_ctb: 5,
            sao: false,
            cu_qp_delta_depth: Some(0),
            ..medium.clone()
        },
    ));
    out.push((
        "x265 ultrafast".into(),
        HevcParams {
            log2_ctb: 5,
            log2_min_cb: 4,
            coded_align_log2: 4,
            sao: false,
            sign_data_hiding: false,
            cu_qp_delta_depth: Some(0),
            weighted_pred: false,
            ..medium.clone()
        },
    ));
    out.push((
        "x265 no-wpp".into(),
        HevcParams {
            entropy_coding_sync: false,
            ..medium.clone()
        },
    ));
    // Intel iHD driver, stable across rate control, B-frames and bit depth apart from init_qp, and
    // cu_qp_delta, which constant-QP mode turns off.
    let intel = HevcParams {
        max_transform_depth_inter: 2,
        max_transform_depth_intra: 2,
        amp: true,
        strong_intra_smoothing: false,
        log2_max_poc_lsb: 12,
        max_dec_pic_buffering: 3,
        num_reorder_pics: 1,
        sign_data_hiding: false,
        transform_skip: true,
        cu_qp_delta_depth: Some(3),
        weighted_pred: false,
        entropy_coding_sync: false,
        ..medium
    };
    out.push(("intel vaapi/qsv".into(), intel.clone()));
    // Apple VideoToolbox, identical on iOS 13 (iPhone X) and iOS 16 (iPhone 12 mini). Keyframes use
    // PPS 0 with the default scaling matrices; the custom matrix in PPS 1 is for inter pictures.
    out.push((
        "apple videotoolbox".into(),
        HevcParams {
            log2_ctb: 5,
            max_transform_depth_inter: 1,
            max_transform_depth_intra: 0,
            scaling_list: true,
            strong_intra_smoothing: false,
            log2_max_poc_lsb: 11,
            sign_data_hiding: false,
            num_ref_idx_l0_default: 2,
            cu_qp_delta_depth: Some(2),
            weighted_pred: false,
            loop_filter_across_slices: false,
            coded_align_log2: 4,
            amp: false,
            transform_skip: false,
            entropy_coding_sync: true,
            max_dec_pic_buffering: 5,
            num_reorder_pics: 2,
            ..HevcParams::x265_default(width, height)
        },
    ));
    out.push((
        "intel vaapi/qsv cqp".into(),
        HevcParams {
            cu_qp_delta_depth: None,
            ..intel
        },
    ));
    // Each again at the other bit depth, after the preferred one: Main10 is as common as Main on
    // phones, and a few parses cannot tell the two apart.
    let (first, second) = if hints.high_bit_depth {
        (10, 8)
    } else {
        (8, 10)
    };
    let mut both: Vec<(String, HevcParams)> = out
        .iter()
        .map(|(l, p)| {
            (
                l.clone(),
                HevcParams {
                    bit_depth: first,
                    ..p.clone()
                },
            )
        })
        .collect();
    both.extend(out.into_iter().map(|(l, p)| {
        (
            format!("{l} {second}-bit"),
            HevcParams {
                bit_depth: second,
                ..p
            },
        )
    }));
    both
}

/// One choice for one field, with how unusual it is. Lower cost is tried first.
type Choice<T> = (T, u32);

/// Header-shaping fields: everything that changes how a slice segment header parses.
#[derive(Clone, Copy, Debug)]
struct HeaderShape {
    wpp: bool,
    sao: bool,
    extra_bits: u32,
    output_flag: bool,
    chroma_qp_offsets: bool,
    deblocking_override: bool,
    deblocking_disabled: bool,
    loop_filter_across: bool,
    dependent_slices: bool,
    log2_poc_lsb: u32,
    tmvp: bool,
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    log2_ctb: u32,
    log2_min_cb: u32,
    align: u32,
}

#[derive(Clone, Copy, Debug)]
struct DataShape {
    log2_max_tb: u32,
    depth_intra: u32,
    sign_hiding: bool,
    cu_qp_delta: Option<u32>,
    transform_skip: bool,
    transquant_bypass: bool,
    high_bit_depth: bool,
}

fn header_shapes(has_non_idr: bool) -> Vec<(HeaderShape, u32)> {
    let bools = |first: bool, cost: u32| -> [Choice<bool>; 2] { [(first, 0), (!first, cost)] };
    let poc: Vec<Choice<u32>> = if has_non_idr {
        std::iter::once((8, 0))
            .chain((4..=16).filter(|&b| b != 8).map(|b| (b, 1)))
            .collect()
    } else {
        // An IDR header carries no POC: any value parses alike, so do not multiply by them.
        vec![(8, 0)]
    };
    let tmvp: Vec<Choice<bool>> = if has_non_idr {
        bools(true, 1).to_vec()
    } else {
        vec![(true, 0)]
    };

    let mut out = Vec::new();
    for (wpp, c0) in bools(true, 1) {
        for (sao, c1) in bools(true, 1) {
            for (extra_bits, c2) in [(0, 0), (1, 3), (2, 3)] {
                for (output_flag, c3) in bools(false, 4) {
                    for (chroma_qp_offsets, c4) in bools(false, 2) {
                        for (deblocking_override, c5) in bools(false, 2) {
                            for (loop_filter_across, c6) in bools(true, 1) {
                                // Without loop_filter_across the disable flag never reaches the
                                // header, so its two values would be the same candidate twice.
                                let disabled: &[Choice<bool>] = if loop_filter_across {
                                    &[(false, 0), (true, 3)]
                                } else {
                                    &[(false, 0)]
                                };
                                for &(deblocking_disabled, c7) in disabled {
                                    for (dependent_slices, c8) in bools(false, 4) {
                                        for &(log2_poc_lsb, c9) in &poc {
                                            for &(tmvp, c10) in &tmvp {
                                                out.push((
                                                    HeaderShape {
                                                        wpp,
                                                        sao,
                                                        extra_bits,
                                                        output_flag,
                                                        chroma_qp_offsets,
                                                        deblocking_override,
                                                        deblocking_disabled,
                                                        loop_filter_across,
                                                        dependent_slices,
                                                        log2_poc_lsb,
                                                        tmvp,
                                                    },
                                                    c0 + c1
                                                        + c2
                                                        + c3
                                                        + c4
                                                        + c5
                                                        + c6
                                                        + c7
                                                        + c8
                                                        + c9
                                                        + c10,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

fn geometries(width: u32, height: u32) -> Vec<(Geometry, u32)> {
    // Keyed by what actually reaches the bitstream: CTB and CB size, and the coded dimensions.
    type Key = (u32, u32, u32, u32);
    let mut out: Vec<(Geometry, u32, Key)> = Vec::new();
    for (log2_ctb, c0) in [(6, 0), (5, 1), (4, 2)] {
        for (log2_min_cb, c1) in [(3, 0), (4, 1)] {
            if log2_min_cb > log2_ctb {
                continue;
            }
            for (align, c2) in [(log2_min_cb, 0), (4, 1), (log2_ctb, 1)] {
                let g = Geometry {
                    log2_ctb,
                    log2_min_cb,
                    align,
                };
                let p = apply(
                    &HevcParams::x265_default(width, height),
                    None,
                    Some(&g),
                    None,
                );
                let key = (log2_ctb, log2_min_cb, p.coded_width(), p.coded_height());
                if !out.iter().any(|(_, _, k)| *k == key) {
                    out.push((g, c0 + c1 + c2, key));
                }
            }
        }
    }
    out.into_iter().map(|(g, c, _)| (g, c)).collect()
}

fn data_shapes(hints: &Hints) -> Vec<(DataShape, u32)> {
    let mut out = Vec::new();
    for (log2_max_tb, c0) in [(5, 0), (4, 2)] {
        for (depth_intra, c1) in [(0, 0), (1, 1), (2, 1), (3, 2), (4, 3)] {
            for (sign_hiding, c2) in [(true, 0), (false, 1)] {
                for (cu_qp_delta, c3) in [
                    (Some(1), 0),
                    (Some(0), 0),
                    (None, 1),
                    (Some(2), 1),
                    (Some(3), 1),
                ] {
                    for (transform_skip, c4) in [(false, 0), (true, 1)] {
                        for (transquant_bypass, c5) in [(false, 0), (true, 4)] {
                            for (high_bit_depth, c6) in
                                [(hints.high_bit_depth, 0), (!hints.high_bit_depth, 2)]
                            {
                                out.push((
                                    DataShape {
                                        log2_max_tb,
                                        depth_intra,
                                        sign_hiding,
                                        cu_qp_delta,
                                        transform_skip,
                                        transquant_bypass,
                                        high_bit_depth,
                                    },
                                    c0 + c1 + c2 + c3 + c4 + c5 + c6,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

fn apply(
    base: &HevcParams,
    h: Option<&HeaderShape>,
    g: Option<&Geometry>,
    d: Option<&DataShape>,
) -> HevcParams {
    let mut p = base.clone();
    if let Some(h) = h {
        p.entropy_coding_sync = h.wpp;
        p.sao = h.sao;
        p.num_extra_slice_header_bits = h.extra_bits;
        p.output_flag_present = h.output_flag;
        p.slice_chroma_qp_offsets_present = h.chroma_qp_offsets;
        p.deblocking_override_enabled = h.deblocking_override;
        p.deblocking_disabled = h.deblocking_disabled;
        p.loop_filter_across_slices = h.loop_filter_across;
        p.dependent_slice_segments = h.dependent_slices;
        p.log2_max_poc_lsb = h.log2_poc_lsb;
        p.temporal_mvp = h.tmvp;
    }
    if let Some(g) = g {
        p.log2_ctb = g.log2_ctb;
        p.log2_min_cb = g.log2_min_cb;
        p.coded_align_log2 = g.align;
    }
    if let Some(d) = d {
        p.log2_max_tb = d.log2_max_tb.min(p.log2_ctb);
        p.max_transform_depth_intra = d.depth_intra;
        p.sign_data_hiding = d.sign_hiding;
        p.cu_qp_delta_depth = d.cu_qp_delta;
        p.transform_skip = d.transform_skip;
        p.transquant_bypass = d.transquant_bypass;
        p.bit_depth = if d.high_bit_depth { 10 } else { 8 };
    }
    p
}

/// Attempts allowed before the sweep gives up.
const MAX_ATTEMPTS: usize = 4_000_000;
const TIME_BUDGET: Duration = Duration::from_secs(180);

/// Runs `check` over `candidates` in order on every core and returns the first (lowest-index)
/// candidate that passes, along with how many were tried.
fn first_passing(
    candidates: &[HevcParams],
    probes: &[Probe],
    deadline: Instant,
    progress: &AtomicUsize,
) -> (Option<usize>, usize) {
    let next = AtomicUsize::new(0);
    let best = Mutex::new(None::<usize>);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut scratch = Scratch::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= candidates.len() || best.lock().unwrap().is_some_and(|b| i > b) {
                        break;
                    }
                    if i.is_multiple_of(64) && Instant::now() > deadline {
                        break;
                    }
                    progress.fetch_add(1, Ordering::Relaxed);
                    if validates(&candidates[i], probes, &mut scratch) {
                        let mut b = best.lock().unwrap();
                        if b.is_none_or(|b| i < b) {
                            *b = Some(i);
                        }
                    }
                }
            });
        }
    });
    let best = best.into_inner().unwrap();
    // Threads race past the winner before they see it; only what precedes it was really needed.
    let tried = best.map_or(next.load(Ordering::Relaxed).min(candidates.len()), |b| {
        b + 1
    });
    (best, tried)
}

/// Searches for a parameter set every probe slice validates against.
pub fn search(
    width: u32,
    height: u32,
    probes: &[Probe],
    hints: &Hints,
    reporter: &mut dyn Reporter,
) -> Option<Found> {
    if probes.is_empty() {
        return None;
    }
    let deadline = Instant::now() + TIME_BUDGET;
    let progress = AtomicUsize::new(0);
    let mut attempts = 0usize;
    let qps = qp_order();

    // Known encoder profiles, each across every init_qp.
    let profiles = known_profiles(width, height, hints);
    let mut candidates = Vec::new();
    let mut labels = Vec::new();
    for (label, p) in &profiles {
        for &qp in &qps {
            candidates.push(HevcParams {
                init_qp: qp,
                ..p.clone()
            });
            labels.push(label.clone());
        }
    }
    reporter.progress(0, candidates.len() as u64, "known encoder profiles");
    let (hit, tried) = first_passing(&candidates, probes, deadline, &progress);
    attempts += tried;
    if let Some(i) = hit {
        return Some(Found {
            params: candidates.swap_remove(i),
            attempts,
            label: labels.swap_remove(i),
        });
    }

    // Stage A: which header layouts and geometries do the probes admit?
    let has_non_idr = probes
        .iter()
        .any(|p| !super::is_idr(super::nal_type(p.payload)));
    let base = HevcParams::x265_default(width, height);
    let geoms = geometries(width, height);
    let mut layouts: Vec<(HeaderShape, Geometry, u32)> = Vec::new();
    // Layouts that read every probe header identically - same data start, QP, SAO flags and entry
    // points - lead to the same slice-data parse, so only the most common of them is swept. They
    // differ in fields none of these headers exercise.
    let mut seen: Vec<(bool, u32, u32, u32, u32, Vec<slice::SliceHeader>)> = Vec::new();
    let mut shapes = header_shapes(has_non_idr);
    shapes.sort_by_key(|s| s.1);
    for (h, hc) in shapes {
        for &(g, gc) in &geoms {
            let p = apply(&base, Some(&h), Some(&g), None);
            let Some(headers) = probes
                .iter()
                .map(|pr| pr.header(&p))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let key = (
                h.wpp,
                g.log2_ctb,
                g.log2_min_cb,
                p.coded_width(),
                p.coded_height(),
                headers,
            );
            if !seen.contains(&key) {
                seen.push(key);
                layouts.push((h, g, hc + gc));
            }
        }
    }
    layouts.sort_by_key(|l| l.2);
    reporter.note(&format!(
        "no known profile matched; {} slice header layout(s) fit, sweeping slice data fields",
        layouts.len()
    ));
    if layouts.is_empty() {
        return None;
    }

    // Stage B: slice-data fields within each layout, cheapest first, in bounded batches so the
    // ordering stays global without materialising millions of candidates at once.
    let datas = data_shapes(hints);
    let mut order: Vec<(u32, u32, u32, u32)> = Vec::new(); // (cost, layout, data, qp index)
    for (li, l) in layouts.iter().enumerate() {
        for (di, (_, dc)) in datas.iter().enumerate() {
            for qi in 0..qps.len() {
                let qc = (qi as u32) / 4;
                order.push((l.2 + dc + qc, li as u32, di as u32, qi as u32));
            }
        }
        if order.len() > MAX_ATTEMPTS {
            break;
        }
    }
    order.sort_unstable();
    order.truncate(MAX_ATTEMPTS);

    let total = order.len() as u64;
    for chunk in order.chunks(8192) {
        if Instant::now() > deadline {
            reporter.note("parameter search ran out of time");
            break;
        }
        let candidates: Vec<HevcParams> = chunk
            .iter()
            .map(|&(_, li, di, qi)| {
                let (h, g, _) = &layouts[li as usize];
                let mut p = apply(&base, Some(h), Some(g), Some(&datas[di as usize].0));
                p.init_qp = qps[qi as usize];
                p
            })
            .collect();
        let (hit, tried) = first_passing(&candidates, probes, deadline, &progress);
        attempts += tried;
        reporter.progress(
            progress.load(Ordering::Relaxed) as u64,
            total,
            &format!("{attempts} candidates tried"),
        );
        if let Some(i) = hit {
            let (params, family) = snap_to_family(&candidates[i], &profiles, has_non_idr);
            return Some(Found {
                params,
                attempts,
                label: format!("generic sweep, nearest {family}"),
            });
        }
    }
    None
}

/// Fields the validator proves, compared to find which encoder family a sweep result belongs to.
fn observable(p: &HevcParams) -> [u32; 10] {
    [
        p.entropy_coding_sync as u32,
        p.sao as u32,
        p.log2_ctb,
        p.log2_min_cb,
        p.log2_max_tb,
        p.max_transform_depth_intra,
        p.sign_data_hiding as u32,
        p.cu_qp_delta_depth.map_or(99, |d| d),
        p.transform_skip as u32,
        p.transquant_bypass as u32,
    ]
}

/// Copies the fields no intra parse can see from the closest known encoder. Strong intra
/// smoothing and chroma QP offsets change the decoded picture without changing a single parsed
/// bin, and inter-only fields matter once P/B pictures are decoded; for those the encoder family
/// is the best evidence there is.
fn snap_to_family(
    found: &HevcParams,
    profiles: &[(String, HevcParams)],
    has_non_idr: bool,
) -> (HevcParams, String) {
    let o = observable(found);
    let Some((label, nearest)) = profiles
        .iter()
        .min_by_key(|(_, p)| observable(p).iter().zip(&o).filter(|(a, b)| a != b).count())
    else {
        return (found.clone(), "none".into());
    };
    let mut p = found.clone();
    p.strong_intra_smoothing = nearest.strong_intra_smoothing;
    p.scaling_list = nearest.scaling_list;
    p.cb_qp_offset = nearest.cb_qp_offset;
    p.cr_qp_offset = nearest.cr_qp_offset;
    p.amp = nearest.amp;
    p.max_transform_depth_inter = nearest.max_transform_depth_inter;
    p.weighted_pred = nearest.weighted_pred;
    p.weighted_bipred = nearest.weighted_bipred;
    p.max_dec_pic_buffering = nearest.max_dec_pic_buffering;
    p.num_reorder_pics = nearest.num_reorder_pics;
    if !has_non_idr {
        // An IDR-only probe set never read a POC or TMVP flag.
        p.log2_max_poc_lsb = nearest.log2_max_poc_lsb;
        p.temporal_mvp = nearest.temporal_mvp;
    }
    (p, label.clone())
}

/// Builds a candidate from x265's option string, so a stream that kept its SEI is matched first
/// try - including the fields no parse can see, such as strong intra smoothing and chroma QP
/// offsets.
pub fn from_x265_options(opts: &str, width: u32, height: u32) -> HevcParams {
    let mut p = HevcParams::x265_default(width, height);
    let mut aq_mode = 2;
    let mut qg_size = None;
    for tok in opts.split_whitespace() {
        let (key, val) = tok.split_once('=').unwrap_or((tok, ""));
        let num = || val.parse::<u32>().ok();
        let log2 = |v: Option<u32>| {
            v.filter(|v| v.is_power_of_two())
                .map(|v| v.trailing_zeros())
        };
        match key {
            "ctu" => p.log2_ctb = log2(num()).unwrap_or(p.log2_ctb),
            "min-cu-size" => p.log2_min_cb = log2(num()).unwrap_or(p.log2_min_cb),
            "max-tu-size" => p.log2_max_tb = log2(num()).unwrap_or(p.log2_max_tb),
            "tu-intra-depth" => p.max_transform_depth_intra = num().unwrap_or(1).saturating_sub(1),
            "tu-inter-depth" => p.max_transform_depth_inter = num().unwrap_or(1).saturating_sub(1),
            "sao" => p.sao = true,
            "no-sao" => p.sao = false,
            "signhide" => p.sign_data_hiding = true,
            "no-signhide" => p.sign_data_hiding = false,
            "wpp" => p.entropy_coding_sync = true,
            "no-wpp" => p.entropy_coding_sync = false,
            "tskip" => p.transform_skip = true,
            "no-tskip" => p.transform_skip = false,
            "strong-intra-smoothing" => p.strong_intra_smoothing = true,
            "no-strong-intra-smoothing" => p.strong_intra_smoothing = false,
            "amp" => p.amp = true,
            "no-amp" => p.amp = false,
            "lossless" => p.transquant_bypass = true,
            "weightp" => p.weighted_pred = true,
            "no-weightp" => p.weighted_pred = false,
            "weightb" => p.weighted_bipred = true,
            "no-weightb" => p.weighted_bipred = false,
            "temporal-mvp" => p.temporal_mvp = true,
            "no-temporal-mvp" => p.temporal_mvp = false,
            "cbqpoffs" => p.cb_qp_offset = val.parse().unwrap_or(0),
            "crqpoffs" => p.cr_qp_offset = val.parse().unwrap_or(0),
            "aq-mode" => aq_mode = num().unwrap_or(aq_mode),
            "qg-size" => qg_size = log2(num()),
            _ => {}
        }
    }
    p.coded_align_log2 = p.log2_min_cb;
    let qg = qg_size
        .unwrap_or(p.log2_ctb)
        .clamp(p.log2_min_cb, p.log2_ctb);
    p.cu_qp_delta_depth = (aq_mode > 0).then_some(p.log2_ctb - qg);
    p
}
