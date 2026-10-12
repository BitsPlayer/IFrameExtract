//! H.265 / HEVC support.
//!
//! The H.264 path can lean on x264's options SEI and on openh264 failing loudly when a guessed
//! parameter set is wrong. Neither carries over: ffmpeg's libx265 wrapper puts its SEI in `hvcC`
//! alongside the parameter sets, and ffmpeg's HEVC decoder turns a wrong guess into a plausible
//! but corrupt picture without complaint. So candidates are checked here instead, by a CABAC
//! parse of the intra slices that has to land exactly on each substream's stop bit.

pub mod cabac;
pub mod extract;
pub mod params;
pub mod search;
pub mod slice;
pub mod validate;

pub const NAL_TSA_N: u8 = 2;
pub const NAL_TSA_R: u8 = 3;
pub const NAL_STSA_N: u8 = 4;
pub const NAL_STSA_R: u8 = 5;
pub const NAL_BLA_W_LP: u8 = 16;
pub const NAL_IDR_W_RADL: u8 = 19;
pub const NAL_IDR_N_LP: u8 = 20;
pub const NAL_CRA: u8 = 21;
pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_AUD: u8 = 35;
pub const NAL_EOS: u8 = 36;
pub const NAL_EOB: u8 = 37;
pub const NAL_FD: u8 = 38;
pub const NAL_SEI_PREFIX: u8 = 39;
pub const NAL_SEI_SUFFIX: u8 = 40;
/// Dolby Vision carries its RPU and enhancement layer in the "unspecified" range; iPhone HDR
/// recordings interleave one RPU per frame, so rejecting them would break the chain.
pub const NAL_DOLBY_RPU: u8 = 62;
pub const NAL_DOLBY_EL: u8 = 63;

pub fn nal_type(payload: &[u8]) -> u8 {
    (payload[0] >> 1) & 0x3f
}

pub fn layer_id(payload: &[u8]) -> u8 {
    ((payload[0] & 1) << 5) | (payload[1] >> 3)
}

pub fn is_vcl(t: u8) -> bool {
    t < 32
}

pub fn is_irap(t: u8) -> bool {
    (NAL_BLA_W_LP..=23).contains(&t)
}

pub fn is_idr(t: u8) -> bool {
    t == NAL_IDR_W_RADL || t == NAL_IDR_N_LP
}

/// Two-byte NAL header check. HEVC headers carry far more redundancy than H.264's: the layer id
/// must be 0 (or 1 for the second view of MV-HEVC spatial video), the temporal id is never zero,
/// and several types pin it to the base sub-layer.
pub fn header_ok(payload: &[u8]) -> bool {
    if payload.len() < 2 || payload[0] & 0x80 != 0 {
        return false;
    }
    let t = nal_type(payload);
    let tid_plus1 = payload[1] & 7;
    if tid_plus1 == 0 || layer_id(payload) > 1 {
        return false;
    }
    let allowed = matches!(t, 0..=9 | 16..=21 | 32..=40 | NAL_DOLBY_RPU | NAL_DOLBY_EL);
    if !allowed {
        return false;
    }
    let base_sublayer = tid_plus1 == 1;
    match t {
        NAL_TSA_N | NAL_TSA_R | NAL_STSA_N | NAL_STSA_R => !base_sublayer,
        16..=21 | NAL_VPS | NAL_SPS | NAL_EOS | NAL_EOB => base_sublayer,
        _ => true,
    }
}

/// x265's user-data-unregistered SEI, if the encoder left it in-band. Present when the stream was
/// muxed from a raw `.265` (MP4Box, ffmpeg `-c copy`) rather than encoded straight into MP4.
pub fn x265_options(payload: &[u8]) -> Option<String> {
    let rbsp = crate::bits::unescape_rbsp(payload.get(2..)?);
    let text = String::from_utf8_lossy(&rbsp);
    let start = text.find("x265 (build")?;
    let opts = text[start..].find("options: ")? + start + 9;
    Some(
        text[opts..]
            .chars()
            .take_while(|c| *c != '\0' && *c != '\u{80}')
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(nal_type: u8, layer: u8, tid_plus1: u8) -> [u8; 2] {
        [
            (nal_type << 1) | (layer >> 5),
            ((layer & 0x1f) << 3) | tid_plus1,
        ]
    }

    #[test]
    fn header_check_uses_the_redundant_fields() {
        assert!(header_ok(&hdr(NAL_IDR_N_LP, 0, 1)));
        assert!(header_ok(&hdr(1, 0, 3)), "TRAIL_R on a higher sub-layer");
        assert!(header_ok(&hdr(1, 1, 1)), "second view of MV-HEVC");
        assert!(header_ok(&hdr(NAL_DOLBY_RPU, 0, 1)));
        assert!(!header_ok(&hdr(1, 2, 1)), "layer ids beyond 1 do not occur");
        assert!(
            !header_ok(&hdr(1, 0, 0)),
            "temporal id plus one is never zero"
        );
        assert!(
            !header_ok(&hdr(NAL_CRA, 0, 2)),
            "IRAP pictures sit on the base sub-layer"
        );
        assert!(
            !header_ok(&hdr(NAL_TSA_N, 0, 1)),
            "TSA never sits on the base sub-layer"
        );
        assert!(!header_ok(&hdr(12, 0, 1)), "reserved VCL type");
        assert!(!header_ok(&[0x80 | (1 << 1), 1]), "forbidden_zero_bit");
    }

    #[test]
    fn x265_option_string_is_found_in_its_sei() {
        let mut nal = vec![NAL_SEI_PREFIX << 1, 1, 5, 40];
        nal.extend([0u8; 16]);
        nal.extend(b"x265 (build 215) - 4.1:[Linux] - options: ctu=32 no-wpp tu-intra-depth=2\0");
        nal.push(0x80);
        let opts = x265_options(&nal).unwrap();
        assert_eq!(opts, "ctu=32 no-wpp tu-intra-depth=2");

        let p = search::from_x265_options(&opts, 1280, 720);
        assert_eq!(p.log2_ctb, 5);
        assert!(!p.entropy_coding_sync);
        assert_eq!(p.max_transform_depth_intra, 1);
        assert_eq!(
            p.cu_qp_delta_depth,
            Some(0),
            "aq on by default, quant group = CTU"
        );
    }
}
