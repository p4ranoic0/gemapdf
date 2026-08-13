use thiserror::Error;

/// Fallo que aborta un análisis o una compresión.
///
/// Las variantes pueden crecer conforme se distingan modos de fallo nuevos;
/// para mostrar el error al usuario, usar `Display`.
///
/// **No hay variante para "documento firmado":**
/// [`SignaturePolicy::Strict`](crate::SignaturePolicy::Strict) no falla,
/// devuelve el documento intacto. El llamador lo observa por
/// [`Report::is_signed`](crate::Report::is_signed) más una salida idéntica a la
/// entrada.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GemaError {
    /// El PDF no se pudo parsear; el texto describe la causa de `lopdf`.
    #[error("no se pudo parsear el PDF: {0}")]
    Parse(String),
    /// El documento está cifrado y no se puede abrir sin contraseña.
    #[error("el PDF está protegido por contraseña")]
    Encrypted,
    /// Fallo al leer o escribir bytes.
    #[error("error de E/S: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_messages_render() {
        assert!(GemaError::Encrypted.to_string().contains("contraseña"));
        assert!(GemaError::Parse("x".into()).to_string().contains("parsear"));
    }
}
