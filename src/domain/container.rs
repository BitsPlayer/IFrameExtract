//! Top-level MP4 box walking, limited to finding the `mdat` payload.
//!
//! `moov` is never read. That is the whole point of the tool: a truncated file has lost its
//! `moov`, and a file padded with `mp4tail` garbage carries a perfectly valid `moov` describing
//! a completely different video. Ignoring it by construction beats detecting it by heuristic.

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy)]
pub struct MdatRange {
    pub payload_start: u64,
    /// End of the payload as declared by the box header, clamped to the file length.
    pub payload_end: u64,
    /// False when the declared size runs past EOF, i.e. the box itself was cut short. A complete
    /// `mdat` means any trailing bytes the NAL chain cannot parse are another track's data rather
    /// than the truncation point.
    pub complete: bool,
}

/// Walks top-level boxes from offset 0 and returns the first `mdat` payload range.
pub fn find_mdat(data: &[u8]) -> Result<MdatRange> {
    let file_len = data.len() as u64;
    let mut off: u64 = 0;

    while off + 8 <= file_len {
        let hdr = &data[off as usize..off as usize + 8];
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let typ = [hdr[4], hdr[5], hdr[6], hdr[7]];

        let (header_len, box_size) = match size32 {
            // size 0 means "extends to end of file"
            0 => (8u64, file_len - off),
            1 => {
                if off + 16 > file_len {
                    bail!("truncated 64-bit box header at offset {off}");
                }
                let large = u64::from_be_bytes(
                    data[off as usize + 8..off as usize + 16]
                        .try_into()
                        .unwrap(),
                );
                (16u64, large)
            }
            n if n < 8 => bail!(
                "invalid box size {n} for type {} at offset {off}",
                fourcc(&typ)
            ),
            n => (8u64, n),
        };

        if &typ == b"mdat" {
            let payload_start = off + header_len;
            let declared_end = off.saturating_add(box_size);
            return Ok(MdatRange {
                payload_start,
                // The declared size is never trusted: a truncated file's mdat header still
                // advertises the original length, which runs well past EOF.
                payload_end: declared_end.min(file_len),
                complete: declared_end <= file_len,
            });
        }

        off = off
            .checked_add(box_size)
            .context("box size overflow while walking")?;
    }

    bail!("no mdat box found in the first {file_len} bytes")
}

fn fourcc(t: &[u8; 4]) -> String {
    t.iter().map(|&b| b as char).collect()
}

/// What the file says about itself before `mdat`. Purely informational: MP4 and QuickTime share
/// the box layout the carver relies on, so nothing downstream branches on it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContainerInfo {
    /// `mov` for QuickTime, `mp4` for ISO base media, `unknown` without an `ftyp`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
}

impl ContainerInfo {
    pub fn read(data: &[u8]) -> Self {
        let brand = (data.len() >= 12 && &data[4..8] == b"ftyp").then(|| fourcc(&[
            data[8], data[9], data[10], data[11],
        ]));
        let kind = match brand.as_deref() {
            Some("qt  ") => "mov",
            Some(_) => "mp4",
            // QuickTime predates `ftyp`; files from old cameras open straight on a `wide`, `mdat`
            // or `moov` atom.
            None if data.len() >= 8 && matches!(&data[4..8], b"wide" | b"mdat" | b"moov" | b"free" | b"skip") => "mov",
            None => "unknown",
        };
        Self {
            kind,
            brand: brand.map(|b| b.trim_end().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend(typ);
        v.extend(body);
        v
    }

    #[test]
    fn quicktime_and_iso_files_are_told_apart() {
        let mov = [boxed(b"ftyp", b"qt  \0\0\x02\0qt  "), boxed(b"wide", b""), boxed(b"mdat", b"x")].concat();
        let info = ContainerInfo::read(&mov);
        assert_eq!((info.kind, info.brand.as_deref()), ("mov", Some("qt")));

        let mp4 = [boxed(b"ftyp", b"isom\0\0\x02\0isom"), boxed(b"mdat", b"x")].concat();
        assert_eq!(ContainerInfo::read(&mp4).kind, "mp4");

        // Pre-ftyp QuickTime, as old cameras write it.
        let old = [boxed(b"wide", b""), boxed(b"mdat", b"x")].concat();
        assert_eq!(ContainerInfo::read(&old).kind, "mov");
    }

    #[test]
    fn mdat_after_a_quicktime_wide_atom_is_found() {
        let mov = [boxed(b"ftyp", b"qt  \0\0\x02\0qt  "), boxed(b"wide", b""), boxed(b"mdat", b"abc")].concat();
        let m = find_mdat(&mov).unwrap();
        assert_eq!(&mov[m.payload_start as usize..m.payload_end as usize], b"abc");
        assert!(m.complete);
    }
}
