//! SPIKE (ROADMAP §3, spec 2026-07-20). NO se integra al pipeline; se conserva
//! como evidencia reproducible del veredicto.
//!
//! ══ VEREDICTO 2026-07-20: MATADO ══════════════════════════════════════════
//! MRC NO aplica a este corpus. El techo 3-15× de la literatura asume escaneos
//! de 300+ dpi; las páginas del corpus son ~120 dpi (1007px para un A4). A esa
//! resolución el texto tiene ~10px de alto y binarizarlo a bilevel DESTRUYE el
//! anti-aliasing que hacía legible el texto pequeño del JPEG → el resultado es
//! ~4× más chico pero con el texto FRAGMENTADO ("Dosihcac ion" vs
//! "Dosificación"). Medido en cl_p90 (formulario) y cl_p151 (manuscrito, mejor
//! por contenido denso); la estructura sí renderiza en poppler (el G4 de `fax`
//! funciona), pero legibilidad < actual en texto tecleado. Una limpieza de
//! speckle reduce el moteado del fondo pero NO la fragmentación (intrínseca a
//! la resolución). Revivir SOLO si el corpus migra a 300+ dpi. Nota: eg_p98
//! (certificado, CMYK) se descartó — este spike lo carga con `image::open`
//! (zune) que lo decodifica NEGRO (el bug del "sello negro"); un MRC real
//! usaría el workaround jpeg-decoder+Adobe de gema-compress.
//! ══════════════════════════════════════════════════════════════════════════
//!
//! Segmenta UN escaneo (imagen ya extraída con `pdfimages -j`) en MRC:
//!   - máscara de texto bilevel (Sauvola) → CCITT G4 (crate `fax`)
//!   - frente = color oscuro CONSTANTE (media de los píxeles de texto)
//!   - fondo = la imagen downsampleada fuerte → JPEG
//!
//! Recompone un PDF de 2 XObjects (fondo + ImageMask pintada en el frente) y
//! reporta tamaños vs baselines JPEG. El gate de calidad es VISUAL (pdftoppm),
//! no se toca aquí.
//!
//! Uso: mrc_spike <img_in> <out_dir> [bg_div=3] [sauvola_k=0.34] [win=31] [--flate-mask]
//!
//! Objetivo: decidir MATAR-O-SEGUIR MRC según si gana tamaño a legibilidad
//! igual-o-mejor en el corpus real.

use fax::encoder::Encoder;
use fax::{Color, VecWriter};
use image::{codecs::jpeg::JpegEncoder, ExtendedColorType, GrayImage, ImageEncoder, RgbImage};
use lopdf::{dictionary, Document, Object, Stream};
use std::path::Path;

fn jpeg_rgb(img: &RgbImage, q: u8) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, q)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgb8,
        )
        .unwrap();
    out
}

/// Binarización adaptativa de Sauvola con integral images (O(1) por píxel).
/// `true` = texto (píxel oscuro que queda por debajo del umbral local).
fn sauvola(gray: &GrayImage, win: u32, k: f64, r: f64) -> Vec<bool> {
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    // integral de suma y suma de cuadrados (con fila/col 0 de padding).
    let mut isum = vec![0u64; (w + 1) * (h + 1)];
    let mut isq = vec![0u64; (w + 1) * (h + 1)];
    for y in 0..h {
        for x in 0..w {
            let v = gray.get_pixel(x as u32, y as u32).0[0] as u64;
            let i = (y + 1) * (w + 1) + (x + 1);
            isum[i] = v + isum[i - 1] + isum[i - (w + 1)] - isum[i - (w + 1) - 1];
            isq[i] = v * v + isq[i - 1] + isq[i - (w + 1)] - isq[i - (w + 1) - 1];
        }
    }
    let rad = (win / 2) as i64;
    let mut mask = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let x0 = (x as i64 - rad).max(0) as usize;
            let y0 = (y as i64 - rad).max(0) as usize;
            let x1 = (x as i64 + rad).min(w as i64 - 1) as usize;
            let y1 = (y as i64 + rad).min(h as i64 - 1) as usize;
            let area = ((x1 - x0 + 1) * (y1 - y0 + 1)) as f64;
            let idx = |xx: usize, yy: usize| (yy) * (w + 1) + (xx);
            let sum = (isum[idx(x1 + 1, y1 + 1)] + isum[idx(x0, y0)]
                - isum[idx(x0, y1 + 1)]
                - isum[idx(x1 + 1, y0)]) as f64;
            let sq = (isq[idx(x1 + 1, y1 + 1)] + isq[idx(x0, y0)]
                - isq[idx(x0, y1 + 1)]
                - isq[idx(x1 + 1, y0)]) as f64;
            let mean = sum / area;
            let var = (sq / area - mean * mean).max(0.0);
            let std = var.sqrt();
            let t = mean * (1.0 + k * (std / r - 1.0));
            let v = gray.get_pixel(x as u32, y as u32).0[0] as f64;
            mask[y * w + x] = v < t; // texto = más oscuro que el umbral local
        }
    }
    mask
}

/// Limpieza barata de speckle (1 pasada 3×3): quita píxeles de texto aislados
/// (<2 vecinos de texto = ruido del JPEG) y rellena pinholes (≥6 vecinos de
/// texto). Aproxima lo que un MRC real hace con componentes conexas.
fn clean_mask(mask: &[bool], w: u32, h: u32) -> Vec<bool> {
    let (w, h) = (w as usize, h as usize);
    let mut out = mask.to_vec();
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let mut cnt = 0;
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    if (dx, dy) != (0, 0) {
                        let nx = (x as i64 + dx) as usize;
                        let ny = (y as i64 + dy) as usize;
                        if mask[ny * w + nx] {
                            cnt += 1;
                        }
                    }
                }
            }
            let i = y * w + x;
            if mask[i] && cnt < 2 {
                out[i] = false; // speckle aislado
            } else if !mask[i] && cnt >= 6 {
                out[i] = true; // pinhole dentro de un trazo
            }
        }
    }
    out
}

/// CCITT G4 de la máscara: texto→Black (sample 0 = se pinta), fondo→White.
fn g4_encode(mask: &[bool], w: u32, h: u32) -> Vec<u8> {
    let mut enc = Encoder::new(VecWriter::new());
    for y in 0..h as usize {
        let row = (0..w as usize).map(|x| {
            if mask[y * w as usize + x] {
                Color::Black
            } else {
                Color::White
            }
        });
        enc.encode_line(row, w).unwrap();
    }
    enc.finish().unwrap().finish()
}

/// Máscara 1bpp Flate (fallback garantizado en poppler). Bit 0 = texto (se
/// pinta en ImageMask), bit 1 = fondo. MSB primero, filas byte-alineadas.
fn flate_1bpp(mask: &[bool], w: u32, h: u32) -> Vec<u8> {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;
    let row_bytes = w.div_ceil(8) as usize;
    let mut packed = vec![0u8; row_bytes * h as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            // texto → bit 0; fondo → bit 1
            if !mask[y * w as usize + x] {
                packed[y * row_bytes + (x / 8)] |= 0x80 >> (x % 8);
            }
        }
    }
    let mut z = ZlibEncoder::new(Vec::new(), Compression::best());
    z.write_all(&packed).unwrap();
    z.finish().unwrap()
}

#[allow(clippy::too_many_arguments)]
fn build_pdf(
    w: u32,
    h: u32,
    bg_jpeg: &[u8],
    bg_w: u32,
    bg_h: u32,
    mask: &[u8],
    flate_mask: bool,
    fg: [u8; 3],
) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let bg_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => bg_w as i64, "Height" => bg_h as i64,
            "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            "Filter" => "DCTDecode",
        },
        bg_jpeg.to_vec(),
    ));

    let mask_dict = if flate_mask {
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => w as i64, "Height" => h as i64,
            "ImageMask" => true, "BitsPerComponent" => 1,
            "Filter" => "FlateDecode",
        }
    } else {
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => w as i64, "Height" => h as i64,
            "ImageMask" => true, "BitsPerComponent" => 1,
            "Filter" => "CCITTFaxDecode",
            "DecodeParms" => dictionary! { "K" => -1, "Columns" => w as i64, "Rows" => h as i64 },
        }
    };
    let mask_id = doc.add_object(Stream::new(mask_dict, mask.to_vec()));

    let content = format!(
        "q {w} 0 0 {h} 0 0 cm /Bg Do Q\n{:.4} {:.4} {:.4} rg\nq {w} 0 0 {h} 0 0 cm /Mk Do Q\n",
        fg[0] as f64 / 255.0,
        fg[1] as f64 / 255.0,
        fg[2] as f64 / 255.0,
    );
    let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
    let resources_id = doc.add_object(dictionary! {
        "XObject" => dictionary! { "Bg" => bg_id, "Mk" => mask_id },
    });
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), (w as i64).into(), (h as i64).into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut buf = Vec::new();
    doc.save_to(&mut buf).unwrap();
    buf
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("uso: mrc_spike <img_in> <out_dir> [bg_div=3] [k=0.34] [win=31] [--flate-mask]");
        std::process::exit(2);
    }
    let img_path = &args[0];
    let out_dir = &args[1];
    let flate_mask = args.iter().any(|a| a == "--flate-mask");
    let nums: Vec<&String> = args[2..].iter().filter(|a| !a.starts_with("--")).collect();
    let bg_div: u32 = nums.first().map(|s| s.parse().unwrap()).unwrap_or(3);
    let k: f64 = nums.get(1).map(|s| s.parse().unwrap()).unwrap_or(0.34);
    let win: u32 = nums.get(2).map(|s| s.parse().unwrap()).unwrap_or(31);

    let dyn_img = image::open(img_path).expect("abrir imagen");
    let rgb = dyn_img.to_rgb8();
    let gray = dyn_img.to_luma8();
    let (w, h) = (rgb.width(), rgb.height());
    let stem = Path::new(img_path).file_stem().unwrap().to_string_lossy();
    let orig_bytes = std::fs::metadata(img_path).map(|m| m.len()).unwrap_or(0);

    // 1. segmentar + limpiar speckle
    let raw_mask = sauvola(&gray, win, k, 128.0);
    let mask = clean_mask(&raw_mask, w, h);
    let text_px = mask.iter().filter(|&&b| b).count();

    // 2. frente = media de los píxeles de texto (color oscuro constante)
    let mut acc = [0u64; 3];
    for (i, px) in rgb.pixels().enumerate() {
        if mask[i] {
            acc[0] += px.0[0] as u64;
            acc[1] += px.0[1] as u64;
            acc[2] += px.0[2] as u64;
        }
    }
    let fg = if text_px > 0 {
        [
            (acc[0] / text_px as u64) as u8,
            (acc[1] / text_px as u64) as u8,
            (acc[2] / text_px as u64) as u8,
        ]
    } else {
        [0, 0, 0]
    };

    // 3. máscara → G4 (+ tamaño Flate 1bpp de comparación)
    let g4 = g4_encode(&mask, w, h);
    let flate = flate_1bpp(&mask, w, h);
    let mask_bytes = if flate_mask {
        flate.clone()
    } else {
        g4.clone()
    };

    // 4. fondo downsampleado → JPEG
    let (bg_w, bg_h) = ((w / bg_div).max(1), (h / bg_div).max(1));
    let bg = image::imageops::resize(&rgb, bg_w, bg_h, image::imageops::FilterType::Triangle);
    let bg_jpeg = jpeg_rgb(&bg, 45);

    // 5. recomponer PDF
    let pdf = build_pdf(w, h, &bg_jpeg, bg_w, bg_h, &mask_bytes, flate_mask, fg);
    let mrc_total = mask_bytes.len() + bg_jpeg.len();

    // baselines: la imagen re-JPEG a q65 (≈ gema sin downsample) y a q40 (agresivo)
    let base_q65 = jpeg_rgb(&rgb, 65).len();
    let base_q40 = jpeg_rgb(&rgb, 40).len();

    std::fs::create_dir_all(out_dir).unwrap();
    let pdf_path = format!("{out_dir}/{stem}_mrc.pdf");
    std::fs::write(&pdf_path, &pdf).unwrap();
    // baseline renderizable: la imagen a q40 en un PDF de 1 imagen, para comparar a ojo
    std::fs::write(format!("{out_dir}/{stem}_base_q40.jpg"), jpeg_rgb(&rgb, 40)).unwrap();

    let mb = |b: usize| b as f64 / 1_048_576.0;
    println!(
        "### {stem}  {w}×{h}  ({:.1}% píxeles de texto)",
        100.0 * text_px as f64 / (w * h) as f64
    );
    println!("  original embebido : {:.2} MB", mb(orig_bytes as usize));
    println!("  baseline JPEG q65 : {:.2} MB", mb(base_q65));
    println!("  baseline JPEG q40 : {:.2} MB", mb(base_q40));
    println!(
        "  MRC total         : {:.2} MB   (máscara {} {:.2} MB + fondo/{} q45 {:.2} MB, frente=rgb{:?})",
        mb(mrc_total),
        if flate_mask { "Flate" } else { "G4" },
        mb(mask_bytes.len()),
        bg_div,
        mb(bg_jpeg.len()),
        fg,
    );
    println!(
        "     (máscara G4 {:.2} MB · Flate 1bpp {:.2} MB)",
        mb(g4.len()),
        mb(flate.len())
    );
    println!(
        "  MRC vs q65: {:.2}×   ·  MRC vs q40: {:.2}×",
        base_q65 as f64 / mrc_total as f64,
        base_q40 as f64 / mrc_total as f64,
    );
    println!("  → PDF: {pdf_path}");
}
