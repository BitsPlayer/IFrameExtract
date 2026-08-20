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
                    data[off as usize + 8..off as usize + 16].try_into().unwrap(),
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
