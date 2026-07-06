use crate::error::GemaError;
use crate::image_opt::jpeg::JpegRecompressor;
use crate::image_opt::{downsample, target_dimensions, RawImage, Recompressor};
use crate::options::{CompressOptions, SignaturePolicy};
use crate::progress::Phase;
use crate::report::{ImageAction, ImageStat, Report, Warning};
use lopdf::{Document, Object};

pub struct CompressResult {
    pub output: Vec<u8>,
    pub report: Report,
}

/// Resultado de procesar una imagen: su stat más cualquier warning generado
/// (canal alfa descartado, imagen no soportada/omitida, etc.). Siempre se
/// produce un `stat` para que las imágenes omitidas tengan señal en el reporte.
struct ImageOutcome {
    stat: ImageStat,
    warnings: Vec<Warning>,
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

/// Recomprime una imagen XObject in-place si conviene. Devuelve un `ImageOutcome`
/// con el `ImageStat` y los warnings asociados. Las imágenes que no son XObject
/// de tipo Image devuelven `None` (no generan stat); las que sí lo son pero no
/// se pueden decodificar/recomprimir se marcan como `Skipped`.
fn process_image(
    doc: &mut Document,
    id: lopdf::ObjectId,
    quality: u8,
    target_dpi: u32,
    downsample_on: bool,
    // DPI efectivo real de la imagen (máximo entre sus usos), derivado del CTM
    // del content stream. `None` si nunca se encontró pintada o el CTM es
    // degenerado: en ese caso NO se hace downsampling (fallback conservador).
    effective_dpi: Option<f32>,
    // La pasada pre-flight de firmas/sellos (`signatures::collect_preserved_images`)
    // marcó esta imagen como preservable: se devuelven sus bytes ORIGINALES sin
    // decodificar ni recomprimir (misma mecánica que /SMask).
    preserve_this: bool,
) -> Option<ImageOutcome> {
    let (orig_len, mut width, mut height, raw_bytes, stream_for_flate) = {
        let stream = doc.get_object(id).ok()?.as_stream().ok()?;
        let dict = &stream.dict;
        // solo XObject de tipo Image
        if dict.get(b"Subtype").and_then(|o| o.as_name()).ok()? != b"Image" {
            return None;
        }
        // Firma/sello detectado por la pre-flight: preservar bytes originales.
        // Va antes de /SMask, validación de dims y decodificación: no tocamos la
        // imagen en absoluto.
        if preserve_this {
            return Some(preserved(id, stream.content.len() as u64));
        }
        // F1: una imagen con máscara de transparencia externa (/SMask) se
        // preserva sin tocar. Recomprimirla a JPEG perdería el canal alfa y
        // dejaría el XObject de la máscara huérfano (lo borraría prune). El
        // soporte real de máscaras se difiere a v2.
        if dict.has(b"SMask") {
            return Some(ImageOutcome {
                stat: ImageStat {
                    object_id: id.0,
                    original_bytes: stream.content.len() as u64,
                    output_bytes: stream.content.len() as u64,
                    action: ImageAction::Skipped,
                },
                warnings: vec![Warning::Other(
                    "imagen con máscara de transparencia (/SMask) preservada sin recomprimir"
                        .into(),
                )],
            });
        }
        // F9: leemos Width/Height como i64 y validamos antes de convertir a u32.
        // Un valor negativo o absurdamente grande se envolvería silenciosamente
        // con `as u32` y se escribiría de vuelta en el dict del output. Si las
        // dimensiones no son sanas (<= 0 o > 100_000 px por lado) omitimos la
        // imagen sin procesarla ni reescribirla.
        let w_i64 = dict.get(b"Width").and_then(|o| o.as_i64()).ok()?;
        let h_i64 = dict.get(b"Height").and_then(|o| o.as_i64()).ok()?;
        const MAX_DIM: i64 = 100_000;
        if w_i64 <= 0 || h_i64 <= 0 || w_i64 > MAX_DIM || h_i64 > MAX_DIM {
            return Some(skipped(id, stream.content.len() as u64));
        }
        let w = w_i64 as u32;
        let h = h_i64 as u32;
        // Clonamos el stream completo para la ruta Flate: el decodificador
        // necesita el dict (Filter/ColorSpace/DecodeParms) además del contenido.
        (
            stream.content.len() as u64,
            w,
            h,
            stream.content.clone(),
            stream.clone(),
        )
    };

    // Estrategia de codec de salida para esta imagen.
    #[derive(Clone, Copy, PartialEq)]
    enum Codec {
        /// Recomprimir a JPEG (DCTDecode). Foto / imagen ya en formato con pérdida.
        Jpeg,
        /// Re-encode sin pérdida a FlateDecode. Línea/texto: nunca DCT (halos).
        FlateLossless,
    }

    // Bug del sello negro: un JPEG CMYK/YCCK (4 componentes) —típico de sellos y
    // escudos generados por Adobe, p. ej. el logo "PERÚ PAE" en documentos
    // firmados— lo mal-decodifica el crate `image` (zune-jpeg) y produce píxeles
    // NEGROS. Lo detectamos y decodificamos correctamente con `jpeg-decoder` +
    // la fórmula Adobe (ver `decode_cmyk_jpeg`), obteniendo el RGB fiel. Así el
    // sello queda correcto Y la imagen sigue la ruta normal de recompresión.
    // Si la decodificación CMYK falla, preservamos el original (no corromper).
    let cmyk_decoded = if crate::image_opt::jpeg::is_cmyk_jpeg(&raw_bytes) {
        match crate::image_opt::jpeg::decode_cmyk_jpeg(&raw_bytes) {
            Some(img) => Some(img),
            None => return Some(skipped(id, orig_len)),
        }
    } else {
        None
    };

    // decodificar. Rutas:
    // 0. JPEG CMYK/YCCK: ya decodificado arriba a RGB → ruta foto → JPEG.
    // 1. Formato que `image` abre directo (DCTDecode/PNG) → ruta foto → JPEG.
    // 2. FlateDecode crudo soportado (P1): inflar a píxeles según ColorSpace/BPC,
    //    luego clasificar contenido para elegir JPEG (foto) vs Flate (línea).
    // Si ninguna aplica (predicho, colorspace/bpc no soportado, longitud que no
    // cuadra, CCITT/JPX…) → Skipped, preservando el stream original (v1, F8).
    let (decoded, codec) = match cmyk_decoded {
        Some(img) => (img, Codec::Jpeg),
        None => match image::load_from_memory(&raw_bytes) {
            Ok(d) => (d, Codec::Jpeg),
            Err(_) => {
                match crate::image_opt::decode::decode_flate_image(
                    doc,
                    &stream_for_flate,
                    width,
                    height,
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
                    None => return Some(skipped(id, orig_len)),
                }
            }
        },
    };

    let mut warnings: Vec<Warning> = Vec::new();
    // Si la imagen tiene canal alfa y vamos a JPEG, el re-encode lo descarta.
    if decoded.color().has_alpha() && codec == Codec::Jpeg {
        warnings.push(Warning::Other("alpha descartado al recomprimir".into()));
    }

    // Downsample por DPI efectivo REAL (P2). El DPI viene del CTM del content
    // stream: `effective_dpi`. Derivamos el tamaño de display en puntos como
    // `display_pt = px / (dpi_eff / 72)` y se lo pasamos a `target_dimensions`,
    // que dispara sólo si el DPI actual supera el objetivo.
    //
    // Fallback conservador: si no conocemos el DPI efectivo (la imagen nunca se
    // encontró en un content stream, o su CTM es degenerado), NO hacemos
    // downsampling — preservamos el comportamiento seguro de v1. Nunca dividimos
    // por cero: `effective_dpi` ya excluye escalas nulas.
    let mut img = decoded;
    let mut action = ImageAction::Recompressed;
    if downsample_on {
        if let Some(dpi_eff) = effective_dpi.filter(|d| d.is_finite() && *d > 0.0) {
            // Headroom del 5%: sólo downsampleamos si el DPI efectivo supera el
            // objetivo con margen (>1.05×). Así una imagen a 151 DPI con objetivo
            // 150 no se re-encoda para arañar un píxel — el coste (recompresión +
            // posible pérdida) no compensa. Las imágenes muy sobre-resolución (los
            // tests high_dpi usan DPIs miles de veces el objetivo) no se ven
            // afectadas por este umbral.
            if dpi_eff > target_dpi as f32 * 1.05 {
                let disp_w = width as f32 / (dpi_eff / 72.0);
                let disp_h = height as f32 / (dpi_eff / 72.0);
                if let Some((nw, nh)) = target_dimensions(width, height, disp_w, disp_h, target_dpi)
                {
                    img = downsample(&img, nw, nh);
                    // Actualizamos las dimensiones que se escribirán en el dict:
                    // tras remuestrear, el /Width /Height del PDF debe coincidir
                    // con los píxeles reales. Crítico para el path Flate (bytes =
                    // W*H*canales); en JPEG evita un dict inconsistente con el SOF.
                    width = nw;
                    height = nh;
                    action = ImageAction::Downsampled;
                }
            }
        }
    }

    // Codificar según el codec elegido. Ambas ramas producen los bytes de salida
    // más los metadatos de dict (filtro + colorspace). El path JPEG preserva gris
    // como gris (L8/DeviceGray) en vez de inflarlo a RGB; el path Flate hace lo
    // mismo. Ambos toman el `color_space` real del encoder, no un valor fijo.
    let (out_bytes, out_filter, out_colorspace): (Vec<u8>, &'static str, &'static str) = match codec
    {
        Codec::Jpeg => match JpegRecompressor.recompress(&RawImage { image: img }, quality) {
            Some(e) => (e.bytes, e.filter, e.color_space),
            None => {
                // no se pudo recomprimir → omitida
                let mut out = skipped(id, orig_len);
                out.warnings.extend(warnings);
                return Some(out);
            }
        },
        Codec::FlateLossless => match crate::image_opt::flate::encode_flate_lossless(&img) {
            Some(e) => (e.bytes, "FlateDecode", e.color_space),
            None => {
                let mut out = skipped(id, orig_len);
                out.warnings.extend(warnings);
                return Some(out);
            }
        },
    };

    if out_bytes.len() as u64 >= orig_len {
        // no mejora → dejar original
        return Some(ImageOutcome {
            stat: ImageStat {
                object_id: id.0,
                original_bytes: orig_len,
                output_bytes: orig_len,
                action: ImageAction::Kept,
            },
            warnings,
        });
    }

    // reemplazar el stream. F2: solo reportamos el tamaño recomprimido si el
    // reemplazo realmente ocurrió; si el acceso mutable falla, el stream original
    // sigue intacto y debemos reportar Skipped (no mentir con el tamaño menor).
    // F5: guardamos la longitud antes de mover `out_bytes` (sin clonar).
    let new_len = out_bytes.len() as u64;
    let replaced = match doc.get_object_mut(id).and_then(|obj| obj.as_stream_mut()) {
        Ok(stream) => {
            stream.set_content(out_bytes);
            stream
                .dict
                .set("Filter", Object::Name(out_filter.as_bytes().to_vec()));
            stream.dict.set("Width", Object::Integer(width as i64));
            stream.dict.set("Height", Object::Integer(height as i64));
            stream.dict.set("BitsPerComponent", Object::Integer(8));
            stream.dict.set(
                "ColorSpace",
                Object::Name(out_colorspace.as_bytes().to_vec()),
            );
            stream.dict.remove(b"DecodeParms");
            // NB: no tocamos /SMask aquí; las imágenes con máscara ya se
            // descartaron arriba (F1), así que este stream no la tiene.
            true
        }
        Err(_) => false,
    };

    if !replaced {
        // el reemplazo no se aplicó: original intacto → omitida
        warnings.push(Warning::ImageSkipped(id.0));
        return Some(ImageOutcome {
            stat: ImageStat {
                object_id: id.0,
                original_bytes: orig_len,
                output_bytes: orig_len,
                action: ImageAction::Skipped,
            },
            warnings,
        });
    }

    Some(ImageOutcome {
        stat: ImageStat {
            object_id: id.0,
            original_bytes: orig_len,
            output_bytes: new_len,
            action,
        },
        warnings,
    })
}

/// Comprime un PDF sin reportar progreso. Envoltorio fino sobre
/// [`compress_with_progress`] con un callback no-op; misma API pública que v1.
pub fn compress(input: &[u8], opts: &CompressOptions) -> Result<CompressResult, GemaError> {
    compress_with_progress(input, opts, &mut |_| {})
}

/// Igual que [`compress`], pero invoca `on_phase` en cada transición de fase
/// del pipeline. Orden garantizado (ver [`Phase`]): `Analyzing` →
/// `OptimizingImages { done: 0..=N, total: N }` → `Rewriting` → `Done`; en el
/// retorno temprano firmado-Strict sólo `Analyzing` → `Done`. El callback es
/// síncrono y corre en el mismo hilo: WASM-compatible (sin threads ni canales).
pub fn compress_with_progress(
    input: &[u8],
    opts: &CompressOptions,
    on_phase: &mut dyn FnMut(Phase),
) -> Result<CompressResult, GemaError> {
    on_phase(Phase::Analyzing);

    // F4: parseamos el PDF una sola vez y derivamos el reporte base del mismo
    // doc (antes se hacía load_mem dentro de analyze() y otra vez aquí).
    let mut doc = Document::load_mem(input).map_err(|e| GemaError::Parse(e.to_string()))?;
    if doc.is_encrypted() {
        return Err(GemaError::Encrypted);
    }
    let report0 = crate::analyze::report_from_doc(&doc, input.len() as u64);

    // política de firma
    if report0.is_signed && opts.signatures == SignaturePolicy::Strict {
        let result = CompressResult {
            output: input.to_vec(),
            report: Report {
                output_size: Some(input.len() as u64),
                warnings: vec![Warning::SignedDocument],
                ..report0
            }
            .with_ratio(),
        };
        on_phase(Phase::Done);
        return Ok(result);
    }

    let params = opts.resolved();

    // Pasada pre-flight de firmas/sellos: produce el set de imágenes XObject a
    // preservar byte-idénticas (apariencias de firma + sellos/logos pequeños).
    // Corre una vez, tras el early-return de `Strict` (un doc con firma cripto ya
    // se devolvió intacto arriba) y antes del bucle de imágenes. No muta el doc.
    let preserve = crate::signatures::collect_preserved_images(&doc);

    // P2: DPI efectivo real de cada imagen a partir del CTM del content stream.
    // Se calcula una vez, antes del bucle de imágenes. Las imágenes ausentes del
    // mapa (nunca pintadas / CTM degenerado) no se downsamplean (fallback v1).
    let dpi_map = crate::geometry::effective_dpi_map(&doc);

    // recolectar ids de imágenes (XObject /Subtype /Image)
    let image_ids: Vec<lopdf::ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(id, obj)| {
            let s = obj.as_stream().ok()?;
            if s.dict.get(b"Subtype").and_then(|o| o.as_name()).ok()? == b"Image" {
                Some(*id)
            } else {
                None
            }
        })
        .collect();

    let total = image_ids.len();
    on_phase(Phase::OptimizingImages { done: 0, total });

    let mut stats = Vec::new();
    let mut img_warnings = Vec::new();
    for (i, id) in image_ids.into_iter().enumerate() {
        let eff_dpi = dpi_map.get(&id).copied();
        let preserve_this = preserve.contains(&id);
        // las imágenes no soportadas (no-Image) simplemente no generan stat
        if let Some(outcome) = process_image(
            &mut doc,
            id,
            params.jpeg_quality,
            params.image_dpi,
            opts.downsample,
            eff_dpi,
            preserve_this,
        ) {
            stats.push(outcome.stat);
            img_warnings.extend(outcome.warnings);
        }
        on_phase(Phase::OptimizingImages { done: i + 1, total });
    }

    on_phase(Phase::Rewriting);

    if opts.remove_metadata {
        crate::rewrite::strip_metadata(&mut doc);
    }
    crate::rewrite::cleanup_and_compress(&mut doc, opts.recompress_streams);
    let output = crate::rewrite::serialize(&mut doc)?;

    // F5: movemos las warnings del reporte base en vez de clonarlas; luego le
    // sumamos las de imágenes.
    let mut report = report0;
    report.warnings.extend(img_warnings);
    report.images = stats;

    // Firmas/sellos preservados: contamos los stats con acción Preserved y, si
    // hubo alguno, emitimos UN solo warning de resumen (no uno por imagen, para
    // no hacer ruido en el reporte).
    let preserved_count = report
        .images
        .iter()
        .filter(|s| s.action == ImageAction::Preserved)
        .count();
    report.preserved_images = preserved_count;
    if preserved_count > 0 {
        report.warnings.push(Warning::Other(format!(
            "{preserved_count} firma(s)/sello(s) preservados sin recomprimir"
        )));
    }

    // F10: piso a nivel-documento. Si tras serializar el output recomprimido
    // resulta MÁS grande que el input (p. ej. la sobrecarga de reescritura
    // supera el ahorro en un PDF ya pequeño), descartamos el output y
    // devolvemos los bytes originales. El reporte refleja que no hubo mejora
    // (output_size = input.len(), ratio = 1.0). La ruta de SignaturePolicy::Strict
    // ya devuelve el original más arriba y no pasa por aquí.
    let result = if output.len() > input.len() {
        report.output_size = Some(input.len() as u64);
        report.warnings.push(Warning::Other(
            "sin mejora: se conservó el documento original".into(),
        ));
        CompressResult {
            output: input.to_vec(),
            report: report.with_ratio(),
        }
    } else {
        report.output_size = Some(output.len() as u64);
        CompressResult {
            output,
            report: report.with_ratio(),
        }
    };

    on_phase(Phase::Done);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::CompressOptions;

    // PDF con `n` imágenes JPEG embebidas grandes, todas pintadas en la página.
    fn pdf_with_jpegs(n: usize) -> Vec<u8> {
        use image::codecs::jpeg::JpegEncoder;
        use image::{ImageEncoder, RgbImage};
        use lopdf::{dictionary, Document, Object, Stream};

        // imagen 800x800 con ruido suave → JPEG no trivial
        let mut rgb = RgbImage::new(800, 800);
        for (x, y, px) in rgb.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .write_image(rgb.as_raw(), 800, 800, image::ExtendedColorType::Rgb8)
            .unwrap();

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut xobjects = lopdf::Dictionary::new();
        let mut content = String::new();
        for i in 0..n {
            let img_dict = dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 800, "Height" => 800,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
            };
            let img_id = doc.add_object(Stream::new(img_dict, jpeg.clone()));
            xobjects.set(format!("Im{i}"), img_id);
            content.push_str(&format!("q 800 0 0 800 0 0 cm /Im{i} Do Q\n"));
        }
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let resources_id = doc.add_object(dictionary! { "XObject" => xobjects });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 800.into(), 800.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    // PDF con una imagen JPEG embebida grande.
    fn pdf_with_jpeg() -> Vec<u8> {
        pdf_with_jpegs(1)
    }

    #[test]
    fn compress_shrinks_image_pdf_and_stays_valid() {
        let input = pdf_with_jpeg();
        let opts = CompressOptions {
            profile: crate::options::Profile::Screen,
            ..Default::default()
        };
        let res = compress(&input, &opts).unwrap();

        assert!(
            res.output.len() < input.len(),
            "output={} input={}",
            res.output.len(),
            input.len()
        );
        assert!(
            Document::load_mem(&res.output).is_ok(),
            "el output debe re-parsear"
        );
        assert_eq!(res.report.pages, 1);
        assert!(res.report.ratio.unwrap() < 1.0);
        eprintln!(
            "compress ratio: {:.4} ({} -> {} bytes)",
            res.report.ratio.unwrap(),
            input.len(),
            res.output.len()
        );
    }

    /// PDF con una imagen JPEG que referencia un /SMask (XObject de máscara).
    /// Devuelve (bytes, contenido original del stream de la imagen, img_id).
    fn pdf_with_smask_image() -> (Vec<u8>, Vec<u8>, u32) {
        use image::codecs::jpeg::JpegEncoder;
        use image::{ImageEncoder, RgbImage};
        use lopdf::{dictionary, Document, Object, Stream};

        let mut rgb = RgbImage::new(400, 400);
        for (x, y, px) in rgb.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .write_image(rgb.as_raw(), 400, 400, image::ExtendedColorType::Rgb8)
            .unwrap();

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();

        // máscara de transparencia (grayscale, 1 componente)
        let smask_stream = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 400, "Height" => 400,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceGray",
                "Filter" => "DCTDecode",
            },
            jpeg.clone(),
        );
        let smask_id = doc.add_object(smask_stream);

        let img_content = jpeg.clone();
        let img_stream = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 400, "Height" => 400,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
                "SMask" => smask_id,
            },
            img_content.clone(),
        );
        let img_id = doc.add_object(img_stream);
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            b"q 400 0 0 400 0 0 cm /Im0 Do Q".to_vec(),
        ));
        let resources_id =
            doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 400.into(), 400.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        (buf, img_content, img_id.0)
    }

    #[test]
    fn smask_image_is_preserved_untouched() {
        let (input, orig_content, img_id) = pdf_with_smask_image();
        let opts = CompressOptions {
            profile: crate::options::Profile::Screen,
            ..Default::default()
        };
        let res = compress(&input, &opts).unwrap();

        // el output debe re-parsear
        let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");

        // el stream de la imagen debe quedar byte-idéntico (no recomprimido)
        let out_stream = out_doc
            .get_object((img_id, 0))
            .unwrap()
            .as_stream()
            .unwrap();
        assert_eq!(
            out_stream.content, orig_content,
            "la imagen con /SMask no debe recomprimirse"
        );

        // /SMask debe conservarse
        assert!(out_stream.dict.has(b"SMask"), "/SMask debe preservarse");

        // el stat debe marcarla Skipped (original == output) y haber un warning /SMask
        let stat = res
            .report
            .images
            .iter()
            .find(|s| s.object_id == img_id)
            .expect("stat de la imagen");
        assert_eq!(stat.action, ImageAction::Skipped);
        assert_eq!(stat.original_bytes, stat.output_bytes);
        assert!(
            res.report
                .warnings
                .iter()
                .any(|w| matches!(w, Warning::Other(m) if m.contains("/SMask"))),
            "debe haber un warning de /SMask preservado, warnings={:?}",
            res.report.warnings
        );
    }

    /// PDF cuyo XObject de imagen declara una dimensión malformada (Width => -5).
    /// Devuelve (bytes, contenido original del stream de la imagen, img_id).
    fn pdf_with_malformed_dimensions() -> (Vec<u8>, Vec<u8>, u32) {
        use image::codecs::jpeg::JpegEncoder;
        use image::{ImageEncoder, RgbImage};
        use lopdf::{dictionary, Document, Object, Stream};

        let mut rgb = RgbImage::new(400, 400);
        for (x, y, px) in rgb.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .write_image(rgb.as_raw(), 400, 400, image::ExtendedColorType::Rgb8)
            .unwrap();

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let img_content = jpeg.clone();
        // Width negativo: con `as u32` se envolvería silenciosamente a un valor enorme.
        let img_stream = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => -5, "Height" => 400,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
            },
            img_content.clone(),
        );
        let img_id = doc.add_object(img_stream);
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            b"q 400 0 0 400 0 0 cm /Im0 Do Q".to_vec(),
        ));
        let resources_id =
            doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 400.into(), 400.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        (buf, img_content, img_id.0)
    }

    #[test]
    fn malformed_dimensions_are_skipped() {
        let (input, orig_content, img_id) = pdf_with_malformed_dimensions();
        let opts = CompressOptions {
            profile: crate::options::Profile::Screen,
            ..Default::default()
        };
        let res = compress(&input, &opts).unwrap();

        // el output debe re-parsear
        let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");

        // el stream de la imagen debe quedar byte-idéntico (no recomprimido)
        let out_stream = out_doc
            .get_object((img_id, 0))
            .unwrap()
            .as_stream()
            .unwrap();
        assert_eq!(
            out_stream.content, orig_content,
            "la imagen con dimensiones malformadas no debe recomprimirse"
        );

        // el stat debe marcarla Skipped (original == output)
        let stat = res
            .report
            .images
            .iter()
            .find(|s| s.object_id == img_id)
            .expect("stat de la imagen");
        assert_eq!(stat.action, ImageAction::Skipped);
        assert_eq!(stat.original_bytes, stat.output_bytes);

        // debe haber un warning ImageSkipped para esta imagen
        assert!(
            res.report
                .warnings
                .iter()
                .any(|w| matches!(w, Warning::ImageSkipped(o) if *o == img_id)),
            "debe haber un Warning::ImageSkipped, warnings={:?}",
            res.report.warnings
        );
    }

    #[test]
    fn progress_phases_for_multi_image_pdf() {
        use crate::progress::Phase;

        const N: usize = 3;
        let input = pdf_with_jpegs(N);
        let opts = CompressOptions {
            profile: crate::options::Profile::Screen,
            ..Default::default()
        };

        let mut phases: Vec<Phase> = Vec::new();
        let res = compress_with_progress(&input, &opts, &mut |p| phases.push(p)).unwrap();
        assert!(!res.output.is_empty());

        // empieza con Analyzing y termina con Done
        assert_eq!(phases.first(), Some(&Phase::Analyzing), "phases={phases:?}");
        assert_eq!(phases.last(), Some(&Phase::Done), "phases={phases:?}");

        // eventos de imágenes: arranca en done=0 y acaba en done==total==N,
        // con `done` monótono y `total` constante
        let img_events: Vec<(usize, usize)> = phases
            .iter()
            .filter_map(|p| match p {
                Phase::OptimizingImages { done, total } => Some((*done, *total)),
                _ => None,
            })
            .collect();
        assert_eq!(img_events.first(), Some(&(0, N)), "phases={phases:?}");
        assert_eq!(img_events.last(), Some(&(N, N)), "phases={phases:?}");
        for w in img_events.windows(2) {
            assert!(w[1].0 >= w[0].0, "done debe ser monótono: {img_events:?}");
            assert_eq!(w[1].1, N, "total debe ser constante: {img_events:?}");
        }

        // Rewriting va después de todos los eventos de imágenes
        let rewriting_idx = phases
            .iter()
            .position(|p| *p == Phase::Rewriting)
            .expect("debe emitirse Rewriting");
        let last_img_idx = phases
            .iter()
            .rposition(|p| matches!(p, Phase::OptimizingImages { .. }))
            .expect("debe haber eventos de imágenes");
        assert!(last_img_idx < rewriting_idx, "phases={phases:?}");
    }

    /// PDF mínimo firmado (page dict con /ByteRange), sin imágenes.
    fn signed_pdf() -> Vec<u8> {
        use lopdf::{dictionary, Document, Object, Stream};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "ByteRange" => vec![0.into(), 100.into(), 200.into(), 50.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn progress_signed_strict_emits_analyzing_then_done_only() {
        use crate::progress::Phase;

        let input = signed_pdf();
        // SignaturePolicy::Strict es el default
        let opts = CompressOptions::default();

        let mut phases: Vec<Phase> = Vec::new();
        let res = compress_with_progress(&input, &opts, &mut |p| phases.push(p)).unwrap();

        assert_eq!(
            phases,
            vec![Phase::Analyzing, Phase::Done],
            "retorno temprano firmado-Strict"
        );
        // el retorno temprano devuelve el original intacto
        assert_eq!(res.output, input);
        assert!(res.report.is_signed);
    }

    #[test]
    fn never_grows_output() {
        // un PDF ya minúsculo no debe crecer. El piso a nivel-documento (F10)
        // garantiza esto: si el output recomprimido crece, se descarta y se
        // conserva el input original, así que output.len() <= input.len() siempre.
        let input = pdf_with_jpeg();
        let res = compress(&input, &CompressOptions::default()).unwrap();
        assert!(res.output.len() <= input.len());
    }

    /// JPEG RGB pequeño de `side`×`side` px con ruido suave (fixtures firma/sello).
    fn stamp_jpeg(side: u32) -> Vec<u8> {
        use image::codecs::jpeg::JpegEncoder;
        use image::{ImageEncoder, RgbImage};
        let mut rgb = RgbImage::new(side, side);
        for (x, y, px) in rgb.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 90)
            .write_image(rgb.as_raw(), side, side, image::ExtendedColorType::Rgb8)
            .unwrap();
        jpeg
    }

    /// PDF con un sello pequeño (150×150, ≤50KB) pintado en la página.
    /// Devuelve (bytes, contenido original del stream de la imagen, img_id).
    fn pdf_with_small_stamp() -> (Vec<u8>, Vec<u8>, u32) {
        use lopdf::{dictionary, Document, Object, Stream};
        let jpeg = stamp_jpeg(150);
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let img_stream = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 150, "Height" => 150,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
            },
            jpeg.clone(),
        );
        let img_id = doc.add_object(img_stream);
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            b"q 150 0 0 150 0 0 cm /Im0 Do Q".to_vec(),
        ));
        let resources_id =
            doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        (buf, jpeg, img_id.0)
    }

    #[test]
    fn small_stamp_is_preserved_end_to_end() {
        let (input, orig_content, img_id) = pdf_with_small_stamp();
        let res = compress(&input, &CompressOptions::default()).unwrap();

        // el output re-parsea y el sello sale byte-idéntico (no recomprimido)
        let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");
        let out_stream = out_doc
            .get_object((img_id, 0))
            .expect("el sello debe sobrevivir")
            .as_stream()
            .unwrap();
        assert_eq!(
            out_stream.content, orig_content,
            "el sello pequeño debe salir byte-idéntico"
        );

        let stat = res
            .report
            .images
            .iter()
            .find(|s| s.object_id == img_id)
            .expect("stat del sello");
        assert_eq!(stat.action, ImageAction::Preserved);
        assert_eq!(stat.original_bytes, stat.output_bytes);
        assert!(res.report.preserved_images >= 1, "debe contar ≥1 preservada");
        assert!(
            res.report
                .warnings
                .iter()
                .any(|w| matches!(w, Warning::Other(m) if m.contains("preservados"))),
            "debe haber un warning resumen de preservados, warnings={:?}",
            res.report.warnings
        );
    }

    /// PDF con un widget de firma (`FT=Sig`) cuya apariencia `/AP/N` es un Form
    /// XObject que embebe una imagen GRANDE (400×400, fuera del umbral de sello):
    /// se preserva por ser firma, NO por tamaño. Ejercita el caso A completo +
    /// la supervivencia a `prune`. Devuelve (bytes, contenido original, img_id).
    fn pdf_with_signature_appearance() -> (Vec<u8>, Vec<u8>, u32) {
        use lopdf::{dictionary, Document, Object, Stream};
        let jpeg = stamp_jpeg(400);
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();

        let img_stream = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 400, "Height" => 400,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
            },
            jpeg.clone(),
        );
        let img_id = doc.add_object(img_stream);

        let form_stream = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                "Resources" => dictionary! {
                    "XObject" => dictionary! { "SImg" => img_id },
                },
            },
            b"q 100 0 0 100 0 0 cm /SImg Do Q".to_vec(),
        );
        let form_id = doc.add_object(form_stream);

        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Sig",
            "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "AP" => dictionary! { "N" => form_id },
        });

        let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Annots" => vec![annot_id.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        (buf, jpeg, img_id.0)
    }

    #[test]
    fn signature_appearance_image_is_preserved_end_to_end() {
        let (input, orig_content, img_id) = pdf_with_signature_appearance();
        let res = compress(&input, &CompressOptions::default()).unwrap();

        let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");
        let out_stream = out_doc
            .get_object((img_id, 0))
            .expect("la imagen de la apariencia debe sobrevivir a prune")
            .as_stream()
            .unwrap();
        assert_eq!(
            out_stream.content, orig_content,
            "la imagen de la apariencia de firma debe salir byte-idéntica"
        );

        let stat = res
            .report
            .images
            .iter()
            .find(|s| s.object_id == img_id)
            .expect("stat de la imagen de firma");
        assert_eq!(
            stat.action,
            ImageAction::Preserved,
            "la imagen de firma (400px, fuera del umbral de sello) se preserva por ser firma"
        );
        assert_eq!(stat.original_bytes, stat.output_bytes);
    }
}
