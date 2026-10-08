//! Recovers I-frames from MP4/H.264 files by carving the bitstream, without ever reading `moov`.

pub mod domain;

pub use domain::{bits, container, nal, report, sei, slice, sps};

pub mod decode;
pub mod extract;
