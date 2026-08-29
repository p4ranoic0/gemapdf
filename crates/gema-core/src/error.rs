use thiserror::Error;

/// Límite configurable asociado a un rechazo o aviso del pipeline.
///
/// `StreamBytes` queda reservado hasta que el límite de inflación de streams
/// tenga productor; los demás se aplican a nivel de documento.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LimitKind {
    /// Cantidad de páginas del documento.
    Pages,
    /// Cantidad de objetos del documento.
    Objects,
    /// Bytes producidos al inflar un stream individual.
    StreamBytes,
    /// Trabajo total estimado para procesar las imágenes.
    TotalWork,
}

impl std::fmt::Display for LimitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pages => "páginas",
            Self::Objects => "objetos",
            Self::StreamBytes => "bytes de stream",
            Self::TotalWork => "trabajo total",
        })
    }
}

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
    /// Un límite opcional de [`CompressOptions`](crate::CompressOptions) fue
    /// excedido. El documento se rechaza entero: nunca se devuelve una salida
    /// parcial.
    #[error("límite excedido ({limit}): {observed} > {allowed}")]
    LimitExceeded {
        /// Límite que se excedió.
        limit: LimitKind,
        /// Valor observado en el documento.
        observed: u64,
        /// Máximo permitido por las opciones.
        allowed: u64,
    },
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
