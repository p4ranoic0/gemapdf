//! Tests de caracterización informados por el análisis de uso real
//! (`docs/USAGE-ANALYSIS.md`).
//!
//! Fijan los dos comportamientos que dominaron el corpus real de v1:
//!   1. Las imágenes en formato no-DCT (FlateDecode/CCITT/JBIG2) se SALTAN sin
//!      corromper el PDF (16.8% de las imágenes reales).
//!   2. El downsampling por DPI es INERTE en v1 (0 disparos en 5 799 imágenes).
//!
//! Cuando v2 implemente decodificación no-DCT y DPI real (ver TODO-v2), estos
//! tests deberán actualizarse: son el ancla de regresión de la limitación actual.

use gema_core::report::ImageAction;
use gema_core::{compress, CompressOptions, Profile, Warning};
use image::codecs::jpeg::JpegEncoder;
use image::{ImageEncoder, RgbImage};
use lopdf::{dictionary, Document, Object, Stream};

/// Envuelve un stream de imagen XObject en un PDF de 1 página y lo serializa.
fn pdf_with_image(img_stream: Stream, w: i64, h: i64) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let img_id = doc.add_object(img_stream);
    let content_id = doc.add_object(Stream::new(dictionary! {}, b"q 1 0 0 1 0 0 cm /Im0 Do Q".to_vec()));
    let resources_id = doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id, "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
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

/// Imagen XObject codificada con FlateDecode (datos crudos comprimidos con zlib),
/// como las que produce un escáner. El decodificador `image` no abre el blob zlib.
fn flate_image_stream(side: u32) -> Stream {
    let raw = vec![137u8; (side * side * 3) as usize]; // RGB sólido crudo
    let mut s = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => side as i64, "Height" => side as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
        },
        raw,
    );
    s.compress().unwrap(); // -> Filter FlateDecode, contenido = blob zlib
    s
}

/// Imagen XObject JPEG (DCTDecode) con gradiente suave.
fn jpeg_image_stream(side: u32, q: u8) -> Stream {
    let mut rgb = RgbImage::new(side, side);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        *px = image::Rgb([((x * 255) / side) as u8, ((y * 255) / side) as u8, 128]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, q)
        .write_image(rgb.as_raw(), side, side, image::ExtendedColorType::Rgb8)
        .unwrap();
    Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => side as i64, "Height" => side as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB", "Filter" => "DCTDecode",
        },
        jpeg,
    )
}

/// Hallazgo 2 del análisis: las imágenes no-DCT (escaneos) se SALTAN, no se corrompen.
#[test]
fn non_dct_image_is_skipped_not_corrupted() {
    let input = pdf_with_image(flate_image_stream(120), 120, 120);
    let res = compress(&input, &CompressOptions::default()).unwrap();

    // El output sigue siendo un PDF válido.
    assert!(Document::load_mem(&res.output).is_ok());
    // Exactamente una imagen, marcada como Skipped (no Recompressed/Downsampled).
    assert_eq!(res.report.images.len(), 1);
    assert_eq!(res.report.images[0].action, ImageAction::Skipped);
    // Y se avisa con ImageSkipped, sin inventar ahorros.
    assert!(res.report.warnings.iter().any(|w| matches!(w, Warning::ImageSkipped(_))));
    assert_eq!(res.report.images[0].original_bytes, res.report.images[0].output_bytes);
}

/// P2 (v2.0) invierte la limitación de v1: ahora `process_image` lee el CTM real
/// del content stream, así que una imagen de alta resolución dibujada en una caja
/// pequeña SÍ se downsamplea. El content stream de `pdf_with_image` es
/// `q 1 0 0 1 0 0 cm /Im0 Do Q`, es decir la imagen se pinta en un cuadrado de
/// 1pt: 1500px / (1/72) = 108 000 DPI, muy por encima del objetivo → Downsampled.
#[test]
fn high_dpi_image_is_downsampled() {
    let input = pdf_with_image(jpeg_image_stream(1500, 90), 300, 300);
    let res = compress(&input, &CompressOptions { profile: Profile::Screen, ..Default::default() }).unwrap();

    // El output sigue siendo un PDF válido y no crece.
    assert!(Document::load_mem(&res.output).is_ok(), "el output debe re-parsear");
    assert!(res.output.len() <= input.len(), "el output no debe crecer");

    // Exactamente una imagen, ahora marcada Downsampled (P2 activo).
    assert_eq!(res.report.images.len(), 1);
    assert_eq!(
        res.report.images[0].action,
        ImageAction::Downsampled,
        "P2: una imagen muy sobre-resolución debe downsamplearse, images={:?}",
        res.report.images
    );
}
