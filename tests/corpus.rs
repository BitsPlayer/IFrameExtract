//! Smoke tests over a handful of representative corpus files, so the crate is self-checking
//! without needing the full 264-file harness in `scripts/evaluate.py`.
//!
//! Skipped automatically when the corpus is not present (it is stored via Git LFS).

use iframe_extract::extract::{Options, extract};
use std::path::{Path, PathBuf};

fn corpus() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.join("test-data-corpus");
    dir.join("manifest.json").exists().then_some(dir)
}

fn run(rel: &str, width: u32, height: u32) -> iframe_extract::extract::Extraction {
    let dir = corpus().expect("corpus present");
    let data = std::fs::read(dir.join(rel)).expect("read input");
    extract(
        &data,
        rel,
        &Options {
            width,
            height,
            include_non_idr: false,
            include_partial: false,
            with_audio: false,
            all_frames: false,
        },
    )
    .expect("extraction succeeds")
}

#[test]
fn intact_file_yields_every_keyframe() {
    if corpus().is_none() {
        return;
    }
    let r = run("good/spring_1280x536_24fps_crf23.mp4", 1280, 536);
    assert_eq!(r.frames.len(), 35, "keyframe count from manifest");
    assert_eq!(r.report.access_units, 1440, "frame count from manifest");
    assert!(r.report.truncation_offset.is_none(), "intact file must not look truncated");
    assert_eq!(r.report.pic_init_qp, 23, "crf 23 pins pic_init_qp exactly");
    assert!(r.frames.iter().all(|(_, f)| f.width == 1280 && f.height == 536));
}

#[test]
fn two_pass_file_recovers_pic_init_qp_by_search() {
    if corpus().is_none() {
        return;
    }
    // Two-pass encodes are the case the SEI cannot pin down, so this exercises the search.
    let r = run("good/spring_1280x536_24fps_vbr2pass_2500k.mp4", 1280, 536);
    assert_eq!(r.frames.len(), 35);
    assert_eq!(r.report.pic_init_qp, 21);
    assert!(r.report.qp_search_attempts > 1, "should have taken more than one candidate");
}

#[test]
fn in_band_parameter_sets_are_preferred() {
    if corpus().is_none() {
        return;
    }
    use iframe_extract::sps::ParamSource;
    let r = run("good/spring_1280x536_24fps_crf23_repeatheaders.mp4", 1280, 536);
    assert_eq!(r.report.param_source, ParamSource::InBand);
    assert_eq!(r.frames.len(), 35);
}

#[test]
fn truncated_file_stops_at_the_cut() {
    if corpus().is_none() {
        return;
    }
    // Ground truth from manifest.json: keyframes_intact = 10, truncate_offset = 2954338.
    let r = run("truncated/spring_1280x536_24fps_crf23__cut25_none.mp4", 1280, 536);
    assert_eq!(r.frames.len(), 10);
    assert!(r.report.truncation_offset.is_some());
    assert!(
        r.report.trusted_data_end <= 2_954_338,
        "must not trust data past the cut, trusted up to {}",
        r.report.trusted_data_end
    );
}

#[test]
fn padding_that_mimics_structure_is_not_mistaken_for_data() {
    if corpus().is_none() {
        return;
    }
    // mp4tail padding appends a genuine moov describing a different video; keyintinf has a single
    // IDR in the whole file, so a carver that wanders into the padding shows up immediately.
    let r = run(
        "truncated/spring_1280x536_24fps_crf23_keyintinf__cut85_mp4tail.mp4",
        1280,
        536,
    );
    assert_eq!(r.frames.len(), 1, "exactly one IDR exists in this encode");
}

#[test]
fn multi_slice_pictures_group_into_one_frame() {
    if corpus().is_none() {
        return;
    }
    let r = run("good/spring_1280x536_24fps_crf23_slices8.mp4", 1280, 536);
    assert_eq!(r.frames.len(), 35, "8 slices per picture must not become 8 frames");
    assert!(r.frames.iter().all(|(rec, _)| rec.slice_count == 8));
}
