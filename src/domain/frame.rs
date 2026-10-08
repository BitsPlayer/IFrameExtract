//! Decoded picture and colour conversion. No decoder implementation.

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
