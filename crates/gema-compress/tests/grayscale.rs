use gema_compress::{compress, CompressOptions, ImageAction, Warning};
use lopdf::{dictionary, Document, Object, Stream};
use std::io::Write;

fn padded(mut data: Vec<u8>) -> Vec<u8> {
    data.resize(data.len().max(51_201), 0);
    data
}

fn jpeg(data: &[u8], color: jpeg_encoder::ColorType, q: u8) -> Vec<u8> {
    let mut bytes = Vec::new();
    jpeg_encoder::Encoder::new(&mut bytes, q)
        .encode(data, 400, 400, color)
        .unwrap();
    bytes
}

fn image_stream(filter: &str, cs: Object, bytes: Vec<u8>) -> Stream {
    Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => 400, "Height" => 400,
            "BitsPerComponent" => 8, "ColorSpace" => cs, "Filter" => filter,
        },
        bytes,
    )
}

fn red_jpeg() -> Stream {
    image_stream(
        "DCTDecode",
        "DeviceRGB".into(),
        padded(jpeg(
            &[255, 0, 0].repeat(160_000),
            jpeg_encoder::ColorType::Rgb,
            95,
        )),
    )
}

fn flate(bytes: &[u8]) -> Vec<u8> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(bytes).unwrap();
    padded(z.finish().unwrap())
}

fn pdf(
    mut doc: Document,
    resources: Vec<lopdf::Dictionary>,
    contents: Vec<Vec<u8>>,
    slack: bool,
) -> Vec<u8> {
    let pages = doc.new_object_id();
    let mut kids = Vec::new();
    for (res, mut content) in resources.into_iter().zip(contents) {
        if slack {
            content.extend_from_slice(&b"% padding de fixture\n".repeat(20_000));
        }
        let cid = doc.add_object(Stream::new(dictionary! {}, content));
        kids.push(
            doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => pages, "Resources" => res, "Contents" => cid,
                "MediaBox" => vec![0.into(), 0.into(), 600.into(), 600.into()],
            })
            .into(),
        );
    }
    let count = kids.len() as i64;
    doc.objects.insert(
        pages,
        dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }.into(),
    );
    let root = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    doc.trailer.set("Root", root);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

fn single(stream: Stream, box_pt: u32, slack: bool) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let id = doc.add_object(stream);
    pdf(
        doc,
        vec![dictionary! { "XObject" => dictionary! { "Im" => id } }],
        vec![format!("q {box_pt} 0 0 {box_pt} 0 0 cm /Im Do Q\n").into_bytes()],
        slack,
    )
}

fn options() -> CompressOptions {
    CompressOptions {
        grayscale: true,
        jpeg_quality: Some(45),
        image_dpi: Some(105),
        ..Default::default()
    }
}

fn first_image(bytes: &[u8]) -> Stream {
    Document::load_mem(bytes)
        .unwrap()
        .objects
        .values()
        .find_map(|o| {
            let s = o.as_stream().ok()?;
            (s.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image"))
                .then(|| s.clone())
        })
        .unwrap()
}

fn dims(s: &Stream) -> (i64, i64) {
    (
        s.dict.get(b"Width").unwrap().as_i64().unwrap(),
        s.dict.get(b"Height").unwrap().as_i64().unwrap(),
    )
}

fn assert_gray(s: &Stream) {
    assert_eq!(
        s.dict.get(b"ColorSpace").unwrap().as_name().unwrap(),
        b"DeviceGray"
    );
}

#[test]
fn gris_jpeg_rgb() {
    let result = compress(&single(red_jpeg(), 600, true), &options()).unwrap();
    let s = first_image(&result.output);
    assert_gray(&s);
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&s.content));
    let pixels = decoder.decode().unwrap();
    assert_eq!(
        decoder.info().unwrap().pixel_format,
        jpeg_decoder::PixelFormat::L8
    ); // SOF: un componente
    assert!((i16::from(pixels[0]) - 77).abs() <= 2);
}

#[test]
fn gris_flate_rgb() {
    let s = image_stream(
        "FlateDecode",
        "DeviceRGB".into(),
        flate(&[255, 0, 0].repeat(160_000)),
    );
    let out = compress(&single(s, 600, true), &options()).unwrap();
    let s = first_image(&out.output);
    assert_gray(&s);
    assert_eq!(
        s.dict.get(b"Filter").unwrap().as_name().unwrap(),
        b"FlateDecode"
    );
    assert_eq!(s.decompressed_content().unwrap()[0], 77);
}

#[test]
fn gris_cmyk_adobe() {
    let bytes = padded(jpeg(
        &[20, 80, 130, 40].repeat(160_000),
        jpeg_encoder::ColorType::Cmyk,
        95,
    ));
    assert!(bytes.windows(5).any(|w| w == b"Adobe"));
    // Misma ruta actual: jpeg-decoder CMYK32 -> (c*k/255, m*k/255, y*k/255).
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(&bytes));
    let px = decoder.decode().unwrap();
    let rgb: Vec<u8> = px[..3]
        .iter()
        .map(|&v| (u32::from(v) * u32::from(px[3]) / 255) as u8)
        .collect();
    let expected =
        (30 * u32::from(rgb[0]) + 59 * u32::from(rgb[1]) + 11 * u32::from(rgb[2]) + 50) / 100;
    let out = compress(
        &single(
            image_stream("DCTDecode", "DeviceCMYK".into(), bytes),
            600,
            true,
        ),
        &options(),
    )
    .unwrap();
    let s = first_image(&out.output);
    assert_gray(&s);
    let gray = image::load_from_memory(&s.content).unwrap().to_luma8();
    assert!((i32::from(gray.get_pixel(0, 0)[0]) - expected as i32).abs() <= 2);
}

#[test]
fn gris_indexed() {
    let cs = vec![
        Object::Name(b"Indexed".to_vec()),
        "DeviceRGB".into(),
        0.into(),
        Object::String(vec![255, 0, 0], lopdf::StringFormat::Literal),
    ]
    .into();
    let out = compress(
        &single(
            image_stream("FlateDecode", cs, flate(&vec![0; 160_000])),
            600,
            true,
        ),
        &options(),
    )
    .unwrap();
    let s = first_image(&out.output);
    assert_gray(&s);
    assert_eq!(s.decompressed_content().unwrap()[0], 77);
}

#[test]
fn gris_icc_n3() {
    let mut doc = Document::with_version("1.5");
    let icc = doc.add_object(Stream::new(dictionary! { "N" => 3 }, vec![]));
    let id = doc.add_object(image_stream(
        "FlateDecode",
        vec![Object::Name(b"ICCBased".to_vec()), icc.into()].into(),
        flate(&[255, 0, 0].repeat(160_000)),
    ));
    let bytes = pdf(
        doc,
        vec![dictionary! { "XObject" => dictionary! { "Im" => id } }],
        vec![b"q 600 0 0 600 0 0 cm /Im Do Q".to_vec()],
        true,
    );
    let out = compress(&bytes, &options()).unwrap();
    let s = first_image(&out.output);
    assert_gray(&s);
    assert_eq!(s.decompressed_content().unwrap()[0], 77);
}

fn minimal_jpeg() -> Stream {
    let mut state = 20261109u32;
    let noise: Vec<u8> = (0..480_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    image_stream(
        "DCTDecode",
        "DeviceRGB".into(),
        padded(jpeg(&noise, jpeg_encoder::ColorType::Rgb, 25)),
    )
}

#[test]
fn geometria_kept() {
    let input = single(minimal_jpeg(), 600, true);
    let color = compress(
        &input,
        &CompressOptions {
            grayscale: false,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(color.report.images[0].action, ImageAction::Kept);
    let gray = compress(&input, &options()).unwrap();
    assert_gray(&first_image(&gray.output));
    assert_eq!(dims(&first_image(&gray.output)), (400, 400));
}

#[test]
fn geometria_downsample() {
    let input = single(red_jpeg(), 144, true);
    let color = compress(
        &input,
        &CompressOptions {
            grayscale: false,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(color.report.images[0].action, ImageAction::Downsampled);
    let gray = compress(&input, &options()).unwrap();
    assert_gray(&first_image(&gray.output));
    assert_eq!(
        dims(&first_image(&gray.output)),
        dims(&first_image(&color.output))
    );
}

#[test]
fn oversize_cuenta() {
    // Fuente q1 con textura de baja frecuencia: re-encode q45 requiere más bytes.
    let mut s = minimal_jpeg();
    // Se usa q100 en origen y un patrón q1 que el decoder conserva: verificar el caso real.
    let noise: Vec<u8> = (0..160_000u32)
        .flat_map(|i| {
            let v = ((i * 73 + i / 400 * 137) % 256) as u8;
            [v, v, v]
        })
        .collect();
    s.content = padded(jpeg(&noise, jpeg_encoder::ColorType::Rgb, 20));
    let result = compress(&single(s, 600, true), &options()).unwrap();
    assert_gray(&first_image(&result.output));
    assert!(result.report.images[0].output_bytes >= result.report.images[0].original_bytes);
    assert!(result.report.warnings.iter().any(|w| matches!(
        w,
        Warning::Grayscale {
            oversize_images: 1,
            ..
        }
    )));
}
