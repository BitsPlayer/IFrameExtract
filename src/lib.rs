//! Recovers I-frames from MP4/MOV files, H.264 or H.265, by carving the bitstream without ever
//! reading `moov`.

pub mod application;
pub mod domain;
pub mod infrastructure;

pub use domain::{bits, codec, container, hevc, nal, report, sei, slice, sps};

pub mod decode {
    pub use crate::application::ports::FrameDecoder;
    pub use crate::domain::annexb::to_annexb;
    pub use crate::domain::frame::{ColorMatrix, Frame};
    pub use crate::infrastructure::decode::{
        OpenH264Backend, decode_annexb_ffmpeg, decode_stream_ffmpeg,
    };
}

pub mod extract;
