use gema_compress::{compress, CompressOptions, Profile};
use image::codecs::jpeg::JpegEncoder;
use image::{ImageEncoder, RgbImage};
use lopdf::{dictionary, Document, Object, Stream};

fn photo_pdf(side: u32, jpeg_q: u8) -> Vec<u8> {
    let mut rgb = RgbImage::new(side, side);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        // gradiente suave (foto-like, comprime bien sin artefactos)
        *px = image::Rgb([((x * 255) / side) as u8, ((y * 255) / side) as u8, 128]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, jpeg_q)
        .write_image(rgb.as_raw(), side, side, image::ExtendedColorType::Rgb8)
        .unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let img_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => side as i64, "Height" => side as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB", "Filter" => "DCTDecode",
        },
        jpeg,
    ));
    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        b"q 1 0 0 1 0 0 cm /Im0 Do Q".to_vec(),
    ));
    let resources_id = doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id, "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), (side as i64).into(), (side as i64).into()],
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

/// Como `photo_pdf`, pero dibuja la imagen en una caja de `box_pt` puntos de
/// lado (vía `cm box_pt 0 0 box_pt 0 0`). El DPI efectivo resultante es
/// `side / (box_pt / 72)`, con lo que se controla si P2 dispara downsampling.
fn photo_pdf_boxed(side: u32, jpeg_q: u8, box_pt: i64) -> Vec<u8> {
    let mut rgb = RgbImage::new(side, side);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        *px = image::Rgb([((x * 255) / side) as u8, ((y * 255) / side) as u8, 128]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, jpeg_q)
        .write_image(rgb.as_raw(), side, side, image::ExtendedColorType::Rgb8)
        .unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let img_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => side as i64, "Height" => side as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB", "Filter" => "DCTDecode",
        },
        jpeg,
    ));
    let content = format!("q {box_pt} 0 0 {box_pt} 0 0 cm /Im0 Do Q");
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
    let resources_id = doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id, "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), box_pt.into(), box_pt.into()],
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

/// Extrae la primera imagen del PDF y la decodifica.
fn first_image(pdf: &[u8]) -> image::DynamicImage {
    let doc = Document::load_mem(pdf).unwrap();
    for obj in doc.objects.values() {
        if let Ok(s) = obj.as_stream() {
            if s.dict
                .get(b"Subtype")
                .and_then(|o| o.as_name())
                .map(|n| n == b"Image")
                .unwrap_or(false)
            {
                return image::load_from_memory(&s.content).unwrap();
            }
        }
    }
    panic!("no image found");
}

fn psnr(a: &image::RgbImage, b: &image::RgbImage) -> f64 {
    assert_eq!(a.dimensions(), b.dimensions());
    let mut mse = 0f64;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        for c in 0..3 {
            let d = pa[c] as f64 - pb[c] as f64;
            mse += d * d;
        }
    }
    mse /= (a.width() * a.height() * 3) as f64;
    if mse == 0.0 {
        return 100.0;
    }
    20.0 * (255f64).log10() - 10.0 * mse.log10()
}

#[test]
fn corpus_outputs_reparse_and_shrink() {
    for side in [400u32, 800, 1200] {
        let input = photo_pdf(side, 95);
        let res = compress(
            &input,
            &CompressOptions {
                profile: Profile::Ebook,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            Document::load_mem(&res.output).is_ok(),
            "side={side}: output debe re-parsear"
        );
        assert!(
            res.output.len() <= input.len(),
            "side={side}: no debe crecer"
        );
        assert_eq!(res.report.pages, 1);
        eprintln!(
            "side={side}: {} → {} bytes ({:.1}% del original)",
            input.len(),
            res.output.len(),
            res.output.len() as f64 / input.len() as f64 * 100.0
        );
    }
}

/// P2: una imagen genuinamente de alta resolución (1200px en una caja de 144pt
/// = 600 DPI) se downsamplea con el perfil ebook (objetivo 150 DPI), y el
/// downsampling entrega ahorro REAL: el output con downsampling activo es más
/// pequeño que sin él. Prueba que P2 no es sólo cosmético.
#[test]
fn high_dpi_image_downsamples_and_saves_more() {
    // 1200px dibujada en 144pt → 1200 / (144/72) = 600 DPI.
    let input = photo_pdf_boxed(1200, 92, 144);

    let with_ds = compress(
        &input,
        &CompressOptions {
            profile: Profile::Ebook,
            downsample: true,
            ..Default::default()
        },
    )
    .unwrap();
    let without_ds = compress(
        &input,
        &CompressOptions {
            profile: Profile::Ebook,
            downsample: false,
            ..Default::default()
        },
    )
    .unwrap();

    // el output con downsampling re-parsea y no crece
    assert!(
        Document::load_mem(&with_ds.output).is_ok(),
        "el output debe re-parsear"
    );
    assert!(with_ds.output.len() < input.len(), "el output debe encoger");

    // la imagen se marca Downsampled
    assert_eq!(with_ds.report.images.len(), 1);
    assert_eq!(
        with_ds.report.images[0].action,
        gema_compress::ImageAction::Downsampled,
        "P2: 600 DPI vs objetivo 150 → debe downsamplear, images={:?}",
        with_ds.report.images
    );

    // y entrega ahorro real: menos bytes que sin downsampling
    assert!(
        with_ds.output.len() < without_ds.output.len(),
        "P2 debe ahorrar más: con_ds={} sin_ds={}",
        with_ds.output.len(),
        without_ds.output.len()
    );
    eprintln!(
        "P2: input={} con_ds={} sin_ds={} (ahorro extra {} bytes)",
        input.len(),
        with_ds.output.len(),
        without_ds.output.len(),
        without_ds.output.len() as i64 - with_ds.output.len() as i64
    );
}

#[test]
fn quality_stays_above_psnr_threshold() {
    // imagen grande recomprimida a Ebook: la calidad perceptual no debe colapsar.
    let input = photo_pdf(1000, 95);
    let before = first_image(&input).to_rgb8();
    let res = compress(
        &input,
        &CompressOptions {
            profile: Profile::Ebook,
            downsample: false,
            ..Default::default()
        },
    )
    .unwrap();
    let after = first_image(&res.output).to_rgb8();
    // con downsample desactivado, las dimensiones coinciden → PSNR comparable
    let score = psnr(&before, &after);
    eprintln!("PSNR medido: {score:.2} dB");
    // Umbral medido: recomprimir un gradiente suave de calidad 95 a calidad Ebook (65)
    // da ~PSNR alto porque el contenido es de banda baja; ver salida del test para el
    // valor real. Mantenemos 30 dB como piso de "no colapsa" sin subir la calidad Ebook.
    assert!(
        score > 30.0,
        "PSNR demasiado bajo: {score:.2} dB (esperado > 30)"
    );
}
