//! Regression: x264 `--stitchable` CRF does not put `round(crf)` in the PPS (it leaves 26).
//! The Spring corpus is all plain CRF, so this used to fail despite being ordinary x264.

use iframe_extract::extract::{Options, extract};
use std::path::PathBuf;
use std::process::Command;

fn generate_stitchable() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("iframe-extract-stitchable");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("stitch_crf23.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=24:duration=1",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-crf",
            "23",
            "-x264-params",
            "stitchable=1:ref=5:bframes=3:b-pyramid=2:keyint=48:scenecut=0:chroma-qp-offset=0",
            "-an",
            "-y",
        ])
        .arg(&path)
        .status()
        .ok()?;
    status.success().then_some(path)
}

#[test]
fn stitchable_crf_recovers_the_idr() {
    let Some(path) = generate_stitchable() else {
        eprintln!("skipping: ffmpeg/libx264 not available");
        return;
    };
    let data = std::fs::read(&path).expect("read generated mp4");
    let result = extract(
        &data,
        "stitch_crf23.mp4",
        &Options {
            width: 1280,
            height: 720,
            include_non_idr: false,
            include_partial: false,
            with_audio: false,
            all_frames: false,
        },
    )
    .expect("extraction succeeds on stitchable CRF");
    assert!(
        !result.frames.is_empty(),
        "expected at least the leading IDR"
    );
    assert_eq!(
        result.report.pic_init_qp, 26,
        "stitchable PPS keeps pic_init_qp at 26, not round(crf)=23"
    );
    assert!(
        result.report.qp_search_attempts > 1,
        "must not have treated CRF as pinning pic_init_qp"
    );
    assert!(result.frames.iter().all(|(_, f)| f.width == 1280 && f.height == 720));
}
