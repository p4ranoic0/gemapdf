//! Decodificación de imágenes FlateDecode a píxeles crudos (P1-Flate, v2.0).
//!
//! Muchas imágenes de escaneos y de generadores PDF no vienen en DCTDecode
//! (JPEG) sino en FlateDecode: el stream es zlib sobre los bytes de píxel
//! crudos. `image::load_from_memory` no abre ese blob, así que hasta v1 estas
//! imágenes se saltaban. Este módulo las descomprime y las interpreta según su
//! `ColorSpace`/`BitsPerComponent`, produciendo un `DynamicImage` que el resto
//! del pipeline puede downsamplear y recomprimir.
//!
//! **Alcance deliberadamente acotado — corrección antes que cobertura.** Sólo
//! decodificamos los casos que podemos reconstruir *exactamente*; cualquier
//! otra cosa devuelve `None` para que el pipeline la marque `Skipped` (seguro,
//! sin corromper), igual que hoy:
//!
//! - **Guarda de predictor (crítica):** si el dict del stream o sus
//!   `/DecodeParms` traen `/Predictor` != 1, SALTAMOS. Los datos predichos
//!   (PNG/TIFF) necesitan un des-filtrado que no está en el alcance de este
//!   paso; intentar leerlos como crudos garabatearía los píxeles.
//! - **ColorSpace/BitsPerComponent soportados:** sólo `DeviceRGB`/8 →
//!   `RgbImage` y `DeviceGray`/8 → `GrayImage`. Indexed, ICCBased, CMYK,
//!   1/2/4-bpc y colorspaces por array/indirectos → SALTAMOS.
//! - **Validación de longitud:** tras inflar, la longitud debe ser exactamente
//!   `W*H*canales`. Si no coincide (stream malformado o interpretación errónea)
//!   SALTAMOS en vez de adivinar.
//!
//! Nunca hace panic ante entrada hostil.

use flate2::read::ZlibDecoder;
use lopdf::{Object, Stream};
use std::io::Read;

/// Colorspaces soportados por el decodificador Flate de v2.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlateColor {
    /// `DeviceRGB`, 3 canales.
    Rgb,
    /// `DeviceGray`, 1 canal.
    Gray,
}

impl FlateColor {
    fn channels(self) -> usize {
        match self {
            FlateColor::Rgb => 3,
            FlateColor::Gray => 1,
        }
    }
}

/// ¿El stream usa un filtro FlateDecode (`FlateDecode` o su abreviatura `Fl`)?
/// El `/Filter` puede ser un nombre o un array de nombres (cadena de filtros).
/// Sólo aceptamos el caso en que Flate es el **único** filtro: una cadena como
/// `[ASCII85Decode FlateDecode]` requeriría des-encadenar y queda fuera de
/// alcance (SKIP).
fn is_flate_only(dict: &lopdf::Dictionary) -> bool {
    match dict.get(b"Filter") {
        Ok(Object::Name(n)) => n == b"FlateDecode" || n == b"Fl",
        Ok(Object::Array(arr)) => {
            arr.len() == 1
                && matches!(arr.first(), Some(Object::Name(n)) if n == b"FlateDecode" || n == b"Fl")
        }
        _ => false,
    }
}

/// ¿Hay un `/Predictor` != 1 en el dict o en sus `/DecodeParms`? En PDF el
/// predictor vive normalmente en `/DecodeParms` (o `/DP`), pero algunos
/// productores lo ponen suelto en el dict del stream. Cualquier valor distinto
/// de 1 (o ausente = 1) implica des-filtrado PNG/TIFF → fuera de alcance.
///
/// `/DecodeParms` puede ser un diccionario o un array de diccionarios (uno por
/// filtro); revisamos todos. Un `/DecodeParms` que sea una referencia indirecta
/// no lo resolvemos aquí (no tenemos el `Document`): lo tratamos como "podría
/// tener predictor" → devolvemos `true` (SKIP conservador).
fn has_predictor(dict: &lopdf::Dictionary) -> bool {
    fn parms_has_predictor(obj: &Object) -> bool {
        match obj {
            Object::Dictionary(d) => match d.get(b"Predictor") {
                Ok(p) => p.as_i64().map(|v| v != 1).unwrap_or(true),
                Err(_) => false,
            },
            Object::Array(arr) => arr.iter().any(parms_has_predictor),
            // referencia indirecta sin resolver → no podemos descartar predictor
            Object::Reference(_) => true,
            _ => false,
        }
    }

    // predictor suelto en el dict del stream
    if let Ok(p) = dict.get(b"Predictor") {
        if p.as_i64().map(|v| v != 1).unwrap_or(true) {
            return true;
        }
    }
    // predictor en DecodeParms / DP
    for key in [b"DecodeParms".as_slice(), b"DP".as_slice()] {
        if let Ok(parms) = dict.get(key) {
            if parms_has_predictor(parms) {
                return true;
            }
        }
    }
    false
}

/// Lee el colorspace soportado del dict, o `None` si no es uno de los dos que
/// manejamos (RGB/8, Gray/8). Un colorspace por array o referencia indirecta
/// (Indexed, ICCBased, …) devuelve `None` → SKIP.
fn supported_color(dict: &lopdf::Dictionary) -> Option<FlateColor> {
    // BitsPerComponent debe ser exactamente 8.
    let bpc = dict.get(b"BitsPerComponent").and_then(|o| o.as_i64()).ok()?;
    if bpc != 8 {
        return None;
    }
    // ColorSpace debe ser un Name simple; array/ref (Indexed, ICCBased, …) no.
    let cs = dict.get(b"ColorSpace").ok()?;
    match cs {
        Object::Name(n) if n == b"DeviceRGB" || n == b"RGB" => Some(FlateColor::Rgb),
        Object::Name(n) if n == b"DeviceGray" || n == b"G" => Some(FlateColor::Gray),
        _ => None,
    }
}

/// Techo de bytes descomprimidos permitidos (~805 MB para RGB 16 384×16 384).
/// Evita que un PDF malicioso con dimensiones enormes (p. ej. 100 000×100 000)
/// provoque una pre-reserva de ~30 GB con `Vec::with_capacity` antes de que el
/// `.take()` pueda acotar la lectura real.
const MAX_DECODE_BYTES: usize = 16_384 * 16_384 * 3; // ≈ 805 MB

/// Descomprime un stream de imagen FlateDecode a un `DynamicImage`, o devuelve
/// `None` si no es un caso soportado (predicho, colorspace/bpc no soportado,
/// filtro encadenado, o longitud descomprimida que no cuadra). Nunca hace
/// panic.
///
/// `width`/`height` son las dimensiones ya validadas (>0, sanas) que el
/// pipeline leyó del dict.
pub(crate) fn decode_flate_image(
    stream: &Stream,
    width: u32,
    height: u32,
) -> Option<image::DynamicImage> {
    let dict = &stream.dict;

    if !is_flate_only(dict) {
        return None;
    }
    // Guarda de predictor (crítica): datos predichos → SKIP.
    if has_predictor(dict) {
        return None;
    }
    let color = supported_color(dict)?;

    // longitud esperada de los píxeles crudos.
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(color.channels())?;

    // Techo de seguridad: rechaza imágenes cuyo tamaño descomprimido supera
    // MAX_DECODE_BYTES para evitar una pre-reserva gigante ante PDFs hostiles.
    if expected > MAX_DECODE_BYTES {
        return None;
    }

    // Inflar el zlib. Limitamos la lectura a `expected + 1` bytes: si el stream
    // produce más (o menos) de lo esperado lo detectamos y saltamos, y de paso
    // acotamos el uso de memoria ante entrada hostil.
    let mut decoder = ZlibDecoder::new(&stream.content[..]);
    let mut buf = Vec::with_capacity(expected);
    // Leemos hasta expected+1 para distinguir "exacto" de "más largo".
    let mut limited = (&mut decoder).take(expected as u64 + 1);
    if limited.read_to_end(&mut buf).is_err() {
        return None;
    }
    if buf.len() != expected {
        // longitud no coincide (malformado o mal interpretado) → SKIP, no adivinar.
        return None;
    }

    match color {
        FlateColor::Rgb => {
            image::RgbImage::from_raw(width, height, buf).map(image::DynamicImage::ImageRgb8)
        }
        FlateColor::Gray => {
            image::GrayImage::from_raw(width, height, buf).map(image::DynamicImage::ImageLuma8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use lopdf::{dictionary, Stream};
    use std::io::Write;

    fn zlib(raw: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(raw).unwrap();
        e.finish().unwrap()
    }

    fn rgb_flate_stream(w: u32, h: u32) -> Stream {
        let raw = vec![137u8; (w * h * 3) as usize];
        Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        )
    }

    #[test]
    fn decodes_devicergb8_to_right_dimensions() {
        let s = rgb_flate_stream(10, 12);
        let img = decode_flate_image(&s, 10, 12).expect("debe decodificar RGB/8");
        assert_eq!(img.width(), 10);
        assert_eq!(img.height(), 12);
        assert!(matches!(img, image::DynamicImage::ImageRgb8(_)));
    }

    #[test]
    fn decodes_devicegray8() {
        let raw = vec![42u8; 8 * 8];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 8, "Height" => 8,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceGray",
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        let img = decode_flate_image(&s, 8, 8).expect("debe decodificar Gray/8");
        assert!(matches!(img, image::DynamicImage::ImageLuma8(_)));
        assert_eq!((img.width(), img.height()), (8, 8));
    }

    #[test]
    fn refuses_predicted_image_in_decodeparms() {
        // Predictor 15 (PNG optimum) en DecodeParms → SKIP aunque el resto cuadre.
        let raw = vec![1u8; 10 * 10 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 10, "Height" => 10,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                "DecodeParms" => dictionary! { "Predictor" => 15, "Colors" => 3, "Columns" => 10 },
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 10, 10).is_none(), "predicho debe saltarse");
    }

    #[test]
    fn refuses_predictor_loose_in_dict() {
        let raw = vec![1u8; 4 * 4 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode", "Predictor" => 2,
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 4, 4).is_none());
    }

    #[test]
    fn refuses_length_mismatch() {
        // El stream infla a 10*10*3 pero decimos que es 12*12 → longitud no cuadra.
        let raw = vec![7u8; 10 * 10 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 12, "Height" => 12,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 12, 12).is_none(), "longitud no coincide → skip");
    }

    #[test]
    fn refuses_unsupported_colorspace_indexed() {
        let raw = vec![0u8; 6 * 6];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 6, "Height" => 6,
                "BitsPerComponent" => 8,
                // Indexed viene como array → no soportado.
                "ColorSpace" => vec![Object::Name(b"Indexed".to_vec())],
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 6, 6).is_none());
    }

    #[test]
    fn refuses_non_8_bpc() {
        let raw = vec![0u8; 8 * 8 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 8, "Height" => 8,
                "BitsPerComponent" => 4, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 8, 8).is_none());
    }

    #[test]
    fn refuses_non_flate_filter() {
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
            },
            vec![0u8; 16],
        );
        assert!(decode_flate_image(&s, 4, 4).is_none());
    }

    #[test]
    fn refuses_chained_filter_array() {
        // [ASCII85Decode FlateDecode] → cadena de filtros, fuera de alcance.
        let raw = vec![1u8; 4 * 4 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"ASCII85Decode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 4, 4).is_none());
    }

    #[test]
    fn does_not_panic_on_garbage_zlib() {
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
            },
            vec![0xFF, 0x00, 0x13, 0x37, 0xAB],
        );
        assert!(decode_flate_image(&s, 4, 4).is_none());
    }

    /// Verifica que dimensiones enormes (100 000×100 000, DeviceRGB) se rechacen
    /// *antes* de cualquier asignación gigante: expected ≈ 30 GB > MAX_DECODE_BYTES.
    /// El stream tiene contenido vacío; la función debe devolver `None` inmediatamente.
    #[test]
    fn refuses_oversized_dimensions() {
        let s = Stream::new(
            dictionary! {
                "Type"             => "XObject",
                "Subtype"          => "Image",
                "Width"            => 100_000_i64,
                "Height"           => 100_000_i64,
                "BitsPerComponent" => 8,
                "ColorSpace"       => "DeviceRGB",
                "Filter"           => "FlateDecode",
            },
            vec![], // contenido vacío; jamás llega a descomprimirse
        );
        assert!(
            decode_flate_image(&s, 100_000, 100_000).is_none(),
            "debe rechazar dimensiones que superan MAX_DECODE_BYTES"
        );
    }
}
