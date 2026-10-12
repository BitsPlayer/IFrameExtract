//! Ports the extraction use case needs. Infrastructure implements them.

use anyhow::Result;

use crate::domain::codec::Codec;
use crate::domain::frame::Frame;

pub trait FrameDecoder {
    /// Decodes one access unit given as an Annex B byte stream. `Ok(None)` means the decoder
    /// accepted the data but produced no picture.
    fn decode_au(&mut self, annexb: &[u8]) -> Result<Option<Frame>>;
}

pub trait VideoDecoder: Sync {
    fn open(&self, conceal_errors: bool) -> Result<Box<dyn FrameDecoder>>;
    /// Decodes a whole Annex B stream, references and all. The only path for H.265, which the
    /// per-access-unit decoder does not support.
    fn decode_stream(
        &self,
        codec: Codec,
        annexb: &[u8],
        width: usize,
        height: usize,
    ) -> Result<Vec<Frame>>;
}
