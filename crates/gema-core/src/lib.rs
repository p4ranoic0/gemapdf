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
//! implementación son privados.
//!
//! **Enums `#[non_exhaustive]`** — el pipeline los hace crecer, así que agregar
//! una variante es aditivo y los consumidores deben dejar un brazo `_`:
//! [`ImageSkipReason`], [`Warning`], [`GemaError`], [`Phase`], [`ImageAction`].
//!
//! **Enums exhaustivos a propósito** — son conceptos de producto con un set
//! cerrado, y conviene que el compilador avise al consumidor si cambian:
//! [`Profile`] y [`SignaturePolicy`].
//!
//! **Structs exhaustivas** — [`CompressOptions`] la arma el llamador con
//! literal de struct, y los tipos de reporte se construyen como fixtures en los
//! tests de los consumidores. `#[non_exhaustive]` en una struct prohíbe el
//! literal desde otro crate *incluso con* `..Default::default()`, así que no se
//! usa. Agregar un campo a [`CompressOptions`], [`Report`], [`ImageStat`],
//! [`ImageSkipSummary`] o [`ProfileParams`] es un cambio incompatible y se
//! versiona como tal.
//!
//! **El JSON tiene su propio versionado**, independiente del de Rust: ver
//! [`ReportJson`] y [`REPORT_SCHEMA_VERSION`].
#![warn(missing_docs)]

mod error;
pub use error::GemaError;

mod report;
pub use report::{ImageAction, ImageSkipReason, ImageSkipSummary, ImageStat, Report, Warning};

mod report_json;
pub use report_json::{
    ByActionJson, DocumentJson, ImageStatJson, ImagesJson, InputJson, OutputJson, ReportJson,
    SkipSummaryJson, WarningJson, REPORT_SCHEMA_VERSION,
};

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
