//! gema-core: compresión PDF portable (Rust puro → WASM + nativo).

pub mod error;
pub use error::GemaError;

pub mod report;
pub use report::{ImageAction, ImageStat, Report, Warning};

#[cfg(test)]
mod smoke {
    #[test]
    fn it_builds() {
        assert_eq!(2 + 2, 4);
    }
}
