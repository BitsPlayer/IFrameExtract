//! Annex B byte-stream wrapping.

/// Wraps NAL payloads into an Annex B byte stream.
pub fn to_annexb(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}
