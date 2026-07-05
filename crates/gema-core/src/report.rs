#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageAction {
    Kept,
    Recompressed,
    Downsampled,
    Skipped,
}

#[derive(Debug, Clone)]
pub struct ImageStat {
    pub object_id: u32,
    pub original_bytes: u64,
    pub output_bytes: u64,
    pub action: ImageAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    SignedDocument,
    ImageSkipped(u32),
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

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub pages: usize,
    pub original_size: u64,
    pub output_size: Option<u64>,
    pub ratio: Option<f32>,
    pub images: Vec<ImageStat>,
    pub is_signed: bool,
    /// No se computa en v1: reservado para el módulo de OCR de v2. Actualmente
    /// siempre es `false`.
    pub has_scanned_pages: bool,
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
