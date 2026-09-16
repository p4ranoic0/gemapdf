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

use gema_compress::{compress, CompressOptions, ImageAction, Profile, Warning};
use image::codecs::jpeg::JpegEncoder;
use image::{ImageEncoder, RgbImage};
use lopdf::{dictionary, Document, Object, Stream};

/// Envuelve un stream de imagen XObject en un PDF de 1 página y lo serializa.
fn pdf_with_image(img_stream: Stream, w: i64, h: i64) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let img_id = doc.add_object(img_stream);
    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        b"q 1 0 0 1 0 0 cm /Im0 Do Q".to_vec(),
    ));
    let resources_id = doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id, "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), w.into(), h.into()],
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

/// Devuelve el `/ColorSpace` (Name) de la primera imagen XObject del documento.
fn output_image_colorspace(doc: &Document) -> Vec<u8> {
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
        .get(b"ColorSpace")
        .unwrap()
        .as_name()
        .unwrap()
        .to_vec()
}

/// Imagen XObject JPEG (DCTDecode) en escala de grises con contenido de tono
/// continuo (no plano): un escaneo en gris ya codificado como JPEG L8/DeviceGray
/// upstream. `image::load_from_memory` la decodifica a `ImageLuma8`.
fn gray_jpeg_image_stream(side: u32, q: u8) -> Stream {
    use image::{GrayImage, Luma};
    let mut gray = GrayImage::new(side, side);
    for (x, y, px) in gray.enumerate_pixels_mut() {
        let fx = x as f32;
        let fy = y as f32;
        let v = ((fx * 0.09).sin() * 0.5 + 0.5) * ((fy * 0.07).cos() * 0.3 + 0.7) * 255.0;
        *px = Luma([v as u8]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, q)
        .write_image(gray.as_raw(), side, side, image::ExtendedColorType::L8)
        .unwrap();
    Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => side as i64, "Height" => side as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceGray", "Filter" => "DCTDecode",
        },
        jpeg,
    )
}

/// Igual que `gray_jpeg_image_stream` pero "inflada" a DeviceRGB (R=G=B), como
/// haría el camino viejo (siempre to_rgb8() antes de re-codificar). Sirve de
/// comparación de tamaño contra el camino L8/DeviceGray real.
fn gray_jpeg_image_as_rgb_stream(side: u32, q: u8) -> Stream {
    let mut rgb = RgbImage::new(side, side);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        let fx = x as f32;
        let fy = y as f32;
        let v = ((fx * 0.09).sin() * 0.5 + 0.5) * ((fy * 0.07).cos() * 0.3 + 0.7) * 255.0;
        let v = v as u8;
        *px = image::Rgb([v, v, v]);
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
    assert_eq!(
        filter, b"FlateDecode",
        "línea/texto debe quedar en Flate, no en JPEG"
    );
}

/// P1b: una imagen Flate con `/Predictor 15` (PNG) en DecodeParms ahora se
/// DECODIFICA correctamente (antes se saltaba). Construimos un fixture PNG-predicho
/// de verdad (byte de tipo por fila + filtro Paeth, luego zlib) y comprobamos que
/// se maneja (no se salta) y que el output re-parsea y no crece.
#[test]
fn predicted_flate_image_is_decoded() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use lopdf::dictionary;
    use std::io::Write;

    let (w, h) = (40usize, 40usize);
    // Patrón determinista (no plano) para que el predictor tenga qué diferenciar.
    let mut pixels = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            pixels.push(((x * 7 + y * 3) % 256) as u8);
            pixels.push(((x * 13 + y * 5 + 11) % 256) as u8);
            pixels.push(((x * 3 + y * 17 + 200) % 256) as u8);
        }
    }
    // Paeth predictor.
    let paeth = |a: u8, b: u8, c: u8| -> u8 {
        let p = a as i32 + b as i32 - c as i32;
        let (pa, pb, pc) = (
            (p - a as i32).abs(),
            (p - b as i32).abs(),
            (p - c as i32).abs(),
        );
        if pa <= pb && pa <= pc {
            a
        } else if pb <= pc {
            b
        } else {
            c
        }
    };
    let bpp = 3usize;
    let rl = w * 3;
    let mut filtered = Vec::with_capacity(h * (rl + 1));
    let zero = vec![0u8; rl];
    for y in 0..h {
        let cur = &pixels[y * rl..(y + 1) * rl];
        let prev: &[u8] = if y == 0 {
            &zero
        } else {
            &pixels[(y - 1) * rl..y * rl]
        };
        filtered.push(4u8); // Paeth
        for i in 0..rl {
            let a = if i >= bpp { cur[i - bpp] } else { 0 };
            let b = prev[i];
            let c = if i >= bpp { prev[i - bpp] } else { 0 };
            filtered.push(cur[i].wrapping_sub(paeth(a, b, c)));
        }
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&filtered).unwrap();
    let content = enc.finish().unwrap();

    let img = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => w as i64, "Height" => h as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => "FlateDecode",
            "DecodeParms" => dictionary! { "Predictor" => 15, "Colors" => 3, "Columns" => w as i64 },
        },
        content,
    );
    let input = pdf_with_image(img, w as i64, h as i64);
    let res = compress(&input, &CompressOptions::default()).unwrap();

    // Sigue siendo un PDF válido y no crece.
    assert!(Document::load_mem(&res.output).is_ok());
    assert!(res.output.len() <= input.len(), "el output no debe crecer");
    // Exactamente una imagen, y NO se saltó (P1b la decodifica).
    assert_eq!(res.report.images.len(), 1);
    assert_ne!(
        res.report.images[0].action,
        ImageAction::Skipped,
        "una imagen predicha ahora se maneja, images={:?}",
        res.report.images
    );
}

/// Cobertura del path de SKIP con un caso GENUINAMENTE no soportado: una cadena
/// de filtros con `LZWDecode` (que no des-encadenamos). Se SALTA sin corromper.
#[test]
fn unsupported_filter_chain_is_skipped_not_corrupted() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use lopdf::dictionary;
    use std::io::Write;

    // >300px a propósito: por debajo del umbral de sello (MAX_STAMP_DIM=300) la
    // pasada de preservación de firmas/sellos marcaría esta imagen como
    // Preserved en vez de Skipped. Aquí probamos el path de SKIP por filtro no
    // soportado en aislamiento, así que usamos una imagen que NO es candidata a
    // sello.
    let side = 400u32;
    let raw = vec![137u8; (side * side * 3) as usize];
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).unwrap();
    let content = enc.finish().unwrap();

    let img = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => side as i64, "Height" => side as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            // LZWDecode no está soportado en la cadena → SKIP.
            "Filter" => vec![
                Object::Name(b"LZWDecode".to_vec()),
                Object::Name(b"FlateDecode".to_vec()),
            ],
        },
        content,
    );
    let input = pdf_with_image(img, side as i64, side as i64);
    let res = compress(&input, &CompressOptions::default()).unwrap();

    // Sigue siendo un PDF válido.
    assert!(Document::load_mem(&res.output).is_ok());
    // Exactamente una imagen, marcada como Skipped (filtro no soportado).
    assert_eq!(res.report.images.len(), 1);
    assert_eq!(res.report.images[0].action, ImageAction::Skipped);
    assert!(res
        .report
        .warnings
        .iter()
        .any(|w| matches!(w, Warning::ImageSkipped(_))));
    assert_eq!(
        res.report.images[0].original_bytes,
        res.report.images[0].output_bytes
    );
}

/// P2 (v2.0) invierte la limitación de v1: ahora `process_image` lee el CTM real
/// del content stream, así que una imagen de alta resolución dibujada en una caja
/// pequeña SÍ se downsamplea. El content stream de `pdf_with_image` es
/// `q 1 0 0 1 0 0 cm /Im0 Do Q`, es decir la imagen se pinta en un cuadrado de
/// 1pt: 1500px / (1/72) = 108 000 DPI, muy por encima del objetivo → Downsampled.
#[test]
fn high_dpi_image_is_downsampled() {
    let input = pdf_with_image(jpeg_image_stream(1500, 90), 300, 300);
    let res = compress(
        &input,
        &CompressOptions {
            profile: Profile::Screen,
            ..Default::default()
        },
    )
    .unwrap();

    // El output sigue siendo un PDF válido y no crece.
    assert!(
        Document::load_mem(&res.output).is_ok(),
        "el output debe re-parsear"
    );
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
    let res = compress(
        &input,
        &CompressOptions {
            profile: Profile::Screen,
            ..Default::default()
        },
    )
    .unwrap();

    let out_doc = Document::load_mem(&res.output).expect("el output debe re-parsear");
    assert_eq!(res.report.images.len(), 1);
    let action = &res.report.images[0].action;
    assert!(
        matches!(action, ImageAction::Recompressed | ImageAction::Downsampled),
        "una foto Flate debe recomprimirse/downsamplearse, no saltarse: {action:?}"
    );
    // La foto va a JPEG (DCTDecode).
    assert_eq!(
        output_image_filter(&out_doc),
        b"DCTDecode",
        "la foto debe ir a JPEG"
    );
    // Y el documento global encoge respecto al original.
    assert!(
        res.output.len() < input.len(),
        "output={} input={}",
        res.output.len(),
        input.len()
    );
    // El stat de la imagen también encoge.
    assert!(res.report.images[0].output_bytes < res.report.images[0].original_bytes);
}

/// ColorSpace fidelity (v2.0): una imagen en escala de grises que llega como
/// JPEG DeviceGray (un escaneo ya en L8, el caso real de `image::load_from_memory`)
/// se recomprime preservando DeviceGray en el dict de salida, sin inflarla a
/// DeviceRGB. Comparamos contra el equivalente RGB-inflado (misma imagen con
/// R=G=B, tal como haría el código viejo que siempre convertía con to_rgb8())
/// para verificar que el camino gris de verdad pesa menos — ese es todo el
/// punto de la optimización, no sólo una etiqueta de colorspace distinta.
///
/// Nota: no usamos el path FlateDecode aquí porque `classify()` nunca puede
/// devolver `Photo` para una imagen en gris puro (máx. 256 tonos, muy por
/// debajo del umbral de 4096 colores distintos) — ver
/// `classify::tests::grayscale_gradient_stays_line_art`. El caso real de
/// gray→L8 en el pipeline es una imagen ya en DCTDecode/PNG que `image`
/// decodifica directo a `ImageLuma8`.
#[test]
fn gray_jpeg_recompresses_to_devicegray_and_is_smaller_than_rgb_equivalent() {
    // >300px a propósito: por debajo del umbral de sello (MAX_STAMP_DIM=300) la
    // preservación de firmas/sellos marcaría esta imagen como Preserved en vez de
    // recomprimirla. Aquí aislamos el path de recompresión gris→DeviceGray, así
    // que la imagen NO debe ser candidata a sello.
    let side = 400u32;

    // Camino gris: JPEG DeviceGray (L8) → debe seguir en JPEG DeviceGray.
    let gray_input = pdf_with_image(gray_jpeg_image_stream(side, 95), side as i64, side as i64);
    let gray_res = compress(
        &gray_input,
        &CompressOptions {
            profile: Profile::Screen,
            ..Default::default()
        },
    )
    .unwrap();
    let gray_out_doc = Document::load_mem(&gray_res.output).expect("el output debe re-parsear");
    assert_eq!(gray_res.report.images.len(), 1);
    assert!(
        matches!(
            gray_res.report.images[0].action,
            ImageAction::Recompressed | ImageAction::Downsampled
        ),
        "una foto gris debe recomprimirse, no saltarse: {:?}",
        gray_res.report.images[0].action
    );
    assert_eq!(
        output_image_filter(&gray_out_doc),
        b"DCTDecode",
        "la foto gris debe seguir en JPEG"
    );
    assert_eq!(
        output_image_colorspace(&gray_out_doc),
        b"DeviceGray",
        "el dict de salida debe declarar DeviceGray, no inflarse a RGB"
    );

    // Comparación: la MISMA imagen pero con R=G=B (el camino RGB-inflado que
    // haría el código viejo). Debe pesar más que el L8 real.
    let rgb_input = pdf_with_image(
        gray_jpeg_image_as_rgb_stream(side, 95),
        side as i64,
        side as i64,
    );
    let rgb_res = compress(
        &rgb_input,
        &CompressOptions {
            profile: Profile::Screen,
            ..Default::default()
        },
    )
    .unwrap();

    assert!(
        gray_res.report.images[0].output_bytes < rgb_res.report.images[0].output_bytes,
        "L8 ({} bytes) debe pesar menos que el RGB-inflado equivalente ({} bytes)",
        gray_res.report.images[0].output_bytes,
        rgb_res.report.images[0].output_bytes
    );
}

/// P1-Flate content-aware: una imagen de LÍNEA/TEXTO en FlateDecode se mantiene
/// SIN PÉRDIDA. Nunca se convierte a JPEG (halos). Asserta que el filtro de
/// salida sigue siendo FlateDecode.
#[test]
fn flate_lineart_stays_lossless() {
    let side = 256u32;
    let input = pdf_with_image(flate_lineart_stream(side), side as i64, side as i64);
    let res = compress(
        &input,
        &CompressOptions {
            profile: Profile::Screen,
            ..Default::default()
        },
    )
    .unwrap();

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
