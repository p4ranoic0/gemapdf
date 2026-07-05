use super::{Encoded, RawImage, Recompressor};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageEncoder};
use std::borrow::Cow;

pub struct JpegRecompressor;

impl Recompressor for JpegRecompressor {
    fn recompress(&self, raw: &RawImage, quality: u8) -> Option<Encoded> {
        // F5: si ya es RGB8 reutilizamos el buffer interno (sin copia);
        // cualquier otro formato se convierte con to_rgb8().
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
        })
    }
}
