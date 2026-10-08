//! Recovers I-frames from MP4/H.264 files by carving the bitstream, without ever reading `moov`.

pub mod application;
pub mod domain;
pub mod infrastructure;

pub use domain::{bits, container, nal, report, sei, slice, sps};

pub mod decode {
    pub use crate::application::ports::FrameDecoder;
    pub use crate::domain::annexb::to_annexb;
    pub use crate::domain::frame::{ColorMatrix, Frame};
    pub use crate::infrastructure::decode::{OpenH264Backend, decode_stream_ffmpeg};
}

pub mod extract;
