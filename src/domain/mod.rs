//! Domain layer: H.264 and ISOBMFF parsing over borrowed bytes. No decoder crates and no filesystem.

pub mod bits;
pub mod container;
pub mod nal;
pub mod report;
pub mod sei;
pub mod slice;
pub mod sps;
