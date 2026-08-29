//! Límites opcionales aplicados al documento completo.

use super::super::*;
use super::fixtures::*;
use crate::{GemaError, LimitKind};

#[test]
fn max_pages_rejects_above_limit_with_exact_values() {
    let input = pdf_with_jpeg();
    let err = compress(
        &input,
        &CompressOptions {
            max_pages: Some(0),
            ..Default::default()
        },
    )
    .err()
    .expect("una página debe exceder el límite cero");

    assert!(matches!(
        err,
        GemaError::LimitExceeded {
            limit: LimitKind::Pages,
            observed: 1,
            allowed: 0,
        }
    ));
}

#[test]
fn max_pages_accepts_value_equal_to_limit() {
    let input = pdf_with_jpeg();
    let result = compress(
        &input,
        &CompressOptions {
            max_pages: Some(1),
            ..Default::default()
        },
    );

    assert!(result.is_ok());
}

#[test]
fn max_objects_rejects_above_and_accepts_equal_limit() {
    let input = pdf_with_jpeg();
    let observed = Document::load_mem(&input).unwrap().objects.len();
    let allowed = observed - 1;
    let err = compress(
        &input,
        &CompressOptions {
            max_objects: Some(allowed),
            ..Default::default()
        },
    )
    .err()
    .expect("la cantidad de objetos debe exceder el límite");

    assert!(matches!(
        err,
        GemaError::LimitExceeded {
            limit: LimitKind::Objects,
            observed: actual,
            allowed: actual_limit,
        } if actual == observed as u64 && actual_limit == allowed as u64
    ));
    assert!(compress(
        &input,
        &CompressOptions {
            max_objects: Some(observed),
            ..Default::default()
        },
    )
    .is_ok());
}

#[test]
fn max_total_work_bytes_rejects_above_limit_with_exact_values() {
    let input = pdf_with_jpeg();
    let doc = Document::load_mem(&input).unwrap();
    let image_id = doc
        .objects
        .iter()
        .find_map(|(id, object)| {
            let stream = object.as_stream().ok()?;
            (stream.dict.get(b"Subtype").and_then(|v| v.as_name()).ok()? == b"Image").then_some(*id)
        })
        .expect("fixture con imagen");
    let observed = crate::image_opt::process::estimated_working_bytes(&doc, image_id, false);
    let allowed = observed - 1;
    let err = compress(
        &input,
        &CompressOptions {
            max_total_work_bytes: Some(allowed),
            ..Default::default()
        },
    )
    .err()
    .expect("el trabajo estimado debe exceder el límite");

    assert!(matches!(
        err,
        GemaError::LimitExceeded {
            limit: LimitKind::TotalWork,
            observed: actual,
            allowed: actual_limit,
        } if actual == observed && actual_limit == allowed
    ));
}

#[test]
fn max_total_work_bytes_saturates_adversarial_image_estimates() {
    use lopdf::{dictionary, Object, Stream};

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let image = || {
        Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4_294_967_295_i64, "Height" => 4_294_967_295_i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            },
            vec![0],
        )
    };
    let first_image_id = doc.add_object(image());
    let second_image_id = doc.add_object(image());
    let content_id = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "Resources" => dictionary! {
            "XObject" => dictionary! {
                "Im0" => first_image_id,
                "Im1" => second_image_id,
            },
        },
        "MediaBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut input = Vec::new();
    doc.save_to(&mut input).unwrap();

    let err = compress(
        &input,
        &CompressOptions {
            max_total_work_bytes: Some(1),
            ..Default::default()
        },
    )
    .err()
    .expect("las estimaciones saturadas deben exceder el límite");

    assert!(matches!(
        err,
        GemaError::LimitExceeded {
            limit: LimitKind::TotalWork,
            observed: u64::MAX,
            allowed: 1,
        }
    ));
}

#[test]
fn unset_document_limits_match_default_output_byte_for_byte() {
    let input = pdf_with_jpeg();
    let default_output = compress(&input, &CompressOptions::default())
        .unwrap()
        .output;
    let explicit_none_output = compress(
        &input,
        &CompressOptions {
            max_pages: None,
            max_objects: None,
            max_total_work_bytes: None,
            ..Default::default()
        },
    )
    .unwrap()
    .output;

    assert_eq!(explicit_none_output, default_output);
}

#[test]
fn limit_kind_and_limit_error_have_stable_display_text() {
    assert_eq!(LimitKind::Pages.to_string(), "páginas");
    assert_eq!(LimitKind::Objects.to_string(), "objetos");
    assert_eq!(LimitKind::StreamBytes.to_string(), "bytes de stream");
    assert_eq!(LimitKind::TotalWork.to_string(), "trabajo total");
    assert_eq!(
        GemaError::LimitExceeded {
            limit: LimitKind::TotalWork,
            observed: 11,
            allowed: 10,
        }
        .to_string(),
        "límite excedido (trabajo total): 11 > 10"
    );
}

#[test]
fn thousands_of_pages_are_rejected_by_limit_and_safe_without_it() {
    use lopdf::{dictionary, Object, Stream};

    const PAGE_COUNT: usize = 4_096;

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let content_id = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
    let mut kids = Vec::with_capacity(PAGE_COUNT);
    for _ in 0..PAGE_COUNT {
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "Resources" => dictionary! {},
            "MediaBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
        });
        kids.push(page_id.into());
    }
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => kids, "Count" => PAGE_COUNT as i64,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut input = Vec::new();
    doc.save_to(&mut input).unwrap();

    let err = compress(
        &input,
        &CompressOptions {
            max_pages: Some(PAGE_COUNT - 1),
            ..Default::default()
        },
    )
    .err()
    .expect("el documento debe exceder max_pages");
    assert!(matches!(
        err,
        GemaError::LimitExceeded {
            limit: LimitKind::Pages,
            observed,
            allowed,
        } if observed == PAGE_COUNT as u64 && allowed == (PAGE_COUNT - 1) as u64
    ));

    let output = compress(&input, &CompressOptions::default())
        .expect("miles de páginas válidas no deben causar panic ni error")
        .output;
    let output_doc = Document::load_mem(&output).expect("la salida Ok debe ser un PDF completo");
    assert_eq!(output_doc.get_pages().len(), PAGE_COUNT);
}

#[test]
fn deeply_nested_objects_are_safe_with_and_without_limits() {
    use lopdf::{dictionary, Object, Stream};

    const NESTING_DEPTH: usize = 64;

    let mut nested = Object::Null;
    for _ in 0..NESTING_DEPTH {
        nested = Object::Array(vec![nested]);
    }

    let mut resources = lopdf::Dictionary::new();
    resources.set("Adversarial", nested);

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let content_id = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "Resources" => resources,
        "MediaBox" => vec![0.into(), 0.into(), 1.into(), 1.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut input = Vec::new();
    doc.save_to(&mut input).unwrap();
    let parsed = Document::load_mem(&input).unwrap();
    assert_eq!(
        parsed.get_pages().len(),
        1,
        "el fixture debe sobrevivir el round-trip de lopdf"
    );

    let bounded = CompressOptions {
        max_pages: Some(1),
        max_objects: Some(parsed.objects.len()),
        max_stream_bytes: Some(1),
        max_total_work_bytes: Some(0),
        ..Default::default()
    };
    for opts in [&CompressOptions::default(), &bounded] {
        let output = compress(&input, opts)
            .expect("objetos profundamente anidados no deben causar panic ni error")
            .output;
        let output_doc =
            Document::load_mem(&output).expect("la salida Ok debe ser un PDF completo");
        assert_eq!(output_doc.get_pages().len(), 1);
    }
}
