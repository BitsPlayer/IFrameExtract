//! Decoder backends. OpenH264 for intra frames and ffmpeg for full streams.

use anyhow::{Result, anyhow};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;

use crate::application::ports::{FrameDecoder, VideoDecoder};
use crate::domain::frame::Frame;

pub struct OpenH264Backend {
    decoder: Decoder,
}

impl OpenH264Backend {
    pub fn new(conceal_errors: bool) -> Result<Self> {
        let api = OpenH264API::from_source();
        let mut decoder = Decoder::with_api_config(api, DecoderConfig::new())
            .map_err(|e| anyhow!("openh264 init failed: {e}"))?;

        if conceal_errors {
            // The crate has no setter for this but exposes the raw API. Without concealment the
            // decoder emits nothing at all for a slice that stops mid-picture, which is exactly
            // the case for the frame straddling the truncation point. Cross-IDR is the variant
            // that will emit a picture for a partial IDR, which has no reference of its own.
            let mut idc = openh264_sys2::ERROR_CON_SLICE_COPY_CROSS_IDR;
            unsafe {
                decoder.raw_api().set_option(
                    openh264_sys2::DECODER_OPTION_ERROR_CON_IDC,
                    std::ptr::from_mut(&mut idc).cast(),
                );
            }
        }

        Ok(Self { decoder })
    }
}

impl FrameDecoder for OpenH264Backend {
    fn decode_au(&mut self, annexb: &[u8]) -> Result<Option<Frame>> {
        let yuv = self
            .decoder
            .decode(annexb)
            .map_err(|e| anyhow!("decode failed: {e}"))?;

        let Some(yuv) = yuv else { return Ok(None) };

        let (w, h) = yuv.dimensions();
        let (sy, su, sv) = yuv.strides();
        let uv_w = w.div_ceil(2);
        let uv_h = h.div_ceil(2);

        let mut out = Vec::with_capacity(w * h + 2 * uv_w * uv_h);
        for row in 0..h {
            out.extend_from_slice(&yuv.y()[row * sy..row * sy + w]);
        }
        for row in 0..uv_h {
            out.extend_from_slice(&yuv.u()[row * su..row * su + uv_w]);
        }
        for row in 0..uv_h {
            out.extend_from_slice(&yuv.v()[row * sv..row * sv + uv_w]);
        }

        Ok(Some(Frame {
            width: w,
            height: h,
            yuv: out,
        }))
    }
}


/// Decodes a continuous Annex B H.264 stream into YUV420P frames using ffmpeg if available.
///
/// Used for full-stream recovery containing P-frames (including High Profile features like 8x8 DCT
/// that OpenH264 does not support). Returns an error if ffmpeg is not installed or fails.
pub fn decode_stream_ffmpeg(annexb: &[u8], width: usize, height: usize) -> Result<Vec<Frame>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("ffmpeg")
        .args([
            "-v", "error",
            "-f", "h264",
            "-i", "-",
            "-f", "rawvideo",
            "-pix_fmt", "yuv420p",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow!("failed to spawn ffmpeg: {e}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open ffmpeg stdin"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to open ffmpeg stdout"))?;

    let input_bytes = annexb.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input_bytes);
    });

    use std::io::Read;
    let mut raw_yuv = Vec::new();
    stdout
        .read_to_end(&mut raw_yuv)
        .map_err(|e| anyhow!("reading ffmpeg output failed: {e}"))?;

    let _ = writer.join();
    let status = child.wait().map_err(|e| anyhow!("ffmpeg failed: {e}"))?;
    if !status.success() && raw_yuv.is_empty() {
        return Err(anyhow!("ffmpeg exited with error: {:?}", status));
    }

    let frame_bytes = width * height * 3 / 2;
    if frame_bytes == 0 {
        return Ok(Vec::new());
    }
    let num_frames = raw_yuv.len() / frame_bytes;
    let mut frames = Vec::with_capacity(num_frames);
    for i in 0..num_frames {
        let yuv = raw_yuv[i * frame_bytes..(i + 1) * frame_bytes].to_vec();
        frames.push(Frame {
            width,
            height,
            yuv,
        });
    }
    Ok(frames)
}

pub struct OpenH264Decoder;

impl VideoDecoder for OpenH264Decoder {
    fn open(&self, conceal_errors: bool) -> Result<Box<dyn FrameDecoder>> {
        Ok(Box::new(OpenH264Backend::new(conceal_errors)?))
    }

    fn decode_stream(
        &self,
        annexb: &[u8],
        width: usize,
        height: usize,
    ) -> Result<Vec<Frame>> {
        decode_stream_ffmpeg(annexb, width, height)
    }
}
