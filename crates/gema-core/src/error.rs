use thiserror::Error;

/// Fallo que aborta un análisis o una compresión.
///
/// Las variantes pueden crecer conforme se distingan modos de fallo nuevos;
/// para mostrar el error al usuario, usar `Display`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GemaError {
    /// El PDF no se pudo parsear; el texto describe la causa de `lopdf`.
    #[error("no se pudo parsear el PDF: {0}")]
    Parse(String),
    /// El documento está cifrado y no se puede abrir sin contraseña.
    #[error("el PDF está protegido por contraseña")]
    Encrypted,
    /// El documento está firmado y la política vigente es
    /// [`SignaturePolicy::Strict`](crate::SignaturePolicy::Strict).
    #[error("documento firmado criptográficamente; usa SignaturePolicy::Flatten para conservar su apariencia visual o Strict para mantener el archivo intacto")]
    SignedDocument,
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
        assert!(GemaError::SignedDocument.to_string().contains("firmado"));
    }
}
