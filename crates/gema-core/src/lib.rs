//! gema-core: compresión PDF portable (Rust puro → WASM + nativo).
//!
//! Dos entradas: [`analyze`] inspecciona un PDF sin tocarlo y [`compress`] (o
//! [`compress_with_progress`]) produce el PDF comprimido más un [`Report`].
//!
//! ```no_run
//! use gema_core::{compress, CompressOptions, Profile};
//!
//! let pdf = std::fs::read("entrada.pdf")?;
//! let res = compress(
//!     &pdf,
//!     &CompressOptions {
//!         profile: Profile::Ebook,
//!         ..Default::default()
//!     },
//! )?;
//! std::fs::write("salida.pdf", &res.output)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Estabilidad de la API
//!
//! La superficie soportada es lo que se re-exporta acá; los módulos de
//! implementación son privados. [`CompressOptions`] es exhaustiva porque la
//! arma el llamador. Los enums que el pipeline hace crecer —
//! [`ImageSkipReason`], [`Warning`], [`GemaError`], [`Phase`],
//! [`ImageAction`] — son `#[non_exhaustive]`: hay que dejarles un brazo `_` al
//! hacer `match`. [`Profile`] y [`SignaturePolicy`] sí son cerrados a
//! propósito: son conceptos de producto y conviene que el compilador avise.
#![warn(missing_docs)]

mod error;
pub use error::GemaError;

mod report;
pub use report::{ImageAction, ImageSkipReason, ImageSkipSummary, ImageStat, Report, Warning};

mod options;
pub use options::{CompressOptions, Profile, ProfileParams, SignaturePolicy};

mod analyze;
pub use analyze::analyze;

mod image_opt;

pub(crate) mod geometry;

pub(crate) mod flatten;
pub(crate) mod signatures;

mod rewrite;

mod progress;
pub use progress::Phase;

mod pipeline;
pub use pipeline::{compress, compress_with_progress, CompressResult};

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
