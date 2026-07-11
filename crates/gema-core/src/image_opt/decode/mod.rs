//! Decodificación de imágenes FlateDecode a píxeles crudos (P1-Flate + P1b, v2.0).
//!
//! Muchas imágenes de escaneos y de generadores PDF no vienen en DCTDecode
//! (JPEG) sino en FlateDecode: el stream es zlib sobre los bytes de píxel
//! crudos. `image::load_from_memory` no abre ese blob, así que hasta v1 estas
//! imágenes se saltaban. Este módulo las descomprime y las interpreta según su
//! `ColorSpace`/`BitsPerComponent`, produciendo un `DynamicImage` que el resto
//! del pipeline puede downsamplear y recomprimir.
//!
//! El trabajo se reparte en tres piezas:
//! - [`filters`]: la cadena `/Filter` y los decodificadores texto-a-binario
//!   (ASCIIHex/ASCII85/RunLength) + inflado zlib.
//! - [`predictor`]: el des-filtrado PNG/TIFF que se aplica tras inflar Flate.
//! - `decode_flate_image` (aquí): orquesta filtro → colorspace → validación.
//!
//! **P1b amplía la cobertura a dos casos antes saltados, sin perder exactitud:**
//!
//! - **Cadenas de filtros** (`/Filter` como array, p. ej.
//!   `[ASCII85Decode FlateDecode]`): des-encadenamos aplicando cada filtro de
//!   izquierda a derecha. Soportamos `ASCIIHexDecode`, `ASCII85Decode`,
//!   `FlateDecode` y `RunLengthDecode` (todos Rust puro, WASM-safe). Cualquier
//!   otro filtro en la cadena (LZW, DCT/JPX/CCITT/JBIG2 intermedios) → SALTAMOS.
//! - **Predictores PNG/TIFF** (`/Predictor` en los `/DecodeParms` de la etapa
//!   Flate): tras inflar aplicamos el des-filtrado exacto — TIFF (predictor 2,
//!   sólo 8-bpc) y PNG (10–15: None/Sub/Up/Average/Paeth). Un des-filtrado
//!   equivocado garabatearía píxeles, así que validamos longitudes y saltamos
//!   ante cualquier caso fuera de alcance en vez de adivinar.
//!
//! **Alcance deliberadamente acotado — corrección antes que cobertura.** Sólo
//! decodificamos los casos que podemos reconstruir *exactamente*; cualquier
//! otra cosa devuelve `None` para que el pipeline la marque `Skipped` (seguro,
//! sin corromper):
//!
//! - **ColorSpace/BitsPerComponent soportados (P1c amplía la cobertura):**
//!   - `DeviceRGB`/8 → `RgbImage`, `DeviceGray`/8 → `GrayImage`.
//!   - `DeviceCMYK`/8 → `RgbImage` con la fórmula estándar
//!     `R = round((255-C)*(255-K)/255)` (sin inversión Adobe: eso es sólo de
//!     DCTDecode, que va por el path de `image`, no por aquí).
//!   - `ICCBased` → se interpreta como el device-space de su `/N`
//!     (1→Gray, 3→RGB, 4→CMYK); ignoramos el perfil (comprimimos, no gestionamos
//!     color). `/N` ausente o ∉{1,3,4} → SALTAMOS.
//!   - `Indexed` (paleta) con índice de **8 bpc** y base DeviceRGB/DeviceGray/
//!     ICCBased(N=1|3): expandimos cada índice a su entrada de paleta (validando
//!     índice ≤ hival y contra los límites, sin lecturas OOB). Base CMYK e índice
//!     sub-byte (1/2/4 bpc) → SALTAMOS.
//!   - Separation/DeviceN, Lab, CalRGB/CalGray, Pattern y demás → SALTAMOS.
//! - **Validación de longitud:** tras des-filtrar, la longitud debe ser
//!   exactamente `W*H*canales` (o `W*H` para Indexed de 8 bpc), y la paleta
//!   `(hival+1)*canales_base`. Si no coincide (stream malformado o
//!   interpretación errónea) SALTAMOS en vez de adivinar. El techo
//!   `MAX_DECODE_BYTES` se valida contra la salida *expandida*, no sólo la entrada.
//!
//! Nunca hace panic ante entrada hostil.

use lopdf::{Document, Stream};

use super::colorspace::{
    device_buffer_to_image, indexed_to_image, interpret_color_space, ColorSpaceKind, FlateColor,
};

mod filters;
mod predictor;

// Re-exportado para `colorspace.rs`, que des-encadena filtros al resolver
// paletas/perfiles vía referencia indirecta (`super::decode::{...}`).
pub(in crate::image_opt) use filters::{apply_filter, decode_parms_for, filter_chain, Filter};
#[allow(unused_imports)] // Re-exportado para Task 2
pub(in crate::image_opt) use filters::unwrap_to_dct;

/// Techo de bytes descomprimidos permitidos (~805 MB para RGB 16 384×16 384).
/// Evita que un PDF malicioso con dimensiones enormes (p. ej. 100 000×100 000)
/// provoque una pre-reserva de ~30 GB antes de que la validación de longitud
/// pueda descartarlo.
pub(in crate::image_opt) const MAX_DECODE_BYTES: usize = 16_384 * 16_384 * 3; // ≈ 805 MB

/// Descomprime un stream de imagen a un `DynamicImage`, o devuelve `None` si no
/// es un caso soportado (filtro no soportado en la cadena, colorspace/bpc no
/// soportado, predictor fuera de alcance, o longitud que no cuadra). Nunca hace
/// panic.
///
/// `doc` se usa sólo para resolver referencias indirectas del `/ColorSpace`
/// (Indexed base/lookup, ICCBased stream). `width`/`height` son las dimensiones
/// ya validadas (>0, sanas) que el pipeline leyó del dict.
pub(crate) fn decode_flate_image(
    doc: &Document,
    stream: &Stream,
    width: u32,
    height: u32,
) -> Option<image::DynamicImage> {
    let dict = &stream.dict;

    // La cadena de filtros debe existir y ser toda soportada (si no → SKIP).
    let chain = filter_chain(dict)?;
    // Debe contener exactamente una etapa Flate (nuestra fuente de píxeles). Sin
    // Flate no sabemos interpretar los bytes; con más de una no tiene sentido.
    if chain.iter().filter(|f| **f == Filter::Flate).count() != 1 {
        return None;
    }

    let bpc = dict
        .get(b"BitsPerComponent")
        .and_then(|o| o.as_i64())
        .ok()?;
    let kind = interpret_color_space(doc, dict, bpc)?;

    // longitud esperada de los datos crudos de imagen (antes de expandir paleta)
    // y de la salida final expandida. Validamos AMBOS contra MAX_DECODE_BYTES:
    // Indexed expande 1 byte de entrada a hasta 3 de salida, y CMYK 4→3.
    let px = (width as usize).checked_mul(height as usize)?;
    let (raw_expected, out_expected) = match &kind {
        ColorSpaceKind::Device(c) => {
            let n = px.checked_mul(c.channels())?;
            // La salida RGB de CMYK es px*3; Gray px*1; RGB px*3.
            let out = match c {
                FlateColor::Gray => px,
                FlateColor::Rgb | FlateColor::Cmyk => px.checked_mul(3)?,
            };
            (n, out)
        }
        ColorSpaceKind::Indexed { base, .. } => {
            // 1 índice (byte) por píxel → salida = px * base.channels() (RGB o Gray).
            let out = px.checked_mul(base.channels())?;
            (px, out)
        }
    };

    // Techo de seguridad: rechaza imágenes cuya entrada o salida supera el techo.
    if raw_expected > MAX_DECODE_BYTES || out_expected > MAX_DECODE_BYTES {
        return None;
    }

    // Des-encadenar: aplicar cada filtro de izquierda a derecha. El predictor
    // (si lo hay) se aplica dentro de `apply_filter` en la etapa Flate.
    let mut data = stream.content.clone();
    for (idx, filter) in chain.iter().enumerate() {
        let parms = decode_parms_for(dict, idx);
        data = apply_filter(*filter, &data, parms)?;
    }

    if data.len() != raw_expected {
        // longitud no coincide (malformado o mal interpretado) → SKIP, no adivinar.
        return None;
    }

    match kind {
        ColorSpaceKind::Device(color) => device_buffer_to_image(color, width, height, data),
        ColorSpaceKind::Indexed {
            base,
            hival,
            lookup,
        } => indexed_to_image(base, hival, &lookup, width, height, &data),
    }
}

#[cfg(test)]
mod tests {
    use super::filters::{decode_ascii85, decode_ascii_hex, decode_run_length};
    use super::predictor::paeth;
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use lopdf::{dictionary, Object, Stream};
    use std::io::Write;

    /// Documento vacío para tests que no usan referencias indirectas.
    fn empty_doc() -> lopdf::Document {
        lopdf::Document::new()
    }

    fn zlib(raw: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(raw).unwrap();
        e.finish().unwrap()
    }

    /// Codifica ASCII85 (con EOD `~>`), como haría un productor PDF.
    fn ascii85_encode(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in input.chunks(4) {
            let mut val: u32 = 0;
            for i in 0..4 {
                val <<= 8;
                if i < chunk.len() {
                    val |= chunk[i] as u32;
                }
            }
            let mut group = [0u8; 5];
            let mut v = val;
            for i in (0..5).rev() {
                group[i] = (v % 85) as u8 + b'!';
                v /= 85;
            }
            out.extend_from_slice(&group[..chunk.len() + 1]);
        }
        out.extend_from_slice(b"~>");
        out
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

    // ---- decodificación básica (sin predictor, sin cadena) ----

    #[test]
    fn decodes_devicergb8_to_right_dimensions() {
        let s = rgb_flate_stream(10, 12);
        let img = decode_flate_image(&empty_doc(), &s, 10, 12).expect("debe decodificar RGB/8");
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
        let img = decode_flate_image(&empty_doc(), &s, 8, 8).expect("debe decodificar Gray/8");
        assert!(matches!(img, image::DynamicImage::ImageLuma8(_)));
        assert_eq!((img.width(), img.height()), (8, 8));
    }

    // ---- predictores: round-trips exactos ----

    /// Genera una imagen RGB8 con un patrón determinista (no plano) para que los
    /// predictores tengan algo que diferenciar.
    fn sample_rgb(w: usize, h: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                v.push(((x * 7 + y * 3) % 256) as u8);
                v.push(((x * 13 + y * 5 + 11) % 256) as u8);
                v.push(((x * 3 + y * 17 + 200) % 256) as u8);
            }
        }
        v
    }

    /// Codifica una imagen RGB8 aplicando el filtro PNG `ftype` fila a fila
    /// (con byte de tipo por fila) y luego zlib. Espejo exacto del decoder.
    fn png_encode_rgb(pixels: &[u8], w: usize, h: usize, ftype: u8) -> Vec<u8> {
        let bpp = 3usize;
        let rl = w * 3;
        let mut filtered = Vec::with_capacity(h * (rl + 1));
        let zero = vec![0u8; rl];
        for y in 0..h {
            let cur = &pixels[y * rl..(y + 1) * rl];
            let prev: &[u8] = if y == 0 {
                &zero
            } else {
                &pixels[(y - 1) * rl..y * rl]
            };
            filtered.push(ftype);
            let mut row = vec![0u8; rl];
            for i in 0..rl {
                let a = if i >= bpp { cur[i - bpp] } else { 0 };
                let b = prev[i];
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                row[i] = match ftype {
                    0 => cur[i],
                    1 => cur[i].wrapping_sub(a),
                    2 => cur[i].wrapping_sub(b),
                    3 => cur[i].wrapping_sub(((a as u16 + b as u16) / 2) as u8),
                    4 => cur[i].wrapping_sub(paeth(a, b, c)),
                    _ => unreachable!(),
                };
            }
            filtered.extend_from_slice(&row);
        }
        zlib(&filtered)
    }

    fn png_predicted_stream(pixels: &[u8], w: u32, h: u32, ftype: u8) -> Stream {
        Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                "DecodeParms" => dictionary! {
                    "Predictor" => 15, "Colors" => 3, "BitsPerComponent" => 8, "Columns" => w as i64
                },
            },
            png_encode_rgb(pixels, w as usize, h as usize, ftype),
        )
    }

    fn assert_png_roundtrip(ftype: u8) {
        let (w, h) = (9usize, 7usize);
        let pixels = sample_rgb(w, h);
        let s = png_predicted_stream(&pixels, w as u32, h as u32, ftype);
        let img = decode_flate_image(&empty_doc(), &s, w as u32, h as u32)
            .unwrap_or_else(|| panic!("predictor PNG tipo {ftype} debe decodificar"));
        let back = img.to_rgb8();
        assert_eq!(
            back.as_raw(),
            &pixels,
            "PNG filtro {ftype}: píxeles no coinciden"
        );
    }

    #[test]
    fn png_predictor_none_roundtrip() {
        assert_png_roundtrip(0);
    }
    #[test]
    fn png_predictor_sub_roundtrip() {
        assert_png_roundtrip(1);
    }
    #[test]
    fn png_predictor_up_roundtrip() {
        assert_png_roundtrip(2);
    }
    #[test]
    fn png_predictor_average_roundtrip() {
        assert_png_roundtrip(3);
    }
    #[test]
    fn png_predictor_paeth_roundtrip() {
        assert_png_roundtrip(4);
    }

    /// Predictor 15 con filas de tipo mixto (cada fila un filtro distinto): así
    /// se ve el mundo real, donde el codificador "optimum" elige por fila.
    #[test]
    fn png_predictor_mixed_rows_roundtrip() {
        let (w, h) = (6usize, 5usize);
        let pixels = sample_rgb(w, h);
        let bpp = 3usize;
        let rl = w * 3;
        let mut filtered = Vec::new();
        let zero = vec![0u8; rl];
        for y in 0..h {
            let ftype = (y % 5) as u8; // 0,1,2,3,4,...
            let cur = &pixels[y * rl..(y + 1) * rl];
            let prev: &[u8] = if y == 0 {
                &zero
            } else {
                &pixels[(y - 1) * rl..y * rl]
            };
            filtered.push(ftype);
            for i in 0..rl {
                let a = if i >= bpp { cur[i - bpp] } else { 0 };
                let b = prev[i];
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                let enc = match ftype {
                    0 => cur[i],
                    1 => cur[i].wrapping_sub(a),
                    2 => cur[i].wrapping_sub(b),
                    3 => cur[i].wrapping_sub(((a as u16 + b as u16) / 2) as u8),
                    4 => cur[i].wrapping_sub(paeth(a, b, c)),
                    _ => unreachable!(),
                };
                filtered.push(enc);
            }
        }
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                "DecodeParms" => dictionary! { "Predictor" => 15, "Colors" => 3, "Columns" => w as i64 },
            },
            zlib(&filtered),
        );
        let img = decode_flate_image(&empty_doc(), &s, w as u32, h as u32)
            .expect("mixto debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &pixels);
    }

    /// Predictor 2 de TIFF (diferenciación horizontal), RGB8.
    #[test]
    fn tiff_predictor2_roundtrip() {
        let (w, h) = (8usize, 4usize);
        let pixels = sample_rgb(w, h);
        let bpp = 3usize;
        let rl = w * 3;
        // Codificar: cada muestra = actual - vecino izquierdo (misma componente).
        let mut enc = pixels.clone();
        for y in 0..h {
            let base = y * rl;
            for i in (bpp..rl).rev() {
                enc[base + i] = pixels[base + i].wrapping_sub(pixels[base + i - bpp]);
            }
        }
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                "DecodeParms" => dictionary! { "Predictor" => 2, "Colors" => 3, "Columns" => w as i64 },
            },
            zlib(&enc),
        );
        let img = decode_flate_image(&empty_doc(), &s, w as u32, h as u32)
            .expect("TIFF 2 debe decodificar");
        assert_eq!(
            img.to_rgb8().as_raw(),
            &pixels,
            "TIFF predictor 2: píxeles no coinciden"
        );
    }

    /// TIFF predictor 2 con bpc != 8 → fuera de alcance → SKIP (sin garabatear).
    #[test]
    fn tiff_predictor2_non8bpc_is_skipped() {
        let raw = vec![10u8; 4 * 4 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                // BitsPerComponent 16 en los parms del predictor → no soportado.
                "DecodeParms" => dictionary! { "Predictor" => 2, "Colors" => 3, "BitsPerComponent" => 16, "Columns" => 4 },
            },
            zlib(&raw),
        );
        assert!(
            decode_flate_image(&empty_doc(), &s, 4, 4).is_none(),
            "TIFF 16-bpc debe saltarse"
        );
    }

    // ---- de-chain ----

    /// `/Filter [ASCII85Decode FlateDecode]`: zlib primero, luego ASCII85 encima.
    #[test]
    fn dechain_ascii85_then_flate_roundtrip() {
        let (w, h) = (5u32, 4u32);
        let pixels = sample_rgb(w as usize, h as usize);
        // Codificación: raw -> zlib -> ascii85 (orden inverso al decode).
        let zipped = zlib(&pixels);
        let encoded = ascii85_encode(&zipped);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"ASCII85Decode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
            },
            encoded,
        );
        let img =
            decode_flate_image(&empty_doc(), &s, w, h).expect("cadena A85+Flate debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &pixels);
    }

    /// Cadena A85+Flate CON predictor PNG en un `/DecodeParms` array paralelo
    /// (null para A85, dict para Flate) — el caso real más completo.
    #[test]
    fn dechain_ascii85_flate_with_png_predictor_roundtrip() {
        let (w, h) = (6u32, 5u32);
        let pixels = sample_rgb(w as usize, h as usize);
        let png_zlib = png_encode_rgb(&pixels, w as usize, h as usize, 4); // Paeth
                                                                           // el contenido zlib ya está; ahora lo pasamos por ascii85.
                                                                           // png_encode_rgb ya devuelve zlib; decodificar necesita: A85 -> Flate(+pred).
        let encoded = ascii85_encode(&png_zlib);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"ASCII85Decode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
                "DecodeParms" => vec![
                    Object::Null,
                    Object::Dictionary(dictionary! { "Predictor" => 15, "Colors" => 3, "Columns" => w as i64 }),
                ],
            },
            encoded,
        );
        let img =
            decode_flate_image(&empty_doc(), &s, w, h).expect("A85+Flate+pred debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &pixels);
    }

    /// `/Filter [FlateDecode]` (array de un solo elemento) debe funcionar igual
    /// que el nombre suelto.
    #[test]
    fn dechain_single_element_flate_array() {
        let raw = sample_rgb(4, 4);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![Object::Name(b"FlateDecode".to_vec())],
            },
            zlib(&raw),
        );
        let img =
            decode_flate_image(&empty_doc(), &s, 4, 4).expect("[FlateDecode] debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &raw);
    }

    /// `/Filter [ASCIIHexDecode FlateDecode]`: cadena con hex.
    #[test]
    fn dechain_asciihex_then_flate_roundtrip() {
        let raw = sample_rgb(4, 3);
        let zipped = zlib(&raw);
        let mut hex = String::new();
        for b in &zipped {
            hex.push_str(&format!("{b:02x}"));
        }
        hex.push('>');
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 3,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"ASCIIHexDecode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
            },
            hex.into_bytes(),
        );
        let img = decode_flate_image(&empty_doc(), &s, 4, 3).expect("AHx+Flate debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &raw);
    }

    /// `/Filter [RunLengthDecode FlateDecode]`: RunLength encima de zlib.
    #[test]
    fn dechain_runlength_then_flate_roundtrip() {
        let raw = sample_rgb(4, 3);
        let zipped = zlib(&raw);
        // Codificar zipped con RunLength en modo literal (bloques de <=128).
        let mut rl = Vec::new();
        for chunk in zipped.chunks(128) {
            rl.push((chunk.len() - 1) as u8);
            rl.extend_from_slice(chunk);
        }
        rl.push(128); // EOD
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 3,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"RunLengthDecode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
            },
            rl,
        );
        let img = decode_flate_image(&empty_doc(), &s, 4, 3).expect("RL+Flate debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &raw);
    }

    // ---- casos que siguen saltándose ----

    /// Un filtro no soportado en la cadena (LZWDecode) → SKIP (sin corromper).
    #[test]
    fn unsupported_filter_in_chain_is_skipped() {
        let raw = sample_rgb(4, 4);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"LZWDecode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
            },
            zlib(&raw),
        );
        assert!(
            decode_flate_image(&empty_doc(), &s, 4, 4).is_none(),
            "LZW en la cadena → SKIP"
        );
    }

    /// DCTDecode como filtro único no es asunto de este decoder (lo maneja el
    /// path `image` upstream): aquí devuelve None.
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
        assert!(decode_flate_image(&empty_doc(), &s, 4, 4).is_none());
    }

    #[test]
    fn refuses_length_mismatch() {
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
        assert!(
            decode_flate_image(&empty_doc(), &s, 12, 12).is_none(),
            "longitud no coincide → skip"
        );
    }

    #[test]
    fn refuses_unsupported_colorspace_indexed() {
        let raw = vec![0u8; 6 * 6];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 6, "Height" => 6,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![Object::Name(b"Indexed".to_vec())],
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&empty_doc(), &s, 6, 6).is_none());
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
        assert!(decode_flate_image(&empty_doc(), &s, 8, 8).is_none());
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
        assert!(decode_flate_image(&empty_doc(), &s, 4, 4).is_none());
    }

    /// Predictor desconocido (p. ej. 99) → SKIP en vez de interpretar mal.
    #[test]
    fn refuses_unknown_predictor() {
        let raw = vec![1u8; 4 * 4 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                "DecodeParms" => dictionary! { "Predictor" => 99, "Colors" => 3, "Columns" => 4 },
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&empty_doc(), &s, 4, 4).is_none());
    }

    /// Dimensiones enormes se rechazan antes de cualquier asignación gigante.
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
            vec![],
        );
        assert!(
            decode_flate_image(&empty_doc(), &s, 100_000, 100_000).is_none(),
            "debe rechazar dimensiones que superan MAX_DECODE_BYTES"
        );
    }

    // ---- pruebas unitarias de los decodificadores texto-a-binario ----

    #[test]
    fn ascii85_unit_roundtrip() {
        let data = b"Hello, ASCII85 world! 12345";
        let enc = ascii85_encode(data);
        assert_eq!(decode_ascii85(&enc).unwrap(), data);
    }

    #[test]
    fn ascii85_z_shortcut() {
        // 'z' representa 4 ceros.
        let enc = b"z~>";
        assert_eq!(decode_ascii85(enc).unwrap(), vec![0, 0, 0, 0]);
    }

    #[test]
    fn asciihex_unit_roundtrip_and_odd() {
        assert_eq!(decode_ascii_hex(b"48656c6c6f>").unwrap(), b"Hello");
        // dígito impar → se completa con 0 bajo: "4" -> 0x40
        assert_eq!(decode_ascii_hex(b"4>").unwrap(), vec![0x40]);
        // blancos ignorados
        assert_eq!(decode_ascii_hex(b"48 65\n6c>").unwrap(), b"Hel");
        // inválido
        assert!(decode_ascii_hex(b"4G>").is_none());
    }

    #[test]
    fn runlength_unit_literal_and_run() {
        // literal: len=2 → copia 3 bytes
        assert_eq!(
            decode_run_length(&[2, 1, 2, 3, 128]).unwrap(),
            vec![1, 2, 3]
        );
        // run: len=254 → repite 257-254=3 veces
        assert_eq!(decode_run_length(&[254, 9, 128]).unwrap(), vec![9, 9, 9]);
        // truncado → None
        assert!(decode_run_length(&[5, 1, 2]).is_none());
    }

    // ---- pruebas de seguridad: entradas hostiles / desbordamiento ----

    /// Ataque de desbordamiento: Colors=2_000_000_000, Columns=2_000_000_000 con
    /// predictor TIFF 2 y payload zlib mínimo válido. Sin el fix, `row_len` y
    /// `bytes_per_pixel` harían `usize` overflow → panic en debug. Con el fix
    /// devuelven `None` sin panic.
    #[test]
    fn overflow_attack_tiff_predictor_huge_colors_columns_returns_none_without_panic() {
        // Un stream zlib válido pero trivial (1 byte de contenido).
        let payload = zlib(&[0u8]);
        let s = Stream::new(
            dictionary! {
                "Type"             => "XObject",
                "Subtype"          => "Image",
                "Width"            => 4_i64,
                "Height"           => 4_i64,
                "BitsPerComponent" => 8,
                "ColorSpace"       => "DeviceRGB",
                "Filter"           => "FlateDecode",
                "DecodeParms"      => dictionary! {
                    "Predictor"        => 2_i64,
                    "Colors"           => 2_000_000_000_i64,
                    "BitsPerComponent" => 8_i64,
                    "Columns"          => 2_000_000_000_i64
                },
            },
            payload,
        );
        // Debe devolver None sin hacer panic (tanto en debug como en release).
        assert!(
            decode_flate_image(&empty_doc(), &s, 4, 4).is_none(),
            "Colors/Columns gigantes con predictor TIFF deben producir None, no panic"
        );
    }

    /// Misma prueba con predictor PNG (10–15) para cubrir también `png_predictor`.
    #[test]
    fn overflow_attack_png_predictor_huge_colors_columns_returns_none_without_panic() {
        let payload = zlib(&[0u8]);
        let s = Stream::new(
            dictionary! {
                "Type"             => "XObject",
                "Subtype"          => "Image",
                "Width"            => 4_i64,
                "Height"           => 4_i64,
                "BitsPerComponent" => 8,
                "ColorSpace"       => "DeviceRGB",
                "Filter"           => "FlateDecode",
                "DecodeParms"      => dictionary! {
                    "Predictor"        => 15_i64,
                    "Colors"           => 2_000_000_000_i64,
                    "BitsPerComponent" => 8_i64,
                    "Columns"          => 2_000_000_000_i64
                },
            },
            payload,
        );
        assert!(
            decode_flate_image(&empty_doc(), &s, 4, 4).is_none(),
            "Colors/Columns gigantes con predictor PNG deben producir None, no panic"
        );
    }

    /// `decode_ascii85` con un grupo de un solo carácter (no forma ningún byte)
    /// → debe devolver `None`, no panic.
    #[test]
    fn ascii85_lone_char_group_returns_none() {
        // Un único carácter válido base-85 seguido de EOD: grupo parcial de longitud 1
        // es explícitamente inválido según el spec y nuestro decoder.
        assert!(
            decode_ascii85(b"!~>").is_none(),
            "grupo de 1 carácter en ASCII85 debe devolver None"
        );
    }

    /// `decode_run_length` con byte de repetición pero sin el byte de datos
    /// → debe devolver `None`, no panic.
    #[test]
    fn runlength_repeat_byte_with_no_data_returns_none() {
        // 0xFE = 254 → modo repetición (257-254=3 veces), pero no hay byte siguiente.
        assert!(
            decode_run_length(&[0xFE]).is_none(),
            "byte de repetición sin dato siguiente debe devolver None"
        );
    }

    /// `decode_ascii_hex` con un carácter no-hex y sin EOD `>`
    /// → debe devolver `None` al encontrar el carácter inválido.
    #[test]
    fn ascii_hex_non_hex_char_no_eod_returns_none() {
        // 'G' no es un dígito hex válido.
        assert!(
            decode_ascii_hex(b"4G").is_none(),
            "carácter no-hex sin EOD debe devolver None"
        );
    }

    // ---- unwrap_to_dct (lever A: cadena que termina en DCT) ----

    #[test]
    fn unwrap_to_dct_unwraps_flate_prefix() {
        // Un "JPEG" de mentira: unwrap sólo des-encadena, no decodifica.
        let jpeg = b"\xFF\xD8fake-jpeg-payload\xFF\xD9".to_vec();
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"FlateDecode".to_vec()),
                    Object::Name(b"DCTDecode".to_vec()),
                ],
            },
            zlib(&jpeg),
        );
        assert_eq!(unwrap_to_dct(&s).unwrap(), jpeg);
    }

    #[test]
    fn unwrap_to_dct_unwraps_ascii85_prefix() {
        let jpeg = b"\xFF\xD8otro-payload\xFF\xD9".to_vec();
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"ASCII85Decode".to_vec()),
                    Object::Name(b"DCTDecode".to_vec()),
                ],
            },
            ascii85_encode(&jpeg),
        );
        assert_eq!(unwrap_to_dct(&s).unwrap(), jpeg);
    }

    #[test]
    fn unwrap_to_dct_rejects_out_of_scope_cases() {
        let mk = |filters: Vec<Object>, content: Vec<u8>| {
            Stream::new(
                dictionary! {
                    "Type" => "XObject", "Subtype" => "Image",
                    "Width" => 4, "Height" => 4,
                    "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                    "Filter" => filters,
                },
                content,
            )
        };
        // DCT en posición no-final → None
        let s = mk(
            vec![
                Object::Name(b"DCTDecode".to_vec()),
                Object::Name(b"FlateDecode".to_vec()),
            ],
            vec![1, 2, 3],
        );
        assert!(unwrap_to_dct(&s).is_none());
        // prefijo no soportado (LZW) → None
        let s = mk(
            vec![
                Object::Name(b"LZWDecode".to_vec()),
                Object::Name(b"DCTDecode".to_vec()),
            ],
            vec![1, 2, 3],
        );
        assert!(unwrap_to_dct(&s).is_none());
        // array de un solo elemento → None (lo maneja el path DCT normal)
        let s = mk(vec![Object::Name(b"DCTDecode".to_vec())], vec![1, 2, 3]);
        assert!(unwrap_to_dct(&s).is_none());
        // /Filter como Name suelto → None (ídem)
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
            },
            vec![1, 2, 3],
        );
        assert!(unwrap_to_dct(&s).is_none());
        // zlib corrupto en el prefijo → None
        let s = mk(
            vec![
                Object::Name(b"FlateDecode".to_vec()),
                Object::Name(b"DCTDecode".to_vec()),
            ],
            vec![0xFF, 0x00, 0x13],
        );
        assert!(unwrap_to_dct(&s).is_none());
    }
}
