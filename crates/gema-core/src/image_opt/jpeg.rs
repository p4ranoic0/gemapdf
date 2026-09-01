use super::{Encoded, RawImage, Recompressor};
use image::DynamicImage;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use std::borrow::Cow;

pub struct JpegRecompressor;

/// Codifica con `jpeg-encoder`: subsampling 4:2:0 (lever B — bate al encoder
/// del crate `image`, que es 4:4:4, a la misma q). Devuelve `None` si las
/// dimensiones exceden u16 (límite del encoder; el pipeline deja la imagen
/// como Kept, sin panic) o si el encode falla. La calidad se acota a 1..=100
/// (jpeg-encoder lo exige).
///
/// NO se activan las tablas Huffman optimizadas (`set_optimized_huffman_tables`):
/// en `jpeg-encoder` 0.6/0.7, activarlas fuerza un modo de codificación con un
/// scan por componente ("sequential", no entrelazado) en vez del scan MCU
/// entrelazado habitual. Combinado con submuestreo de croma (4:2:0), el bitstream
/// resultante es válido — `libjpeg-turbo`/`djpeg` lo decodifica bien — pero el
/// decoder `zune-jpeg` que usa el crate `image` (nuestro propio `image::
/// load_from_memory`, incl. en `image_opt::process::decode`) lo mal-decodifica:
/// deja los planos Cb/Cr en cero, produciendo una imagen con un fuerte tinte
/// verde y PSNR de ~6 dB (medido con un gradiente RGB de prueba). Como gemapdf
/// puede volver a decodificar sus propios streams DCTDecode en una recompresión
/// posterior, esto no es solo un problema de test: corrompería el color de
/// forma visible en un re-proceso. Se prioriza corrección sobre el ~2-4%
/// adicional que darían las tablas optimizadas.
fn encode_jpeg(data: &[u8], w: u32, h: u32, color: ColorType, quality: u8) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
        return None;
    }
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out, quality.clamp(1, 100));
    enc.set_sampling_factor(SamplingFactor::F_2_2); // 4:2:0
    enc.encode(data, w as u16, h as u16, color).ok()?;
    Some(out)
}

impl Recompressor for JpegRecompressor {
    fn recompress(&self, raw: &RawImage, quality: u8) -> Option<Encoded> {
        // ColorSpace fidelity: una imagen ya en gris (Luma8, p. ej. un escaneo
        // DeviceGray decodificado por el path Flate) se codifica como JPEG L8
        // (1 canal) en vez de inflarla a RGB8 (3 canales).
        if let DynamicImage::ImageLuma8(gray) = &raw.image {
            let bytes = encode_jpeg(
                gray.as_raw(),
                gray.width(),
                gray.height(),
                ColorType::Luma,
                quality,
            )?;
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
        let bytes = encode_jpeg(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ColorType::Rgb,
            quality,
        )?;
        Some(Encoded {
            bytes,
            filter: "DCTDecode",
            color_space: "DeviceRGB",
        })
    }
}

/// Número de componentes de color de un JPEG, leído de su marcador SOF
/// (Start Of Frame). Devuelve `None` si `bytes` no es un JPEG reconocible.
///
/// Los JPEG de 4 componentes son CMYK/YCCK (típicos de sellos y escudos
/// generados por Adobe). El crate `image` los mal-decodifica —la transformada
/// APP14/YCCK invertida— y produce píxeles NEGROS. Detectarlos permite
/// preservarlos sin recomprimir en vez de corromperlos.
///
/// Panic-safe ante bytes arbitrarios/truncados (todo acceso es acotado).
pub(crate) fn jpeg_components(bytes: &[u8]) -> Option<u8> {
    // SOI: FF D8
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 1 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        // Los marcadores pueden ir precedidos de bytes de relleno 0xFF.
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] == 0xFF {
            j += 1;
        }
        if j >= bytes.len() {
            return None;
        }
        let marker = bytes[j];
        // Marcadores sin segmento de longitud: SOI, EOI, TEM y RSTn.
        if matches!(marker, 0xD8 | 0xD9 | 0x01) || (0xD0..=0xD7).contains(&marker) {
            i = j + 1;
            continue;
        }
        // Segmento con longitud de 2 bytes.
        let len_pos = j + 1;
        if len_pos + 1 >= bytes.len() {
            return None;
        }
        let seg_len = ((bytes[len_pos] as usize) << 8) | bytes[len_pos + 1] as usize;
        // Marcadores SOF (frame): C0..CF EXCEPTO C4 (DHT), C8 (JPG) y CC (DAC).
        let is_sof = matches!(
            marker,
            0xC0 | 0xC1
                | 0xC2
                | 0xC3
                | 0xC5
                | 0xC6
                | 0xC7
                | 0xC9
                | 0xCA
                | 0xCB
                | 0xCD
                | 0xCE
                | 0xCF
        );
        if is_sof {
            // Layout del SOF: [len:2][precision:1][height:2][width:2][Nf:1]...
            // Nf está en len_pos + 7.
            return bytes.get(len_pos + 7).copied();
        }
        if seg_len < 2 {
            return None; // longitud inválida → evitamos bucle infinito
        }
        i = len_pos + seg_len;
    }
    None
}

/// `true` si los bytes son un JPEG CMYK/YCCK (4 componentes). El crate `image`
/// (zune-jpeg) los mal-decodifica → salen negros; hay que usar `decode_cmyk_jpeg`.
pub(crate) fn is_cmyk_jpeg(bytes: &[u8]) -> bool {
    jpeg_components(bytes) == Some(4)
}

/// Decodifica un JPEG CMYK/YCCK (4 componentes) a RGB **correctamente**, usando
/// `jpeg-decoder` (que devuelve CMYK crudo) y la fórmula Adobe. Es la vía buena
/// para los sellos/escudos CMYK que `image`/zune-jpeg ennegrece.
///
/// Adobe almacena el CMYK invertido; `jpeg-decoder` lo entrega tal cual (tras
/// YCCK→CMYK), así que `R = c·k/255`, `G = m·k/255`, `B = y·k/255` da el color
/// correcto (verificado contra el sello real "PERÚ PAE"). Devuelve `None` si no
/// es CMYK32, si la longitud no cuadra, o si la decodificación falla — el
/// llamador entonces preserva el stream original. Panic-safe.
pub(crate) fn decode_cmyk_jpeg(bytes: &[u8]) -> Option<image::DynamicImage> {
    let mut d = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let px = d.decode().ok()?;
    let info = d.info()?;
    if info.pixel_format != jpeg_decoder::PixelFormat::CMYK32 {
        return None;
    }
    let (w, h) = (info.width as u32, info.height as u32);
    let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if px.len() != expected {
        return None;
    }
    let mut rgb = Vec::with_capacity(expected / 4 * 3);
    for chunk in px.chunks_exact(4) {
        let (c, m, y, k) = (
            chunk[0] as u32,
            chunk[1] as u32,
            chunk[2] as u32,
            chunk[3] as u32,
        );
        rgb.push((c * k / 255) as u8);
        rgb.push((m * k / 255) as u8);
        rgb.push((y * k / 255) as u8);
    }
    Some(image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(
        w, h, rgb,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;
    use image::GenericImageView;
    use image::ImageEncoder;

    const SYNTHETIC_CMYK_DIMENSIONS: (u32, u32) = (64, 64);

    /// Codifica cuatro planos de gris con mozjpeg y los combina como los cuatro
    /// componentes de un JPEG CMYK baseline. APP14 hace que `jpeg-decoder`
    /// entregue estos planos en la convención invertida de Adobe.
    fn synthetic_cmyk_jpeg() -> Vec<u8> {
        let (width, height) = SYNTHETIC_CMYK_DIMENSIONS;
        let mut planes = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for y in 0..height {
            for x in 0..width {
                // Fondo claro con gradientes y bandas diferentes por componente.
                // Los samples bajos se invierten al leer el CMYK Adobe.
                planes[0].push(20u8.saturating_add(((x * 3 + y) % 50) as u8));
                planes[1].push(30u8.saturating_add(((x + y * 5) % 55) as u8));
                planes[2].push(40u8.saturating_add(((x * 5 + y * 3) % 60) as u8));
                planes[3].push(10u8.saturating_add(((x / 8 + y / 8) % 2) as u8 * 18));
            }
        }

        let encoded: Vec<Vec<u8>> = planes
            .iter()
            .map(|plane| {
                mozjpeg_rs::Encoder::new(mozjpeg_rs::Preset::BaselineFastest)
                    .quality(90)
                    .encode_gray(plane, width, height)
                    .expect("mozjpeg debe codificar el plano sintético")
            })
            .collect();

        let (dqt, dht, _) = grayscale_jpeg_parts(&encoded[0]);
        let mut jpeg = Vec::new();
        jpeg.extend_from_slice(&[0xFF, 0xD8]);
        // APP14 Adobe, transform 0: los cuatro componentes son CMYK invertido.
        jpeg.extend_from_slice(&[
            0xFF, 0xEE, 0x00, 0x0E, b'A', b'd', b'o', b'b', b'e', 0x00, 0x64, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ]);
        jpeg.extend_from_slice(dqt);

        // SOF0 con cuatro componentes, todos 1x1 y usando la tabla de cuantización 0.
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x14, 0x08]);
        jpeg.extend_from_slice(&(height as u16).to_be_bytes());
        jpeg.extend_from_slice(&(width as u16).to_be_bytes());
        jpeg.push(4);
        for component in 1..=4 {
            jpeg.extend_from_slice(&[component, 0x11, 0x00]);
        }
        jpeg.extend_from_slice(dht);

        // Un scan no entrelazado por componente; los cuatro usan las mismas
        // tablas Huffman baseline que mozjpeg emitió para cada plano de gris.
        for (index, plane_jpeg) in encoded.iter().enumerate() {
            let (_, _, entropy) = grayscale_jpeg_parts(plane_jpeg);
            jpeg.extend_from_slice(&[
                0xFF,
                0xDA,
                0x00,
                0x08,
                0x01,
                (index + 1) as u8,
                0x00,
                0x00,
                0x3F,
                0x00,
            ]);
            jpeg.extend_from_slice(entropy);
        }
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI
        jpeg
    }

    /// Extrae DQT, DHT y los datos entrópicos de un JPEG grayscale baseline
    /// generado por mozjpeg. El preset elegido emite exactamente una tabla de
    /// cada clase y un único scan.
    fn grayscale_jpeg_parts(jpeg: &[u8]) -> (&[u8], &[u8], &[u8]) {
        assert!(jpeg.starts_with(&[0xFF, 0xD8]));
        assert!(jpeg.ends_with(&[0xFF, 0xD9]));
        let mut offset = 2;
        let mut dqt = None;
        let mut dht = None;
        while offset + 4 <= jpeg.len() {
            assert_eq!(jpeg[offset], 0xFF);
            let marker = jpeg[offset + 1];
            let length = u16::from_be_bytes([jpeg[offset + 2], jpeg[offset + 3]]) as usize;
            let end = offset + 2 + length;
            assert!(end <= jpeg.len());
            match marker {
                0xDB => dqt = Some(&jpeg[offset..end]),
                0xC4 => dht = Some(&jpeg[offset..end]),
                0xDA => {
                    return (
                        dqt.expect("mozjpeg debe emitir DQT"),
                        dht.expect("mozjpeg debe emitir DHT"),
                        &jpeg[end..jpeg.len() - 2],
                    );
                }
                _ => {}
            }
            offset = end;
        }
        panic!("mozjpeg debe emitir un scan SOS")
    }

    fn mean_rgb_brightness(image: &image::DynamicImage) -> f64 {
        let rgb = image.to_rgb8();
        let (sum, count) = rgb.pixels().fold((0u64, 0u64), |(sum, count), pixel| {
            let pixel_sum = pixel
                .0
                .iter()
                .fold(0u64, |sum, channel| sum.saturating_add(u64::from(*channel)));
            (sum.saturating_add(pixel_sum), count.saturating_add(3))
        });
        sum as f64 / count as f64
    }

    /// Construye una cabecera JPEG mínima con un SOF0 de `nf` componentes.
    fn jpeg_with_components(nf: u8) -> Vec<u8> {
        let seg_len: u16 = 8 + (nf as u16) * 3; // 2 len + 1 prec + 4 dims + 1 Nf + 3·Nf
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xC0];
        v.push((seg_len >> 8) as u8);
        v.push((seg_len & 0xFF) as u8);
        v.push(8); // precision
        v.extend_from_slice(&[0x00, 0x10, 0x00, 0x10]); // height, width
        v.push(nf);
        for c in 0..nf {
            v.extend_from_slice(&[c + 1, 0x11, 0x00]); // id, sampling, quant table
        }
        v.extend_from_slice(&[0xFF, 0xD9]); // EOI
        v
    }

    #[test]
    fn detects_4_component_cmyk_jpeg() {
        assert_eq!(jpeg_components(&jpeg_with_components(4)), Some(4));
        assert!(is_cmyk_jpeg(&jpeg_with_components(4)));
    }

    #[test]
    fn three_component_jpeg_is_not_cmyk() {
        assert_eq!(jpeg_components(&jpeg_with_components(3)), Some(3));
        assert!(!is_cmyk_jpeg(&jpeg_with_components(3)));
    }

    #[test]
    fn real_rgb_jpeg_is_3_components_not_cmyk() {
        // Un JPEG RGB real (3 componentes) codificado por el crate `image`.
        let img = image::RgbImage::from_fn(24, 24, |x, y| {
            image::Rgb([(x * 9) as u8, (y * 9) as u8, 128])
        });
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 80)
            .write_image(img.as_raw(), 24, 24, image::ExtendedColorType::Rgb8)
            .unwrap();
        assert_eq!(jpeg_components(&jpeg), Some(3));
        assert!(!is_cmyk_jpeg(&jpeg));
    }

    #[test]
    fn non_jpeg_and_truncated_do_not_panic() {
        assert_eq!(jpeg_components(b""), None);
        assert_eq!(jpeg_components(b"not a jpeg at all"), None);
        assert_eq!(jpeg_components(&[0x89, 0x50, 0x4E, 0x47]), None); // PNG magic
        assert_eq!(jpeg_components(&[0xFF, 0xD8]), None); // SOI sin frame
        assert_eq!(jpeg_components(&[0xFF, 0xD8, 0xFF, 0xC0, 0x00]), None); // SOF truncado
        assert!(!is_cmyk_jpeg(&[0xFF, 0xD8, 0xFF]));
    }

    /// Regresión del "bug del sello negro": un JPEG CMYK Adobe mayormente claro
    /// debe conservar su color al pasar por el decodificador específico.
    #[test]
    fn synthetic_cmyk_jpeg_decodes_to_color_not_black() {
        let bytes = synthetic_cmyk_jpeg();
        assert!(is_cmyk_jpeg(&bytes), "el fixture debe ser CMYK (4 comp)");
        let image = decode_cmyk_jpeg(&bytes).expect("debe decodificar el CMYK");
        assert_eq!(image.dimensions(), SYNTHETIC_CMYK_DIMENSIONS);
        let mean = mean_rgb_brightness(&image);
        assert!(
            mean > 100.0,
            "el CMYK decodificado no debe ser casi negro (brillo medio {mean:.0}/255)"
        );
    }

    /// Gate de fidelidad: el fixture sólo es útil si reproduce el ennegrecido de
    /// `image`/zune-jpeg y a la vez se ve claro con `decode_cmyk_jpeg`.
    #[test]
    fn generic_decoder_blackens_synthetic_cmyk_but_ours_does_not() {
        let bytes = synthetic_cmyk_jpeg();
        let generic = image::load_from_memory(&bytes).expect("image debe abrir el CMYK");
        let ours = decode_cmyk_jpeg(&bytes).expect("debe decodificar el CMYK");
        let generic_mean = mean_rgb_brightness(&generic);
        let ours_mean = mean_rgb_brightness(&ours);
        eprintln!(
            "gate de fidelidad CMYK: image/zune={generic_mean:.2}/255, nuestro={ours_mean:.2}/255"
        );
        assert!(
            generic_mean < 32.0,
            "image/zune debe reproducir el ennegrecido (brillo medio {generic_mean:.2}/255)"
        );
        assert!(
            ours_mean > 100.0,
            "nuestro decoder debe conservar el color (brillo medio {ours_mean:.2}/255)"
        );
    }

    #[test]
    fn decode_cmyk_jpeg_rejects_non_cmyk_and_garbage() {
        // Un JPEG RGB (3 comp) no es CMYK32 → None.
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([20, 40, 60]));
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 70)
            .write_image(img.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        assert!(decode_cmyk_jpeg(&jpeg).is_none());
        assert!(decode_cmyk_jpeg(b"garbage").is_none());
        assert!(decode_cmyk_jpeg(&[0xFF, 0xD8, 0xFF]).is_none());
    }

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

    /// Patrón RGB con croma real (canales distintos) — el caso donde 4:2:0 gana.
    fn color_pattern(w: u32, h: u32) -> image::RgbImage {
        image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([
                ((x * 7) % 256) as u8,
                ((y * 13) % 256) as u8,
                (((x + y) * 5) % 256) as u8,
            ])
        })
    }

    #[test]
    fn new_encoder_beats_image_crate_at_same_quality() {
        let img = color_pattern(128, 128);
        let ours = JpegRecompressor
            .recompress(
                &RawImage {
                    image: DynamicImage::ImageRgb8(img.clone()),
                },
                45,
            )
            .unwrap();
        let mut old = Vec::new();
        JpegEncoder::new_with_quality(&mut old, 45)
            .write_image(img.as_raw(), 128, 128, image::ExtendedColorType::Rgb8)
            .unwrap();
        assert!(
            ours.bytes.len() < old.len(),
            "el encoder nuevo ({}) debe ganar al del crate image ({})",
            ours.bytes.len(),
            old.len()
        );
        // sigue siendo un JPEG válido que `image` puede reabrir
        assert!(image::load_from_memory(&ours.bytes).is_ok());
    }

    #[test]
    fn oversized_dimensions_return_none_not_panic() {
        // jpeg-encoder toma dims u16; >65535 debe dar None (→ Kept), no panic.
        let img = image::RgbImage::new(70_000, 1);
        let raw = RawImage {
            image: DynamicImage::ImageRgb8(img),
        };
        assert!(JpegRecompressor.recompress(&raw, 50).is_none());
    }
}
