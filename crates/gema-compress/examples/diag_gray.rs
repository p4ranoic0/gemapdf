//! Scratch: detecta imágenes RGB que son EFECTIVAMENTE grises (escaneo B/N
//! guardado a color) y simula el ahorro de encodearlas L8 en vez de RGB 4:2:0.
//!
//! Uso: diag_gray <quality> <pdf...>

use lopdf::{Document, Object};

/// Muestrea la imagen y devuelve (fracción de píxeles "casi grises", desviación
/// media de croma). Casi gris = max|R-G|,|G-B|,|R-B| <= 12 (tolerancia de ruido
/// de escáner).
fn grayness(img: &image::DynamicImage) -> (f64, f64) {
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    let step = ((w as u64 * h as u64 / 20_000).max(1)) as usize; // ~20k muestras
    let (mut near, mut total, mut dev_sum) = (0u64, 0u64, 0u64);
    for (i, p) in rgb.pixels().enumerate() {
        if i % step != 0 {
            continue;
        }
        let [r, g, b] = p.0;
        let dev = r.abs_diff(g).max(g.abs_diff(b)).max(r.abs_diff(b));
        if dev <= 12 {
            near += 1;
        }
        dev_sum += dev as u64;
        total += 1;
    }
    (near as f64 / total as f64, dev_sum as f64 / total as f64)
}

fn encode(img: &image::DynamicImage, q: u8, gray: bool) -> usize {
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, q);
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
    if gray {
        let l = img.to_luma8();
        enc.encode(
            l.as_raw(),
            l.width() as u16,
            l.height() as u16,
            jpeg_encoder::ColorType::Luma,
        )
        .ok();
    } else {
        let r = img.to_rgb8();
        enc.encode(
            r.as_raw(),
            r.width() as u16,
            r.height() as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .ok();
    }
    out.len()
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let q: u8 = args.remove(0).parse().expect("quality");
    for path in &args {
        let bytes = std::fs::read(path).expect("leer");
        let doc = Document::load_mem(&bytes).expect("parsear");
        let (mut n_gray, mut n_color) = (0u32, 0u32);
        let (mut rgb_bytes, mut gray_bytes, mut colorful_bytes) = (0usize, 0usize, 0usize);
        for obj in doc.objects.values() {
            let Ok(s) = obj.as_stream() else { continue };
            if s.dict.get(b"Subtype").and_then(|o| o.as_name()).ok() != Some(b"Image".as_slice()) {
                continue;
            }
            // solo DCT razonablemente grandes (donde vive el peso)
            let is_dct = matches!(s.dict.get(b"Filter"), Ok(Object::Name(n)) if n == b"DCTDecode");
            if !is_dct || s.content.len() < 30_000 {
                continue;
            }
            let Ok(img) = image::load_from_memory(&s.content) else {
                continue;
            };
            // solo las que hoy son RGB (las gris ya van L8)
            if img.color().channel_count() < 3 {
                continue;
            }
            let (frac, _dev) = grayness(&img);
            let as_rgb = encode(&img, q, false);
            if frac >= 0.98 {
                n_gray += 1;
                rgb_bytes += as_rgb;
                gray_bytes += encode(&img, q, true);
            } else {
                n_color += 1;
                colorful_bytes += as_rgb;
            }
        }
        let name = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        println!(
            "{name}: efectivamente-grises {} imgs (RGB {:.2} MB -> L8 {:.2} MB, ahorro {:.1}%) | con color real {} imgs ({:.2} MB)",
            n_gray,
            rgb_bytes as f64 / 1048576.0,
            gray_bytes as f64 / 1048576.0,
            if rgb_bytes > 0 {
                (1.0 - gray_bytes as f64 / rgb_bytes as f64) * 100.0
            } else {
                0.0
            },
            n_color,
            colorful_bytes as f64 / 1048576.0,
        );
    }
}
