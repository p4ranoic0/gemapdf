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

/// Calcula el ancho/alto objetivo para no exceder `target_dpi` dado el tamaño
/// físico de la imagen en la página (en puntos PDF, 72 pt = 1 pulgada).
/// Devuelve None si la imagen ya está por debajo del DPI objetivo.
pub fn target_dimensions(
    px_w: u32,
    px_h: u32,
    display_w_pt: f32,
    display_h_pt: f32,
    target_dpi: u32,
) -> Option<(u32, u32)> {
    let display_w_in = display_w_pt / 72.0;
    let display_h_in = display_h_pt / 72.0;
    if display_w_in <= 0.0 || display_h_in <= 0.0 {
        return None;
    }
    let cur_dpi_w = px_w as f32 / display_w_in;
    let cur_dpi_h = px_h as f32 / display_h_in;
    let cur_dpi = cur_dpi_w.max(cur_dpi_h);
    if cur_dpi <= target_dpi as f32 {
        return None; // ya está por debajo del objetivo
    }
    let scale = target_dpi as f32 / cur_dpi;
    let new_w = ((px_w as f32) * scale).round().max(1.0) as u32;
    let new_h = ((px_h as f32) * scale).round().max(1.0) as u32;
    Some((new_w, new_h))
}

/// Remuestrea con Lanczos3 (alta calidad).
pub fn downsample(img: &image::DynamicImage, w: u32, h: u32) -> image::DynamicImage {
    img.resize_exact(w, h, image::imageops::FilterType::Lanczos3)
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

    #[test]
    fn downsamples_when_over_dpi() {
        // 1200px en 2 pulgadas (144pt) = 600 DPI; objetivo 150 → escala 0.25
        let dims = target_dimensions(1200, 1200, 144.0, 144.0, 150).unwrap();
        assert_eq!(dims, (300, 300));
    }
    #[test]
    fn no_downsample_when_under_dpi() {
        // 200px en 2 pulgadas = 100 DPI; objetivo 150 → None
        assert_eq!(target_dimensions(200, 200, 144.0, 144.0, 150), None);
    }
}
