//! gema-core: compresión PDF portable (Rust puro → WASM + nativo).

pub mod error;
pub use error::GemaError;

#[cfg(test)]
mod smoke {
    #[test]
    fn it_builds() {
        assert_eq!(2 + 2, 4);
    }
}
