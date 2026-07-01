//! Re-encode sin pérdida a FlateDecode (P1-Flate, v2.0).
//!
//! Para imágenes de línea/texto (clasificadas `LineArt`) nunca usamos JPEG: la
//! DCT crea halos en los bordes nítidos. En su lugar tomamos los píxeles crudos
//! (posiblemente ya downsampleados) y los deflateamos con zlib, produciendo un
//! stream FlateDecode sin pérdida.
//!
//! Este es el hermano sin pérdida del `JpegRecompressor`: en vez de un
//! `&'static str` de filtro fijo, devuelve también el nombre del `ColorSpace`
//! y `BitsPerComponent` que el dict de salida debe declarar, porque a
//! diferencia del path JPEG (siempre DeviceRGB) aquí preservamos gris como
//! gris (1 canal) para no inflar los escaneos.

use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::Write;

/// Resultado de un re-encode Flate sin pérdida: bytes zlib + los metadatos del
/// dict de imagen que deben escribirse.
pub(crate) struct FlateEncoded {
    pub bytes: Vec<u8>,
    /// Nombre del ColorSpace PDF: `DeviceRGB` o `DeviceGray`.
    pub color_space: &'static str,
}

/// Deflatea los píxeles crudos de `img` como un stream FlateDecode sin pérdida.
///
/// Preserva el colorspace: si la imagen es gris (Luma8) emite 1 canal
/// (`DeviceGray`); cualquier otra cosa se normaliza a `DeviceRGB` (3 canales).
/// `BitsPerComponent` es siempre 8. Devuelve `None` si el encoder zlib falla
/// (no debería con memoria).
pub(crate) fn encode_flate_lossless(img: &image::DynamicImage) -> Option<FlateEncoded> {
    // Elegimos canales según el tipo real de la imagen decodificada. Gray se
    // mantiene gris (no lo inflamos a RGB como haría el path JPEG).
    let (raw, color_space): (Vec<u8>, &'static str) = match img {
        image::DynamicImage::ImageLuma8(g) => (g.as_raw().clone(), "DeviceGray"),
        image::DynamicImage::ImageRgb8(r) => (r.as_raw().clone(), "DeviceRGB"),
        other => (other.to_rgb8().into_raw(), "DeviceRGB"),
    };

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(&raw).ok()?;
    let bytes = encoder.finish().ok()?;

    Some(FlateEncoded { bytes, color_space })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_opt::decode::decode_flate_image;
    use lopdf::{dictionary, Stream};

    #[test]
    fn rgb_roundtrips_through_flate() {
        // Codificamos una RGB conocida, luego la volvemos a decodificar y debe
        // coincidir píxel a píxel (sin pérdida).
        let mut src = image::RgbImage::new(9, 7);
        for (x, y, px) in src.enumerate_pixels_mut() {
            *px = image::Rgb([x as u8, y as u8, (x + y) as u8]);
        }
        let dynimg = image::DynamicImage::ImageRgb8(src.clone());
        let enc = encode_flate_lossless(&dynimg).unwrap();
        assert_eq!(enc.color_space, "DeviceRGB");

        // reconstruimos un stream FlateDecode y lo decodificamos con el módulo real.
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 9, "Height" => 7,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
            },
            enc.bytes,
        );
        let back = decode_flate_image(&s, 9, 7).unwrap().to_rgb8();
        assert_eq!(back.as_raw(), src.as_raw(), "round-trip sin pérdida");
    }

    #[test]
    fn gray_stays_gray() {
        let g = image::GrayImage::from_pixel(4, 4, image::Luma([30]));
        let dynimg = image::DynamicImage::ImageLuma8(g);
        let enc = encode_flate_lossless(&dynimg).unwrap();
        assert_eq!(enc.color_space, "DeviceGray");
    }
}
