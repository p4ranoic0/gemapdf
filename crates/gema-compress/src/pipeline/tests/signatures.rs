//! Preservación de sellos y apariencias de firma, y la política Flatten por
//! defecto: la apariencia visible se integra al contenido de página y el widget
//! desaparece, sacrificando la validez criptográfica que la compresión ya
//! rompía de todos modos.

use super::super::*;
use super::fixtures::*;
use crate::options::CompressOptions;
use lopdf::Object;

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
