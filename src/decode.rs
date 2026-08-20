//! Decoding backend.
//!
//! Behind a trait so a hand-written Rust intra decoder can be added later as a second backend
//! without touching the carver.

use anyhow::{Result, anyhow};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;

pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// Planar 4:2:0, strides removed.
    pub yuv: Vec<u8>,
}

/// Which YUV-to-RGB matrix to render with. The colour matrix lives in the SPS VUI, which a
/// truncated file has lost along with everything else in `moov`, so it cannot be recovered from
/// the bitstream and has to be told to us. BT.709 is right for essentially all HD content.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
}

impl ColorMatrix {
    /// Returns (Kr, Kb) luma weights.
    fn coefficients(self) -> (f32, f32) {
        match self {
            ColorMatrix::Bt601 => (0.299, 0.114),
            ColorMatrix::Bt709 => (0.2126, 0.0722),
        }
    }
}

impl Frame {
    pub fn to_rgb(&self, matrix: ColorMatrix) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let mut rgb = vec![0u8; w * h * 3];
        let uv_w = w.div_ceil(2);
        let y_plane = &self.yuv[..w * h];
        let u_plane = &self.yuv[w * h..w * h + uv_w * h.div_ceil(2)];
        let v_plane = &self.yuv[w * h + uv_w * h.div_ceil(2)..];

        let (kr, kb) = matrix.coefficients();
        for row in 0..h {
            for col in 0..w {
                let y = y_plane[row * w + col] as f32;
                // Chroma is replicated, not interpolated - this matches what reference renderers
                // do when no rescaling is involved.
                let u = u_plane[(row / 2) * uv_w + col / 2] as f32 - 128.0;
                let v = v_plane[(row / 2) * uv_w + col / 2] as f32 - 128.0;

                // Limited range: luma occupies 16..235, chroma 16..240.
                let c = (y - 16.0) * (255.0 / 219.0);
                let o = (row * w + col) * 3;
                let kg = 1.0 - kr - kb;
                rgb[o] = to_u8(c + v * (2.0 - 2.0 * kr) * (255.0 / 224.0));
                rgb[o + 1] = to_u8(
                    c - u * (2.0 - 2.0 * kb) * (kb / kg) * (255.0 / 224.0)
                        - v * (2.0 - 2.0 * kr) * (kr / kg) * (255.0 / 224.0),
                );
                rgb[o + 2] = to_u8(c + u * (2.0 - 2.0 * kb) * (255.0 / 224.0));
            }
        }
        rgb
    }
}

/// Truncates rather than rounds, matching the fixed-point right-shift that reference renderers
/// use. Measured against ffmpeg this halves the mean error to well under one LSB.
fn to_u8(v: f32) -> u8 {
    v.clamp(0.0, 255.0) as u8
}

pub trait FrameDecoder {
    /// Decodes one access unit given as an Annex B byte stream. `Ok(None)` means the decoder
    /// accepted the data but produced no picture.
    fn decode_au(&mut self, annexb: &[u8]) -> Result<Option<Frame>>;
}

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

/// Wraps NAL payloads into an Annex B byte stream.
pub fn to_annexb(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}
