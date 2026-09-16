//! Pipeline por-imagen: recomprime un XObject de imagen in-place si conviene.
//!
//! Descompone el procesamiento de UNA imagen en etapas aisladas y testeables,
//! en el orden del diseño v2 (`Analyze → Decode → Decide → Encode → Rewrite`):
//!
//! 1. [`load`]    — lee el stream, aplica las guardas tempranas (preservación de
//!    firma/sello, /SMask, dimensiones malformadas) y produce una [`ImageSource`].
//! 2. [`decode`]  — enruta por formato (CMYK-JPEG, DCT/PNG, Flate crudo) a píxeles
//!    y elige el codec de salida (foto→JPEG, línea→Flate sin pérdida).
//! 3. [`transform`] — decide y aplica el downsampling por DPI efectivo real.
//! 4. [`encode`]  — codifica los píxeles al codec elegido.
//! 5. [`commit_prepared`] — aplica el piso por-imagen y reescribe el stream.
//!
//! [`prepare_image`] encadena las etapas 1-4 (todas read-only sobre el doc) y
//! produce un [`Prepared`]; [`commit_prepared`] (etapa 5, la única que muta el
//! doc) lo traduce en el [`ImageOutcome`] correspondiente. Ese corte read-only /
//! mutación es lo que deja al pipeline paralelizar el cómputo (§1.4). La lógica y
//! los invariantes (comentarios F1/F2/F5/F8/F9/F10, P1/P2, bug del sello negro)
//! son los mismos que tenía el monolito previo en `pipeline.rs`.

use crate::image_opt::jpeg::JpegRecompressor;
use crate::image_opt::{downsample, target_dimensions, Encoded, RawImage, Recompressor};
use crate::report::{ImageAction, ImageSkipReason, ImageStat, Warning};
use lopdf::{Document, Object, Stream};

/// Resultado de procesar una imagen: su stat más cualquier warning generado
/// (canal alfa descartado, imagen no soportada/omitida, etc.). Siempre se
/// produce un `stat` para que las imágenes omitidas tengan señal en el reporte.
pub(crate) struct ImageOutcome {
    pub(crate) stat: ImageStat,
    pub(crate) warnings: Vec<Warning>,
}

/// Construye un outcome que marca la imagen como omitida (no soportada).
fn skipped(id: lopdf::ObjectId, orig_len: u64, reason: ImageSkipReason) -> ImageOutcome {
    ImageOutcome {
        stat: ImageStat {
            object_id: id.0,
            original_bytes: orig_len,
            output_bytes: orig_len,
            action: ImageAction::Skipped,
            skip_reason: Some(reason),
        },
        warnings: vec![Warning::ImageSkipped(id.0)],
    }
}

fn memory_skipped(
    id: lopdf::ObjectId,
    orig_len: u64,
    estimated_bytes: u64,
    limit: u64,
) -> ImageOutcome {
    ImageOutcome {
        stat: ImageStat {
            object_id: id.0,
            original_bytes: orig_len,
            output_bytes: orig_len,
            action: ImageAction::Skipped,
            skip_reason: Some(ImageSkipReason::MemoryLimit),
        },
        warnings: vec![Warning::Other(format!(
            "imagen omitida por memoria: estimado {estimated_bytes} bytes, límite {limit}"
        ))],
    }
}

/// Construye un outcome que marca la imagen como preservada byte-idéntica
/// (firma/sello). No genera warning por-imagen: el pipeline emite un único
/// resumen tras el bucle para no hacer ruido.
fn preserved(id: lopdf::ObjectId, orig_len: u64) -> ImageOutcome {
    ImageOutcome {
        stat: ImageStat {
            object_id: id.0,
            original_bytes: orig_len,
            output_bytes: orig_len,
            action: ImageAction::Preserved,
            skip_reason: None,
        },
        warnings: vec![],
    }
}

/// Outcome de preservación conservadora para casos de /SMask que no se pueden
/// recomprimir sin riesgo (Matte/premultiplicado, alfa propio, máscara rara).
///
/// NB: el pipeline no toca el stream, pero el paso doc-level
/// `rewrite::cleanup_and_compress` envuelve en Flate los streams SIN /Filter;
/// el invariante "Skipped ⇒ byte-idéntico en el output" rige para streams con
/// filtro (todas las imágenes reales).
fn smask_skip(
    id: lopdf::ObjectId,
    orig_len: u64,
    reason: ImageSkipReason,
    why: &str,
) -> ImageOutcome {
    ImageOutcome {
        stat: ImageStat {
            object_id: id.0,
            original_bytes: orig_len,
            output_bytes: orig_len,
            action: ImageAction::Skipped,
            skip_reason: Some(reason),
        },
        warnings: vec![Warning::Other(format!(
            "imagen con /SMask preservada sin recomprimir ({why})"
        ))],
    }
}

/// Estrategia de codec de salida para una imagen.
#[derive(Clone, Copy, PartialEq)]
enum Codec {
    /// Recomprimir a JPEG (DCTDecode). Foto / imagen ya en formato con pérdida.
    Jpeg,
    /// Re-encode sin pérdida a FlateDecode. Línea/texto: nunca DCT (halos).
    FlateLossless,
}

/// Fuente de imagen lista para decodificar: bytes crudos, dimensiones validadas y
/// el stream completo, que la ruta Flate necesita por su dict
/// (Filter/ColorSpace/DecodeParms).
struct ImageSource {
    orig_len: u64,
    width: u32,
    height: u32,
    raw_bytes: Vec<u8>,
    stream_for_flate: Stream,
    /// La imagen (base) declara /SMask: conservar la entrada al reescribir y
    /// preservar si el decode revela alfa propio.
    has_smask: bool,
}

/// Estimación conservadora del raster decodificado. Ocho bytes por píxel cubre
/// RGBA16, el formato más ancho que puede materializar el decoder `image` en
/// las rutas soportadas.
pub(crate) fn estimated_decoded_bytes(width: u32, height: u32) -> u64 {
    u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(8)
}

/// Estima el working set de preparar una imagen sin decodificarla. Incluye el
/// stream clonado y varias generaciones del raster (decode, transform, encode;
/// el modo perceptual mantiene proxies/candidatos adicionales). Sólo gobierna
/// el tamaño de los lotes: sobreestimar reduce paralelismo pero no cambia bytes.
pub(crate) fn estimated_working_bytes(
    doc: &Document,
    id: lopdf::ObjectId,
    perceptual: bool,
) -> u64 {
    let Ok(stream) = doc.get_object(id).and_then(|o| o.as_stream()) else {
        return 1;
    };
    let width = stream
        .dict
        .get(b"Width")
        .and_then(|o| o.as_i64())
        .ok()
        .filter(|v| *v > 0)
        .and_then(|v| u32::try_from(v).ok());
    let height = stream
        .dict
        .get(b"Height")
        .and_then(|o| o.as_i64())
        .ok()
        .filter(|v| *v > 0)
        .and_then(|v| u32::try_from(v).ok());
    let raster = match (width, height) {
        (Some(w), Some(h)) => estimated_decoded_bytes(w, h),
        _ => 1,
    };
    let raster_copies = if perceptual { 5 } else { 3 };
    raster
        .saturating_mul(raster_copies)
        .saturating_add((stream.content.len() as u64).saturating_mul(2))
        .max(1)
}

/// Resultado de la etapa de carga+guardas ([`load`]).
enum Load {
    /// No es un XObject de tipo Image (o le faltan dims): el pipeline no genera
    /// stat para ella.
    NotImage,
    /// Guarda temprana resuelta (preservada, /SMask o dims malformadas): el
    /// `ImageOutcome` ya está listo, no se decodifica nada.
    Done(ImageOutcome),
    /// Imagen válida y procesable.
    Ready(ImageSource),
}

/// Etapa 1 — carga + guardas. Lee el stream y aplica, en este orden: filtro por
/// `Subtype=Image`, preservación de firma/sello (`preserve_this`), /SMask (F1) y
/// validación de dimensiones (F9). Sólo si nada dispara se clona el contenido y
/// el stream para las etapas siguientes.
fn load(
    doc: &Document,
    id: lopdf::ObjectId,
    preserve_this: bool,
    max_image_bytes: Option<u64>,
) -> Load {
    let stream = match doc.get_object(id).and_then(|o| o.as_stream()) {
        Ok(s) => s,
        Err(_) => return Load::NotImage,
    };
    let dict = &stream.dict;
    // solo XObject de tipo Image
    match dict.get(b"Subtype").and_then(|o| o.as_name()) {
        Ok(name) if name == b"Image" => {}
        _ => return Load::NotImage,
    }
    // Firma/sello detectado por la pre-flight: preservar bytes originales. Va
    // antes de /SMask, validación de dims y decodificación: no tocamos la imagen
    // en absoluto.
    if preserve_this {
        return Load::Done(preserved(id, stream.content.len() as u64));
    }
    // Lever C: la base con /SMask ya no se preserva entera — se recomprime
    // conservando la referencia a la máscara. Sólo se preserva en los casos
    // que perderían información real:
    //  - /Matte en la máscara (color premultiplicado: los píxeles de la base
    //    están acoplados a ella) → preservar.
    //  - /SMask que no es Reference o no se puede inspeccionar → preservar
    //    (conservador: no adivinar).
    let has_smask = match dict.get(b"SMask") {
        Err(_) => false,
        Ok(Object::Name(name)) if name == b"None" => false,
        Ok(Object::Reference(mid)) => {
            match doc.get_object(*mid).ok().and_then(|o| o.as_stream().ok()) {
                Some(mask) if mask.dict.has(b"Matte") => {
                    return Load::Done(smask_skip(
                        id,
                        stream.content.len() as u64,
                        ImageSkipReason::SoftMaskMatte,
                        "/Matte",
                    ));
                }
                Some(_) => {}
                None => {
                    return Load::Done(smask_skip(
                        id,
                        stream.content.len() as u64,
                        ImageSkipReason::SoftMaskUninspectable,
                        "máscara no inspeccionable",
                    ));
                }
            }
            true
        }
        Ok(_) => {
            return Load::Done(smask_skip(
                id,
                stream.content.len() as u64,
                ImageSkipReason::SoftMaskUnsupported,
                "/SMask no es referencia",
            ));
        }
    };
    // F9: leemos Width/Height como i64 y validamos antes de convertir a u32. Un
    // valor negativo o absurdamente grande se envolvería silenciosamente con
    // `as u32`. Si las dimensiones no son sanas (<= 0 o > 100_000 px por lado)
    // omitimos la imagen sin procesarla ni reescribirla.
    let w_i64 = match dict.get(b"Width").and_then(|o| o.as_i64()) {
        Ok(v) => v,
        Err(_) => return Load::NotImage,
    };
    let h_i64 = match dict.get(b"Height").and_then(|o| o.as_i64()) {
        Ok(v) => v,
        Err(_) => return Load::NotImage,
    };
    const MAX_DIM: i64 = 100_000;
    if w_i64 <= 0 || h_i64 <= 0 || w_i64 > MAX_DIM || h_i64 > MAX_DIM {
        return Load::Done(skipped(
            id,
            stream.content.len() as u64,
            ImageSkipReason::InvalidDimensions,
        ));
    }
    let decoded_bytes = estimated_decoded_bytes(w_i64 as u32, h_i64 as u32);
    if let Some(limit) = max_image_bytes {
        if decoded_bytes > limit {
            return Load::Done(memory_skipped(
                id,
                stream.content.len() as u64,
                decoded_bytes,
                limit,
            ));
        }
    }
    // Clonamos el stream completo para la ruta Flate: el decodificador necesita
    // el dict (Filter/ColorSpace/DecodeParms) además del contenido.
    Load::Ready(ImageSource {
        orig_len: stream.content.len() as u64,
        width: w_i64 as u32,
        height: h_i64 as u32,
        raw_bytes: stream.content.clone(),
        stream_for_flate: stream.clone(),
        has_smask,
    })
}

/// Imagen decodificada a píxeles + el codec de salida elegido.
struct Decoded {
    image: image::DynamicImage,
    codec: Codec,
    /// Los píxeles salieron SOLO de los bytes del stream (ruta DCT/sniffing,
    /// tras des-encadenar el prefijo /Filter): habilita el cache perceptual por
    /// identidad de fuente. La ruta Flate lee además el dict (ColorSpace/BPC/
    /// DecodeParms) → `false` (no cacheable por identidad raw+filtro).
    #[cfg_attr(not(feature = "perceptual"), allow(dead_code))]
    bytes_only: bool,
    /// La fuente llegó SIN pérdida (raster en Flate) — si además sale como
    /// JPEG, es un transcodificado de PRIMERA generación y usa sus propias
    /// perillas. Las que ya venían en DCT son de segunda generación.
    lossless_source: bool,
}

enum DecodeFailure {
    Unsupported,
    Memory { estimated_bytes: u64, limit: u64 },
}

fn filter_contains(dict: &lopdf::Dictionary, expected: &[u8]) -> bool {
    match dict.get(b"Filter") {
        Ok(Object::Name(name)) => name.as_slice() == expected,
        Ok(Object::Array(filters)) => filters
            .iter()
            .any(|filter| filter.as_name().is_ok_and(|name| name == expected)),
        _ => false,
    }
}

/// Clasifica el caso no soportado sin volver a decodificar. Es telemetría
/// conservadora: sólo asigna categorías específicas cuando el diccionario las
/// declara explícitamente; lo demás queda en `unsupported_encoding`.
fn unsupported_reason(doc: &Document, stream: &Stream) -> ImageSkipReason {
    let dict = &stream.dict;
    if filter_contains(dict, b"JPXDecode") {
        return ImageSkipReason::Jpx;
    }
    if filter_contains(dict, b"CCITTFaxDecode") || filter_contains(dict, b"CCF") {
        return ImageSkipReason::Ccit;
    }
    if filter_contains(dict, b"JBIG2Decode") {
        return ImageSkipReason::Jbig2;
    }
    if filter_contains(dict, b"LZWDecode") || filter_contains(dict, b"LZW") {
        return ImageSkipReason::Lzw;
    }

    let bpc = dict.get(b"BitsPerComponent").and_then(Object::as_i64).ok();
    let color_space = dict.get(b"ColorSpace").ok().and_then(|value| match value {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    });
    let indexed = color_space
        .and_then(|value| value.as_array().ok())
        .and_then(|items| items.first())
        .and_then(|head| head.as_name().ok())
        .is_some_and(|head| head == b"Indexed" || head == b"I");
    if indexed && bpc.is_some_and(|bits| matches!(bits, 1 | 2 | 4)) {
        return ImageSkipReason::IndexedSubByte;
    }
    if bpc.is_some_and(|bits| bits != 8) {
        return ImageSkipReason::UnsupportedBitDepth;
    }

    let supported_color_space = match color_space {
        Some(Object::Name(name)) => matches!(
            name.as_slice(),
            b"DeviceRGB" | b"RGB" | b"DeviceGray" | b"G" | b"DeviceCMYK" | b"CMYK"
        ),
        Some(Object::Array(items)) => items
            .first()
            .and_then(|head| head.as_name().ok())
            .is_some_and(|head| head == b"ICCBased" || head == b"Indexed" || head == b"I"),
        _ => false,
    };
    if !supported_color_space {
        return ImageSkipReason::UnsupportedColorSpace;
    }
    ImageSkipReason::DecodeFailed
}

fn encoded_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn load_encoded_image(
    bytes: &[u8],
    max_image_bytes: Option<u64>,
) -> image::ImageResult<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    if let Some(max_alloc) = max_image_bytes {
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(max_alloc);
        reader.limits(limits);
    }
    reader.decode()
}

/// Etapa 2 — decodificar y clasificar. Rutas:
/// -1. Cadena `[…, DCTDecode]` (lever A): `unwrap_to_dct` des-encadena el prefijo
///     (Flate/A85/AHx/RL) y obtiene los bytes JPEG internos; éstos siguen las
///     rutas 0/1 de abajo. `None` = no es ese caso.
/// 0. JPEG CMYK/YCCK (bug del sello negro): `image`/zune-jpeg lo mal-decodifica a
///    píxeles NEGROS. Lo detectamos y decodificamos con `jpeg-decoder` + fórmula
///    Adobe → RGB fiel → ruta foto → JPEG.
/// 1. Formato que `image` abre directo (DCTDecode/PNG) → ruta foto → JPEG.
/// 2. FlateDecode crudo soportado (P1): inflar según ColorSpace/BPC, luego
///    clasificar contenido para elegir JPEG (foto) vs Flate (línea).
///
/// `Err(())` significa "omitir preservando el stream original" (CMYK que no
/// decodifica, o colorspace/bpc/CCITT/JPX no soportado): el orquestador lo
/// traduce a `Skipped`.
fn decode(
    doc: &Document,
    src: &ImageSource,
    max_image_bytes: Option<u64>,
) -> Result<Decoded, DecodeFailure> {
    // Lever A: cadena `[…, DCTDecode]` — des-encadena el prefijo (Flate/A85/
    // AHx/RL) para obtener el JPEG interno; esos bytes siguen la ruta DCT
    // normal (incluida la detección CMYK). `None` = no es ese caso.
    let unwrapped = crate::image_opt::decode::unwrap_to_dct(&src.stream_for_flate);
    let dct_bytes: &[u8] = unwrapped.as_deref().unwrap_or(&src.raw_bytes);

    // No confiamos sólo en /Width y /Height: un JPEG/PNG hostil puede declarar
    // dimensiones pequeñas en el diccionario PDF y enormes en su header real.
    if let Some(limit) = max_image_bytes {
        if let Some((width, height)) = encoded_dimensions(dct_bytes) {
            let estimated_bytes = estimated_decoded_bytes(width, height);
            if estimated_bytes > limit {
                return Err(DecodeFailure::Memory {
                    estimated_bytes,
                    limit,
                });
            }
        }
    }

    // Si la decodificación CMYK falla, preservamos el original (no corromper).
    let cmyk_decoded = if crate::image_opt::jpeg::is_cmyk_jpeg(dct_bytes) {
        match crate::image_opt::jpeg::decode_cmyk_jpeg(dct_bytes) {
            Some(img) => Some(img),
            None => return Err(DecodeFailure::Unsupported),
        }
    } else {
        None
    };

    let (image, codec, bytes_only, lossless_source) = match cmyk_decoded {
        Some(img) => (img, Codec::Jpeg, true, false),
        None => match load_encoded_image(dct_bytes, max_image_bytes) {
            Ok(d) => (d, Codec::Jpeg, true, false),
            Err(_) => match crate::image_opt::decode::decode_flate_image(
                doc,
                &src.stream_for_flate,
                src.width,
                src.height,
            ) {
                Some(d) => {
                    // Clasificación content-aware: sólo las fotos se vuelven JPEG;
                    // línea/texto se mantiene sin pérdida para no crear halos.
                    let codec = match crate::image_opt::classify::classify(&d) {
                        crate::image_opt::classify::Content::Photo => Codec::Jpeg,
                        crate::image_opt::classify::Content::LineArt => Codec::FlateLossless,
                    };
                    (d, codec, false, true)
                }
                None => return Err(DecodeFailure::Unsupported),
            },
        },
    };
    Ok(Decoded {
        image,
        codec,
        bytes_only,
        lossless_source,
    })
}

/// Imagen tras la etapa de decisión: píxeles (posiblemente remuestreados), codec,
/// las dimensiones que se escribirán en el dict y la acción reportada.
struct Transformed {
    image: image::DynamicImage,
    codec: Codec,
    width: u32,
    height: u32,
    action: ImageAction,
    warnings: Vec<Warning>,
}

/// Etapa 3 — decidir y aplicar downsampling (P2). El DPI efectivo viene del CTM
/// del content stream (`effective_dpi`); derivamos el tamaño de display en puntos
/// y sólo remuestreamos si el DPI actual supera el objetivo con margen (>1.05×).
///
/// Fallback conservador: sin DPI efectivo conocido (imagen nunca pintada o CTM
/// degenerado) NO se downsamplea (comportamiento seguro de v1). También emite el
/// warning de alfa descartado cuando una imagen con canal alfa va a JPEG.
fn transform(
    decoded: Decoded,
    src: &ImageSource,
    downsample_on: bool,
    target_dpi: u32,
    effective_dpi: Option<f32>,
) -> Transformed {
    let Decoded { image, codec, .. } = decoded;

    let mut warnings: Vec<Warning> = Vec::new();
    // Si la imagen tiene canal alfa y vamos a JPEG, el re-encode lo descarta.
    if image.color().has_alpha() && codec == Codec::Jpeg {
        warnings.push(Warning::Other("alpha descartado al recomprimir".into()));
    }

    let mut img = image;
    let mut width = src.width;
    let mut height = src.height;
    let mut action = ImageAction::Recompressed;
    if downsample_on {
        if let Some(dpi_eff) = effective_dpi.filter(|d| d.is_finite() && *d > 0.0) {
            // Headroom del 5%: una imagen a 151 DPI con objetivo 150 no se
            // re-encoda para arañar un píxel. Las muy sobre-resolución no se ven
            // afectadas por este umbral.
            if dpi_eff > target_dpi as f32 * 1.05 {
                let disp_w = width as f32 / (dpi_eff / 72.0);
                let disp_h = height as f32 / (dpi_eff / 72.0);
                if let Some((nw, nh)) = target_dimensions(width, height, disp_w, disp_h, target_dpi)
                {
                    img = downsample(&img, nw, nh);
                    // Tras remuestrear, el /Width /Height del PDF debe coincidir
                    // con los píxeles reales. Crítico para el path Flate (bytes =
                    // W*H*canales); en JPEG evita un dict inconsistente con el SOF.
                    width = nw;
                    height = nh;
                    action = ImageAction::Downsampled;
                }
            }
        }
    }

    Transformed {
        image: img,
        codec,
        width,
        height,
        action,
        warnings,
    }
}

/// Etapa 4 — codificar los píxeles al codec elegido. El path JPEG preserva gris
/// como gris (L8/DeviceGray) en vez de inflarlo a RGB; el path Flate hace lo
/// mismo. Ambos toman el `color_space` real del encoder, no un valor fijo.
/// `None` si el encoder no pudo producir bytes (el orquestador lo traduce a
/// `Skipped`).
fn encode(image: image::DynamicImage, codec: Codec, quality: u8) -> Option<Encoded> {
    match codec {
        Codec::Jpeg => JpegRecompressor.recompress(&RawImage { image }, quality),
        Codec::FlateLossless => {
            crate::image_opt::flate::encode_flate_lossless(&image).map(|e| Encoded {
                bytes: e.bytes,
                filter: "FlateDecode",
                color_space: e.color_space,
            })
        }
    }
}

/// Trabajo por-imagen listo para aplicarse al documento. Lo produce
/// [`prepare_image`] (etapas 1-4, read-only sobre el doc → paralelizable) y lo
/// consume [`commit_prepared`] (etapa 5, serial: la única que muta el doc).
pub(crate) enum Prepared {
    /// No es un XObject de tipo Image: no genera stat.
    Skip,
    /// Outcome final que NO requiere mutar el doc: preservada/omitida/`Kept`/
    /// máscara. Ya trae su `ImageStat` y warnings.
    Ready(ImageOutcome),
    /// La recompresión mejora: hay que reescribir el stream (fase serial).
    Write(WriteReq),
}

/// Petición de reescritura de un stream de imagen — la parte que muta el doc,
/// diferida a la fase serial para poder paralelizar el cómputo previo.
pub(crate) struct WriteReq {
    id: lopdf::ObjectId,
    encoded: Encoded,
    width: u32,
    height: u32,
    orig_len: u64,
    action: ImageAction,
    warnings: Vec<Warning>,
}

/// Etapa 5 (serial) — traduce un [`Prepared`] en `ImageOutcome`, mutando el doc
/// sólo en el caso `Write`. Es la ÚNICA función del pipeline por-imagen que toma
/// `&mut Document`, así que corre en serie tras el cómputo (potencialmente
/// paralelo) de [`prepare_image`].
pub(crate) fn commit_prepared(doc: &mut Document, prepared: Prepared) -> Option<ImageOutcome> {
    match prepared {
        Prepared::Skip => None,
        Prepared::Ready(outcome) => Some(outcome),
        Prepared::Write(w) => Some(write_image(doc, w)),
    }
}

/// Reescribe el stream y su dict (Filter/Width/Height/BPC/ColorSpace, se quita
/// DecodeParms). F2: sólo reportamos el tamaño menor si el reemplazo mutable
/// realmente ocurrió; si falla, el original sigue intacto y reportamos `Skipped`
/// (no mentir con el tamaño menor). El piso por-imagen (Kept si no mejora) ya lo
/// resolvió [`prepare_image`]: aquí `encoded` siempre es más chico que el
/// original.
fn write_image(doc: &mut Document, w: WriteReq) -> ImageOutcome {
    let WriteReq {
        id,
        encoded,
        width,
        height,
        orig_len,
        action,
        mut warnings,
    } = w;
    // F5: guardamos la longitud antes de mover `bytes` (sin clonar).
    let new_len = encoded.bytes.len() as u64;
    let replaced = match doc.get_object_mut(id).and_then(|obj| obj.as_stream_mut()) {
        Ok(stream) => {
            stream.set_content(encoded.bytes);
            stream
                .dict
                .set("Filter", Object::Name(encoded.filter.as_bytes().to_vec()));
            stream.dict.set("Width", Object::Integer(width as i64));
            stream.dict.set("Height", Object::Integer(height as i64));
            stream.dict.set("BitsPerComponent", Object::Integer(8));
            stream.dict.set(
                "ColorSpace",
                Object::Name(encoded.color_space.as_bytes().to_vec()),
            );
            stream.dict.remove(b"DecodeParms");
            // NB: la entrada /SMask (si existe) se deja intacta a propósito:
            // la máscara sigue referenciada y prune no la borra (lever C).
            true
        }
        Err(_) => false,
    };

    if !replaced {
        // el reemplazo no se aplicó: original intacto → omitida
        warnings.push(Warning::ImageSkipped(id.0));
        return ImageOutcome {
            stat: ImageStat {
                object_id: id.0,
                original_bytes: orig_len,
                output_bytes: orig_len,
                action: ImageAction::Skipped,
                skip_reason: Some(ImageSkipReason::RewriteFailed),
            },
            warnings,
        };
    }

    ImageOutcome {
        stat: ImageStat {
            object_id: id.0,
            original_bytes: orig_len,
            output_bytes: new_len,
            action,
            skip_reason: None,
        },
        warnings,
    }
}

/// Parámetros por-imagen que el bucle del pipeline pasa a [`prepare_image`].
pub(crate) struct ImageParams {
    pub(crate) quality: u8,
    /// Modo perceptual: SSIM2 objetivo (spec 2026-07-11). `Some(τ)` solo si la
    /// imagen es elegible (el pipeline ya filtró tamaño mínimo); la etapa
    /// encode busca la menor q con score ≥ τ en vez de usar `quality`.
    pub(crate) quality_target: Option<f32>,
    pub(crate) target_dpi: u32,
    /// DPI objetivo alternativo para transcodificados de primera generación
    /// (fuente sin pérdida → JPEG). `None` = usar `target_dpi` para todo.
    pub(crate) transcode_dpi: Option<u32>,
    /// Calidad JPEG alternativa para transcodificados de primera generación.
    /// `None` = usar `quality` para todo.
    pub(crate) transcode_quality: Option<u8>,
    /// Tope estimado del raster decodificado para esta imagen.
    pub(crate) max_image_bytes: Option<u64>,
    /// Downsampling habilitado (opts.downsample).
    pub(crate) downsample: bool,
    /// DPI efectivo real de la imagen (máximo entre sus usos), derivado del
    /// CTM. `None` → no se hace downsampling (fallback conservador).
    pub(crate) effective_dpi: Option<f32>,
    /// La pre-flight de firmas/sellos la marcó preservable: bytes ORIGINALES.
    pub(crate) preserve: bool,
    /// El XObject se usa como `/SMask` de otra imagen: fuerza re-encode sin
    /// pérdida (Flate) y desactiva el downsampling, para no mover valores de
    /// transparencia.
    pub(crate) is_smask: bool,
}

/// Etapas 1-4 (read-only sobre el doc) — decide qué hacer con una imagen XObject
/// y devuelve un [`Prepared`] listo para que [`commit_prepared`] lo aplique en
/// serie. NO muta el documento: por eso el pipeline puede correr esta parte (que
/// es el 99% del CPU: decode + búsqueda + encode) en paralelo sobre `&Document`.
///
/// Las imágenes que no son XObject de tipo Image devuelven `Prepared::Skip` (no
/// generan stat); las que sí lo son pero no se pueden decodificar/recomprimir se
/// marcan como `Skipped` dentro de un `Prepared::Ready`.
pub(crate) fn prepare_image(
    doc: &Document,
    id: lopdf::ObjectId,
    p: &ImageParams,
    #[cfg(feature = "perceptual")] cache: &crate::image_opt::perceptual::SearchCache,
) -> Prepared {
    let src = match load(doc, id, p.preserve, p.max_image_bytes) {
        Load::NotImage => return Prepared::Skip,
        Load::Done(outcome) => return Prepared::Ready(outcome),
        Load::Ready(src) => src,
    };

    let mut decoded = match decode(doc, &src, p.max_image_bytes) {
        Ok(d) => d,
        Err(DecodeFailure::Unsupported) => {
            let reason = unsupported_reason(doc, &src.stream_for_flate);
            return Prepared::Ready(skipped(id, src.orig_len, reason));
        }
        Err(DecodeFailure::Memory {
            estimated_bytes,
            limit,
        }) => {
            return Prepared::Ready(memory_skipped(id, src.orig_len, estimated_bytes, limit));
        }
    };

    // Lever C: la base decodificó con alfa PROPIO y además tiene /SMask
    // externa — re-encodear (JPEG o Flate-RGB) perdería ese alfa → preservar.
    if src.has_smask && decoded.image.color().has_alpha() {
        return Prepared::Ready(smask_skip(
            id,
            src.orig_len,
            ImageSkipReason::EmbeddedAlphaWithSoftMask,
            "alfa propio",
        ));
    }

    // Lever C: máscara de transparencia — nunca lossy, sin downsample.
    // Sólo máscaras grises (Luma8, el caso PDF-válido); una máscara que
    // decodifica a otra cosa se preserva intacta (más seguro que convertir:
    // el redondeo de luma movería valores de alfa).
    if p.is_smask {
        match &decoded.image {
            image::DynamicImage::ImageLuma8(_) => decoded.codec = Codec::FlateLossless,
            _ => {
                return Prepared::Ready(smask_skip(
                    id,
                    src.orig_len,
                    ImageSkipReason::NonGraySoftMask,
                    "máscara no-gris",
                ));
            }
        }
    }

    // Antes de que `transform` consuma `decoded`: ¿los píxeles son función
    // exclusiva de los bytes? (habilita el cache perceptual por identidad).
    #[cfg(feature = "perceptual")]
    let bytes_only = decoded.bytes_only;

    // Transcodificado de PRIMERA generación: la fuente llegó sin pérdida y sale
    // como JPEG. `transform` no toca el codec (sólo avisa por alfa), así que
    // decidirlo acá es equivalente a decidirlo después. Ver `transcode_dpi`.
    let first_gen = decoded.lossless_source && decoded.codec == Codec::Jpeg;
    let target_dpi = match p.transcode_dpi {
        Some(dpi) if first_gen => dpi,
        _ => p.target_dpi,
    };
    let quality = match p.transcode_quality {
        Some(q) if first_gen => q,
        _ => p.quality,
    };

    let Transformed {
        image,
        codec,
        width,
        height,
        action,
        mut warnings,
    } = transform(
        decoded,
        &src,
        p.downsample && !p.is_smask,
        target_dpi,
        p.effective_dpi,
    );

    // Modo perceptual (opt-in): solo path JPEG y solo si la imagen es elegible
    // (dims post-transform razonables — las máscaras y line-art van por Flate
    // y no entran aquí porque su codec no es Jpeg — y el piso de bytes: las
    // miniaturas no pagan la búsqueda).
    let eligible_target = p
        .quality_target
        .filter(|_| codec == Codec::Jpeg && width.min(height) >= 64 && src.orig_len >= 10_240);

    let encoded_result: Option<Encoded> = match eligible_target {
        Some(target) => {
            #[cfg(feature = "perceptual")]
            {
                // Cache por identidad de fuente (raw+filtro+dims+τ), solo ruta
                // DCT: copias idénticas del mismo stream no repiten la búsqueda.
                match crate::image_opt::perceptual::encode_jpeg_at_target_cached(
                    cache,
                    bytes_only.then(|| crate::image_opt::perceptual::CacheKeySrc {
                        raw_bytes: &src.raw_bytes,
                        filter: src.stream_for_flate.dict.get(b"Filter").ok().cloned(),
                        decode_parms: src.stream_for_flate.dict.get(b"DecodeParms").ok().cloned(),
                        dp: src.stream_for_flate.dict.get(b"DP").ok().cloned(),
                    }),
                    &RawImage { image },
                    target,
                ) {
                    Some((e, reached)) => {
                        if !reached {
                            warnings.push(Warning::Other(format!(
                                "quality_target {target} no alcanzado; mejor esfuerzo a q={}",
                                crate::image_opt::perceptual::Q_MAX
                            )));
                        }
                        Some(e)
                    }
                    None => None,
                }
            }
            #[cfg(not(feature = "perceptual"))]
            {
                warnings.push(Warning::Other(
                    "quality_target ignorado: build sin el feature 'perceptual'".into(),
                ));
                let _ = target;
                encode(image, codec, quality)
            }
        }
        None => encode(image, codec, quality),
    };

    let encoded = match encoded_result {
        Some(e) => e,
        None => {
            // no se pudo recomprimir → omitida (conservando los warnings previos)
            let mut out = skipped(id, src.orig_len, ImageSkipReason::EncodeFailed);
            out.warnings.extend(warnings);
            return Prepared::Ready(out);
        }
    };

    // Piso por-imagen (antes en `finalize`, ahora read-only): si el output no es
    // más chico que el original, conservamos el original (`Kept`) — no hace falta
    // mutar el doc, así que es un outcome final sin fase de escritura.
    if encoded.bytes.len() as u64 >= src.orig_len {
        return Prepared::Ready(ImageOutcome {
            stat: ImageStat {
                object_id: id.0,
                original_bytes: src.orig_len,
                output_bytes: src.orig_len,
                action: ImageAction::Kept,
                skip_reason: None,
            },
            warnings,
        });
    }

    Prepared::Write(WriteReq {
        id,
        encoded,
        width,
        height,
        orig_len: src.orig_len,
        action,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Document, Stream};

    fn reason_for(dict: lopdf::Dictionary) -> ImageSkipReason {
        unsupported_reason(&Document::with_version("1.5"), &Stream::new(dict, vec![]))
    }

    #[test]
    fn unsupported_reason_identifies_standard_codecs() {
        assert_eq!(
            reason_for(dictionary! { "Filter" => "JPXDecode" }),
            ImageSkipReason::Jpx
        );
        assert_eq!(
            reason_for(dictionary! { "Filter" => "CCITTFaxDecode" }),
            ImageSkipReason::Ccit
        );
        assert_eq!(
            reason_for(dictionary! { "Filter" => "JBIG2Decode" }),
            ImageSkipReason::Jbig2
        );
        assert_eq!(
            reason_for(dictionary! { "Filter" => "LZWDecode" }),
            ImageSkipReason::Lzw
        );
    }

    #[test]
    fn unsupported_reason_identifies_sub_byte_indexed() {
        let reason = reason_for(dictionary! {
            "Filter" => "FlateDecode",
            "BitsPerComponent" => 4,
            "ColorSpace" => vec![
                Object::Name(b"Indexed".to_vec()),
                Object::Name(b"DeviceRGB".to_vec()),
                Object::Integer(15),
                Object::String(vec![0; 48], lopdf::StringFormat::Literal),
            ],
        });
        assert_eq!(reason, ImageSkipReason::IndexedSubByte);
    }

    #[test]
    fn unsupported_reason_separates_bit_depth_color_space_and_bad_data() {
        assert_eq!(
            reason_for(dictionary! {
                "Filter" => "FlateDecode", "BitsPerComponent" => 1,
                "ColorSpace" => "DeviceGray",
            }),
            ImageSkipReason::UnsupportedBitDepth
        );
        assert_eq!(
            reason_for(dictionary! {
                "Filter" => "FlateDecode", "BitsPerComponent" => 8,
                "ColorSpace" => vec![Object::Name(b"CalRGB".to_vec()), dictionary! {}.into()],
            }),
            ImageSkipReason::UnsupportedColorSpace
        );
        assert_eq!(
            reason_for(dictionary! {
                "Filter" => "FlateDecode", "BitsPerComponent" => 8,
                "ColorSpace" => "DeviceRGB",
            }),
            ImageSkipReason::DecodeFailed
        );
    }
}
