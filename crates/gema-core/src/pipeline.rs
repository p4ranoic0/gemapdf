use crate::error::GemaError;
use crate::image_opt::jpeg::JpegRecompressor;
use crate::image_opt::{downsample, target_dimensions, RawImage, Recompressor};
use crate::options::{CompressOptions, SignaturePolicy};
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
) -> Option<ImageOutcome> {
    let (orig_len, width, height, raw_bytes) = {
        let stream = doc.get_object(id).ok()?.as_stream().ok()?;
        let dict = &stream.dict;
        // solo XObject de tipo Image
        if dict.get(b"Subtype").and_then(|o| o.as_name()).ok()? != b"Image" {
            return None;
        }
        let w = dict.get(b"Width").and_then(|o| o.as_i64()).ok()? as u32;
        let h = dict.get(b"Height").and_then(|o| o.as_i64()).ok()? as u32;
        (stream.content.len() as u64, w, h, stream.content.clone())
    };

    // decodificar: intentar como imagen estándar (JPEG embebido = DCTDecode).
    // Si `image` no soporta el filtro PDF (Flate raw, CCITT, JPX), la marcamos
    // como omitida en vez de descartarla en silencio.
    let decoded = match image::load_from_memory(&raw_bytes) {
        Ok(d) => d,
        Err(_) => return Some(skipped(id, orig_len)),
    };

    let mut warnings: Vec<Warning> = Vec::new();
    // Si la imagen tiene canal alfa, el re-encode a JPEG lo descarta.
    if decoded.color().has_alpha() {
        warnings.push(Warning::Other("alpha descartado al recomprimir".into()));
    }

    // downsample por DPI efectivo (asumimos display = tamaño nativo a 72dpi si no
    // tenemos la caja; heurística conservadora: usar pulgadas = px/target como tope)
    let mut img = decoded;
    let mut action = ImageAction::Recompressed;
    if downsample_on {
        // display estimado: tratamos la imagen como colocada a su tamaño en pt = px (72dpi)
        let disp_w = width as f32;
        let disp_h = height as f32;
        if let Some((nw, nh)) = target_dimensions(width, height, disp_w, disp_h, target_dpi) {
            img = downsample(&img, nw, nh);
            action = ImageAction::Downsampled;
        }
    }

    let enc = match JpegRecompressor.recompress(&RawImage { image: img }, quality) {
        Some(e) => e,
        None => {
            // no se pudo recomprimir → omitida
            let mut out = skipped(id, orig_len);
            out.warnings.extend(warnings);
            return Some(out);
        }
    };
    if enc.bytes.len() as u64 >= orig_len {
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

    // reemplazar el stream
    if let Ok(obj) = doc.get_object_mut(id) {
        if let Ok(stream) = obj.as_stream_mut() {
            stream.set_content(enc.bytes.clone());
            stream.dict.set("Filter", Object::Name(enc.filter.as_bytes().to_vec()));
            stream.dict.set("Width", Object::Integer(width as i64));
            stream.dict.set("Height", Object::Integer(height as i64));
            stream.dict.set("BitsPerComponent", Object::Integer(8));
            stream.dict.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
            stream.dict.remove(b"DecodeParms");
            stream.dict.remove(b"SMask");
        }
    }
    Some(ImageOutcome {
        stat: ImageStat {
            object_id: id.0,
            original_bytes: orig_len,
            output_bytes: enc.bytes.len() as u64,
            action,
        },
        warnings,
    })
}

pub fn compress(input: &[u8], opts: &CompressOptions) -> Result<CompressResult, GemaError> {
    let report0 = crate::analyze::analyze(input)?;

    // política de firma
    if report0.is_signed && opts.signatures == SignaturePolicy::Strict {
        return Ok(CompressResult {
            output: input.to_vec(),
            report: Report {
                output_size: Some(input.len() as u64),
                warnings: vec![Warning::SignedDocument],
                ..report0
            }
            .with_ratio(),
        });
    }

    let mut doc = Document::load_mem(input).map_err(|e| GemaError::Parse(e.to_string()))?;
    let params = opts.resolved();

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

    let mut stats = Vec::new();
    let mut img_warnings = Vec::new();
    for id in image_ids {
        // las imágenes no soportadas (no-Image) simplemente no generan stat
        if let Some(outcome) =
            process_image(&mut doc, id, params.jpeg_quality, params.image_dpi, opts.downsample)
        {
            stats.push(outcome.stat);
            img_warnings.extend(outcome.warnings);
        }
    }

    if opts.remove_metadata {
        crate::rewrite::strip_metadata(&mut doc);
    }
    crate::rewrite::cleanup_and_compress(&mut doc, opts.recompress_streams);
    let output = crate::rewrite::serialize(&mut doc)?;

    let mut warnings = report0.warnings.clone();
    warnings.extend(img_warnings);

    let report = Report {
        output_size: Some(output.len() as u64),
        images: stats,
        warnings,
        ..report0
    }
    .with_ratio();

    Ok(CompressResult { output, report })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::CompressOptions;

    // PDF con una imagen JPEG embebida grande.
    fn pdf_with_jpeg() -> Vec<u8> {
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
        let img_dict = dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => 800, "Height" => 800,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => "DCTDecode",
        };
        let img_stream = Stream::new(img_dict, jpeg);
        let img_id = doc.add_object(img_stream);
        let content_id = doc.add_object(Stream::new(dictionary! {}, b"q 800 0 0 800 0 0 cm /Im0 Do Q".to_vec()));
        let resources_id = doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 800.into(), 800.into()],
        });
        doc.objects.insert(pages_id, Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }));
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn compress_shrinks_image_pdf_and_stays_valid() {
        let input = pdf_with_jpeg();
        let opts = CompressOptions { profile: crate::options::Profile::Screen, ..Default::default() };
        let res = compress(&input, &opts).unwrap();

        assert!(res.output.len() < input.len(), "output={} input={}", res.output.len(), input.len());
        assert!(Document::load_mem(&res.output).is_ok(), "el output debe re-parsear");
        assert_eq!(res.report.pages, 1);
        assert!(res.report.ratio.unwrap() < 1.0);
        eprintln!(
            "compress ratio: {:.4} ({} -> {} bytes)",
            res.report.ratio.unwrap(),
            input.len(),
            res.output.len()
        );
    }

    #[test]
    fn never_grows_output() {
        // un PDF ya minúsculo no debe crecer de forma absurda
        let input = pdf_with_jpeg();
        let res = compress(&input, &CompressOptions::default()).unwrap();
        assert!(res.output.len() <= input.len() + input.len() / 10);
    }
}
