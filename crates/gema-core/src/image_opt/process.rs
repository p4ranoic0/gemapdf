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
//! 5. [`finalize`] — aplica el piso por-imagen y reescribe el stream + su dict.
//!
//! El orquestador [`process_image`] encadena las etapas y traduce cada salida
//! temprana en el [`ImageOutcome`] correspondiente. La lógica y los invariantes
//! (comentarios F1/F2/F5/F8/F9/F10, P1/P2, bug del sello negro) son los mismos
//! que tenía el monolito previo en `pipeline.rs`; aquí sólo están repartidos.

use crate::image_opt::jpeg::JpegRecompressor;
use crate::image_opt::{downsample, target_dimensions, Encoded, RawImage, Recompressor};
use crate::report::{ImageAction, ImageStat, Warning};
use lopdf::{Document, Object, Stream};

/// Resultado de procesar una imagen: su stat más cualquier warning generado
/// (canal alfa descartado, imagen no soportada/omitida, etc.). Siempre se
/// produce un `stat` para que las imágenes omitidas tengan señal en el reporte.
pub(crate) struct ImageOutcome {
    pub(crate) stat: ImageStat,
    pub(crate) warnings: Vec<Warning>,
}

/// Construye un outcome que marca la imagen como omitida (no soportada).
fn skipped(id: lopdf::ObjectId, orig_len: u64) -> ImageOutcome {
    ImageOutcome {
        stat: ImageStat {
            object_id: id.0,
            original_bytes: orig_len,
            output_bytes: orig_len,
            action: ImageAction::Skipped,
        },
        warnings: vec![Warning::ImageSkipped(id.0)],
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
fn smask_skip(id: lopdf::ObjectId, orig_len: u64, why: &str) -> ImageOutcome {
    ImageOutcome {
        stat: ImageStat {
            object_id: id.0,
            original_bytes: orig_len,
            output_bytes: orig_len,
            action: ImageAction::Skipped,
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
fn load(doc: &Document, id: lopdf::ObjectId, preserve_this: bool) -> Load {
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
        Ok(Object::Reference(mid)) => {
            let matte_or_unknown = doc
                .get_object(*mid)
                .ok()
                .and_then(|o| o.as_stream().ok())
                .map(|s| s.dict.has(b"Matte"))
                .unwrap_or(true);
            if matte_or_unknown {
                return Load::Done(smask_skip(
                    id,
                    stream.content.len() as u64,
                    "/Matte o máscara no inspeccionable",
                ));
            }
            true
        }
        Ok(_) => {
            return Load::Done(smask_skip(
                id,
                stream.content.len() as u64,
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
        return Load::Done(skipped(id, stream.content.len() as u64));
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
fn decode(doc: &Document, src: &ImageSource) -> Result<Decoded, ()> {
    // Lever A: cadena `[…, DCTDecode]` — des-encadena el prefijo (Flate/A85/
    // AHx/RL) para obtener el JPEG interno; esos bytes siguen la ruta DCT
    // normal (incluida la detección CMYK). `None` = no es ese caso.
    let unwrapped = crate::image_opt::decode::unwrap_to_dct(&src.stream_for_flate);
    let dct_bytes: &[u8] = unwrapped.as_deref().unwrap_or(&src.raw_bytes);

    // Si la decodificación CMYK falla, preservamos el original (no corromper).
    let cmyk_decoded = if crate::image_opt::jpeg::is_cmyk_jpeg(dct_bytes) {
        match crate::image_opt::jpeg::decode_cmyk_jpeg(dct_bytes) {
            Some(img) => Some(img),
            None => return Err(()),
        }
    } else {
        None
    };

    let (image, codec) = match cmyk_decoded {
        Some(img) => (img, Codec::Jpeg),
        None => match image::load_from_memory(dct_bytes) {
            Ok(d) => (d, Codec::Jpeg),
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
                    (d, codec)
                }
                None => return Err(()),
            },
        },
    };
    Ok(Decoded { image, codec })
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
    let Decoded { image, codec } = decoded;

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

/// Etapa 5 — piso por-imagen + reescritura. Si el output recomprimido no es más
/// chico que el original se conserva el original (`Kept`). Si mejora, se
/// reemplaza el stream y su dict (Filter/Width/Height/BPC/ColorSpace, se quita
/// DecodeParms). F2: sólo reportamos el tamaño menor si el reemplazo mutable
/// realmente ocurrió; si falla, el original sigue intacto y reportamos `Skipped`
/// (no mentir con el tamaño menor).
fn finalize(
    doc: &mut Document,
    id: lopdf::ObjectId,
    encoded: Encoded,
    dims: (u32, u32),
    orig_len: u64,
    action: ImageAction,
    mut warnings: Vec<Warning>,
) -> ImageOutcome {
    let (width, height) = dims;
    if encoded.bytes.len() as u64 >= orig_len {
        // no mejora → dejar original
        return ImageOutcome {
            stat: ImageStat {
                object_id: id.0,
                original_bytes: orig_len,
                output_bytes: orig_len,
                action: ImageAction::Kept,
            },
            warnings,
        };
    }

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
        },
        warnings,
    }
}

/// Parámetros por-imagen que el bucle del pipeline pasa a [`process_image`].
pub(crate) struct ImageParams {
    pub(crate) quality: u8,
    /// Modo perceptual: SSIM2 objetivo (spec 2026-07-11). `Some(τ)` solo si la
    /// imagen es elegible (el pipeline ya filtró tamaño mínimo); la etapa
    /// encode busca la menor q con score ≥ τ en vez de usar `quality`.
    pub(crate) quality_target: Option<f32>,
    pub(crate) target_dpi: u32,
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

/// Recomprime una imagen XObject in-place si conviene, encadenando las cinco
/// etapas. Devuelve un `ImageOutcome` con el `ImageStat` y los warnings
/// asociados. Las imágenes que no son XObject de tipo Image devuelven `None` (no
/// generan stat); las que sí lo son pero no se pueden decodificar/recomprimir se
/// marcan como `Skipped`.
pub(crate) fn process_image(
    doc: &mut Document,
    id: lopdf::ObjectId,
    p: &ImageParams,
) -> Option<ImageOutcome> {
    let src = match load(doc, id, p.preserve) {
        Load::NotImage => return None,
        Load::Done(outcome) => return Some(outcome),
        Load::Ready(src) => src,
    };

    let mut decoded = match decode(doc, &src) {
        Ok(d) => d,
        Err(()) => return Some(skipped(id, src.orig_len)),
    };

    // Lever C: la base decodificó con alfa PROPIO y además tiene /SMask
    // externa — re-encodear (JPEG o Flate-RGB) perdería ese alfa → preservar.
    if src.has_smask && decoded.image.color().has_alpha() {
        return Some(smask_skip(id, src.orig_len, "alfa propio"));
    }

    // Lever C: máscara de transparencia — nunca lossy, sin downsample.
    // Sólo máscaras grises (Luma8, el caso PDF-válido); una máscara que
    // decodifica a otra cosa se preserva intacta (más seguro que convertir:
    // el redondeo de luma movería valores de alfa).
    if p.is_smask {
        match &decoded.image {
            image::DynamicImage::ImageLuma8(_) => decoded.codec = Codec::FlateLossless,
            _ => return Some(smask_skip(id, src.orig_len, "máscara no-gris")),
        }
    }

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
        p.target_dpi,
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
                match crate::image_opt::perceptual::encode_jpeg_at_target(
                    &RawImage { image },
                    target,
                ) {
                    Some((e, reached)) => {
                        if !reached {
                            warnings.push(Warning::Other(format!(
                                "quality_target {target} no alcanzado; mejor esfuerzo a q=90"
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
                encode(image, codec, p.quality)
            }
        }
        None => encode(image, codec, p.quality),
    };

    let encoded = match encoded_result {
        Some(e) => e,
        None => {
            // no se pudo recomprimir → omitida (conservando los warnings previos)
            let mut out = skipped(id, src.orig_len);
            out.warnings.extend(warnings);
            return Some(out);
        }
    };

    Some(finalize(
        doc,
        id,
        encoded,
        (width, height),
        src.orig_len,
        action,
        warnings,
    ))
}
