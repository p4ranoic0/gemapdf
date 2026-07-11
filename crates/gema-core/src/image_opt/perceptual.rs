//! Búsqueda de q por calidad perceptual (feature `perceptual`, spec 2026-07-11).
//!
//! Para una imagen decodificada, encuentra la MENOR q∈[Q_MIN, Q_MAX] cuyo
//! re-decode (con `image`/zune — el decoder real del pipeline) puntúa
//! SSIMULACRA2 ≥ target contra los píxeles fuente. El score se calcula sobre
//! un proxy de ≤1 MPx (mismo reduce CatmullRom para fuente y candidata: el
//! sesgo es consistente y la búsqueda solo necesita orden, no valor absoluto).
//! Si ni Q_MAX alcanza el target devuelve el encode a Q_MAX con `false`
//! (mejor esfuerzo — el orquestador lo reporta como warning).

use crate::image_opt::jpeg::JpegRecompressor;
use crate::image_opt::{Encoded, RawImage, Recompressor};
use ssimulacra2::{compute_frame_ssimulacra2, ColorPrimaries, Rgb, TransferCharacteristic};

const Q_MIN: u8 = 20;
const Q_MAX: u8 = 90;
const PROXY_MAX_PX: u64 = 1_000_000;

/// Reduce a ≤1 MPx si hace falta (CatmullRom ≈ interpolación de viewer).
fn proxy(img: &image::RgbImage) -> image::RgbImage {
    let (w, h) = img.dimensions();
    let px = w as u64 * h as u64;
    if px <= PROXY_MAX_PX {
        return img.clone();
    }
    let s = (PROXY_MAX_PX as f64 / px as f64).sqrt();
    image::imageops::resize(
        img,
        ((w as f64 * s) as u32).max(1),
        ((h as f64 * s) as u32).max(1),
        image::imageops::FilterType::CatmullRom,
    )
}

/// Convierte a la representación del métrico. `None` si la conversión falla
/// (dims inválidas): el llamador degrada a q fija, nunca panic.
fn to_metric(img: &image::RgbImage) -> Option<Rgb> {
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
    .ok()
}

/// SSIM2 de `bytes` re-decodificados contra el proxy fuente. `None` si zune no
/// abre los bytes o el métrico falla.
fn score(src_proxy: &image::RgbImage, src_metric: &Rgb, bytes: &[u8]) -> Option<f64> {
    let dec = image::load_from_memory(bytes).ok()?.to_rgb8();
    // misma reducción que la fuente; dims explícitas para garantizar igualdad
    let dec_proxy = if dec.dimensions() == src_proxy.dimensions() {
        dec
    } else {
        image::imageops::resize(
            &dec,
            src_proxy.width(),
            src_proxy.height(),
            image::imageops::FilterType::CatmullRom,
        )
    };
    let dm = to_metric(&dec_proxy)?;
    compute_frame_ssimulacra2(src_metric.clone(), dm).ok()
}

/// Ver doc del módulo. `None` solo si el ENCODER falla (dims > u16, etc.) —
/// el orquestador lo traduce a Skipped igual que hoy.
#[allow(dead_code)] // TODO(task 3): lo consume el orquestador
pub(crate) fn encode_jpeg_at_target(raw: &RawImage, target: f32) -> Option<(Encoded, bool)> {
    let src = raw.image.to_rgb8();
    let src_proxy = proxy(&src);
    let Some(src_metric) = to_metric(&src_proxy) else {
        // métrico no construible → degradar a mejor esfuerzo a Q_MAX
        return JpegRecompressor.recompress(raw, Q_MAX).map(|e| (e, false));
    };

    let (mut lo, mut hi) = (Q_MIN, Q_MAX);
    let mut best: Option<Encoded> = None;
    while lo <= hi {
        let mid = ((lo as u16 + hi as u16) / 2) as u8;
        let enc = JpegRecompressor.recompress(raw, mid)?;
        match score(&src_proxy, &src_metric, &enc.bytes) {
            Some(s) if s >= target as f64 => {
                best = Some(enc);
                if mid == Q_MIN {
                    break;
                }
                hi = mid - 1;
            }
            _ => lo = mid + 1,
        }
    }
    match best {
        Some(e) => Some((e, true)),
        None => JpegRecompressor.recompress(raw, Q_MAX).map(|e| (e, false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Foto sintética con contenido real (gradientes + textura): 400×400.
    fn photo() -> RawImage {
        let img = image::RgbImage::from_fn(400, 400, |x, y| {
            let fx = x as f32 / 31.0;
            let fy = y as f32 / 23.0;
            image::Rgb([
                (128.0 + 90.0 * (fx.sin() * fy.cos())) as u8,
                (128.0 + 70.0 * ((fx * 0.7 + 1.0).cos())) as u8,
                (128.0 + 80.0 * ((fy * 1.3).sin())) as u8,
            ])
        });
        RawImage {
            image: image::DynamicImage::ImageRgb8(img),
        }
    }

    #[test]
    fn reaches_reasonable_target_and_score_holds() {
        let raw = photo();
        let (enc, reached) = encode_jpeg_at_target(&raw, 60.0).expect("debe encodear");
        assert!(reached, "τ=60 debe ser alcanzable en una foto sintética");
        // el score real del resultado (misma vía que la búsqueda) cumple el target
        let src = raw.image.to_rgb8();
        let sp = proxy(&src);
        let sm = to_metric(&sp).unwrap();
        let s = score(&sp, &sm, &enc.bytes).expect("re-decode debe abrir");
        assert!(s >= 60.0, "score {s:.1} < target");
        assert_eq!(enc.filter, "DCTDecode");
    }

    #[test]
    fn unreachable_target_falls_back_to_qmax() {
        let raw = photo();
        let (enc, reached) = encode_jpeg_at_target(&raw, 99.9).expect("debe encodear");
        assert!(!reached, "τ=99.9 no debe alcanzarse con JPEG q90");
        let at_qmax = JpegRecompressor.recompress(&raw, Q_MAX).unwrap();
        assert_eq!(
            enc.bytes, at_qmax.bytes,
            "el mejor esfuerzo debe ser exactamente q=Q_MAX"
        );
    }

    #[test]
    fn higher_target_costs_more_bytes() {
        let raw = photo();
        let (lo, _) = encode_jpeg_at_target(&raw, 45.0).unwrap();
        let (hi, _) = encode_jpeg_at_target(&raw, 75.0).unwrap();
        assert!(
            hi.bytes.len() >= lo.bytes.len(),
            "τ mayor no puede costar menos bytes ({} vs {})",
            hi.bytes.len(),
            lo.bytes.len()
        );
    }

    #[test]
    fn grayscale_stays_l8_devicegray() {
        let gray = image::GrayImage::from_fn(400, 400, |x, y| {
            image::Luma([((x * 7 + y * 13) % 256) as u8])
        });
        let raw = RawImage {
            image: image::DynamicImage::ImageLuma8(gray),
        };
        let (enc, _) = encode_jpeg_at_target(&raw, 55.0).expect("gris debe encodear");
        assert_eq!(enc.color_space, "DeviceGray");
    }
}
