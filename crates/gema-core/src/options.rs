#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Profile {
    Screen,
    Ebook,
    Printer,
    /// Usa valores base tipo Ebook; pensado para sobreescribirse vía
    /// `image_dpi`/`jpeg_quality` en `CompressOptions`.
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SignaturePolicy {
    /// Si el documento está firmado criptográficamente, no se recomprime nada.
    Strict,
    /// Comprime de todos modos (rompe la firma criptográfica).
    Ignore,
}

/// Parámetros resueltos de un perfil.
#[derive(Debug, Clone, Copy)]
pub struct ProfileParams {
    pub image_dpi: u32,
    pub jpeg_quality: u8,
}

#[derive(Debug, Clone)]
pub struct CompressOptions {
    pub profile: Profile,
    pub image_dpi: Option<u32>,
    pub jpeg_quality: Option<u8>,
    pub downsample: bool,
    pub recompress_streams: bool,
    pub remove_metadata: bool,
    pub dedupe_images: bool,
    pub signatures: SignaturePolicy,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            profile: Profile::Ebook,
            image_dpi: None,
            jpeg_quality: None,
            downsample: true,
            recompress_streams: true,
            remove_metadata: true,
            dedupe_images: true,
            signatures: SignaturePolicy::Strict,
        }
    }
}

impl CompressOptions {
    /// Resuelve DPI y calidad finales (override > perfil).
    pub fn resolved(&self) -> ProfileParams {
        let base = match self.profile {
            Profile::Screen => ProfileParams { image_dpi: 72, jpeg_quality: 40 },
            Profile::Ebook => ProfileParams { image_dpi: 150, jpeg_quality: 65 },
            Profile::Printer => ProfileParams { image_dpi: 300, jpeg_quality: 80 },
            Profile::Custom => ProfileParams { image_dpi: 150, jpeg_quality: 65 },
        };
        ProfileParams {
            image_dpi: self.image_dpi.unwrap_or(base.image_dpi),
            jpeg_quality: self.jpeg_quality.unwrap_or(base.jpeg_quality),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ebook_defaults() {
        let p = CompressOptions::default().resolved();
        assert_eq!(p.image_dpi, 150);
        assert_eq!(p.jpeg_quality, 65);
    }
    #[test]
    fn override_wins_over_profile() {
        let opts = CompressOptions { profile: Profile::Screen, image_dpi: Some(120), ..Default::default() };
        assert_eq!(opts.resolved().image_dpi, 120);
        assert_eq!(opts.resolved().jpeg_quality, 40); // del perfil Screen
    }
}
