//! Recovers I-frames from MP4/H.264 files by carving the bitstream, without ever reading `moov`.

pub mod bits;
pub mod container;
pub mod decode;
pub mod extract;
pub mod nal;
pub mod sei;
pub mod slice;
pub mod sps;
