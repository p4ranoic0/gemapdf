use thiserror::Error;

#[derive(Debug, Error)]
pub enum GemaError {
    #[error("no se pudo parsear el PDF: {0}")]
    Parse(String),
    #[error("el PDF está protegido por contraseña")]
    Encrypted,
    #[error("imagen no soportada (object {0})")]
    UnsupportedImage(u32),
    #[error("documento firmado criptográficamente; usa SignaturePolicy::Ignore para forzar")]
    SignedDocument,
    #[error("error de E/S: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_messages_render() {
        assert!(GemaError::Encrypted.to_string().contains("contraseña"));
        assert!(GemaError::UnsupportedImage(7).to_string().contains("7"));
    }
}
