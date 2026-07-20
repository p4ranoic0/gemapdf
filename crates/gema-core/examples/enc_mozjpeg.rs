//! Spike v2.1: compara el encoder actual (jpeg-encoder 4:2:0, sin huffman-opt)
//! contra mozjpeg-rs (BaselineBalanced: baseline + trellis + huffman-opt +
//! deringing) sobre los JPEG más pesados de un PDF real.
//!
//! Las escalas de q de encoders distintos NO son comparables, y PSNR castiga
//! injustamente a trellis (sacrifica error cuadrático donde el ojo no ve). El
//! árbitro es SSIMULACRA2 (métrica perceptual, escala absoluta 0-100): para
//! cada imagen se busca la MENOR q de mozjpeg cuyo SSIM2 (re-decodificado con
//! zune-jpeg, nuestro decoder real) iguala o supera el del encoder actual, y
//! se comparan tamaños ahí (iso-calidad-perceptual).
//!
//! Uso: enc_mozjpeg <quality> <pdf> [top_n]

use lopdf::{Document, Object};
use ssimulacra2::{compute_frame_ssimulacra2, ColorPrimaries, Rgb, TransferCharacteristic};

fn to_ssim_rgb(img: &image::RgbImage) -> Rgb {
    let data: Vec<[f32; 3]> = img
        .pixels()
        .map(|p| {
            [
                p.0[0] as f32 / 255.0,
                p.0[1] as f32 / 255.0,
                p.0[2] as f32 / 255.0,
            ]
        })
        .collect();
    Rgb::new(
        data,
        img.width() as usize,
        img.height() as usize,
        TransferCharacteristic::SRGB,
        ColorPrimaries::BT709,
    )
    .expect("Rgb::new")
}

/// SSIM2 de `bytes` re-decodificados con zune contra `src` (None si no abre).
fn redecode_ssim2(src: &image::RgbImage, bytes: &[u8]) -> Option<f64> {
    let dec = image::load_from_memory(bytes).ok()?.to_rgb8();
    if dec.dimensions() != src.dimensions() {
        return None;
    }
    compute_frame_ssimulacra2(to_ssim_rgb(src), to_ssim_rgb(&dec)).ok()
}

/// Marcador SOF del bitstream: C0 baseline, C2 progressive.
fn sof_marker(bytes: &[u8]) -> Option<u8> {
    let mut i = 2usize;
    while i + 3 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let m = bytes[i + 1];
        if matches!(m, 0xC0..=0xC3) {
            return Some(m);
        }
        if matches!(m, 0xD8 | 0xD9 | 0x01) || (0xD0..=0xD7).contains(&m) {
            i += 2;
            continue;
        }
        let len = ((bytes[i + 2] as usize) << 8) | bytes[i + 3] as usize;
        i += 2 + len;
    }
    None
}

fn moz_encode(rgb: &image::RgbImage, q: u8) -> Vec<u8> {
    mozjpeg_rs::Encoder::new(mozjpeg_rs::Preset::BaselineBalanced)
        .quality(q)
        .subsampling(mozjpeg_rs::Subsampling::S420)
        .encode_rgb(rgb.as_raw(), rgb.width(), rgb.height())
        .expect("mozjpeg-rs encode")
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let q: u8 = args.remove(0).parse().expect("quality");
    let path = args.remove(0);
    let top_n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(6);

    let bytes = std::fs::read(&path).expect("leer pdf");
    let doc = Document::load_mem(&bytes).expect("parsear");

    let mut imgs: Vec<(usize, image::DynamicImage)> = Vec::new();
    for (_, obj) in doc.objects.iter() {
        let Ok(s) = obj.as_stream() else { continue };
        if s.dict.get(b"Subtype").and_then(|o| o.as_name()).ok() != Some(b"Image".as_slice()) {
            continue;
        }
        if !matches!(s.dict.get(b"Filter"), Ok(Object::Name(n)) if n == b"DCTDecode") {
            continue;
        }
        if let Ok(img) = image::load_from_memory(&s.content) {
            if img.color().channel_count() >= 3 {
                imgs.push((s.content.len(), img));
            }
        }
    }
    imgs.sort_by_key(|(len, _)| std::cmp::Reverse(*len));
    imgs.truncate(top_n);

    println!(
        "{:>3} {:>10} {:>7} {:>10} {:>6} {:>7} {:>7} {:>5}",
        "img", "jenc_KB", "ssim2", "moz_KB", "moz_q", "ssim2", "delta", "SOF"
    );
    // t_a = jpeg-encoder (producción); t_b = mozjpeg SIEMPRE; t_sel = SELECCIÓN
    // por-imagen min(jenc, moz) a iso-SSIM2 (lo que haría el bake-off §2);
    // n_moz = imágenes donde mozjpeg gana.
    let (mut t_a, mut t_b, mut t_sel, mut n_moz) = (0usize, 0usize, 0usize, 0usize);
    for (i, (_len, img)) in imgs.iter().enumerate() {
        let rgb = img.to_rgb8();
        let (w, h) = rgb.dimensions();

        // A: encoder actual de producción a la q pedida.
        let mut a = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut a, q);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
        enc.encode(
            rgb.as_raw(),
            w as u16,
            h as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .expect("jpeg-encoder");
        let Some(a_ssim) = redecode_ssim2(&rgb, &a) else {
            println!("{i:>3} zune no re-decodifica al encoder actual (?)");
            continue;
        };

        // B: menor q de mozjpeg con SSIM2 >= a_ssim (búsqueda binaria).
        let (mut lo, mut hi) = (10u8, 95u8);
        let mut best: Option<(u8, Vec<u8>, f64)> = None;
        while lo <= hi {
            let mid = ((lo as u16 + hi as u16) / 2) as u8;
            let b = moz_encode(&rgb, mid);
            match redecode_ssim2(&rgb, &b) {
                Some(s) if s >= a_ssim => {
                    best = Some((mid, b, s));
                    if mid == 0 {
                        break;
                    }
                    hi = mid - 1;
                }
                _ => {
                    lo = mid + 1;
                }
            }
        }

        t_a += a.len();
        match best {
            Some((mq, b, b_ssim)) => {
                let sof = sof_marker(&b)
                    .map(|m| format!("C{:X}", m & 0x0F))
                    .unwrap_or_else(|| "?".into());
                t_b += b.len();
                t_sel += a.len().min(b.len());
                if b.len() < a.len() {
                    n_moz += 1;
                }
                println!(
                    "{:>3} {:>10.1} {:>7.1} {:>10.1} {:>6} {:>7.1} {:>6.1}% {:>5}",
                    i,
                    a.len() as f64 / 1024.0,
                    a_ssim,
                    b.len() as f64 / 1024.0,
                    mq,
                    b_ssim,
                    (b.len() as f64 / a.len() as f64 - 1.0) * 100.0,
                    sof
                );
            }
            None => {
                // mozjpeg no alcanzó la calidad → la selección se queda con jenc.
                t_sel += a.len();
                println!("{i:>3} sin q de mozjpeg que alcance SSIM2 {a_ssim:.1}");
            }
        }
    }
    let mb = |b: usize| b as f64 / 1048576.0;
    println!(
        "TOTAL iso-SSIM2 ({} imgs, mozjpeg gana en {n_moz}):",
        imgs.len()
    );
    println!("  jenc (producción actual) : {:.2} MB", mb(t_a));
    println!(
        "  mozjpeg SIEMPRE           : {:.2} MB  ({:.1}% del actual)",
        mb(t_b),
        t_b as f64 / t_a as f64 * 100.0
    );
    println!(
        "  SELECCIÓN §2 min(jenc,moz): {:.2} MB  ({:.1}% del actual → net −{:.1}%)",
        mb(t_sel),
        t_sel as f64 / t_a as f64 * 100.0,
        (1.0 - t_sel as f64 / t_a as f64) * 100.0
    );
}
