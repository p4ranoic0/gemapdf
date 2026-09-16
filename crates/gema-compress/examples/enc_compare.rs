//! Scratch: compara el encoder JPEG actual (image crate, 4:4:4, Huffman fijo)
//! contra jpeg-encoder (4:2:0 + Huffman optimizado) sobre las imágenes JPEG más
//! pesadas de un PDF real, a la misma calidad. Dimensiona el lever "encoder".
//!
//! Uso: enc_compare <quality> <pdf> [top_n]

use lopdf::{Document, Object};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let q: u8 = args.remove(0).parse().expect("quality");
    let path = args.remove(0);
    let top_n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(10);

    let bytes = std::fs::read(&path).expect("leer pdf");
    let doc = Document::load_mem(&bytes).expect("parsear");

    // Recolectar imágenes DCT abribles, ordenadas por peso desc.
    let mut imgs: Vec<(usize, image::DynamicImage)> = Vec::new();
    for obj in doc.objects.values() {
        let Ok(s) = obj.as_stream() else { continue };
        if s.dict.get(b"Subtype").and_then(|o| o.as_name()).ok() != Some(b"Image".as_slice()) {
            continue;
        }
        let is_dct = matches!(s.dict.get(b"Filter"), Ok(Object::Name(n)) if n == b"DCTDecode");
        if !is_dct {
            continue;
        }
        if let Ok(img) = image::load_from_memory(&s.content) {
            imgs.push((s.content.len(), img));
        }
    }
    imgs.sort_by_key(|(len, _)| std::cmp::Reverse(*len));
    imgs.truncate(top_n);

    println!(
        "{:>4} {:>10} {:>12} {:>12} {:>8}",
        "img", "orig_KB", "image444_KB", "jenc420_KB", "delta"
    );
    let (mut t_o, mut t_a, mut t_b) = (0usize, 0usize, 0usize);
    for (i, (orig_len, img)) in imgs.iter().enumerate() {
        let rgb = img.to_rgb8();
        let (w, h) = rgb.dimensions();

        // A: encoder actual (image crate): 4:4:4, tablas Huffman por defecto.
        let mut a = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut a, q)
            .encode(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .expect("encode image-crate");

        // B: jpeg-encoder: 4:2:0 + Huffman optimizado.
        let mut b = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut b, q);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2); // 4:2:0
        enc.set_optimized_huffman_tables(true);
        enc.encode(
            rgb.as_raw(),
            w as u16,
            h as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .expect("encode jpeg-encoder");

        t_o += orig_len;
        t_a += a.len();
        t_b += b.len();
        println!(
            "{:>4} {:>10.1} {:>12.1} {:>12.1} {:>7.1}%",
            i,
            *orig_len as f64 / 1024.0,
            a.len() as f64 / 1024.0,
            b.len() as f64 / 1024.0,
            (b.len() as f64 / a.len() as f64 - 1.0) * 100.0
        );
    }
    println!(
        "TOTAL orig={:.2}MB image444={:.2}MB jenc420={:.2}MB -> encoder nuevo = {:.1}% del actual",
        t_o as f64 / 1048576.0,
        t_a as f64 / 1048576.0,
        t_b as f64 / 1048576.0,
        t_b as f64 / t_a as f64 * 100.0
    );
}
