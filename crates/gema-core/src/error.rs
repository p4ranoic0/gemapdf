use thiserror::Error;

#[derive(Debug, Error)]
pub enum GemaError {
    #[error("no se pudo parsear el PDF: {0}")]
    Parse(String),
    #[error("el PDF está protegido por contraseña")]
    Encrypted,
    #[error("documento firmado criptográficamente; usa SignaturePolicy::Flatten para conservar su apariencia visual o Strict para mantener el archivo intacto")]
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
        assert!(GemaError::SignedDocument.to_string().contains("firmado"));
    }
}
