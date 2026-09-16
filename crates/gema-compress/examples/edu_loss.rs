//! Experimento educativo: anatomía de la pérdida de calidad del pipeline.
//! Aísla cada etapa sobre la MISMA página (render 300dpi como master):
//!   V1 = solo downsample a 90dpi (re-ampliado para ver)   → pérdida de RESOLUCIÓN
//!   V2 = downsample + JPEG q45 4:4:4                      → + CUANTIZACIÓN DCT
//!   V3 = downsample + JPEG q45 4:2:0  (pipeline actual)   → + SUBSAMPLING DE CROMA
//!   V4 = JPEG q45 4:2:0 SIN downsample                    → cuantización sola, resolución intacta
//!
//! Emite: crops JPEG q90 por región×variante (reescalados a ancho ≤640 para la
//! página comparativa), y un TSV con tamaño-en-PDF + SSIM2 (proxy 1MPx) por variante.
//!
//! Uso: edu_loss <master.png> <outdir>

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

/// Reduce a ~1MPx para puntuar (mismo reduce para todos → sesgo consistente).
fn proxy(img: &image::RgbImage) -> image::RgbImage {
    let (w, h) = img.dimensions();
    let px = w as u64 * h as u64;
    if px <= 1_000_000 {
        return img.clone();
    }
    let s = (1_000_000f64 / px as f64).sqrt();
    image::imageops::resize(
        img,
        (w as f64 * s) as u32,
        (h as f64 * s) as u32,
        image::imageops::FilterType::CatmullRom,
    )
}

fn jpeg_bytes(img: &image::RgbImage, q: u8, sf: jpeg_encoder::SamplingFactor) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, q);
    enc.set_sampling_factor(sf);
    enc.encode(
        img.as_raw(),
        img.width() as u16,
        img.height() as u16,
        jpeg_encoder::ColorType::Rgb,
    )
    .expect("encode");
    out
}

fn flate_len(img: &image::RgbImage) -> usize {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;
    let mut e = ZlibEncoder::new(Vec::new(), Compression::best());
    e.write_all(img.as_raw()).unwrap();
    e.finish().unwrap().len()
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let master_path = args.remove(0);
    let outdir = args.remove(0);
    std::fs::create_dir_all(&outdir).unwrap();

    let master = image::open(&master_path).expect("abrir master").to_rgb8();
    let (mw, mh) = master.dimensions();
    let scale = 90.0 / 300.0;
    let (dw, dh) = ((mw as f64 * scale) as u32, (mh as f64 * scale) as u32);

    // downsample como el pipeline (Lanczos3)
    let down = image::imageops::resize(&master, dw, dh, image::imageops::FilterType::Lanczos3);

    // upscale de vuelta a dims master, como interpola un viewer (CatmullRom≈bilinear suave)
    let up = |img: &image::RgbImage| -> image::RgbImage {
        image::imageops::resize(img, mw, mh, image::imageops::FilterType::CatmullRom)
    };
    let dec = |bytes: &[u8]| -> image::RgbImage {
        image::load_from_memory(bytes).expect("decode").to_rgb8()
    };

    use jpeg_encoder::SamplingFactor::{F_1_1, F_2_2};
    let v2_bytes = jpeg_bytes(&down, 45, F_1_1);
    let v3_bytes = jpeg_bytes(&down, 45, F_2_2);
    let v4_bytes = jpeg_bytes(&master, 45, F_2_2);

    // buffers a dims master para crops comparables
    let variants: Vec<(&str, image::RgbImage, usize, &str)> = vec![
        ("master", master.clone(), 0, "original (render 300dpi)"),
        ("v1", up(&down), flate_len(&down), "solo 90dpi (lossless)"),
        (
            "v2",
            up(&dec(&v2_bytes)),
            v2_bytes.len(),
            "90dpi + q45 4:4:4",
        ),
        (
            "v3",
            up(&dec(&v3_bytes)),
            v3_bytes.len(),
            "90dpi + q45 4:2:0 = HOY",
        ),
        ("v4", dec(&v4_bytes), v4_bytes.len(), "300dpi + q45 4:2:0"),
    ];

    // SSIM2 vs master (proxy)
    let master_proxy = proxy(&master);
    println!("variant\tdesc\tbytes_en_pdf\tssim2");
    let mut scores = Vec::new();
    for (name, buf, size, desc) in &variants {
        let s = if *name == "master" {
            100.0
        } else {
            compute_frame_ssimulacra2(to_ssim_rgb(&master_proxy), to_ssim_rgb(&proxy(buf)))
                .unwrap_or(f64::NAN)
        };
        scores.push(s);
        println!("{name}\t{desc}\t{size}\t{s:.1}");
    }

    // crops: (nombre, x, y, w, h) en coords master
    let regions: [(&str, u32, u32, u32, u32); 3] = [
        ("texto", 600, 1000, 1200, 900),
        ("sello", 1700, 1100, 700, 600),
        ("rojo", 2050, 150, 430, 600),
    ];
    for (rname, x, y, w, h) in regions {
        for (vname, buf, _, _) in &variants {
            let crop = image::imageops::crop_imm(buf, x, y, w, h).to_image();
            // reescala a ancho ≤640 para la página comparativa
            let cw = crop.width().min(640);
            let ch = (crop.height() as f64 * cw as f64 / crop.width() as f64) as u32;
            let small =
                image::imageops::resize(&crop, cw, ch, image::imageops::FilterType::CatmullRom);
            let mut out = Vec::new();
            let mut enc = jpeg_encoder::Encoder::new(&mut out, 90);
            enc.set_sampling_factor(F_1_1); // el códec de EMBEBIDO no debe meter su propia pérdida visible
            enc.encode(
                small.as_raw(),
                small.width() as u16,
                small.height() as u16,
                jpeg_encoder::ColorType::Rgb,
            )
            .unwrap();
            std::fs::write(format!("{outdir}/{rname}_{vname}.jpg"), out).unwrap();
        }
    }
    eprintln!("crops escritos en {outdir}");
}
