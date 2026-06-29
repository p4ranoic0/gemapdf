//! gema-core: compresión PDF portable (Rust puro → WASM + nativo).

pub mod error;
pub use error::GemaError;

pub mod report;
pub use report::{ImageAction, ImageStat, Report, Warning};

pub mod options;
pub use options::{CompressOptions, Profile, ProfileParams, SignaturePolicy};

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
