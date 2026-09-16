//! Edición regional de texto en documentos PDF.
//!
//! El llamador señala una región de la página y la librería modifica sólo lo
//! que cae dentro. No comprime, no aplana firmas y no recorre el documento
//! entero: para eso está `gema-compress`.
#![warn(missing_docs)]

mod error;
pub use error::EditError;

pub(crate) mod matrix;
pub(crate) mod text_geometry;

mod text_erase;
pub use text_erase::{erase_text, EraseRegion, EraseResult, EraseStatus, RegionReport};
