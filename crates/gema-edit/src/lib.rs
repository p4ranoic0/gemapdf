//! Edición regional de texto en documentos PDF.
//!
//! El llamador señala una región de la página y la librería modifica sólo lo
//! que cae dentro. No comprime, no aplana firmas y no recorre el documento
//! entero: para eso está `gema-compress`.
//!
//! # Qué garantiza y qué no
//!
//! [`remove_text_glyphs`] reescribe el content stream directo de la página
//! para que los glifos dentro de la región dejen de emitirse. **No** es una
//! redacción: el texto puede sobrevivir en superficies que la función no
//! reescribe, y la función se limita a detectarlas e informarlas.
//!
//! Metadata y XMP quedan **fuera** de la inspección por decisión explícita:
//! son superficie de documento, no de región, y afirmar algo sobre ellas
//! exigiría parsear XMP sin tope. Si el llamador necesita limpiarlas, es una
//! operación aparte.
//!
//! # `redact()`
//!
//! El nombre `redact` queda **reservado** para una operación futura que sí
//! garantice la eliminación en todas las superficies. No existe todavía y
//! nada en este crate debe usarlo para otra cosa.
#![warn(missing_docs)]

mod error;
pub use error::{EditError, LimitKind};

pub mod options;
pub use options::{EditOptions, ObjectBudget};

pub(crate) mod matrix;
pub(crate) mod text_geometry;

mod text_removal;
pub use text_removal::{
    remove_text_glyphs, remove_text_glyphs_with, RegionReport, RemovalResult, RemovalStatus,
    TextRegion,
};
