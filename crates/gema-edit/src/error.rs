//! Fallo que aborta una operación de edición.

use thiserror::Error;

/// Fallo que aborta una edición.
///
/// Las variantes pueden crecer conforme se distingan modos de fallo nuevos;
/// para mostrar el error al usuario, usar `Display`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EditError {
    /// El PDF no se pudo parsear; el texto describe la causa de `lopdf`.
    #[error("no se pudo parsear el PDF: {0}")]
    Parse(String),
    /// Fallo al leer o escribir bytes.
    #[error("error de E/S: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_identical_to_the_old_core_error() {
        // Los textos se copian LITERALMENTE de gema-core: este plan no puede
        // cambiar ni una palabra de lo que ve el usuario.
        assert_eq!(
            EditError::Parse("x".into()).to_string(),
            "no se pudo parsear el PDF: x"
        );
        assert_eq!(EditError::Io("y".into()).to_string(), "error de E/S: y");
    }
}
