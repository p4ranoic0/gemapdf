//! Conversión sobre el RGB que ya interpreta el motor, sin gestión ICC adicional.
#[allow(dead_code)] // Cableado al pipeline en la tarea 2.
pub(crate) fn rgb8_to_gray8(r: u8, g: u8, b: u8) -> u8 {
    ((30 * u32::from(r) + 59 * u32::from(g) + 11 * u32::from(b) + 50) / 100) as u8
}

#[allow(dead_code)]
pub(crate) fn to_luma8(image: &image::DynamicImage) -> image::GrayImage {
    let rgb = image.to_rgb8();
    image::GrayImage::from_fn(rgb.width(), rgb.height(), |x, y| {
        let image::Rgb([r, g, b]) = *rgb.get_pixel(x, y);
        image::Luma([rgb8_to_gray8(r, g, b)])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn u8_redondea() {
        assert_eq!(rgb8_to_gray8(255, 0, 0), 77);
        assert_eq!(rgb8_to_gray8(0, 0, 255), 28);
        assert_eq!(rgb8_to_gray8(128, 128, 128), 128);
    }
    #[test]
    fn luma8_de_rgb() {
        let rgb = image::RgbImage::from_raw(2, 1, vec![255, 0, 0, 0, 0, 255]).unwrap();
        assert_eq!(
            to_luma8(&image::DynamicImage::ImageRgb8(rgb)).into_raw(),
            vec![77, 28]
        );
    }
}
