//! Tests del pipeline de compresión, agrupados por dominio.

mod fixtures;

use super::*;
use crate::options::CompressOptions;
use crate::report::ImageSkipReason;
use fixtures::*;
use lopdf::Object;

#[test]
fn image_deduplication_is_opt_in_and_reported() {
    let input = pdf_with_duplicate_unsupported_images();
    let base = CompressOptions {
        downsample: false,
        recompress_streams: false,
        remove_metadata: false,
        ..Default::default()
    };
    let without = compress(&input, &base).unwrap();
    assert_eq!(without.report.deduplicated_images, 0);
    assert_eq!(without.report.deduplicated_image_bytes, 0);
    assert_eq!(without.report.image_skip_summary.len(), 1);
    assert_eq!(
        without.report.image_skip_summary[0].reason,
        ImageSkipReason::Jpx
    );
    assert_eq!(without.report.image_skip_summary[0].images, 2);

    let with = compress(
        &input,
        &CompressOptions {
            dedupe_images: true,
            ..base
        },
    )
    .unwrap();
    assert_eq!(with.report.deduplicated_images, 1);
    assert_eq!(with.report.deduplicated_image_bytes, 128 * 1024);
    assert!(with.output.len() < without.output.len());
    assert!(Document::load_mem(&with.output).is_ok());
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

#[test]
fn parallel_image_loop_is_deterministic() {
    // El bucle de imágenes corre en paralelo (rayon, nativo). Debe dar el
    // MISMO output byte a byte en cada corrida: el orden de cómputo no puede
    // influir (cada imagen se procesa independiente sobre el doc original y
    // las escrituras se aplican en orden de image_ids). Un race o cualquier
    // dependencia de orden aparecería como outputs distintos entre corridas.
    let input = pdf_with_jpegs(6);
    let opts = CompressOptions {
        profile: crate::options::Profile::Screen,
        ..Default::default()
    };
    let a = compress(&input, &opts).unwrap();
    let b = compress(&input, &opts).unwrap();
    assert_eq!(
        a.output, b.output,
        "el pipeline debe ser determinista corrida a corrida"
    );
}

#[test]
fn memory_batch_planner_respects_budget_and_parallel_limit() {
    assert_eq!(image_batch_ranges(&[], Some(10), Some(2)), Vec::new());
    assert_eq!(
        image_batch_ranges(&[4, 4, 4, 20, 1], Some(10), Some(2)),
        vec![0..2, 2..3, 3..4, 4..5]
    );
    assert_eq!(
        image_batch_ranges(&[1, 1, 1], None, Some(2)),
        vec![0..2, 2..3]
    );
    assert_eq!(
        image_batch_ranges(&[1, 1], Some(0), Some(0)),
        vec![0..1, 1..2]
    );
}

#[test]
fn bounded_batches_are_byte_identical_to_unbounded_pipeline() {
    let input = pdf_with_jpegs(6);
    let common = CompressOptions {
        profile: crate::options::Profile::Screen,
        ..Default::default()
    };
    let unbounded = compress(
        &input,
        &CompressOptions {
            max_memory_bytes: None,
            max_parallel_images: None,
            ..common.clone()
        },
    )
    .unwrap();
    let one_at_a_time = compress(
        &input,
        &CompressOptions {
            max_memory_bytes: Some(1),
            max_parallel_images: Some(1),
            ..common
        },
    )
    .unwrap();
    assert_eq!(unbounded.output, one_at_a_time.output);
    assert_eq!(
        unbounded.report.images.len(),
        one_at_a_time.report.images.len()
    );
}

#[test]
fn per_image_memory_limit_skips_without_corrupting() {
    let input = pdf_with_jpeg();
    let res = compress(
        &input,
        &CompressOptions {
            profile: crate::options::Profile::Screen,
            max_image_bytes: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(Document::load_mem(&res.output).is_ok());
    assert!(res
        .report
        .images
        .iter()
        .any(|s| s.action == ImageAction::Skipped));
    assert!(res
        .report
        .warnings
        .iter()
        .any(|w| matches!(w, Warning::Other(m) if m.contains("omitida por memoria"))));
}

#[test]
fn per_image_limit_checks_encoded_header_not_only_pdf_dimensions() {
    let mut doc = Document::load_mem(&pdf_with_jpeg()).unwrap();
    for obj in doc.objects.values_mut() {
        if let Ok(stream) = obj.as_stream_mut() {
            if stream.dict.get(b"Subtype").and_then(|o| o.as_name()).ok()
                == Some(b"Image".as_slice())
            {
                // El JPEG real es 800x800; el dict hostil pretende 1x1.
                stream.dict.set("Width", 1);
                stream.dict.set("Height", 1);
            }
        }
    }
    let mut input = Vec::new();
    doc.save_to(&mut input).unwrap();

    let res = compress(
        &input,
        &CompressOptions {
            max_image_bytes: Some(1024 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(res
        .report
        .warnings
        .iter()
        .any(|w| matches!(w, Warning::Other(m) if m.contains("omitida por memoria"))));
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
    let out_stream = out_doc
        .get_object((img_id, 0))
        .unwrap()
        .as_stream()
        .unwrap();

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
    assert!(
        mask_filter_ok,
        "la máscara no debe re-encodearse con pérdida"
    );
}

#[test]
fn smask_none_is_treated_as_no_soft_mask() {
    let (input, orig_content, img_id) = pdf_with_smask_image();
    let mut doc = Document::load_mem(&input).unwrap();
    doc.get_object_mut((img_id, 0))
        .unwrap()
        .as_stream_mut()
        .unwrap()
        .dict
        .set("SMask", Object::Name(b"None".to_vec()));
    let mut input_with_none = Vec::new();
    doc.save_to(&mut input_with_none).unwrap();

    let result = compress(
        &input_with_none,
        &CompressOptions {
            profile: crate::options::Profile::Screen,
            ..Default::default()
        },
    )
    .unwrap();
    let output = Document::load_mem(&result.output).unwrap();
    let image = output.get_object((img_id, 0)).unwrap().as_stream().unwrap();
    assert!(image.content.len() < orig_content.len());
    let stat = result
        .report
        .images
        .iter()
        .find(|stat| stat.object_id == img_id)
        .unwrap();
    assert!(matches!(
        stat.action,
        ImageAction::Recompressed | ImageAction::Downsampled
    ));
    assert_eq!(stat.skip_reason, None);
}

#[test]
fn matte_smask_base_is_preserved_untouched() {
    let (input, orig_content, img_id) = pdf_with_matte_smask();
    let res = compress(&input, &CompressOptions::default()).unwrap();
    let out_doc = Document::load_mem(&res.output).expect("re-parsea");
    let out_stream = out_doc
        .get_object((img_id, 0))
        .unwrap()
        .as_stream()
        .unwrap();
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
    assert_eq!(stat.skip_reason, Some(ImageSkipReason::SoftMaskMatte));
    assert!(res.report.image_skip_summary.iter().any(|summary| {
        summary.reason == ImageSkipReason::SoftMaskMatte && summary.images == 1
    }));
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
    let resources_id = doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
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
    let base = out_doc
        .get_object((img_id.0, 0))
        .unwrap()
        .as_stream()
        .unwrap();
    assert!(
        base.content.len() < jpeg.len(),
        "la base debe recomprimirse"
    );
    assert!(base.dict.has(b"SMask"), "/SMask debe conservarse");
    // máscara intacta byte-idéntica
    let mask = out_doc
        .get_object((smask_id.0, 0))
        .expect("máscara sobrevive")
        .as_stream()
        .unwrap();
    assert_eq!(
        mask.content, mask_content,
        "máscara no-decodificable intacta"
    );
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
        .write_image(rgba.as_raw(), 400, 400, image::ExtendedColorType::Rgba8)
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
    let resources_id = doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
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
    let base = out_doc
        .get_object((img_id.0, 0))
        .unwrap()
        .as_stream()
        .unwrap();
    assert_eq!(
        base.content, png_content,
        "base con alfa propio + /SMask debe quedar intacta"
    );
}

#[test]
fn malformed_dimensions_are_skipped() {
    for (width, height) in [(-5, 400), (400, -5), (0, 400), (400, 0), (400, 100_001)] {
        let (input, orig_content, img_id) = pdf_with_malformed_dimensions(width, height);
        let opts = CompressOptions {
            profile: crate::options::Profile::Screen,
            ..Default::default()
        };
        let res = compress(&input, &opts).unwrap();

        let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");
        let out_stream = out_doc
            .get_object((img_id, 0))
            .unwrap()
            .as_stream()
            .unwrap();
        assert_eq!(
            out_stream.content, orig_content,
            "la imagen {width}x{height} no debe recomprimirse"
        );

        let stat = res
            .report
            .images
            .iter()
            .find(|s| s.object_id == img_id)
            .expect("stat de la imagen");
        assert_eq!(stat.action, ImageAction::Skipped);
        assert_eq!(stat.skip_reason, Some(ImageSkipReason::InvalidDimensions));
        assert_eq!(stat.original_bytes, stat.output_bytes);
        assert!(
            res.report
                .warnings
                .iter()
                .any(|w| matches!(w, Warning::ImageSkipped(o) if *o == img_id)),
            "debe haber un Warning::ImageSkipped, warnings={:?}",
            res.report.warnings
        );
        assert_eq!(res.report.image_skip_summary.len(), 1);
        assert_eq!(
            res.report.image_skip_summary[0].reason,
            ImageSkipReason::InvalidDimensions
        );
    }
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

#[test]
fn progress_signed_strict_emits_analyzing_then_done_only() {
    use crate::progress::Phase;

    let input = signed_pdf();
    // Strict es opt-in: ejercita el retorno temprano byte-idéntico sin
    // cambiar el default de producto, que preserva la apariencia visual.
    let opts = CompressOptions {
        signatures: SignaturePolicy::Strict,
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

#[test]
fn signature_appearance_image_is_preserved_end_to_end() {
    let (input, orig_content, img_id) = pdf_with_signature_appearance();
    let opts = CompressOptions {
        // Prueba la preservación de la imagen durante una transformación;
        // Strict retornaría antes de entrar al pipeline de imágenes.
        signatures: SignaturePolicy::Ignore,
        ..Default::default()
    };
    let res = compress(&input, &opts).unwrap();

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
fn default_flatten_policy_bakes_signature_and_drops_form() {
    let (input, _orig, img_id) = pdf_with_signature_appearance();
    let res = compress(&input, &CompressOptions::default()).unwrap();

    assert!(
        res.report.flattened_signatures >= 1,
        "debe aplanar ≥1 firma"
    );

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
    let s = out_doc
        .get_object((img_id, 0))
        .unwrap()
        .as_stream()
        .unwrap();
    assert!(
        matches!(s.dict.get(b"Filter"), Ok(Object::Name(_))),
        "el filtro de salida debe ser un Name único"
    );
}

// ---- perillas de transcodificado (fuente sin pérdida → JPEG) ----

/// Calibración 2026-07-26: los escaneos que llegan SIN pérdida (Flate) y se
/// transcodifican a JPEG son de primera generación y rinden más gastando
/// bytes en resolución que en cuantización — al revés que los que ya venían
/// en JPEG. `transcode_dpi` les da su propio objetivo de resolución.
#[test]
fn transcode_dpi_overrides_target_for_lossless_sources() {
    let (input, img_id) = pdf_with_flate_scan();
    let opts = CompressOptions {
        image_dpi: Some(90),
        transcode_dpi: Some(110),
        jpeg_quality: Some(45),
        ..Default::default()
    };
    let res = compress(&input, &opts).unwrap();
    // 800 px a 200 dpi efectivos: 90 dpi ⇒ 360 px, 110 dpi ⇒ 440 px.
    assert_eq!(
        output_width(&res.output, img_id),
        440,
        "el escaneo en Flate debe remuestrearse al dpi de transcodificado"
    );
}

/// La otra mitad de la calibración: los transcodificados de primera
/// generación toleran una q más baja que los de segunda. Con la única
/// imagen del doc viniendo de Flate, fijar `transcode_quality` tiene que
/// dar exactamente lo mismo que fijar esa q globalmente.
#[test]
fn transcode_quality_overrides_quality_for_lossless_sources() {
    let (input, _) = pdf_with_flate_scan();
    let scoped = compress(
        &input,
        &CompressOptions {
            jpeg_quality: Some(45),
            transcode_quality: Some(30),
            ..Default::default()
        },
    )
    .unwrap();
    let global = compress(
        &input,
        &CompressOptions {
            jpeg_quality: Some(30),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        scoped.output, global.output,
        "para una fuente sin pérdida, transcode_quality debe mandar sobre jpeg_quality"
    );
}

/// Guard del scope: una imagen que YA venía en JPEG es de segunda
/// generación y las perillas de transcodificado no deben tocarla. Sin este
/// guard, la calibración medida para escaneos en Flate se derramaría sobre
/// documentos como `doc-A`, donde midió +9.6% de peso sin ganancia.
#[test]
fn transcode_knobs_do_not_touch_dct_sources() {
    let input = pdf_with_jpeg();
    let base = compress(&input, &CompressOptions::default()).unwrap();
    let with_knobs = compress(
        &input,
        &CompressOptions {
            transcode_dpi: Some(110),
            transcode_quality: Some(30),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        base.output, with_knobs.output,
        "las perillas de transcodificado deben ser inertes sobre fuentes DCT"
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
