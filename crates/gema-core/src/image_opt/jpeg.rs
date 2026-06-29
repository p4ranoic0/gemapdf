use super::{Encoded, RawImage, Recompressor};
use image::codecs::jpeg::JpegEncoder;
use image::ImageEncoder;

pub struct JpegRecompressor;

impl Recompressor for JpegRecompressor {
    fn recompress(&self, raw: &RawImage, quality: u8) -> Option<Encoded> {
        let rgb = raw.image.to_rgb8();
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
        Some(Encoded { bytes, filter: "DCTDecode" })
    }
}
