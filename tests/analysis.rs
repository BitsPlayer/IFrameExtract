//! `analyze` + `decode` must agree with the one-shot `extract_with`. Uses a generated x264 file and skips
//! when ffmpeg/libx264 is unavailable.

use iframe_extract::extract::{
    CarveOptions, Options, Selection, analyze, decode, extract,
};
use iframe_extract::report::Silent;
use std::path::PathBuf;
use std::process::Command;

const FILE: &str = "analysis_crf23.mp4";

fn generate() -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join("iframe-extract-analysis");
    let _ = std::fs::create_dir_all(&dir);
    let path: PathBuf = dir.join(FILE);
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
            "testsrc2=size=1280x720:rate=24:duration=2", "-pix_fmt", "yuv420p", "-c:v",
            "libx264", "-crf", "23", "-x264-params", "keyint=12:scenecut=0:bframes=0", "-an", "-y",
        ])
        .arg(&path)
        .status()
        .ok()?;
    status.success().then(|| std::fs::read(&path).ok()).flatten()
}

fn carve() -> CarveOptions {
    CarveOptions {
        width: 1280,
        height: 720,
        with_audio: false,
    }
}

#[test]
fn analyze_then_decode_matches_extract() {
    let Some(data) = generate() else {
        eprintln!("skipping: ffmpeg/libx264 not available");
        return;
    };
    let expected = extract(
        &data,
        FILE,
        &Options {
            width: 1280,
            height: 720,
            include_non_idr: false,
            include_partial: false,
            with_audio: false,
            all_frames: false,
        },
    )
    .unwrap();

    let analysis = analyze(&data, &carve(), &mut Silent).unwrap();
    let got = decode(
        &data,
        &analysis,
        &Selection::Filtered {
            include_non_idr: false,
            include_partial: false,
            all_frames: false,
        },
        FILE,
        &mut Silent,
    )
    .unwrap();

    assert_eq!(got.frames.len(), expected.frames.len());
    let offsets = |e: &iframe_extract::extract::Extraction| {
        e.report.frames.iter().map(|r| (r.index, r.offset)).collect::<Vec<_>>()
    };
    assert_eq!(offsets(&got), offsets(&expected));
    assert_eq!(got.annexb, expected.annexb);
}

#[test]
fn single_non_intra_decodes_one_frame() {
    let Some(data) = generate() else {
        eprintln!("skipping: ffmpeg/libx264 not available");
        return;
    };
    let analysis = analyze(&data, &carve(), &mut Silent).unwrap();
    let i = analysis
        .access_units
        .iter()
        .position(|au| !au.is_intra && au.complete)
        .expect("a complete P/B access unit");
    let r = decode(&data, &analysis, &Selection::Single(i), FILE, &mut Silent).unwrap();
    assert_eq!(r.report.frames.len(), 1);
    assert_eq!(r.report.frames[0].offset, analysis.access_units[i].offset);
    assert_eq!(r.frames.len(), 1, "the requested picture decodes");
}
