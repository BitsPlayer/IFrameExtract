//! Domain layer: H.264, H.265 and ISOBMFF parsing over borrowed bytes. No decoder crates and no
//! filesystem.

pub mod annexb;
pub mod bits;
pub mod codec;
pub mod container;
pub mod frame;
pub mod hevc;
pub mod model;
pub mod nal;
pub mod report;
pub mod sei;
pub mod slice;
pub mod sps;
