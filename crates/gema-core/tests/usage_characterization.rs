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

/// Imagen XObject FlateDecode con contenido de FOTO (tono continuo): muchos
/// colores distintos, como una fotografía. Debe clasificarse Photo → JPEG.
fn flate_photo_stream(side: u32) -> Stream {
    let mut raw = Vec::with_capacity((side * side * 3) as usize);
    for y in 0..side {
        for x in 0..side {
            let fx = x as f32;
            let fy = y as f32;
            let r = ((fx * 0.09).sin() * 0.5 + 0.5) * 255.0;
            let g = ((fy * 0.07 + fx * 0.013).cos() * 0.5 + 0.5) * 255.0;
            let b = (((fx + fy) * 0.05).sin() * 0.5 + 0.5) * 255.0;
            raw.push(r as u8);
            raw.push(g as u8);
            raw.push(b as u8);
        }
    }
    let mut s = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => side as i64, "Height" => side as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
        },
        raw,
    );
    s.compress().unwrap();
    s
}

/// Imagen XObject FlateDecode con contenido de LÍNEA/TEXTO (pocos colores,
/// franjas nítidas). Debe clasificarse LineArt → se mantiene Flate, nunca JPEG.
fn flate_lineart_stream(side: u32) -> Stream {
    let mut raw = Vec::with_capacity((side * side * 3) as usize);
    for _y in 0..side {
        for x in 0..side {
            let v: u8 = if x % 7 < 2 { 0 } else { 255 };
            raw.push(v);
            raw.push(v);
            raw.push(v);
        }
    }
    let mut s = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => side as i64, "Height" => side as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
        },
        raw,
    );
    s.compress().unwrap();
    s
}

/// Devuelve el filtro (Name) de la primera imagen XObject del documento.
fn output_image_filter(doc: &Document) -> Vec<u8> {
    let img_id = doc
        .objects
        .iter()
        .find_map(|(id, obj)| {
            let s = obj.as_stream().ok()?;
            (s.dict.get(b"Subtype").ok()?.as_name().ok()? == b"Image").then_some(*id)
        })
        .expect("debe existir una imagen");
    doc.get_object(img_id)
        .unwrap()
        .as_stream()
        .unwrap()
        .dict
        .get(b"Filter")
        .unwrap()
        .as_name()
        .unwrap()
        .to_vec()
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

/// P1-Flate (v2.0) invierte la limitación de v1 para imágenes FlateDecode
/// SOPORTADAS: una imagen DeviceRGB/8 sin predictor ahora se DECODIFICA y se
/// maneja (no se salta). Un raster sólido clasifica como línea/texto → se
/// mantiene SIN PÉRDIDA (FlateDecode), nunca se convierte a JPEG (evita halos).
/// Su acción será Recompressed (si el re-deflate mejora) o Kept (si no), pero
/// NUNCA Skipped, y el filtro de salida sigue siendo FlateDecode.
#[test]
fn supported_flate_image_is_decoded_and_handled_losslessly() {
    let input = pdf_with_image(flate_image_stream(120), 120, 120);
    let res = compress(&input, &CompressOptions::default()).unwrap();

    // El output sigue siendo un PDF válido.
    let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");

    // Exactamente una imagen y NO se saltó (P1 la decodifica).
    assert_eq!(res.report.images.len(), 1);
    assert_ne!(
        res.report.images[0].action,
        ImageAction::Skipped,
        "una imagen Flate soportada ya no se salta, images={:?}",
        res.report.images
    );

    // La imagen de salida debe seguir siendo FlateDecode (sin pérdida), NO DCTDecode:
    // el raster sólido es línea/texto y nunca debe pasar por JPEG.
    let img_id = out_doc
        .objects
        .iter()
        .find_map(|(id, obj)| {
            let s = obj.as_stream().ok()?;
            (s.dict.get(b"Subtype").ok()?.as_name().ok()? == b"Image").then_some(*id)
        })
        .expect("debe existir la imagen en el output");
    let out_stream = out_doc.get_object(img_id).unwrap().as_stream().unwrap();
    let filter = out_stream.dict.get(b"Filter").unwrap().as_name().unwrap();
    assert_eq!(filter, b"FlateDecode", "línea/texto debe quedar en Flate, no en JPEG");
}

/// Cobertura del path de SKIP con un caso GENUINAMENTE no soportado: una imagen
/// Flate con `/Predictor 15` (PNG) en DecodeParms. Los datos predichos necesitan
/// un des-filtrado fuera de alcance en v2.0, así que se SALTA sin corromper.
#[test]
fn predicted_flate_image_is_skipped_not_corrupted() {
    use lopdf::dictionary;
    let side = 40u32;
    let raw = vec![137u8; (side * side * 3) as usize];
    let mut img = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => side as i64, "Height" => side as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "DecodeParms" => dictionary! { "Predictor" => 15, "Colors" => 3, "Columns" => side as i64 },
        },
        raw,
    );
    img.compress().unwrap(); // -> Filter FlateDecode
    let input = pdf_with_image(img, side as i64, side as i64);
    let res = compress(&input, &CompressOptions::default()).unwrap();

    // Sigue siendo un PDF válido.
    assert!(Document::load_mem(&res.output).is_ok());
    // Exactamente una imagen, marcada como Skipped (predicho → fuera de alcance).
    assert_eq!(res.report.images.len(), 1);
    assert_eq!(res.report.images[0].action, ImageAction::Skipped);
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

/// P1-Flate: una imagen FOTO en FlateDecode (antes saltada) ahora se decodifica,
/// se clasifica Photo y se recomprime a JPEG. La imagen de salida es DCTDecode y
/// el documento encoge — prueba de que imágenes antes Skipped hoy comprimen.
#[test]
fn flate_photo_is_recompressed_and_shrinks() {
    // Caja grande (MediaBox = side) → DPI ~72, por debajo del objetivo, así que
    // NO se downsamplea: aislamos el efecto del re-encode por codec (JPEG).
    let side = 256u32;
    let input = pdf_with_image(flate_photo_stream(side), side as i64, side as i64);
    let res = compress(&input, &CompressOptions { profile: Profile::Screen, ..Default::default() }).unwrap();

    let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");
    assert_eq!(res.report.images.len(), 1);
    let action = &res.report.images[0].action;
    assert!(
        matches!(action, ImageAction::Recompressed | ImageAction::Downsampled),
        "una foto Flate debe recomprimirse/downsamplearse, no saltarse: {action:?}"
    );
    // La foto va a JPEG (DCTDecode).
    assert_eq!(output_image_filter(&out_doc), b"DCTDecode", "la foto debe ir a JPEG");
    // Y el documento global encoge respecto al original.
    assert!(res.output.len() < input.len(), "output={} input={}", res.output.len(), input.len());
    // El stat de la imagen también encoge.
    assert!(res.report.images[0].output_bytes < res.report.images[0].original_bytes);
}

/// P1-Flate content-aware: una imagen de LÍNEA/TEXTO en FlateDecode se mantiene
/// SIN PÉRDIDA. Nunca se convierte a JPEG (halos). Asserta que el filtro de
/// salida sigue siendo FlateDecode.
#[test]
fn flate_lineart_stays_lossless() {
    let side = 256u32;
    let input = pdf_with_image(flate_lineart_stream(side), side as i64, side as i64);
    let res = compress(&input, &CompressOptions { profile: Profile::Screen, ..Default::default() }).unwrap();

    let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");
    assert_eq!(res.report.images.len(), 1);
    assert_ne!(res.report.images[0].action, ImageAction::Skipped);
    // El punto clave: NO se le metió DCT. Sigue en FlateDecode (sin pérdida).
    assert_eq!(
        output_image_filter(&out_doc),
        b"FlateDecode",
        "línea/texto NUNCA debe convertirse a JPEG"
    );
}
