/// Qué se hizo con una imagen del documento.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageAction {
    /// Se reescribió sin ganancia y se conservó la codificación original.
    Kept,
    /// Se recomprimió a la misma resolución.
    Recompressed,
    /// Se bajó la resolución (y se recomprimió).
    Downsampled,
    /// No se pudo procesar; ver [`ImageStat::skip_reason`].
    Skipped,
    /// Imagen preservada byte-idéntica por ser firma/sello (no se recomprime).
    Preserved,
}

/// Resultado por imagen. El `Vec` completo vive en [`Report::images`].
#[derive(Debug, Clone)]
pub struct ImageStat {
    /// Número de objeto del XObject de imagen en el PDF de entrada.
    pub object_id: u32,
    /// Bytes del stream codificado en la entrada.
    pub original_bytes: u64,
    /// Bytes del stream codificado en la salida.
    pub output_bytes: u64,
    /// Qué se hizo con esta imagen.
    pub action: ImageAction,
    /// Motivo estructurado cuando `action == Skipped`.
    pub skip_reason: Option<ImageSkipReason>,
}

/// Motivo estable por el que una imagen se conservó sin optimizar.
///
/// Los identificadores de [`ImageSkipReason::as_str`] son la superficie
/// estable (telemetría, CLI, JSON del binding); las variantes pueden crecer
/// conforme el pipeline distingue casos nuevos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ImageSkipReason {
    /// `/Width` o `/Height` ausentes, cero, negativos o fuera de rango.
    InvalidDimensions,
    /// La imagen excede `max_image_bytes` decodificada.
    MemoryLimit,
    /// `/JPXDecode` (JPEG 2000): sin decoder en Rust puro.
    Jpx,
    /// `/CCITTFaxDecode`: fuera del decoder actual.
    Ccit,
    /// `/JBIG2Decode`: fuera del decoder actual.
    Jbig2,
    /// `/LZWDecode`: fuera del decoder actual.
    Lzw,
    /// Colorspace `/Indexed` con índices empaquetados a 1/2/4 bits por componente.
    IndexedSubByte,
    /// `/BitsPerComponent` distinto de los soportados.
    UnsupportedBitDepth,
    /// Colorspace que `interpret_color_space` no sabe convertir.
    UnsupportedColorSpace,
    /// El stream falló al decodificarse pese a tener un filtro soportado.
    DecodeFailed,
    /// La máscara suave declara `/Matte` (color premultiplicado).
    SoftMaskMatte,
    /// La `/SMask` existe pero no se pudo inspeccionar.
    SoftMaskUninspectable,
    /// Forma de `/SMask` fuera del alcance del lever de máscaras.
    SoftMaskUnsupported,
    /// La imagen trae canal alfa embebido y además una `/SMask`.
    EmbeddedAlphaWithSoftMask,
    /// La `/SMask` no es de un solo canal en escala de grises.
    NonGraySoftMask,
    /// El encoder falló al producir la salida.
    EncodeFailed,
    /// La reescritura del objeto en el documento falló.
    RewriteFailed,
}

impl ImageSkipReason {
    /// Identificador estable en `snake_case`, apto para telemetría y JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidDimensions => "invalid_dimensions",
            Self::MemoryLimit => "memory_limit",
            Self::Jpx => "jpx",
            Self::Ccit => "ccitt",
            Self::Jbig2 => "jbig2",
            Self::Lzw => "lzw",
            Self::IndexedSubByte => "indexed_sub_byte",
            Self::UnsupportedBitDepth => "unsupported_bit_depth",
            Self::UnsupportedColorSpace => "unsupported_color_space",
            Self::DecodeFailed => "decode_failed",
            Self::SoftMaskMatte => "soft_mask_matte",
            Self::SoftMaskUninspectable => "soft_mask_uninspectable",
            Self::SoftMaskUnsupported => "soft_mask_unsupported",
            Self::EmbeddedAlphaWithSoftMask => "embedded_alpha_with_soft_mask",
            Self::NonGraySoftMask => "non_gray_soft_mask",
            Self::EncodeFailed => "encode_failed",
            Self::RewriteFailed => "rewrite_failed",
        }
    }
}

/// Agregado documental de oportunidades no procesadas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSkipSummary {
    /// Motivo compartido por las imágenes de este grupo.
    pub reason: ImageSkipReason,
    /// Cuántas imágenes se omitieron por ese motivo.
    pub images: usize,
    /// Suma de bytes codificados de entrada de esas imágenes.
    pub original_bytes: u64,
}

/// Aviso no fatal producido durante el análisis o la compresión.
///
/// Las variantes pueden crecer; para mostrarlas al usuario, usar `Display` en
/// vez de hacer `match` sobre ellas.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Warning {
    /// El documento tiene una firma criptográfica.
    SignedDocument,
    /// La imagen con ese número de objeto no se pudo optimizar.
    ImageSkipped(u32),
    /// Aviso sin estructura propia; el texto ya es legible.
    Other(String),
}

impl std::fmt::Display for Warning {
    /// Render legible para humanos; lo consumen los bindings (wasm) y la CLI
    /// para exponer las warnings como texto plano.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Warning::SignedDocument => write!(f, "documento firmado criptográficamente"),
            Warning::ImageSkipped(id) => write!(f, "imagen omitida (object {id})"),
            Warning::Other(msg) => f.write_str(msg),
        }
    }
}

/// Resultado observable de un análisis o una compresión.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Páginas del documento.
    pub pages: usize,
    /// Bytes del PDF de entrada.
    pub original_size: u64,
    /// Bytes del PDF de salida. `None` en `analyze` (no hay salida).
    pub output_size: Option<u64>,
    /// `output_size / original_size`: cuánto del original quedó (menor = más
    /// comprimido). `None` si no hay salida.
    pub ratio: Option<f32>,
    /// Una entrada por imagen procesada.
    pub images: Vec<ImageStat>,
    /// Imágenes omitidas y bytes de entrada agrupados por motivo estable.
    pub image_skip_summary: Vec<ImageSkipSummary>,
    /// Número de imágenes preservadas byte-idénticas por ser firma/sello.
    pub preserved_images: usize,
    /// Número de firmas/sellos aplanados al contenido de página (política Flatten).
    pub flattened_signatures: usize,
    /// Imágenes redundantes adicionales eliminadas por `dedupe_images`.
    pub deduplicated_images: usize,
    /// Bytes codificados adicionales de streams redundantes eliminados.
    pub deduplicated_image_bytes: u64,
    /// El documento trae una firma criptográfica.
    pub is_signed: bool,
    /// Estimación conservadora: al menos una página contiene un raster grande
    /// que cubre casi toda la página y muy poco texto pintado.
    pub has_scanned_pages: bool,
    /// Avisos no fatales acumulados.
    pub warnings: Vec<Warning>,
}

impl Report {
    /// Calcula `ratio` a partir de `original_size` y `output_size`.
    pub fn with_ratio(mut self) -> Self {
        if let Some(out) = self.output_size {
            if self.original_size > 0 {
                self.ratio = Some(out as f32 / self.original_size as f32);
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ratio_is_output_over_original() {
        let r = Report {
            original_size: 100,
            output_size: Some(40),
            ..Default::default()
        }
        .with_ratio();
        assert_eq!(r.ratio, Some(0.4));
    }
    #[test]
    fn ratio_none_when_no_output() {
        let r = Report {
            original_size: 100,
            ..Default::default()
        }
        .with_ratio();
        assert_eq!(r.ratio, None);
    }
    #[test]
    fn warnings_render_human_readable() {
        assert!(Warning::SignedDocument.to_string().contains("firmado"));
        assert!(Warning::ImageSkipped(7).to_string().contains('7'));
        assert_eq!(
            Warning::Other("texto libre".into()).to_string(),
            "texto libre"
        );
    }
}
