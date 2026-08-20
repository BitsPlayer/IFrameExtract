//! x264's user-data-unregistered SEI carries the full encoder option string, and it is the first
//! NAL of the file, so it survives any tail truncation. It supplies almost every parameter that
//! would otherwise have been read from the lost `avcC`.

use crate::bits::unescape_rbsp;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct X264Options {
    pub cabac: bool,
    pub transform_8x8: bool,
    pub bframes: u32,
    pub refs: u32,
    pub chroma_qp_offset: i32,
    pub constrained_intra: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    /// `crf`, `abr`, `2pass`, `cqp` ...
    pub rc: String,
    pub crf: Option<f32>,
    pub qp: Option<i32>,
}

impl X264Options {
    fn from_map(raw: HashMap<String, String>) -> Self {
        let get_u32 = |k: &str, d: u32| raw.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        let get_i32 = |k: &str, d: i32| raw.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        let get_bool = |k: &str, d: bool| raw.get(k).map(|v| v != "0").unwrap_or(d);

        Self {
            cabac: get_bool("cabac", true),
            transform_8x8: get_bool("8x8dct", true),
            bframes: get_u32("bframes", 3),
            refs: get_u32("ref", 3),
            chroma_qp_offset: get_i32("chroma_qp_offset", 0),
            constrained_intra: get_bool("constrained_intra", false),
            weighted_pred: get_u32("weightp", 2) > 0,
            weighted_bipred: get_bool("weightb", true),
            rc: raw.get("rc").cloned().unwrap_or_default(),
            crf: raw.get("crf").and_then(|v| v.parse().ok()),
            qp: raw.get("qp").and_then(|v| v.parse().ok()),
        }
    }
}

/// Scans an SEI NAL payload for a user-data-unregistered message containing an x264 option string.
pub fn parse_x264_sei(nal_payload: &[u8]) -> Option<X264Options> {
    // Skip the 1-byte NAL header, then undo emulation prevention.
    let rbsp = unescape_rbsp(nal_payload.get(1..)?);
    let mut i = 0usize;

    while i < rbsp.len() {
        let mut payload_type = 0usize;
        while *rbsp.get(i)? == 0xff {
            payload_type += 255;
            i += 1;
        }
        payload_type += *rbsp.get(i)? as usize;
        i += 1;

        let mut payload_size = 0usize;
        while *rbsp.get(i)? == 0xff {
            payload_size += 255;
            i += 1;
        }
        payload_size += *rbsp.get(i)? as usize;
        i += 1;

        let end = (i + payload_size).min(rbsp.len());
        // 5 == user_data_unregistered: 16-byte UUID followed by free-form data.
        if payload_type == 5 && payload_size > 16 {
            let text = String::from_utf8_lossy(&rbsp[i + 16..end]);
            if let Some(opts) = text.find("options: ") {
                return Some(X264Options::from_map(parse_options(&text[opts + 9..])));
            }
        }
        i = end;
    }
    None
}

fn parse_options(s: &str) -> HashMap<String, String> {
    s.split_whitespace()
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.trim_end_matches('\0').to_string()))
        .collect()
}
