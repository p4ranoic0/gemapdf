//! gema-core: compresión PDF portable (Rust puro → WASM + nativo).

pub mod error;
pub use error::GemaError;

pub mod report;
pub use report::{ImageAction, ImageStat, Report, Warning};

pub mod options;
pub use options::{CompressOptions, Profile, ProfileParams, SignaturePolicy};

pub mod analyze;
pub use analyze::analyze;

pub mod image_opt;

pub mod rewrite;

pub mod pipeline;
pub use pipeline::{compress, CompressResult};

#[cfg(test)]
mod smoke {
    use super::*;
    #[test]
    fn public_api_constructs() {
        let opts = CompressOptions::default();
        let _report = Report::default();
        assert_eq!(opts.resolved().image_dpi, 150);
    }
}
