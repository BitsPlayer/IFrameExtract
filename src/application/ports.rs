//! Ports the extraction use case needs. Infrastructure implements them.

use anyhow::Result;

use crate::domain::frame::Frame;

pub trait FrameDecoder {
    /// Decodes one access unit given as an Annex B byte stream. `Ok(None)` means the decoder
    /// accepted the data but produced no picture.
    fn decode_au(&mut self, annexb: &[u8]) -> Result<Option<Frame>>;
}

pub trait VideoDecoder: Sync {
    fn open(&self, conceal_errors: bool) -> Result<Box<dyn FrameDecoder>>;
    fn decode_stream(
        &self,
        annexb: &[u8],
        width: usize,
        height: usize,
    ) -> Result<Vec<Frame>>;
}
