//! Scratch: SSIM2 entre dos PNGs (renders de la misma página, antes/después).
//! Uso: ssim2_cmp <a.png> <b.png>

use ssimulacra2::{compute_frame_ssimulacra2, ColorPrimaries, Rgb, TransferCharacteristic};

fn to_rgb(img: &image::RgbImage) -> Rgb {
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

fn main() {
    let mut args = std::env::args().skip(1);
    let a = image::open(args.next().expect("a.png"))
        .expect("abrir a")
        .to_rgb8();
    let b_img = image::open(args.next().expect("b.png")).expect("abrir b");
    let mut b = b_img.to_rgb8();
    if b.dimensions() != a.dimensions() {
        b = image::imageops::resize(
            &b,
            a.width(),
            a.height(),
            image::imageops::FilterType::CatmullRom,
        );
    }
    let s = compute_frame_ssimulacra2(to_rgb(&a), to_rgb(&b)).expect("ssim2");
    println!("{s:.2}");
}
