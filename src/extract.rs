//! Public extraction API. Wires the application use case to the OpenH264 backend.

use anyhow::Result;

use crate::application::extract::{self as app, extract_with_decoder};
use crate::domain::report::{Reporter, Silent};
use crate::infrastructure::decode::OpenH264Decoder;

pub use crate::domain::model::{
    AccessUnit, CarveAnalysis, CarveOptions, Extraction, FrameRecord, Options, Report, Selection,
};
pub use crate::domain::nal::Nal;
pub use crate::domain::sps::ParamSource;

pub fn extract(data: &[u8], input_name: &str, opts: &Options) -> Result<Extraction> {
    extract_with(data, input_name, opts, &mut Silent)
}

pub fn extract_with(
    data: &[u8],
    input_name: &str,
    opts: &Options,
    reporter: &mut (dyn Reporter + Send),
) -> Result<Extraction> {
    extract_with_decoder(
        data,
        input_name,
        opts,
        reporter,
        &OpenH264Decoder,
    )
}

pub fn analyze(
    data: &[u8],
    opts: &CarveOptions,
    reporter: &mut dyn Reporter,
) -> Result<CarveAnalysis> {
    app::analyze(data, opts, reporter, &OpenH264Decoder)
}

pub fn decode(
    data: &[u8],
    analysis: &CarveAnalysis,
    selection: &Selection,
    input_name: &str,
    reporter: &mut (dyn Reporter + Send),
) -> Result<Extraction> {
    app::decode(data, analysis, selection, input_name, reporter, &OpenH264Decoder)
}
