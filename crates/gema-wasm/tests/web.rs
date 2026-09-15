#![cfg(target_arch = "wasm32")]

use lopdf::{dictionary, Document, Object, Stream};
use wasm_bindgen::JsValue;
use wasm_bindgen_test::*;

fn minimal_pdf() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let content_id = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[wasm_bindgen_test]
fn analyze_returns_a_plain_js_report() {
    let report = gema_wasm::analyze(&minimal_pdf()).unwrap();
    let input = js_sys::Reflect::get(&report, &"input".into()).unwrap();
    let pages = js_sys::Reflect::get(&input, &"pages".into()).unwrap();
    assert_eq!(pages.as_f64(), Some(1.0));
    let document = js_sys::Reflect::get(&report, &"document".into()).unwrap();
    let scanned = js_sys::Reflect::get(&document, &"has_scanned_pages".into()).unwrap();
    assert_eq!(scanned.as_bool(), Some(false));
}

#[wasm_bindgen_test]
fn compress_returns_parseable_pdf_bytes() {
    let output = gema_wasm::compress(&minimal_pdf(), "ebook").unwrap();
    assert!(Document::load_mem(&output).is_ok());
}

#[wasm_bindgen_test]
fn unknown_profile_is_a_js_error() {
    assert!(gema_wasm::compress(&minimal_pdf(), "unknown").is_err());
}

#[wasm_bindgen_test]
fn compress_with_report_accepts_memory_limits() {
    let options = js_sys::Object::new();
    js_sys::Reflect::set(
        &options,
        &"max_memory_bytes".into(),
        &JsValue::from_f64(64.0 * 1024.0 * 1024.0),
    )
    .unwrap();
    js_sys::Reflect::set(
        &options,
        &"max_parallel_images".into(),
        &JsValue::from_f64(1.0),
    )
    .unwrap();
    js_sys::Reflect::set(
        &options,
        &"max_image_bytes".into(),
        &JsValue::from_f64(32.0 * 1024.0 * 1024.0),
    )
    .unwrap();
    js_sys::Reflect::set(&options, &"dedupe_images".into(), &JsValue::TRUE).unwrap();

    let result =
        gema_wasm::compress_with_report(&minimal_pdf(), "ebook", options.into(), None).unwrap();
    let output = js_sys::Reflect::get(&result, &"output".into()).unwrap();
    let bytes = js_sys::Uint8Array::new(&output).to_vec();
    assert!(Document::load_mem(&bytes).is_ok());
    let report = js_sys::Reflect::get(&result, &"report".into()).unwrap();
    let images = js_sys::Reflect::get(&report, &"images".into()).unwrap();
    let deduplicated = js_sys::Reflect::get(&images, &"deduplicated".into()).unwrap();
    assert_eq!(deduplicated.as_f64(), Some(0.0));
}

fn pdf_with_text(content: &[u8]) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

/// El contrato que consume el editor: regiones como objetos JS planos, salida
/// `Uint8Array` y estados en snake_case con el `id` devuelto tal cual.
#[wasm_bindgen_test]
fn erase_text_speaks_plain_js() {
    let input = pdf_with_text(b"BT /F1 12 Tf 1 0 0 1 100 700 Tm (Secreto) Tj ET");
    let region = js_sys::Object::new();
    for (key, value) in [
        ("page", 0.0),
        ("x", 90.0),
        ("y", 690.0),
        ("width", 200.0),
        ("height", 30.0),
    ] {
        js_sys::Reflect::set(&region, &key.into(), &JsValue::from_f64(value)).unwrap();
    }
    js_sys::Reflect::set(&region, &"id".into(), &"bloque-7".into()).unwrap();
    let regions = js_sys::Array::of1(&region);

    let result = gema_wasm::erase_text(&input, regions.into()).unwrap();
    let output = js_sys::Reflect::get(&result, &"output".into()).unwrap();
    let bytes = js_sys::Uint8Array::new(&output).to_vec();
    let doc = Document::load_mem(&bytes).unwrap();
    let page_id = doc.get_pages()[&1];
    let content = doc.get_page_content(page_id).unwrap();
    assert!(!content.windows(7).any(|window| window == b"Secreto"));

    let report = js_sys::Reflect::get(&result, &"report".into()).unwrap();
    let reported = js_sys::Reflect::get(&report, &"regions".into()).unwrap();
    let first = js_sys::Array::from(&reported).get(0);
    let field = |name: &str| js_sys::Reflect::get(&first, &name.into()).unwrap();
    assert_eq!(field("id").as_string().as_deref(), Some("bloque-7"));
    assert_eq!(field("status").as_string().as_deref(), Some("erased"));
    assert_eq!(field("erased_glyphs").as_f64(), Some(7.0));
}

#[wasm_bindgen_test]
fn erase_text_rejects_malformed_regions() {
    let input = pdf_with_text(b"BT /F1 12 Tf 1 0 0 1 100 700 Tm (Hola) Tj ET");
    assert!(gema_wasm::erase_text(&input, JsValue::from_str("no es un array")).is_err());
}
