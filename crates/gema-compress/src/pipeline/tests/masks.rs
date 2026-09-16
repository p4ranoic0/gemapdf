//! Máscaras suaves: qué se recomprime, qué se preserva intacto y por qué.
//!
//! `/Matte`, alfa embebido y máscaras no inspeccionables tienen cada uno su
//! propio motivo de skip; estos tests fijan cuál corresponde a cada caso y que
//! la máscara nunca se re-encodea con pérdida.

use super::super::*;
use super::fixtures::*;
use crate::options::CompressOptions;
use crate::report::ImageSkipReason;
use lopdf::Object;

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
