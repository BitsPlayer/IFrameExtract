use iframe_extract::decode::ColorMatrix;
use iframe_extract::extract;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Format {
    Png,
    /// Planar 4:2:0, for byte-exact comparison against a reference decoder.
    Yuv,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Matrix {
    Bt709,
    Bt601,
}

/// Recovers I-frames from MP4/H.264 files by carving the bitstream, without ever reading `moov`.
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

    /// Colour matrix for PNG output. Lives in the SPS VUI, which is lost with `moov`, so it
    /// cannot be recovered from a truncated file.
    #[arg(long, value_enum, default_value_t = Matrix::Bt709)]
    matrix: Matrix,

    /// Also emit intra slices that are not IDR.
    #[arg(long)]
    include_non_idr: bool,

    /// Also emit the frame straddling the truncation point, decoded with error concealment.
    #[arg(long)]
    include_partial: bool,

    /// Write a JSON report.
    #[arg(long)]
    json: Option<PathBuf>,

    /// Write the recovered Annex B elementary stream, for cross-checking with other decoders.
    #[arg(long)]
    emit_annexb: Option<PathBuf>,

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

fn main() -> Result<()> {
    let args = Args::parse();
    let (width, height) = parse_size(&args.size)?;

    let data = std::fs::read(&args.input)
        .with_context(|| format!("reading {}", args.input.display()))?;

    let opts = extract::Options {
        width,
        height,
        include_non_idr: args.include_non_idr,
        include_partial: args.include_partial,
    };

    let name = args.input.display().to_string();
    let result = extract::extract(&data, &name, &opts)?;

    std::fs::create_dir_all(&args.output)
        .with_context(|| format!("creating {}", args.output.display()))?;

    for (record, frame) in &result.frames {
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
    }

    if let Some(path) = &args.emit_annexb {
        std::fs::write(path, &result.annexb)
            .with_context(|| format!("writing {}", path.display()))?;
    }

    if let Some(path) = &args.json {
        let json = serde_json::to_string_pretty(&result.report)?;
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    }

    if !args.quiet {
        let r = &result.report;
        println!(
            "{}: {} NALs, {} access units, {} frames written ({:?} params, qp {}, {} attempt(s))",
            args.input.display(),
            r.nal_count,
            r.access_units,
            result.frames.len(),
            r.param_source,
            r.pic_init_qp,
            r.qp_search_attempts,
        );
        match r.truncation_offset {
            Some(off) => println!("  chain broke at offset {off} (file is truncated)"),
            None => println!("  chain ran cleanly to the end of mdat"),
        }
    }

    if result.frames.is_empty() {
        bail!("no frames recovered");
    }

    Ok(())
}
