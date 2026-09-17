//! Fallo que aborta una operación de edición.

use std::fmt;

use thiserror::Error;

/// Fallo que aborta una edición.
///
/// Las variantes pueden crecer conforme se distingan modos de fallo nuevos;
/// para mostrar el error al usuario, usar `Display`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EditError {
    /// El PDF no se pudo parsear; el texto describe la causa de `lopdf`.
    #[error("could not parse the PDF: {0}")]
    Parse(String),
    /// Fallo al leer o escribir bytes.
    #[error("I/O error: {0}")]
    Io(String),
    /// Un límite de [`crate::EditOptions`] se superó **en el camino de
    /// borrado**. La llamada aborta porque no se puede reescribir lo que no
    /// se leyó entero. Los límites superados durante la inspección residual
    /// no producen este error: dejan `inspection_incomplete`.
    #[error("limit exceeded: {0}")]
    LimitExceeded(LimitKind),
}

/// Qué límite se superó.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum LimitKind {
    /// `EditOptions::max_input_bytes`.
    InputBytes,
    /// `EditOptions::max_regions`.
    Regions,
    /// `EditOptions::max_decompressed_bytes`: contenido de **una página**
    /// (la suma de sus content streams).
    DecompressedBytes,
    /// `ObjectBudget::max_total_decompressed_bytes`: toda la llamada.
    TotalDecompressedBytes,
    /// `ObjectBudget::max_streams`.
    Streams,
    /// `ObjectBudget::max_reference_depth`.
    ReferenceDepth,
    /// `ObjectBudget::max_inspected_objects`.
    InspectedObjects,
}

impl LimitKind {
    /// Nombre estable, idéntico al que produce serde.
    pub fn as_str(&self) -> &'static str {
        match self {
            LimitKind::InputBytes => "input_bytes",
            LimitKind::Regions => "regions",
            LimitKind::DecompressedBytes => "decompressed_bytes",
            LimitKind::TotalDecompressedBytes => "total_decompressed_bytes",
            LimitKind::Streams => "streams",
            LimitKind::ReferenceDepth => "reference_depth",
            LimitKind::InspectedObjects => "inspected_objects",
        }
    }
}

impl fmt::Display for LimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_english_and_names_the_limit() {
        assert_eq!(
            EditError::Parse("x".into()).to_string(),
            "could not parse the PDF: x"
        );
        assert_eq!(EditError::Io("y".into()).to_string(), "I/O error: y");
        assert_eq!(
            EditError::LimitExceeded(LimitKind::DecompressedBytes).to_string(),
            "limit exceeded: decompressed_bytes"
        );
    }

    #[test]
    fn limit_kind_as_str_is_snake_case() {
        let all = [
            (LimitKind::InputBytes, "input_bytes"),
            (LimitKind::Regions, "regions"),
            (LimitKind::DecompressedBytes, "decompressed_bytes"),
            (
                LimitKind::TotalDecompressedBytes,
                "total_decompressed_bytes",
            ),
            (LimitKind::Streams, "streams"),
            (LimitKind::ReferenceDepth, "reference_depth"),
            (LimitKind::InspectedObjects, "inspected_objects"),
        ];
        for (kind, expected) in all {
            assert_eq!(kind.as_str(), expected);
        }
    }
}
