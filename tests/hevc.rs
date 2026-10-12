//! H.265 and QuickTime end to end, on files generated with ffmpeg so the suite needs no corpus.
//!
//! libx265 through ffmpeg keeps VPS/SPS/PPS and its options SEI in `hvcC`, so every file here has
//! to be recovered from synthesised parameter sets - the case a truncated file is always in.
//! Skipped when ffmpeg or libx265 is missing.

use iframe_extract::codec::Codec;
use iframe_extract::extract::{Extraction, Options, extract};
use iframe_extract::hevc::params::{self, HevcParams};
use iframe_extract::hevc::validate::{Scratch, validate_slice};
use iframe_extract::sps::ParamSource;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workdir() -> PathBuf {
    let dir = std::env::temp_dir().join("iframe-extract-hevc");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

/// Two seconds of test pattern with a keyframe every half second, optionally with AAC interleaved.
fn encode(name: &str, size: &str, codec_args: &[&str], audio: bool) -> Option<PathBuf> {
    let path = workdir().join(name);
    let src = format!("testsrc2=size={size}:rate=24:duration=2");
    let mut args = vec!["-f", "lavfi", "-i", &src];
    if audio {
        args.extend(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"]);
    }
    args.extend(["-pix_fmt", "yuv420p"]);
    args.extend(codec_args);
    if audio {
        args.extend(["-c:a", "aac", "-shortest"]);
    } else {
        args.push("-an");
    }
    args.extend(["-y", path.to_str()?]);
    ffmpeg(&args).then_some(path)
}

fn x265(name: &str, size: &str, params: &str, audio: bool) -> Option<PathBuf> {
    let p = format!("log-level=error:keyint=12:{params}");
    encode(name, size, &["-c:v", "libx265", "-x265-params", &p], audio)
}

/// ffmpeg's keyframes from the intact file, as one planar 4:2:0 blob.
fn reference_keyframes(path: &Path) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-skip_frame", "nokey", "-i"])
        .arg(path)
        .args(["-fps_mode", "passthrough", "-pix_fmt", "yuv420p", "-f", "rawvideo", "-"])
        .output()
        .expect("ffmpeg runs");
    out.stdout
}

fn run(data: &[u8], width: u32, height: u32, with_audio: bool) -> Extraction {
    extract(
        data,
        "generated",
        &Options {
            width,
            height,
            include_non_idr: false,
            include_partial: false,
            with_audio,
            all_frames: false,
            codec: None,
        },
    )
    .expect("extraction succeeds")
}

fn ours(r: &Extraction) -> Vec<u8> {
    r.frames.iter().flat_map(|(_, f)| f.yuv.clone()).collect()
}

#[test]
fn x265_keyframes_match_ffmpeg_byte_for_byte() {
    let Some(path) = x265("default.mp4", "640x360", "", false) else {
        eprintln!("skipping: ffmpeg/libx265 not available");
        return;
    };
    let r = run(&std::fs::read(&path).unwrap(), 640, 360, false);
    assert_eq!(r.report.codec, Codec::H265);
    assert_eq!(r.report.param_source, ParamSource::Synthesized);
    assert!(r.report.truncation_offset.is_none());
    assert_eq!(r.frames.len(), 4, "IDR plus three CRAs");
    assert_eq!(ours(&r), reference_keyframes(&path));
}

#[test]
fn truncated_quicktime_with_audio_stops_at_the_cut() {
    let Some(path) = x265("audio.mov", "640x360", "", true) else {
        eprintln!("skipping: ffmpeg/libx265 not available");
        return;
    };
    let intact = std::fs::read(&path).unwrap();
    let mdat = intact.windows(4).position(|w| w == b"mdat").unwrap() - 4;
    let size = u32::from_be_bytes(intact[mdat..mdat + 4].try_into().unwrap()) as usize;
    let cut = mdat + size * 6 / 10;
    let mut truncated = intact[..cut].to_vec();
    // Pseudo-random padding after the cut, the kind most likely to fake a successor. Kept shorter
    // than the lost remainder so the file still ends inside the box `mdat` declares.
    let padding = (mdat + size - cut) / 2;
    let mut x = 0x2545_f491_u32;
    truncated.extend((0..padding).map(|_| {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x as u8
    }));

    let r = run(&truncated, 640, 360, true);
    assert_eq!(r.report.container.kind, "mov");
    assert!(r.report.truncation_offset.is_some());
    assert!(r.report.trusted_data_end <= cut as u64, "no byte past the cut is trusted");
    assert!(!r.frames.is_empty());
    let reference = reference_keyframes(&path);
    let got = ours(&r);
    assert_eq!(got, reference[..got.len()], "every frame equals its intact twin");
}

#[test]
fn unusual_encoder_settings_are_found_by_the_sweep() {
    let Some(path) = x265(
        "custom.mp4",
        "640x360",
        "ctu=32:no-wpp=1:tu-intra-depth=3:no-signhide=1:qg-size=16:no-sao=1",
        false,
    ) else {
        eprintln!("skipping: ffmpeg/libx265 not available");
        return;
    };
    let r = run(&std::fs::read(&path).unwrap(), 640, 360, false);
    let p = r.report.hevc_params.as_ref().expect("synthesised parameters reported");
    assert_eq!(p.log2_ctb, 5);
    assert!(!p.entropy_coding_sync && !p.sao && !p.sign_data_hiding);
    assert_eq!(ours(&r), reference_keyframes(&path));
}

#[test]
fn codec_is_detected_from_the_bitstream() {
    let (Some(h264), Some(h265)) = (
        encode("h264.mov", "320x240", &["-c:v", "libx264", "-g", "12"], false),
        x265("small.mov", "320x240", "", false),
    ) else {
        eprintln!("skipping: ffmpeg/libx264/libx265 not available");
        return;
    };
    assert_eq!(run(&std::fs::read(h264).unwrap(), 320, 240, false).report.codec, Codec::H264);
    assert_eq!(run(&std::fs::read(h265).unwrap(), 320, 240, false).report.codec, Codec::H265);
}

/// Splits an Annex B stream into NAL payloads.
fn annexb_units(d: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let mut e = starts.get(k + 1).map_or(d.len(), |&n| n - 3);
            while e > s && d[e - 1] == 0 {
                e -= 1;
            }
            &d[s..e]
        })
        .collect()
}

/// The validator is the whole basis for trusting a synthesised parameter set, so it must accept
/// the encoder's real one and reject each field changed on its own.
#[test]
fn validator_accepts_the_real_parameters_and_rejects_perturbations() {
    let raw = workdir().join("raw.265");
    let src = "testsrc2=size=640x360:rate=24:duration=2";
    if !ffmpeg(&[
        "-f", "lavfi", "-i", src, "-pix_fmt", "yuv420p", "-c:v", "libx265", "-x265-params",
        "log-level=error:keyint=12:tu-intra-depth=2", "-f", "hevc", "-y", raw.to_str().unwrap(),
    ]) {
        eprintln!("skipping: ffmpeg/libx265 not available");
        return;
    }
    let data = std::fs::read(&raw).unwrap();
    let units = annexb_units(&data);
    let of_type = |t: u8| units.iter().find(|u| (u[0] >> 1) & 0x3f == t).copied().unwrap();
    let truth = params::parse(of_type(33), of_type(34)).expect("x265 parameter sets are modelled");
    let keyframes: Vec<&[u8]> = units
        .iter()
        .filter(|u| (16..=21).contains(&((u[0] >> 1) & 0x3f)))
        .copied()
        .collect();
    assert!(keyframes.len() >= 2);

    let mut scratch = Scratch::default();
    for k in &keyframes {
        assert!(validate_slice(&truth, k, &mut scratch).is_ok());
    }

    type Perturb = fn(&mut HevcParams);
    let perturbations: Vec<(&str, Perturb)> = vec![
        ("init_qp + 1", |p| p.init_qp += 1),
        ("init_qp - 1", |p| p.init_qp -= 1),
        ("sao", |p| p.sao = !p.sao),
        ("sign hiding", |p| p.sign_data_hiding = !p.sign_data_hiding),
        ("wpp", |p| p.entropy_coding_sync = !p.entropy_coding_sync),
        ("transform skip", |p| p.transform_skip = !p.transform_skip),
        ("cu_qp_delta off", |p| p.cu_qp_delta_depth = None),
        ("intra depth", |p| p.max_transform_depth_intra -= 1),
        ("ctb size", |p| p.log2_ctb -= 1),
        ("max tb size", |p| p.log2_max_tb -= 1),
    ];
    for (name, f) in perturbations {
        let mut wrong = truth.clone();
        f(&mut wrong);
        let accepted = keyframes
            .iter()
            .filter(|k| validate_slice(&wrong, k, &mut scratch).is_ok())
            .count();
        assert_eq!(accepted, 0, "{name} must not validate");
    }
}
