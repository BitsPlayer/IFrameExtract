use super::progress::PacmanBar;
use iframe_extract::codec::Codec;
use iframe_extract::decode::ColorMatrix;
use iframe_extract::extract;
use iframe_extract::report::{self, Reporter};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Format {
    Png,
    /// Planar 4:2:0, for byte-exact comparison against a reference decoder.
    Yuv,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum CodecArg {
    /// Detect from the bitstream.
    Auto,
    H264,
    /// H.265 / HEVC.
    H265,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Matrix {
    Bt709,
    Bt601,
}

/// Recovers I-frames from MP4/MOV files (H.264 or H.265) by carving the bitstream, without ever
/// reading `moov`.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    input: PathBuf,

    /// Frame size, e.g. 1280x536. Required: resolution cannot be recovered without an SPS.
    #[arg(short, long)]
    size: String,

    #[arg(short, long, default_value = "out")]
    output: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Format::Png)]
    format: Format,

    /// Video codec. Nothing outside `moov` names it, so by default it is detected from the
    /// bitstream.
    #[arg(long, value_enum, default_value_t = CodecArg::Auto)]
    codec: CodecArg,

    /// Colour matrix for PNG output. Lives in the SPS VUI, which is lost with `moov`, so it
    /// cannot be recovered from a truncated file.
    #[arg(long, value_enum, default_value_t = Matrix::Bt709)]
    matrix: Matrix,

    /// Also emit intra slices that are not IDR.
    #[arg(long)]
    include_non_idr: bool,

    /// Also attempt the frame straddling the truncation point, decoded with error concealment.
    #[arg(long)]
    include_partial: bool,

    /// Enable audio-aware walking for files that interleave an audio track in mdat. Adds a
    /// second resync pass that steps over non-video regions. Not needed for video-only files.
    #[arg(long)]
    with_audio: bool,

    /// Also recover and decode P-frames (inter-coded pictures), recovering the full video stream.
    #[arg(long)]
    all_frames: bool,

    /// Also remux the recovered stream into a standard MP4 video (<output>/recovered.mp4). Requires ffmpeg.
    #[arg(long)]
    mp4: bool,

    /// Write a JSON report.
    #[arg(long)]
    json: Option<PathBuf>,

    /// Write the recovered Annex B elementary stream to a custom path (defaults to
    /// <output>/recovered.264, or recovered.265 for H.265).
    #[arg(long)]
    emit_annexb: Option<PathBuf>,

    /// Do not write the recovered Annex B stream file.
    #[arg(long)]
    no_annexb: bool,

    /// Print nothing on success.
    #[arg(short, long)]
    quiet: bool,
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .with_context(|| format!("expected WIDTHxHEIGHT, got {s:?}"))?;
    Ok((w.trim().parse()?, h.trim().parse()?))
}

fn read_input(path: &Path, reporter: &mut dyn Reporter) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let total = file.metadata().map(|m| m.len()).unwrap_or(0);
    reporter.phase("reading file");
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut data = Vec::new();
    if total > 0 {
        data.reserve(total as usize);
    }
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        reporter.progress(
            data.len() as u64,
            total.max(data.len() as u64),
            &report::human_bytes(data.len() as u64),
        );
    }
    reporter.finish_phase(&report::human_bytes(data.len() as u64));
    Ok(data)
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    let (width, height) = parse_size(&args.size)?;

    let mut reporter = PacmanBar::new(!args.quiet);
    if !args.quiet {
        let size = std::fs::metadata(&args.input)
            .map(|m| report::human_bytes(m.len()))
            .unwrap_or_else(|_| "?".into());
        reporter.header(&format!(
            "extracting {} ({size}, {width}x{height})",
            args.input.display()
        ));
    }

    let data = read_input(&args.input, &mut reporter)?;

    let opts = extract::Options {
        width,
        height,
        include_non_idr: args.include_non_idr,
        include_partial: args.include_partial,
        with_audio: args.with_audio,
        all_frames: args.all_frames,
        codec: match args.codec {
            CodecArg::Auto => None,
            CodecArg::H264 => Some(Codec::H264),
            CodecArg::H265 => Some(Codec::H265),
        },
    };

    let name = args.input.display().to_string();
    let result = extract::extract_with(&data, &name, &opts, &mut reporter)?;

    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("creating {}", args.output.display()))?;

    let ext = match args.format {
        Format::Png => "PNG",
        Format::Yuv => "YUV",
    };
    reporter.phase(&format!("writing {ext}"));
    let nframes = result.frames.len();
    for (i, (record, frame)) in result.frames.iter().enumerate() {
        let stem = format!(
            "frame_{:05}_off_{:010}{}",
            record.index,
            record.offset,
            if record.partial { "_partial" } else { "" }
        );
        match args.format {
            Format::Png => {
                let path = args.output.join(format!("{stem}.png"));
                let rgb = frame.to_rgb(match args.matrix {
                    Matrix::Bt709 => ColorMatrix::Bt709,
                    Matrix::Bt601 => ColorMatrix::Bt601,
                });
                let img = image::RgbImage::from_raw(frame.width as u32, frame.height as u32, rgb)
                    .context("frame dimensions did not match buffer")?;
                img.save(&path)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            Format::Yuv => {
                let path = args.output.join(format!("{stem}.yuv"));
                std::fs::write(&path, &frame.yuv)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
        }
        reporter.progress(
            (i + 1) as u64,
            nframes as u64,
            &format!("{}/{nframes}  {stem}", i + 1),
        );
    }
    reporter.finish_phase(&format!("{nframes} files in {}", args.output.display()));

    let annexb_path = if args.no_annexb {
        None
    } else {
        Some(
            args.emit_annexb
                .clone()
                .unwrap_or_else(|| {
                    args.output.join(format!(
                        "recovered.{}",
                        result.report.codec.annexb_extension()
                    ))
                }),
        )
    };
    if let Some(path) = &annexb_path {
        reporter.note(&format!("writing Annex B stream to {}", path.display()));
        std::fs::write(path, &result.annexb)
            .with_context(|| format!("writing {}", path.display()))?;

        if args.mp4 || args.all_frames {
            let mp4_path = args.output.join("recovered.mp4");
            let status = std::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-v", "error",
                    "-r", "30",
                    "-f", result.report.codec.ffmpeg_format(),
                    "-i", &path.to_string_lossy(),
                    "-c", "copy",
                    &mp4_path.to_string_lossy(),
                ])
                .status();
            if let Ok(s) = status && s.success() {
                reporter.note(&format!("wrote MP4 video to {}", mp4_path.display()));
            }
        }
    }

    if let Some(path) = &args.json {
        reporter.note(&format!("writing JSON report to {}", path.display()));
        let json = serde_json::to_string_pretty(&result.report)?;
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    }

    if !args.quiet {
        let r = &result.report;
        reporter.summary(&format!(
            "{} in {}: {} NALs, {} access units, {} frames written ({:?} params, qp {}, {} attempt(s))",
            r.codec.label(),
            r.container.kind,
            r.nal_count,
            r.access_units,
            result.frames.len(),
            r.param_source,
            r.pic_init_qp,
            r.qp_search_attempts,
        ));
        match r.truncation_offset {
            Some(off) => reporter.summary(&format!(
                "chain broke at offset {off} ({:.2}% of file)",
                off as f64 / data.len().max(1) as f64 * 100.0
            )),
            None => reporter.summary("chain ran cleanly to the end of mdat"),
        }
    }

    if result.frames.is_empty() {
        bail!("no frames recovered");
    }

    Ok(())
}
