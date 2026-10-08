//! Public extraction API. Wires the application use case to the OpenH264 backend.

use anyhow::Result;

use crate::application::extract::extract_with_decoder;
use crate::domain::report::{Reporter, Silent};
use crate::infrastructure::decode::OpenH264Decoder;

pub use crate::domain::model::{AccessUnit, Extraction, FrameRecord, Options, Report};

pub fn extract(data: &[u8], input_name: &str, opts: &Options) -> Result<Extraction> {
    extract_with(data, input_name, opts, &mut Silent)
}

pub fn extract_with(
    data: &[u8],
    input_name: &str,
    opts: &Options,
    reporter: &mut dyn Reporter,
) -> Result<Extraction> {
    extract_with_decoder(
        data,
        input_name,
        opts,
        reporter,
        &OpenH264Decoder,
    )
}
