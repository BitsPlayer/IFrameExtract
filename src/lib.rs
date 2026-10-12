//! Recovers I-frames from MP4/MOV files, H.264 or H.265, by carving the bitstream without ever
//! reading `moov`.

pub mod bits;
pub mod codec;
pub mod container;
pub mod decode;
pub mod extract;
pub mod hevc;
pub mod nal;
pub mod report;
pub mod sei;
pub mod slice;
pub mod sps;
