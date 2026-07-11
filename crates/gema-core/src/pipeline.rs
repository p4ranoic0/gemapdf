use crate::error::GemaError;
use crate::image_opt::process::{process_image, ImageParams};
use crate::options::{CompressOptions, SignaturePolicy};
use crate::progress::Phase;
use crate::report::{ImageAction, Report, Warning};
use lopdf::Document;

pub struct CompressResult {
    pub output: Vec<u8>,
    pub report: Report,
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

    // Política Flatten: hornea las firmas visibles al contenido de página. Va
    // DESPUÉS de calcular `preserve` (que necesita los widgets en /Annots) y
    // ANTES del bucle de imágenes (las imágenes de firma siguen preservándose por
    // ObjectId). Sacrifica la validez cripto (ya rota por la compresión) a cambio
    // de que Acrobat renderice las firmas.
    let flattened = if opts.signatures == SignaturePolicy::Flatten {
        crate::flatten::flatten_signatures(&mut doc)
    } else {
        0
    };

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

    // Pre-pasada de máscaras (lever C): imágenes usadas como /SMask.
    let smask_ids = crate::image_opt::masks::collect_smask_ids(&doc);

    let mut stats = Vec::new();
    let mut img_warnings = Vec::new();
    for (i, id) in image_ids.into_iter().enumerate() {
        let img_params = ImageParams {
            quality: params.jpeg_quality,
            quality_target: opts.quality_target,
            target_dpi: params.image_dpi,
            downsample: opts.downsample,
            effective_dpi: dpi_map.get(&id).copied(),
            preserve: preserve.contains(&id),
            is_smask: smask_ids.contains(&id),
        };
        // las imágenes no soportadas (no-Image) simplemente no generan stat
        if let Some(outcome) = process_image(&mut doc, id, &img_params) {
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
    // Tras comprimir (para que el XMP no se recomprima): estampa la marca gemaPDF.
    crate::rewrite::brand_metadata(&mut doc);
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
    report.flattened_signatures = flattened;
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
    //
    // Excepción: cuando se aplanaron firmas (flattened > 0) se hicieron cambios
    // semánticos intencionales (widget eliminado, /AcroForm quitado, marca
    // gemaPDF estampada). Incluso si el output resulta ligeramente mayor que el
    // input por overhead de firma+branding, se devuelve el output procesado —no
    // el original sin aplanar— para que el documento sea universalmente visible
    // en Acrobat. Sin firmas (flattened == 0) la política es irrelevante y el
    // piso sigue aplicando.
    let result = if output.len() > input.len() && flattened == 0 {
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
    use lopdf::Object;

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
    fn smask_base_is_recompressed_and_mask_survives() {
        let (input, orig_content, img_id) = pdf_with_smask_image();
        let opts = CompressOptions {
            profile: crate::options::Profile::Screen,
            ..Default::default()
        };
        let res = compress(&input, &opts).unwrap();

        let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");
        let out_stream = out_doc.get_object((img_id, 0)).unwrap().as_stream().unwrap();

        // Lever C: la base con /SMask ya NO se preserva — se recomprime más chica.
        assert!(
            out_stream.content.len() < orig_content.len(),
            "la base con /SMask debe recomprimirse ({} vs {})",
            out_stream.content.len(),
            orig_content.len()
        );
        let stat = res
            .report
            .images
            .iter()
            .find(|s| s.object_id == img_id)
            .expect("stat de la base");
        assert!(matches!(
            stat.action,
            ImageAction::Recompressed | ImageAction::Downsampled
        ));

        // La referencia /SMask se conserva y su objeto sobrevive a prune.
        let smask_id = out_stream
            .dict
            .get(b"SMask")
            .expect("/SMask debe conservarse")
            .as_reference()
            .expect("/SMask debe ser referencia");
        let mask = out_doc
            .get_object(smask_id)
            .expect("la máscara debe sobrevivir a prune")
            .as_stream()
            .unwrap();

        // La máscara NUNCA se re-encodea con pérdida: o quedó byte-idéntica
        // (Kept/Skipped) o salió FlateDecode (lossless). Ambas son correctas.
        let mask_filter_ok = match mask.dict.get(b"Filter") {
            Ok(Object::Name(n)) if n == b"FlateDecode" => true,
            Ok(Object::Name(n)) if n == b"DCTDecode" => {
                // sólo válido si quedó byte-idéntica al original (sin re-encode)
                let mask_stat = res.report.images.iter().find(|s| s.object_id == smask_id.0);
                mask_stat.is_none_or(|s| s.original_bytes == s.output_bytes)
            }
            _ => false,
        };
        assert!(mask_filter_ok, "la máscara no debe re-encodearse con pérdida");
    }

    /// Como `pdf_with_smask_image` pero la máscara lleva /Matte (color
    /// premultiplicado): la base debe preservarse entera.
    fn pdf_with_matte_smask() -> (Vec<u8>, Vec<u8>, u32) {
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
        let smask_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 400, "Height" => 400,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceGray",
                "Filter" => "DCTDecode",
                // premultiplicado: los píxeles de la base están acoplados a la máscara
                "Matte" => vec![0.into(), 0.into(), 0.into()],
            },
            jpeg.clone(),
        ));
        let img_content = jpeg.clone();
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 400, "Height" => 400,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
                "SMask" => smask_id,
            },
            img_content.clone(),
        ));
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
    fn matte_smask_base_is_preserved_untouched() {
        let (input, orig_content, img_id) = pdf_with_matte_smask();
        let res = compress(&input, &CompressOptions::default()).unwrap();
        let out_doc = Document::load_mem(&res.output).expect("re-parsea");
        let out_stream = out_doc.get_object((img_id, 0)).unwrap().as_stream().unwrap();
        assert_eq!(
            out_stream.content, orig_content,
            "base con /SMask+/Matte debe quedar byte-idéntica"
        );
        let stat = res
            .report
            .images
            .iter()
            .find(|s| s.object_id == img_id)
            .unwrap();
        assert_eq!(stat.action, ImageAction::Skipped);
    }

    /// Máscara que el decoder NO soporta (bpc=1): queda intacta, pero la base
    /// se recomprime igual y la referencia /SMask se conserva.
    #[test]
    fn undecodable_mask_stays_intact_base_recompresses() {
        use flate2::{write::ZlibEncoder, Compression};
        use image::codecs::jpeg::JpegEncoder;
        use image::{ImageEncoder, RgbImage};
        use lopdf::{dictionary, Document, Object, Stream};
        use std::io::Write;

        let mut rgb = RgbImage::new(400, 400);
        for (x, y, px) in rgb.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .write_image(rgb.as_raw(), 400, 400, image::ExtendedColorType::Rgb8)
            .unwrap();
        // máscara bilevel 400x400: 400*400/8 = 20000 bytes, zlib
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(&vec![0xAAu8; 20_000]).unwrap();
        let mask_content = z.finish().unwrap();

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let smask_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 400, "Height" => 400,
                "BitsPerComponent" => 1, // bpc=1: fuera del alcance del decoder
                "ColorSpace" => "DeviceGray",
                "Filter" => "FlateDecode",
            },
            mask_content.clone(),
        ));
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 400, "Height" => 400,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
                "SMask" => smask_id,
            },
            jpeg.clone(),
        ));
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

        let opts = CompressOptions {
            profile: crate::options::Profile::Screen,
            ..Default::default()
        };
        let res = compress(&buf, &opts).unwrap();
        let out_doc = Document::load_mem(&res.output).expect("re-parsea");
        // base recomprimida
        let base = out_doc.get_object((img_id.0, 0)).unwrap().as_stream().unwrap();
        assert!(base.content.len() < jpeg.len(), "la base debe recomprimirse");
        assert!(base.dict.has(b"SMask"), "/SMask debe conservarse");
        // máscara intacta byte-idéntica
        let mask = out_doc
            .get_object((smask_id.0, 0))
            .expect("máscara sobrevive")
            .as_stream()
            .unwrap();
        assert_eq!(mask.content, mask_content, "máscara no-decodificable intacta");
    }

    /// Base cuyo decode trae alfa PROPIO (PNG RGBA embebido sin /Filter) además
    /// de /SMask externa: re-encodear perdería ese alfa → se preserva.
    #[test]
    fn base_with_own_alpha_and_smask_is_preserved() {
        use image::codecs::png::PngEncoder;
        use image::ImageEncoder;
        use lopdf::{dictionary, Document, Object, Stream};

        // PNG RGBA real de 400x400 con alfa variable
        let rgba = image::RgbaImage::from_fn(400, 400, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 100, (x % 200) as u8])
        });
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(
                rgba.as_raw(),
                400,
                400,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encodear PNG del fixture");
        let png_content = png.clone();

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let smask_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 400, "Height" => 400,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceGray",
                "Filter" => "DCTDecode",
            },
            vec![0xFF, 0xD8, 0xFF, 0xD9], // contenido irrelevante para este test
        ));
        // decode() intenta image::load_from_memory sobre los bytes crudos sin
        // mirar /Filter, así que un PNG "disfrazado" de DCTDecode igual
        // decodifica a RGBA por sniffing de magic bytes. /Filter SÍ debe estar
        // presente en el dict (con cualquier nombre): si el stream quedara sin
        // /Filter, el paso final `Document::compress()` de lopdf lo Flate-
        // envolvería igual (cualquier stream sin /Filter, esté o no
        // preservado por Lever C), rompiendo la igualdad byte a byte que
        // este test verifica y que es ajena a la guarda bajo prueba.
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 400, "Height" => 400,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
                "SMask" => smask_id,
            },
            png_content.clone(),
        ));
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

        let res = compress(&buf, &CompressOptions::default()).unwrap();
        let out_doc = Document::load_mem(&res.output).expect("re-parsea");
        let base = out_doc.get_object((img_id.0, 0)).unwrap().as_stream().unwrap();
        assert_eq!(
            base.content, png_content,
            "base con alfa propio + /SMask debe quedar intacta"
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
        // Strict ya no es el default (ahora es Flatten); lo pedimos explícito
        // para ejercitar el retorno temprano byte-idéntico.
        let opts = CompressOptions {
            signatures: crate::options::SignaturePolicy::Strict,
            ..Default::default()
        };

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
        assert!(
            res.report.preserved_images >= 1,
            "debe contar ≥1 preservada"
        );
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
        // AcroForm con NeedAppearances (el flag que en Acrobat oculta la firma):
        // así el aplanado tiene un form real que quitar y la aserción del test
        // "sin /AcroForm" no es vacua.
        let acro_id = doc.add_object(dictionary! {
            "Fields" => vec![annot_id.into()], "NeedAppearances" => true, "SigFlags" => 3,
        });
        let catalog_id = doc.add_object(
            dictionary! { "Type" => "Catalog", "Pages" => pages_id, "AcroForm" => acro_id },
        );
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

    #[test]
    fn flatten_policy_bakes_signature_and_drops_form() {
        let (input, _orig, img_id) = pdf_with_signature_appearance();
        // Flatten es el default
        let res = compress(&input, &CompressOptions::default()).unwrap();

        assert!(res.report.flattened_signatures >= 1, "debe aplanar ≥1 firma");

        let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");
        // Sin /AcroForm en el catálogo → Acrobat no regenera campos en blanco.
        assert!(
            out_doc.catalog().unwrap().get(b"AcroForm").is_err(),
            "/AcroForm debe desaparecer tras aplanar"
        );
        // La imagen de la firma sigue presente (preservada, ahora vía recursos).
        assert!(
            out_doc.get_object((img_id, 0)).is_ok(),
            "la imagen de firma debe sobrevivir"
        );
        // Producer gemaPDF estampado.
        let info_ref = out_doc.trailer.get(b"Info").unwrap();
        let (_, info) = out_doc.dereference(info_ref).unwrap();
        let producer = info.as_dict().unwrap().get(b"Producer").unwrap();
        if let Object::String(b, _) = producer {
            assert!(String::from_utf8_lossy(b).contains("gemaPDF"));
        } else {
            panic!("Producer debe ser string");
        }
    }

    /// PDF con una imagen JPEG envuelta en zlib: /Filter [FlateDecode DCTDecode]
    /// (lever A). Devuelve (bytes, img_id).
    fn pdf_with_flate_wrapped_jpeg() -> (Vec<u8>, u32) {
        use flate2::{write::ZlibEncoder, Compression};
        use image::codecs::jpeg::JpegEncoder;
        use image::{ImageEncoder, RgbImage};
        use lopdf::{dictionary, Document, Object, Stream};
        use std::io::Write;

        let mut rgb = RgbImage::new(800, 800);
        for (x, y, px) in rgb.enumerate_pixels_mut() {
            let fx = x as f32;
            let fy = y as f32;
            let r = ((fx * 0.09).sin() * 0.5 + 0.5) * 255.0;
            let g = ((fy * 0.07 + fx * 0.013).cos() * 0.5 + 0.5) * 255.0;
            let b = (((fx + fy) * 0.05).sin() * 0.5 + 0.5) * 255.0;
            *px = image::Rgb([r as u8, g as u8, b as u8]);
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .write_image(rgb.as_raw(), 800, 800, image::ExtendedColorType::Rgb8)
            .unwrap();
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(&jpeg).unwrap();
        let wrapped = z.finish().unwrap();

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 800, "Height" => 800,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"FlateDecode".to_vec()),
                    Object::Name(b"DCTDecode".to_vec()),
                ],
            },
            wrapped,
        ));
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            b"q 800 0 0 800 0 0 cm /Im0 Do Q".to_vec(),
        ));
        let resources_id =
            doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
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
        (buf, img_id.0)
    }

    #[test]
    fn flate_wrapped_jpeg_is_recompressed() {
        let (input, img_id) = pdf_with_flate_wrapped_jpeg();
        let opts = CompressOptions {
            profile: crate::options::Profile::Screen,
            ..Default::default()
        };
        let res = compress(&input, &opts).unwrap();

        let stat = res
            .report
            .images
            .iter()
            .find(|s| s.object_id == img_id)
            .expect("stat de la imagen");
        assert!(
            matches!(
                stat.action,
                ImageAction::Recompressed | ImageAction::Downsampled
            ),
            "la cadena Flate+DCT debe recomprimirse, no {:?}",
            stat.action
        );
        assert!(stat.output_bytes < stat.original_bytes);

        // el output re-parsea y la cadena colapsa a un único filtro
        let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");
        let s = out_doc.get_object((img_id, 0)).unwrap().as_stream().unwrap();
        assert!(
            matches!(s.dict.get(b"Filter"), Ok(Object::Name(_))),
            "el filtro de salida debe ser un Name único"
        );
    }

    // ---- modo perceptual (quality_target) ----

    /// Test de ORO del opt-in: con `quality_target: None` el output es
    /// byte-idéntico al de las mismas opciones sin el campo (mismo build).
    #[test]
    fn none_target_is_inert_byte_identical() {
        let input = pdf_with_jpeg();
        let base = CompressOptions {
            profile: crate::options::Profile::Screen,
            ..Default::default()
        };
        let with_none = CompressOptions {
            profile: crate::options::Profile::Screen,
            quality_target: None,
            ..Default::default()
        };
        let a = compress(&input, &base).unwrap();
        let b = compress(&input, &with_none).unwrap();
        assert_eq!(a.output, b.output, "None debe ser totalmente inerte");
    }

    #[cfg(feature = "perceptual")]
    #[test]
    fn modest_target_beats_overpreserving_fixed_q() {
        // El fixture 800×800 pintado a 72dpi con perfil Screen usa q40 fija.
        // Con un target modesto (τ=45) la búsqueda debe encontrar una q menor
        // (menos bytes) manteniendo el score.
        let input = pdf_with_jpeg();
        let fixed = compress(
            &input,
            &CompressOptions {
                profile: crate::options::Profile::Screen,
                ..Default::default()
            },
        )
        .unwrap();
        let target = compress(
            &input,
            &CompressOptions {
                profile: crate::options::Profile::Screen,
                quality_target: Some(45.0),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            target.output.len() < fixed.output.len(),
            "τ=45 debe producir menos bytes que q40 fija sobre-preservadora ({} vs {})",
            target.output.len(),
            fixed.output.len()
        );
    }

    #[cfg(feature = "perceptual")]
    #[test]
    fn unreachable_target_warns_and_still_compresses() {
        let input = pdf_with_jpeg();
        let res = compress(
            &input,
            &CompressOptions {
                profile: crate::options::Profile::Screen,
                quality_target: Some(99.9),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            res.report
                .warnings
                .iter()
                .any(|w| matches!(w, Warning::Other(m) if m.contains("quality_target"))),
            "debe avisar que no alcanzó el target, warnings={:?}",
            res.report.warnings
        );
        // y aún así el doc procesa (no error, output válido)
        assert!(Document::load_mem(&res.output).is_ok());
    }

    #[cfg(not(feature = "perceptual"))]
    #[test]
    fn target_without_feature_degrades_to_fixed_q_with_warning() {
        let input = pdf_with_jpeg();
        let fixed = compress(
            &input,
            &CompressOptions {
                profile: crate::options::Profile::Screen,
                ..Default::default()
            },
        )
        .unwrap();
        let res = compress(
            &input,
            &CompressOptions {
                profile: crate::options::Profile::Screen,
                quality_target: Some(68.0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            res.output, fixed.output,
            "sin feature, el target degrada a q fija (misma salida)"
        );
        assert!(
            res.report
                .warnings
                .iter()
                .any(|w| matches!(w, Warning::Other(m) if m.contains("perceptual"))),
            "debe avisar que el build no trae el feature"
        );
    }
}
