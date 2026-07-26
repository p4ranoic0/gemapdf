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

/// Fracción mínima de "grano" para considerar que una imagen es un raster
/// capturado (escaneo) y no arte sintético. Medido sobre el corpus real
/// (`doc-B2`, 8 páginas escaneadas completas): grano 0.336-0.422.
/// El único sintético grande del corpus (firma institucional 1436×340) da
/// 0.091. El umbral parte esa separación con margen a ambos lados.
const PHOTO_GRAIN: f32 = 0.20;

/// Lado menor mínimo para que el grano pueda decidir. Los emblemas y sellos
/// vectoriales chicos, al reducirse, generan tanto grano como un escaneo (el
/// escudo del Perú a 110×112: 0.356) — la señal no los separa, así que debajo
/// de este tamaño mandamos el sesgo conservador del módulo. Las páginas
/// escaneadas del corpus miden ~1000×1570; el mayor sintético medido, 1436×340.
const GRAIN_MIN_SIDE: u32 = 400;

/// Diferencia mínima entre vecinos para contar como grano. Por debajo de 2 el
/// ruido de cuantización y las rampas suaves (un degradado avanza de a 1)
/// producirían falsos positivos.
const GRAIN_MIN_DELTA: i16 = 2;

/// Diferencia máxima para contar como grano. Un salto mayor es un BORDE (texto
/// sobre fondo, dithering bilevel), no ruido de sensor: excluirlo es lo que
/// impide que el arte de línea nítido dispare esta señal.
const GRAIN_MAX_DELTA: i16 = 32;

/// Luma entera aproximada (Rec.601) de un píxel RGB8.
fn luma(px: &[u8]) -> i16 {
    ((77 * px[0] as i32 + 151 * px[1] as i32 + 28 * px[2] as i32) >> 8) as i16
}

/// Fracción de píxeles horizontalmente adyacentes cuya diferencia de luma cae
/// en `[GRAIN_MIN_DELTA, GRAIN_MAX_DELTA)`.
///
/// Es la huella del ruido del sensor: un escaneo la tiene en toda la superficie
/// (no existe una sola región perfectamente plana), mientras que el arte
/// sintético alterna regiones exactamente constantes (diferencia 0) con bordes
/// duros (diferencia grande), y ninguna de las dos cuenta.
///
/// Muestrea filas completas con un stride derivado del tamaño: la adyacencia
/// horizontal se conserva intacta, y el coste queda acotado como en `classify`.
fn grain(raw: &[u8], w: u32, h: u32) -> f32 {
    if w < 2 || h == 0 {
        return 0.0;
    }
    let row_stride = (((w as u64) * (h as u64)) / SAMPLE_TARGET).max(1) as u32;

    let mut pairs: u64 = 0;
    let mut grainy: u64 = 0;
    let mut y = 0;
    while y < h {
        let row = (y as usize) * (w as usize) * 3;
        for x in 0..(w as usize - 1) {
            let d = (luma(&raw[row + x * 3..]) - luma(&raw[row + (x + 1) * 3..])).abs();
            if (GRAIN_MIN_DELTA..GRAIN_MAX_DELTA).contains(&d) {
                grainy += 1;
            }
            pairs += 1;
        }
        y += row_stride;
    }

    if pairs == 0 {
        0.0
    } else {
        grainy as f32 / pairs as f32
    }
}

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

    // Tonalmente pobre, pero eso no basta para llamarlo línea: un escaneo de
    // papel también lo es. Si es lo bastante grande para ser una página
    // capturada y tiene grano de sensor, va a JPEG.
    if w.min(h) >= GRAIN_MIN_SIDE && grain(raw, w, h) >= PHOTO_GRAIN {
        return Content::Photo;
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

    /// Generador determinista de ruido (LCG): los fixtures no dependen de
    /// `rand` ni varían entre corridas.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }

        /// Entero en `[-amp, amp]`.
        fn noise(&mut self, amp: i16) -> i16 {
            (self.next() >> 33) as i16 % (2 * amp + 1) - amp
        }
    }

    /// Papel escaneado: fondo casi blanco con RUIDO de sensor en todos lados y
    /// renglones de texto oscuros. Es tonalmente pobre (pocos colores distintos
    /// → la heurística de colores lo da por LineArt) pero NO es línea sintética:
    /// no tiene ni una región perfectamente plana. Reproduce lo medido sobre los
    /// escaneos reales del corpus (`doc-B2`: 62 colores
    /// cuantizados, grano 0.34-0.42).
    fn scanned_paper(w: u32, h: u32) -> image::DynamicImage {
        let mut rng = Lcg(0x5EED);
        let mut img = image::RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            // renglones de texto: 3 filas oscuras cada 16, con huecos entre
            // "palabras" para que no sea una franja continua.
            let on_text_row = (y % 16) < 3;
            let in_word = (x / 37) % 4 != 3;
            let base: i16 = if on_text_row && in_word { 70 } else { 242 };
            let v = (base + rng.noise(5)).clamp(0, 255) as u8;
            *px = image::Rgb([v, v, v]);
        }
        image::DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn gradient_is_photo() {
        assert_eq!(classify(&gradient(512, 512)), Content::Photo);
    }

    /// Un escaneo de papel debe ir a JPEG. Con la heurística de sólo-colores
    /// caía en LineArt y se quedaba en Flate sin pérdida, que es la brecha de
    /// ~30 MB contra Ghostscript en los documentos cuyos escaneos vienen en
    /// Flate en vez de DCT.
    #[test]
    fn scanned_paper_is_photo() {
        assert_eq!(classify(&scanned_paper(512, 512)), Content::Photo);
    }

    /// El grano NO distingue un escaneo de un emblema vectorial chico: medido
    /// sobre el corpus, el escudo del Perú a 110×112 da grano 0.356, dentro del
    /// rango de los escaneos reales (0.336-0.422) — al reducirse, cada borde se
    /// vuelve una rampa suave de saltos chicos. Por debajo del gate de tamaño
    /// mandamos el sesgo conservador del módulo (ante la duda, sin pérdida).
    /// Esas imágenes además pesan poco y el heurístico de sellos ya las preserva.
    #[test]
    fn small_grainy_image_stays_line_art() {
        assert_eq!(classify(&scanned_paper(200, 200)), Content::LineArt);
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
