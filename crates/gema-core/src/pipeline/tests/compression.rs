//! Comportamiento de compresión propiamente dicho: recompresión y downsampling,
//! las perillas de transcodificado para fuentes de primera generación, el dedupe
//! opt-in, la política de skips y el piso documental (nunca crecer el archivo).

use super::super::*;
use super::fixtures::*;
use crate::options::CompressOptions;
use crate::report::ImageSkipReason;
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
fn never_grows_output() {
    // un PDF ya minúsculo no debe crecer. El piso a nivel-documento (F10)
    // garantiza esto: si el output recomprimido crece, se descarta y se
    // conserva el input original, así que output.len() <= input.len() siempre.
    let input = pdf_with_jpeg();
    let res = compress(&input, &CompressOptions::default()).unwrap();
    assert!(res.output.len() <= input.len());
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
