use super::{Encoded, RawImage, Recompressor};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageEncoder};
use std::borrow::Cow;

pub struct JpegRecompressor;

impl Recompressor for JpegRecompressor {
    fn recompress(&self, raw: &RawImage, quality: u8) -> Option<Encoded> {
        // ColorSpace fidelity: una imagen ya en gris (Luma8, p. ej. un escaneo
        // DeviceGray decodificado por el path Flate) se codifica como JPEG L8
        // (1 canal) en vez de inflarla a RGB8 (3 canales). Cualquier otro
        // formato se normaliza a RGB8 como antes (F5: si ya es RGB8 reutilizamos
        // el buffer interno sin copia).
        if let DynamicImage::ImageLuma8(gray) = &raw.image {
            let mut bytes = Vec::new();
            let encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
            encoder
                .write_image(
                    gray.as_raw(),
                    gray.width(),
                    gray.height(),
                    image::ExtendedColorType::L8,
                )
                .ok()?;
            return Some(Encoded {
                bytes,
                filter: "DCTDecode",
                color_space: "DeviceGray",
            });
        }

        let rgb: Cow<image::RgbImage> = match &raw.image {
            DynamicImage::ImageRgb8(img) => Cow::Borrowed(img),
            other => Cow::Owned(other.to_rgb8()),
        };
        let mut bytes = Vec::new();
        let encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
        encoder
            .write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
            .ok()?;
        Some(Encoded {
            bytes,
            filter: "DCTDecode",
            color_space: "DeviceRGB",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un patrón de gris no plano (no un solo tono) para que la compresión JPEG
    /// tenga contenido real que codificar, en vez de un bloque uniforme que
    /// comprimiría trivialmente en cualquier modo de color.
    fn gray_pattern(w: u32, h: u32) -> image::GrayImage {
        image::GrayImage::from_fn(w, h, |x, y| image::Luma([((x * 7 + y * 13) % 256) as u8]))
    }

    /// Misma imagen pero "inflada" a RGB (R=G=B=gris), como la haría el path
    /// viejo (siempre to_rgb8()). Sirve de comparación para medir el tamaño.
    fn gray_as_rgb(w: u32, h: u32) -> image::RgbImage {
        let g = gray_pattern(w, h);
        image::RgbImage::from_fn(w, h, |x, y| {
            let v = g.get_pixel(x, y).0[0];
            image::Rgb([v, v, v])
        })
    }

    #[test]
    fn grayscale_image_encodes_as_l8_devicegray_and_is_smaller_than_rgb() {
        let (w, h) = (64, 64);
        let gray_raw = RawImage {
            image: DynamicImage::ImageLuma8(gray_pattern(w, h)),
        };
        let enc_gray = JpegRecompressor.recompress(&gray_raw, 70).unwrap();
        assert_eq!(enc_gray.filter, "DCTDecode");
        assert_eq!(enc_gray.color_space, "DeviceGray");
        assert_eq!(&enc_gray.bytes[0..2], &[0xFF, 0xD8]); // cabecera JPEG SOI

        let rgb_raw = RawImage {
            image: DynamicImage::ImageRgb8(gray_as_rgb(w, h)),
        };
        let enc_rgb = JpegRecompressor.recompress(&rgb_raw, 70).unwrap();
        assert_eq!(enc_rgb.color_space, "DeviceRGB");

        assert!(
            enc_gray.bytes.len() < enc_rgb.bytes.len(),
            "L8 ({} bytes) debe pesar menos que el equivalente RGB-inflado ({} bytes)",
            enc_gray.bytes.len(),
            enc_rgb.bytes.len()
        );
    }

    #[test]
    fn rgb_image_still_encodes_as_devicergb() {
        let img = image::RgbImage::from_pixel(16, 16, image::Rgb([10, 200, 90]));
        let raw = RawImage {
            image: DynamicImage::ImageRgb8(img),
        };
        let enc = JpegRecompressor.recompress(&raw, 60).unwrap();
        assert_eq!(enc.filter, "DCTDecode");
        assert_eq!(enc.color_space, "DeviceRGB");
    }
}
