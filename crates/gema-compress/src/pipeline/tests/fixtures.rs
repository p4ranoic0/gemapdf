//! Constructores de PDF sintéticos compartidos por los tests del pipeline.
//!
//! Cada uno arma el documento mínimo que ejercita un camino concreto y lo
//! devuelve ya serializado, junto con los ids de objeto que el test necesita
//! inspeccionar en la salida.
//!
//! Cada función trae sus propios `use` locales de `lopdf`/`image`, así que este
//! módulo no importa nada del pipeline.

// PDF con `n` imágenes JPEG embebidas grandes, todas pintadas en la página.
pub(super) fn pdf_with_jpegs(n: usize) -> Vec<u8> {
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageEncoder, RgbImage};
    use lopdf::{dictionary, Document, Object, Stream};

    // imagen 800x800 con ruido suave → JPEG no trivial
    let mut rgb = RgbImage::new(800, 800);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 95)
        .write_image(rgb.as_raw(), 800, 800, image::ExtendedColorType::Rgb8)
        .unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut xobjects = lopdf::Dictionary::new();
    let mut content = String::new();
    for i in 0..n {
        let img_dict = dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => 800, "Height" => 800,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => "DCTDecode",
        };
        let img_id = doc.add_object(Stream::new(img_dict, jpeg.clone()));
        xobjects.set(format!("Im{i}"), img_id);
        content.push_str(&format!("q 800 0 0 800 0 0 cm /Im{i} Do Q\n"));
    }
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
    let resources_id = doc.add_object(dictionary! { "XObject" => xobjects });
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 800.into(), 800.into()],
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

/// Dos XObjects grandes no decodificables con bytes idénticos y sólo un
/// `/Name` distinto. El pipeline de imagen los conserva; únicamente la
/// deduplicación opt-in debe colapsarlos.
pub(super) fn pdf_with_duplicate_unsupported_images() -> Vec<u8> {
    use lopdf::{dictionary, Document, Object, Stream};

    let mut encoded = Vec::with_capacity(128 * 1024);
    let mut state = 0x1234_5678u32;
    for _ in 0..128 * 1024 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        encoded.push(state as u8);
    }

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let image = |name: &str| {
        Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Name" => name,
                "Width" => 100, "Height" => 100,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "JPXDecode",
            },
            encoded.clone(),
        )
    };
    let a = doc.add_object(image("ImA"));
    let b = doc.add_object(image("ImB"));
    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        b"q 100 0 0 100 0 0 cm /ImA Do Q q 100 0 0 100 100 0 cm /ImB Do Q".to_vec(),
    ));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "Resources" => dictionary! { "XObject" => dictionary! { "ImA" => a, "ImB" => b } },
        "MediaBox" => vec![0.into(), 0.into(), 200.into(), 100.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

// PDF con una imagen JPEG embebida grande.
pub(super) fn pdf_with_jpeg() -> Vec<u8> {
    pdf_with_jpegs(1)
}

/// PDF con una imagen JPEG que referencia un /SMask (XObject de máscara).
/// Devuelve (bytes, contenido original del stream de la imagen, img_id).
pub(super) fn pdf_with_smask_image() -> (Vec<u8>, Vec<u8>, u32) {
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageEncoder, RgbImage};
    use lopdf::{dictionary, Document, Object, Stream};

    let mut rgb = RgbImage::new(400, 400);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 95)
        .write_image(rgb.as_raw(), 400, 400, image::ExtendedColorType::Rgb8)
        .unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    // máscara de transparencia (grayscale, 1 componente)
    let smask_stream = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => 400, "Height" => 400,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceGray",
            "Filter" => "DCTDecode",
        },
        jpeg.clone(),
    );
    let smask_id = doc.add_object(smask_stream);

    let img_content = jpeg.clone();
    let img_stream = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => 400, "Height" => 400,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => "DCTDecode",
            "SMask" => smask_id,
        },
        img_content.clone(),
    );
    let img_id = doc.add_object(img_stream);
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
    (buf, img_content, img_id.0)
}

/// Como `pdf_with_smask_image` pero la máscara lleva /Matte (color
/// premultiplicado): la base debe preservarse entera.
pub(super) fn pdf_with_matte_smask() -> (Vec<u8>, Vec<u8>, u32) {
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageEncoder, RgbImage};
    use lopdf::{dictionary, Document, Object, Stream};

    let mut rgb = RgbImage::new(400, 400);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 95)
        .write_image(rgb.as_raw(), 400, 400, image::ExtendedColorType::Rgb8)
        .unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let smask_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => 400, "Height" => 400,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceGray",
            "Filter" => "DCTDecode",
            // premultiplicado: los píxeles de la base están acoplados a la máscara
            "Matte" => vec![0.into(), 0.into(), 0.into()],
        },
        jpeg.clone(),
    ));
    let img_content = jpeg.clone();
    let img_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => 400, "Height" => 400,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => "DCTDecode",
            "SMask" => smask_id,
        },
        img_content.clone(),
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
    (buf, img_content, img_id.0)
}

/// PDF cuyo XObject de imagen declara dimensiones configurables.
/// Devuelve (bytes, contenido original del stream de la imagen, img_id).
pub(super) fn pdf_with_malformed_dimensions(width: i64, height: i64) -> (Vec<u8>, Vec<u8>, u32) {
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageEncoder, RgbImage};
    use lopdf::{dictionary, Document, Object, Stream};

    let mut rgb = RgbImage::new(400, 400);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 95)
        .write_image(rgb.as_raw(), 400, 400, image::ExtendedColorType::Rgb8)
        .unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let img_content = jpeg.clone();
    let img_stream = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => width, "Height" => height,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => "DCTDecode",
        },
        img_content.clone(),
    );
    let img_id = doc.add_object(img_stream);
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
    (buf, img_content, img_id.0)
}

/// PDF mínimo firmado (page dict con /ByteRange), sin imágenes.
pub(super) fn signed_pdf() -> Vec<u8> {
    use lopdf::{dictionary, Document, Object, Stream};
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "ByteRange" => vec![0.into(), 100.into(), 200.into(), 50.into()],
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

/// JPEG RGB pequeño de `side`×`side` px con ruido suave (fixtures firma/sello).
fn stamp_jpeg(side: u32) -> Vec<u8> {
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageEncoder, RgbImage};
    let mut rgb = RgbImage::new(side, side);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 90)
        .write_image(rgb.as_raw(), side, side, image::ExtendedColorType::Rgb8)
        .unwrap();
    jpeg
}

/// PDF con un sello pequeño (150×150, ≤50KB) pintado en la página.
/// Devuelve (bytes, contenido original del stream de la imagen, img_id).
pub(super) fn pdf_with_small_stamp() -> (Vec<u8>, Vec<u8>, u32) {
    use lopdf::{dictionary, Document, Object, Stream};
    let jpeg = stamp_jpeg(150);
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let img_stream = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => 150, "Height" => 150,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => "DCTDecode",
        },
        jpeg.clone(),
    );
    let img_id = doc.add_object(img_stream);
    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        b"q 150 0 0 150 0 0 cm /Im0 Do Q".to_vec(),
    ));
    let resources_id = doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
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
    (buf, jpeg, img_id.0)
}

/// PDF con un widget de firma (`FT=Sig`) cuya apariencia `/AP/N` es un Form
/// XObject que embebe una imagen GRANDE (400×400, fuera del umbral de sello):
/// se preserva por ser firma, NO por tamaño. Ejercita el caso A completo +
/// la supervivencia a `prune`. Devuelve (bytes, contenido original, img_id).
pub(super) fn pdf_with_signature_appearance() -> (Vec<u8>, Vec<u8>, u32) {
    use lopdf::{dictionary, Document, Object, Stream};
    let jpeg = stamp_jpeg(400);
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let img_stream = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => 400, "Height" => 400,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => "DCTDecode",
        },
        jpeg.clone(),
    );
    let img_id = doc.add_object(img_stream);

    let form_stream = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! { "SImg" => img_id },
            },
        },
        b"q 100 0 0 100 0 0 cm /SImg Do Q".to_vec(),
    );
    let form_id = doc.add_object(form_stream);

    let annot_id = doc.add_object(dictionary! {
        "Type" => "Annot", "Subtype" => "Widget", "FT" => "Sig",
        "Rect" => vec![0.into(), 0.into(), 100.into(), 100.into()],
        "AP" => dictionary! { "N" => form_id },
    });

    let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Annots" => vec![annot_id.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    // AcroForm con NeedAppearances (el flag que en Acrobat oculta la firma):
    // así el aplanado tiene un form real que quitar y la aserción del test
    // "sin /AcroForm" no es vacua.
    let acro_id = doc.add_object(dictionary! {
        "Fields" => vec![annot_id.into()], "NeedAppearances" => true, "SigFlags" => 3,
    });
    let catalog_id = doc.add_object(
        dictionary! { "Type" => "Catalog", "Pages" => pages_id, "AcroForm" => acro_id },
    );
    doc.trailer.set("Root", catalog_id);
    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    (buf, jpeg, img_id.0)
}

/// PDF con una imagen JPEG envuelta en zlib: /Filter [FlateDecode DCTDecode]
/// (lever A). Devuelve (bytes, img_id).
pub(super) fn pdf_with_flate_wrapped_jpeg() -> (Vec<u8>, u32) {
    use flate2::{write::ZlibEncoder, Compression};
    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageEncoder, RgbImage};
    use lopdf::{dictionary, Document, Object, Stream};
    use std::io::Write;

    let mut rgb = RgbImage::new(800, 800);
    for (x, y, px) in rgb.enumerate_pixels_mut() {
        let fx = x as f32;
        let fy = y as f32;
        let r = ((fx * 0.09).sin() * 0.5 + 0.5) * 255.0;
        let g = ((fy * 0.07 + fx * 0.013).cos() * 0.5 + 0.5) * 255.0;
        let b = (((fx + fy) * 0.05).sin() * 0.5 + 0.5) * 255.0;
        *px = image::Rgb([r as u8, g as u8, b as u8]);
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 95)
        .write_image(rgb.as_raw(), 800, 800, image::ExtendedColorType::Rgb8)
        .unwrap();
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(&jpeg).unwrap();
    let wrapped = z.finish().unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let img_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => 800, "Height" => 800,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => vec![
                Object::Name(b"FlateDecode".to_vec()),
                Object::Name(b"DCTDecode".to_vec()),
            ],
        },
        wrapped,
    ));
    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        b"q 800 0 0 800 0 0 cm /Im0 Do Q".to_vec(),
    ));
    let resources_id = doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 800.into(), 800.into()],
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
    (buf, img_id.0)
}

/// PDF con un escaneo de papel guardado en Flate CRUDO (no un JPEG
/// envuelto): es el caso de `doc-B2`. 800×800 px pintados
/// sobre 288 pt ⇒ **200 dpi efectivos**, para que 90 y 110 den anchos
/// distintos y medibles. El ruido de sensor hace que `classify` lo reconozca
/// como raster capturado y lo mande a JPEG.
pub(super) fn pdf_with_flate_scan() -> (Vec<u8>, u32) {
    use flate2::{write::ZlibEncoder, Compression};
    use lopdf::{dictionary, Document, Object, Stream};
    use std::io::Write;

    let (w, h) = (800u32, 800u32);
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    let mut seed = 0x5EEDu64;
    for y in 0..h {
        for x in 0..w {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let noise = (seed >> 33) as i16 % 11 - 5;
            let on_text_row = (y % 16) < 3;
            let in_word = (x / 37) % 4 != 3;
            let base: i16 = if on_text_row && in_word { 70 } else { 242 };
            let v = (base + noise).clamp(0, 255) as u8;
            px.extend_from_slice(&[v, v, v]);
        }
    }
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(&px).unwrap();
    let flate = z.finish().unwrap();

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let img_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => w as i64, "Height" => h as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => "FlateDecode",
        },
        flate,
    ));
    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        b"q 288 0 0 288 0 0 cm /Im0 Do Q".to_vec(),
    ));
    let resources_id = doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 288.into(), 288.into()],
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
    (buf, img_id.0)
}

/// Ancho en píxeles con que quedó escrita una imagen en el output.
pub(super) fn output_width(out: &[u8], img_id: u32) -> i64 {
    use lopdf::Document;

    Document::load_mem(out)
        .expect("el output debe re-parsear")
        .get_object((img_id, 0))
        .unwrap()
        .as_stream()
        .unwrap()
        .dict
        .get(b"Width")
        .unwrap()
        .as_i64()
        .unwrap()
}
