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

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub pages: usize,
    pub original_size: u64,
    pub output_size: Option<u64>,
    pub ratio: Option<f32>,
    pub images: Vec<ImageStat>,
    pub is_signed: bool,
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
        let r = Report { original_size: 100, output_size: Some(40), ..Default::default() }.with_ratio();
        assert_eq!(r.ratio, Some(0.4));
    }
    #[test]
    fn ratio_none_when_no_output() {
        let r = Report { original_size: 100, ..Default::default() }.with_ratio();
        assert_eq!(r.ratio, None);
    }
}
