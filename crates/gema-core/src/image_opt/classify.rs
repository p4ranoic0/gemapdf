//! Clasificación de contenido: foto vs línea/texto (P1-Flate, v2.0).
//!
//! Decisión de diseño (usuario): **content-aware**. Nunca convertir línea/texto
//! sin pérdida en JPEG con pérdida — la DCT crea halos alrededor de los bordes
//! nítidos del texto (pesado y borroso). Sólo las fotos (tono continuo) se
//! recomprimen a JPEG; el resto se mantiene sin pérdida (Flate).
//!
//! La heurística es barata y **conservadora: ante la duda, LineArt** (sin
//! pérdida). Muestreamos la imagen (cada N píxeles para acotar el coste),
//! cuantizamos a 5 bits por canal y contamos colores distintos. Una foto de
//! tono continuo tiene *muchos* colores distintos; texto/línea/diagramas están
//! dominados por unos pocos. Sólo clasificamos **Photo** cuando hay claramente
//! muchísimos colores distintos.

/// Tipo de contenido de una imagen, que decide el codec de salida.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Content {
    /// Tono continuo (foto) → JPEG con pérdida.
    Photo,
    /// Línea/texto/diagrama → sin pérdida (Flate).
    LineArt,
}

/// Umbral de colores distintos (muestreados, cuantizados a 5-bit/canal) por
/// encima del cual una imagen se considera foto. El espacio cuantizado tiene
/// 32³ = 32 768 celdas; exigir >4096 uniques garantiza variedad tonal real y
/// no sólo ruido en un puñado de tonos.
const PHOTO_UNIQUE_COLORS: usize = 4096;

/// Objetivo de cantidad de píxeles muestreados: acota el coste en imágenes
/// grandes. Se deriva un stride para no visitar más de ~esto.
const SAMPLE_TARGET: u64 = 200_000;

/// Clasifica una imagen decodificada como `Photo` o `LineArt`.
///
/// Muestrea con un stride derivado del tamaño para no recorrer más de
/// ~`SAMPLE_TARGET` píxeles. Cuenta colores distintos cuantizados a 5 bits por
/// canal. Devuelve `Photo` sólo si supera `PHOTO_UNIQUE_COLORS`; si no,
/// `LineArt` (sesgo a sin pérdida).
pub(crate) fn classify(img: &image::DynamicImage) -> Content {
    use image::GenericImageView;

    let (w, h) = img.dimensions();
    let total = w as u64 * h as u64;
    if total == 0 {
        return Content::LineArt;
    }

    // stride para muestrear ~SAMPLE_TARGET píxeles como máximo. Recorremos con
    // paso `stride` sobre el índice lineal de píxel.
    let stride = (total / SAMPLE_TARGET).max(1);

    // Conjunto de colores cuantizados vistos. 5 bits/canal → clave de 15 bits.
    let mut seen = std::collections::HashSet::new();
    let rgb = img.to_rgb8();
    let raw = rgb.as_raw(); // W*H*3, RGB8

    let px_count = (w as u64) * (h as u64);
    let mut i: u64 = 0;
    while i < px_count {
        let base = (i as usize) * 3;
        // cuantiza a 5 bits por canal (>>3) y empaqueta en 15 bits.
        let r = (raw[base] >> 3) as u16;
        let g = (raw[base + 1] >> 3) as u16;
        let b = (raw[base + 2] >> 3) as u16;
        let key = (r << 10) | (g << 5) | b;
        seen.insert(key);
        // corto circuito: en cuanto superamos el umbral, ya es Photo.
        if seen.len() > PHOTO_UNIQUE_COLORS {
            return Content::Photo;
        }
        i += stride;
    }

    Content::LineArt
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Imagen de tono continuo tipo foto: cada canal varía de forma
    /// (semi-)independiente para producir muchos colores distintos, como una
    /// fotografía real (no un gradiente lineal de 2 ejes, que sólo daría ~1024
    /// tonos). Combina ondas de distinta frecuencia por canal.
    fn gradient(w: u32, h: u32) -> image::DynamicImage {
        let mut img = image::RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let fx = x as f32;
            let fy = y as f32;
            let r = ((fx * 0.09).sin() * 0.5 + 0.5) * 255.0;
            let g = ((fy * 0.07 + fx * 0.013).cos() * 0.5 + 0.5) * 255.0;
            let b = (((fx + fy) * 0.05).sin() * 0.5 + 0.5) * 255.0;
            *px = image::Rgb([r as u8, g as u8, b as u8]);
        }
        image::DynamicImage::ImageRgb8(img)
    }

    /// Imagen de 2 tonos (texto negro sobre blanco) → línea → LineArt.
    fn two_tone(w: u32, h: u32) -> image::DynamicImage {
        let mut img = image::RgbImage::new(w, h);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            // franjas: simula trazos de texto/línea.
            *px = if x % 7 < 2 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            };
        }
        image::DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn gradient_is_photo() {
        assert_eq!(classify(&gradient(512, 512)), Content::Photo);
    }

    #[test]
    fn two_tone_is_line_art() {
        assert_eq!(classify(&two_tone(512, 512)), Content::LineArt);
    }

    #[test]
    fn solid_is_line_art() {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            256,
            256,
            image::Rgb([90, 90, 90]),
        ));
        assert_eq!(classify(&img), Content::LineArt);
    }

    #[test]
    fn grayscale_gradient_stays_line_art() {
        // Un gradiente de grises tiene sólo 256 tonos distintos como máximo,
        // muy por debajo del umbral → LineArt (sesgo conservador). Correcto:
        // preferimos no meterle DCT a algo que podría ser un degradado de fondo.
        let mut img = image::GrayImage::new(512, 512);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = image::Luma([((x * 255) / 512) as u8]);
        }
        let dynimg = image::DynamicImage::ImageLuma8(img);
        assert_eq!(classify(&dynimg), Content::LineArt);
    }
}
