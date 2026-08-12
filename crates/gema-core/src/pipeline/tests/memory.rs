//! Presupuesto de memoria: formación de lotes, límite por imagen y la garantía
//! de que acotar el scheduling no cambia un solo byte de la salida.

use super::super::*;
use super::fixtures::*;
use crate::options::CompressOptions;

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
