pub mod jpeg;

/// Imagen decodificada lista para recomprimir.
pub struct RawImage {
    pub image: image::DynamicImage,
}

/// Resultado de recompresión: bytes + el filtro PDF correspondiente.
pub struct Encoded {
    pub bytes: Vec<u8>,
    /// Nombre del filtro PDF: "DCTDecode" para JPEG, "FlateDecode" para PNG/raw.
    pub filter: &'static str,
}

/// Backend de recompresión de imágenes. Permite enchufar codecs nativos en v2.
pub trait Recompressor {
    /// Recomprime la imagen al `quality` dado (1..=100). Devuelve None si no aplica.
    fn recompress(&self, raw: &RawImage, quality: u8) -> Option<Encoded>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::jpeg::JpegRecompressor;

    fn solid_image() -> RawImage {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(64, 64, image::Rgb([120, 30, 200])));
        RawImage { image: img }
    }

    #[test]
    fn jpeg_backend_produces_dctdecode_bytes() {
        let enc = JpegRecompressor.recompress(&solid_image(), 60).unwrap();
        assert_eq!(enc.filter, "DCTDecode");
        assert!(!enc.bytes.is_empty());
        // cabecera JPEG SOI
        assert_eq!(&enc.bytes[0..2], &[0xFF, 0xD8]);
    }
}
