//! Interpretación de `/ColorSpace` a píxeles de salida (capa de colorspace, v2.0).
//!
//! Esta capa es la parte auto-contenida del decoder de imágenes Flate: toma un
//! `/ColorSpace` (resolviendo referencias indirectas) y decide cómo convertir el
//! buffer de muestras ya des-filtrado en un `DynamicImage` de salida. La lógica
//! de Flate/filtros/predictores vive en `decode.rs`; aquí sólo interpretamos el
//! espacio de color.
//!
//! **ColorSpace/BitsPerComponent soportados (P1c):**
//! - `DeviceRGB`/8 → `RgbImage`, `DeviceGray`/8 → `GrayImage`.
//! - `DeviceCMYK`/8 → `RgbImage` con la fórmula estándar
//!   `R = round((255-C)*(255-K)/255)` (sin inversión Adobe: eso es sólo de
//!   DCTDecode, que va por el path de `image`, no por aquí).
//! - `ICCBased` → se interpreta como el device-space de su `/N`
//!   (1→Gray, 3→RGB, 4→CMYK); ignoramos el perfil (comprimimos, no gestionamos
//!   color). `/N` ausente o ∉{1,3,4} → SALTAMOS.
//! - `Indexed` (paleta) con índice de **8 bpc** y base DeviceRGB/DeviceGray/
//!   ICCBased(N=1|3): expandimos cada índice a su entrada de paleta (validando
//!   índice ≤ hival y contra los límites, sin lecturas OOB). Base CMYK e índice
//!   sub-byte (1/2/4 bpc) → SALTAMOS.
//! - Separation/DeviceN, Lab, CalRGB/CalGray, Pattern y demás → SALTAMOS.

use lopdf::{Document, Object};

use super::decode::{apply_filter, decode_parms_for, filter_chain};

/// Espacio de color de dispositivo con un número fijo de componentes por muestra.
/// Es la unidad básica que sabemos convertir a píxeles RGB/Gray de salida.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlateColor {
    /// `DeviceRGB`, 3 canales → `RgbImage`.
    Rgb,
    /// `DeviceGray`, 1 canal → `GrayImage`.
    Gray,
    /// `DeviceCMYK`, 4 canales → convertido a `RgbImage`.
    Cmyk,
}

impl FlateColor {
    pub(super) fn channels(self) -> usize {
        match self {
            FlateColor::Rgb => 3,
            FlateColor::Gray => 1,
            FlateColor::Cmyk => 4,
        }
    }

    /// Componentes de un espacio base a partir de su número de canales `/N`
    /// (usado por ICCBased y como base de Indexed). Sólo 1/3/4 en alcance.
    fn from_n(n: i64) -> Option<FlateColor> {
        match n {
            1 => Some(FlateColor::Gray),
            3 => Some(FlateColor::Rgb),
            4 => Some(FlateColor::Cmyk),
            _ => None,
        }
    }
}

/// Interpretación de `/ColorSpace` que sabemos convertir a píxeles de salida.
///
/// P1c añade tres casos a los device-spaces directos: **Indexed** (paleta),
/// **ICCBased** (interpretado como el device-space de su `/N`) y **DeviceCMYK**.
/// Cualquier otro (Separation/DeviceN, Lab, CalRGB/Gray, Pattern, …) → SKIP.
pub(super) enum ColorSpaceKind {
    /// Espacio de dispositivo directo: 1 byte por componente, `channels()` por
    /// muestra. Gray→GrayImage; RGB/CMYK→RgbImage.
    Device(FlateColor),
    /// Indexed: 1 índice de 8 bits por píxel; cada índice se expande a
    /// `base.channels()` bytes de la paleta. `hival` es el índice máximo válido.
    Indexed {
        base: FlateColor,
        hival: usize,
        /// Paleta ya materializada: `(hival+1) * base.channels()` bytes.
        lookup: Vec<u8>,
    },
}

/// Resuelve un `Object` que puede ser una referencia indirecta a su objeto
/// concreto, de forma segura (nunca panic; ref rota/ausente → `None`).
fn resolve<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    match obj {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    }
}

/// Lee `/N` (número de componentes) de un stream ICCBased (resolviendo la ref al
/// stream) y lo mapea a un `FlateColor`. Sólo acepta un `Object::Stream` (como
/// exige el spec PDF): un diccionario suelto → `None` (SKIP), para no
/// malinterpretar un dict ajeno que casualmente tenga una clave `/N`. `/N`
/// ausente o ∉{1,3,4} → `None`.
fn icc_base(doc: &Document, stream_ref: &Object) -> Option<FlateColor> {
    let obj = resolve(doc, stream_ref)?;
    let dict = match obj {
        Object::Stream(s) => &s.dict,
        _ => return None, // bare dict u otro objeto → SKIP
    };
    let n = dict.get(b"N").and_then(|o| o.as_i64()).ok()?;
    FlateColor::from_n(n)
}

/// Mapea un `/ColorSpace` que es un espacio base *simple* (Name device, o
/// ICCBased array) a su `FlateColor`. Usado tanto para la imagen directa como
/// para la base de un Indexed. NO maneja Indexed en sí (eso lo hace el llamador).
fn base_color(doc: &Document, cs: &Object) -> Option<FlateColor> {
    match cs {
        Object::Name(n) if n == b"DeviceRGB" || n == b"RGB" => Some(FlateColor::Rgb),
        Object::Name(n) if n == b"DeviceGray" || n == b"G" => Some(FlateColor::Gray),
        Object::Name(n) if n == b"DeviceCMYK" || n == b"CMYK" => Some(FlateColor::Cmyk),
        Object::Array(arr) => {
            // Sólo `[/ICCBased <ref>]` en alcance como base.
            let head = arr.first()?.as_name().ok()?;
            if head == b"ICCBased" {
                icc_base(doc, arr.get(1)?)
            } else {
                None // CalRGB/CalGray/Lab/Separation/DeviceN/... → SKIP
            }
        }
        _ => None,
    }
}

/// Materializa la tabla `lookup` de un Indexed: puede ser un string literal o un
/// stream (posiblemente con filtros → se decodifica). Devuelve los bytes crudos
/// de la paleta o `None` si no se puede obtener/decodificar.
fn indexed_lookup_bytes(doc: &Document, lookup: &Object) -> Option<Vec<u8>> {
    match resolve(doc, lookup)? {
        Object::String(bytes, _) => Some(bytes.clone()),
        Object::Stream(s) => {
            // La paleta puede venir comprimida (FlateDecode, etc.). Reusamos el
            // des-encadenador; sin filtros devolvemos el contenido tal cual.
            match filter_chain(&s.dict) {
                Some(chain) => {
                    let mut data = s.content.clone();
                    for (idx, filter) in chain.iter().enumerate() {
                        let parms = decode_parms_for(&s.dict, idx);
                        data = apply_filter(*filter, &data, parms)?;
                    }
                    Some(data)
                }
                None => Some(s.content.clone()),
            }
        }
        _ => None,
    }
}

/// Interpreta `/ColorSpace` completo (resolviendo indirectos) al `ColorSpaceKind`
/// que sabemos decodificar. `bpc` es `/BitsPerComponent` de la imagen.
///
/// Reglas de bpc: los device-spaces e ICCBased sólo en 8 bpc. Indexed: sólo
/// índice de 8 bpc (sub-byte → SKIP, ver más abajo). Cualquier caso fuera de
/// alcance → `None` (SKIP).
pub(super) fn interpret_color_space(
    doc: &Document,
    dict: &lopdf::Dictionary,
    bpc: i64,
) -> Option<ColorSpaceKind> {
    let cs = resolve(doc, dict.get(b"ColorSpace").ok()?)?;

    // ¿Es Indexed? `/ColorSpace [/Indexed <base> <hival> <lookup>]`.
    if let Object::Array(arr) = cs {
        if let Ok(head) = arr.first()?.as_name() {
            if head == b"Indexed" || head == b"I" {
                // El índice debe ser de 8 bpc: sub-byte (1/2/4) → SKIP (no
                // implementamos desempaquetado de bits; nota en el reporte).
                if bpc != 8 {
                    return None;
                }
                let base_obj = resolve(doc, arr.get(1)?)?;
                let base = base_color(doc, base_obj)?;
                // CMYK como base de Indexed es raro: lo saltamos (aceptable).
                if base == FlateColor::Cmyk {
                    return None;
                }
                let hival = resolve(doc, arr.get(2)?)?.as_i64().ok()?;
                if !(0..=255).contains(&hival) {
                    return None; // índice de 8 bits: hival ∈ [0,255]
                }
                let hival = hival as usize;
                let lookup = indexed_lookup_bytes(doc, arr.get(3)?)?;
                let expected_len = hival.checked_add(1)?.checked_mul(base.channels())?;
                if lookup.len() != expected_len {
                    return None; // paleta con longitud incorrecta → SKIP
                }
                return Some(ColorSpaceKind::Indexed {
                    base,
                    hival,
                    lookup,
                });
            }
        }
    }

    // No-Indexed: device-space directo o ICCBased. Requiere 8 bpc.
    if bpc != 8 {
        return None;
    }
    base_color(doc, cs).map(ColorSpaceKind::Device)
}

/// Convierte una muestra CMYK de 8 bits (C,M,Y,K) a RGB con la fórmula estándar
/// (sin la inversión Adobe, que sólo aplica a DCTDecode). `out` recibe R,G,B.
#[inline]
fn cmyk_to_rgb(c: u8, m: u8, y: u8, k: u8) -> [u8; 3] {
    let kf = 255u16 - k as u16;
    let conv = |ch: u8| -> u8 {
        // round(255 * (1-ch/255) * (1-k/255)) = round((255-ch)*(255-k)/255)
        let num = (255u32 - ch as u32) * kf as u32;
        ((num + 127) / 255) as u8
    };
    [conv(c), conv(m), conv(y)]
}

/// Expande un buffer de muestras CMYK (`n*4` bytes) a un `RgbImage`.
fn cmyk_buffer_to_rgb(width: u32, height: u32, data: &[u8]) -> Option<image::DynamicImage> {
    let px = (width as usize).checked_mul(height as usize)?;
    let mut rgb = Vec::with_capacity(px.checked_mul(3)?);
    for chunk in data.chunks_exact(4) {
        rgb.extend_from_slice(&cmyk_to_rgb(chunk[0], chunk[1], chunk[2], chunk[3]));
    }
    image::RgbImage::from_raw(width, height, rgb).map(image::DynamicImage::ImageRgb8)
}

/// Convierte un buffer de device-space (ya des-filtrado, longitud `W*H*canales`)
/// al `DynamicImage` de salida.
pub(super) fn device_buffer_to_image(
    color: FlateColor,
    width: u32,
    height: u32,
    data: Vec<u8>,
) -> Option<image::DynamicImage> {
    match color {
        FlateColor::Rgb => {
            image::RgbImage::from_raw(width, height, data).map(image::DynamicImage::ImageRgb8)
        }
        FlateColor::Gray => {
            image::GrayImage::from_raw(width, height, data).map(image::DynamicImage::ImageLuma8)
        }
        FlateColor::Cmyk => cmyk_buffer_to_rgb(width, height, &data),
    }
}

/// Expande datos indexados (1 índice de 8 bits por píxel) a un `DynamicImage`
/// usando la paleta `lookup`. Valida cada índice contra `hival` y contra los
/// límites de la paleta (índice fuera de rango → `None`, SKIP, nunca OOB).
pub(super) fn indexed_to_image(
    base: FlateColor,
    hival: usize,
    lookup: &[u8],
    width: u32,
    height: u32,
    data: &[u8],
) -> Option<image::DynamicImage> {
    let comps = base.channels();
    let px = (width as usize).checked_mul(height as usize)?;
    match base {
        FlateColor::Gray => {
            let mut out = Vec::with_capacity(px);
            for &idx in data {
                let i = idx as usize;
                if i > hival {
                    return None; // índice fuera de rango → SKIP
                }
                let off = i.checked_mul(comps)?;
                let val = *lookup.get(off)?; // bounds-check sin OOB
                out.push(val);
            }
            image::GrayImage::from_raw(width, height, out).map(image::DynamicImage::ImageLuma8)
        }
        FlateColor::Rgb => {
            let mut out = Vec::with_capacity(px.checked_mul(3)?);
            for &idx in data {
                let i = idx as usize;
                if i > hival {
                    return None;
                }
                let off = i.checked_mul(comps)?;
                let end = off.checked_add(comps)?;
                let entry = lookup.get(off..end)?; // bounds-check sin OOB
                out.extend_from_slice(entry);
            }
            image::RgbImage::from_raw(width, height, out).map(image::DynamicImage::ImageRgb8)
        }
        FlateColor::Cmyk => None, // base CMYK ya se saltó antes; defensivo.
    }
}

#[cfg(test)]
mod tests {
    use super::super::decode::decode_flate_image;
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

    /// Genera una imagen RGB8 con un patrón determinista (no plano).
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

    // ==================================================================
    // P1c: DeviceCMYK, ICCBased, Indexed — round-trips pixel-exactos.
    // ==================================================================

    /// Fórmula CMYK→RGB de referencia (espejo de `cmyk_to_rgb`), calculada de
    /// forma independiente en el test para no ser tautológica.
    fn expected_cmyk_rgb(c: u8, m: u8, y: u8, k: u8) -> [u8; 3] {
        let conv = |ch: u8| -> u8 {
            let v = 255.0 * (1.0 - ch as f32 / 255.0) * (1.0 - k as f32 / 255.0);
            v.round() as u8
        };
        [conv(c), conv(m), conv(y)]
    }

    /// DeviceCMYK/8 con valores conocidos → RGB según la fórmula estándar.
    #[test]
    fn cmyk_to_rgb_roundtrip() {
        let (w, h) = (3u32, 2u32);
        // 6 píxeles con CMYK variado (incluye negro puro K=255 y blanco 0000).
        let samples: [[u8; 4]; 6] = [
            [0, 0, 0, 0],       // blanco
            [0, 0, 0, 255],     // negro
            [255, 0, 0, 0],     // cyan
            [0, 255, 0, 0],     // magenta
            [0, 0, 255, 0],     // amarillo
            [50, 100, 150, 40], // arbitrario
        ];
        let mut raw = Vec::new();
        for s in &samples {
            raw.extend_from_slice(s);
        }
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceCMYK",
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        let img = decode_flate_image(&empty_doc(), &s, w, h).expect("CMYK debe decodificar");
        let rgb = img.to_rgb8();
        for (i, sample) in samples.iter().enumerate() {
            let x = (i as u32) % w;
            let y = (i as u32) / w;
            let got = rgb.get_pixel(x, y).0;
            let exp = expected_cmyk_rgb(sample[0], sample[1], sample[2], sample[3]);
            assert_eq!(got, exp, "píxel CMYK {i} = {sample:?}");
        }
    }

    /// Construye un Document con un stream ICCBased de `n` componentes y devuelve
    /// su referencia, para usarla en un `/ColorSpace [/ICCBased ref]`.
    fn doc_with_icc(n: i64) -> (lopdf::Document, Object) {
        let mut doc = lopdf::Document::new();
        let icc = Stream::new(
            dictionary! { "N" => n },
            vec![0u8; 4], // contenido irrelevante (ignoramos el perfil)
        );
        let id = doc.add_object(Object::Stream(icc));
        (doc, Object::Reference(id))
    }

    /// ICCBased N=3 decodifica idéntico a los mismos bytes como DeviceRGB.
    #[test]
    fn iccbased_n3_matches_devicergb() {
        let (w, h) = (5u32, 4u32);
        let pixels = sample_rgb(w as usize, h as usize);
        let (doc, icc_ref) = doc_with_icc(3);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![Object::Name(b"ICCBased".to_vec()), icc_ref],
                "Filter" => "FlateDecode",
            },
            zlib(&pixels),
        );
        let img = decode_flate_image(&doc, &s, w, h).expect("ICCBased N=3 debe decodificar");
        assert!(matches!(img, image::DynamicImage::ImageRgb8(_)));
        assert_eq!(img.to_rgb8().as_raw(), &pixels);
    }

    /// ICCBased N=1 decodifica como Gray.
    #[test]
    fn iccbased_n1_matches_devicegray() {
        let (w, h) = (6u32, 4u32);
        let raw: Vec<u8> = (0..(w * h) as usize).map(|i| (i * 3 % 256) as u8).collect();
        let (doc, icc_ref) = doc_with_icc(1);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![Object::Name(b"ICCBased".to_vec()), icc_ref],
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        let img = decode_flate_image(&doc, &s, w, h).expect("ICCBased N=1 debe decodificar");
        assert!(matches!(img, image::DynamicImage::ImageLuma8(_)));
        assert_eq!(img.to_luma8().as_raw(), &raw);
    }

    /// ICCBased sin `/N` → SKIP (no adivinar).
    #[test]
    fn iccbased_missing_n_is_skipped() {
        let (w, h) = (4u32, 4u32);
        let mut doc = lopdf::Document::new();
        let icc = Stream::new(dictionary! { "Foo" => 1 }, vec![0u8; 4]);
        let id = doc.add_object(Object::Stream(icc));
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![Object::Name(b"ICCBased".to_vec()), Object::Reference(id)],
                "Filter" => "FlateDecode",
            },
            zlib(&vec![0u8; (w * h * 3) as usize]),
        );
        assert!(decode_flate_image(&doc, &s, w, h).is_none(), "ICCBased sin /N → SKIP");
    }

    /// ICCBased cuyo objeto es un diccionario suelto (no stream) → SKIP (M1).
    /// Un ICCBased malformado como bare dict no debe interpretarse: podría ser un
    /// dict ajeno que casualmente tenga una clave `/N`.
    #[test]
    fn iccbased_bare_dictionary_is_skipped() {
        let (w, h) = (4u32, 4u32);
        let mut doc = lopdf::Document::new();
        // Diccionario suelto (NO stream) con una `/N` válida.
        let bare = doc.add_object(Object::Dictionary(dictionary! { "N" => 3 }));
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![Object::Name(b"ICCBased".to_vec()), Object::Reference(bare)],
                "Filter" => "FlateDecode",
            },
            zlib(&vec![0u8; (w * h * 3) as usize]),
        );
        assert!(
            decode_flate_image(&doc, &s, w, h).is_none(),
            "ICCBased como diccionario suelto → SKIP"
        );
    }

    /// Indexed base DeviceRGB, índice 8-bit: cada píxel = su entrada de paleta.
    #[test]
    fn indexed_rgb_roundtrip() {
        let (w, h) = (4u32, 2u32);
        // Paleta de 4 colores (hival=3), RGB.
        let palette: Vec<u8> = vec![
            10, 20, 30, // idx 0
            40, 50, 60, // idx 1
            70, 80, 90, // idx 2
            200, 210, 220, // idx 3
        ];
        // 8 índices (uno por píxel), todos ≤ 3.
        let indices: Vec<u8> = vec![0, 1, 2, 3, 3, 2, 1, 0];
        let mut doc = lopdf::Document::new();
        let pal_ref = doc.add_object(Object::String(palette.clone(), lopdf::StringFormat::Literal));
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![
                    Object::Name(b"Indexed".to_vec()),
                    Object::Name(b"DeviceRGB".to_vec()),
                    Object::Integer(3),
                    Object::Reference(pal_ref),
                ],
                "Filter" => "FlateDecode",
            },
            zlib(&indices),
        );
        let img = decode_flate_image(&doc, &s, w, h).expect("Indexed RGB debe decodificar");
        let rgb = img.to_rgb8();
        for (i, &idx) in indices.iter().enumerate() {
            let x = (i as u32) % w;
            let y = (i as u32) / w;
            let base = idx as usize * 3;
            let exp = [palette[base], palette[base + 1], palette[base + 2]];
            assert_eq!(rgb.get_pixel(x, y).0, exp, "píxel indexado {i} (idx {idx})");
        }
    }

    /// Indexed con lookup como string literal inline (no stream), base Gray.
    #[test]
    fn indexed_gray_inline_lookup_roundtrip() {
        let (w, h) = (3u32, 1u32);
        let palette: Vec<u8> = vec![5, 128, 250]; // hival=2, base Gray (1 comp)
        let indices: Vec<u8> = vec![2, 0, 1];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![
                    Object::Name(b"Indexed".to_vec()),
                    Object::Name(b"DeviceGray".to_vec()),
                    Object::Integer(2),
                    Object::String(palette.clone(), lopdf::StringFormat::Literal),
                ],
                "Filter" => "FlateDecode",
            },
            zlib(&indices),
        );
        let img = decode_flate_image(&empty_doc(), &s, w, h).expect("Indexed Gray debe decodificar");
        let g = img.to_luma8();
        assert_eq!(g.get_pixel(0, 0).0[0], palette[2]);
        assert_eq!(g.get_pixel(1, 0).0[0], palette[0]);
        assert_eq!(g.get_pixel(2, 0).0[0], palette[1]);
    }

    /// Indexed con un índice fuera de rango (> hival) → SKIP, sin panic ni OOB.
    #[test]
    fn indexed_out_of_range_index_is_skipped() {
        let (w, h) = (2u32, 1u32);
        let palette: Vec<u8> = vec![0, 0, 0, 255, 255, 255]; // hival=1
        let indices: Vec<u8> = vec![0, 5]; // 5 > hival=1
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![
                    Object::Name(b"Indexed".to_vec()),
                    Object::Name(b"DeviceRGB".to_vec()),
                    Object::Integer(1),
                    Object::String(palette, lopdf::StringFormat::Literal),
                ],
                "Filter" => "FlateDecode",
            },
            zlib(&indices),
        );
        assert!(
            decode_flate_image(&empty_doc(), &s, w, h).is_none(),
            "índice fuera de rango → SKIP sin panic"
        );
    }

    /// Indexed con paleta de longitud incorrecta → SKIP.
    #[test]
    fn indexed_wrong_palette_length_is_skipped() {
        let (w, h) = (2u32, 1u32);
        // hival=3 → esperaría (3+1)*3 = 12 bytes, damos sólo 6.
        let palette: Vec<u8> = vec![0, 0, 0, 255, 255, 255];
        let indices: Vec<u8> = vec![0, 1];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![
                    Object::Name(b"Indexed".to_vec()),
                    Object::Name(b"DeviceRGB".to_vec()),
                    Object::Integer(3),
                    Object::String(palette, lopdf::StringFormat::Literal),
                ],
                "Filter" => "FlateDecode",
            },
            zlib(&indices),
        );
        assert!(
            decode_flate_image(&empty_doc(), &s, w, h).is_none(),
            "paleta con longitud incorrecta → SKIP"
        );
    }

    /// Indexed sub-byte (bpc=4) → SKIP (no implementamos desempaquetado de bits).
    #[test]
    fn indexed_subbyte_bpc_is_skipped() {
        let (w, h) = (2u32, 1u32);
        let palette: Vec<u8> = vec![0, 0, 0, 255, 255, 255];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 4,
                "ColorSpace" => vec![
                    Object::Name(b"Indexed".to_vec()),
                    Object::Name(b"DeviceRGB".to_vec()),
                    Object::Integer(1),
                    Object::String(palette, lopdf::StringFormat::Literal),
                ],
                "Filter" => "FlateDecode",
            },
            zlib(&[0u8; 1]),
        );
        assert!(
            decode_flate_image(&empty_doc(), &s, w, h).is_none(),
            "Indexed sub-byte → SKIP"
        );
    }

    /// Colorspace realmente no soportado (Separation) → SKIP.
    #[test]
    fn separation_colorspace_is_skipped() {
        let (w, h) = (4u32, 4u32);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![
                    Object::Name(b"Separation".to_vec()),
                    Object::Name(b"Spot1".to_vec()),
                    Object::Name(b"DeviceCMYK".to_vec()),
                ],
                "Filter" => "FlateDecode",
            },
            zlib(&vec![0u8; (w * h) as usize]),
        );
        assert!(decode_flate_image(&empty_doc(), &s, w, h).is_none(), "Separation → SKIP");
    }

    /// Lab colorspace → SKIP.
    #[test]
    fn lab_colorspace_is_skipped() {
        let (w, h) = (4u32, 4u32);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![
                    Object::Name(b"Lab".to_vec()),
                    Object::Dictionary(dictionary! { "WhitePoint" => vec![Object::Real(0.9505), Object::Real(1.0), Object::Real(1.089)] }),
                ],
                "Filter" => "FlateDecode",
            },
            zlib(&vec![0u8; (w * h * 3) as usize]),
        );
        assert!(decode_flate_image(&empty_doc(), &s, w, h).is_none(), "Lab → SKIP");
    }

    /// Indexed con base CMYK → SKIP (fuera de alcance, aceptable).
    #[test]
    fn indexed_cmyk_base_is_skipped() {
        let (w, h) = (2u32, 1u32);
        let palette: Vec<u8> = vec![0, 0, 0, 0, 255, 255, 255, 255]; // hival=1, 4 comps
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![
                    Object::Name(b"Indexed".to_vec()),
                    Object::Name(b"DeviceCMYK".to_vec()),
                    Object::Integer(1),
                    Object::String(palette, lopdf::StringFormat::Literal),
                ],
                "Filter" => "FlateDecode",
            },
            zlib(&[0u8; 2]),
        );
        assert!(
            decode_flate_image(&empty_doc(), &s, w, h).is_none(),
            "Indexed base CMYK → SKIP"
        );
    }

    /// Indexed con lookup en un stream comprimido (FlateDecode) → se decodifica.
    #[test]
    fn indexed_lookup_as_flate_stream_roundtrip() {
        let (w, h) = (2u32, 1u32);
        let palette: Vec<u8> = vec![11, 22, 33, 44, 55, 66]; // hival=1, RGB
        let lookup_stream = Stream::new(dictionary! { "Filter" => "FlateDecode" }, zlib(&palette));
        let mut doc = lopdf::Document::new();
        let pal_ref = doc.add_object(Object::Stream(lookup_stream));
        let indices: Vec<u8> = vec![1, 0];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![
                    Object::Name(b"Indexed".to_vec()),
                    Object::Name(b"DeviceRGB".to_vec()),
                    Object::Integer(1),
                    Object::Reference(pal_ref),
                ],
                "Filter" => "FlateDecode",
            },
            zlib(&indices),
        );
        let img = decode_flate_image(&doc, &s, w, h).expect("lookup-stream debe decodificar");
        let rgb = img.to_rgb8();
        assert_eq!(rgb.get_pixel(0, 0).0, [44, 55, 66]); // idx 1
        assert_eq!(rgb.get_pixel(1, 0).0, [11, 22, 33]); // idx 0
    }
}
