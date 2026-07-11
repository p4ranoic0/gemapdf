//! Scratch de diagnóstico fino: desestructura el resultado de compresión por
//! bucket de acción (peso de entrada Y de salida) y clasifica la CAUSA de cada
//! imagen Skipped/Kept cruzando el reporte con el dict original.
//!
//! Uso: diag_buckets <dpi> <quality> <pdf...>

use gema_core::{compress, CompressOptions, ImageAction};
use lopdf::{Document, Object};
use std::collections::HashMap;
use std::path::Path;

/// Describe la cadena /Filter como string corto ("DCT", "Flate", "LZW+Flate"…).
fn filter_desc(dict: &lopdf::Dictionary) -> String {
    let short = |n: &[u8]| -> String {
        match n {
            b"DCTDecode" => "DCT".into(),
            b"FlateDecode" => "Flate".into(),
            b"JPXDecode" => "JPX".into(),
            b"CCITTFaxDecode" => "CCITT".into(),
            b"JBIG2Decode" => "JBIG2".into(),
            b"LZWDecode" => "LZW".into(),
            other => String::from_utf8_lossy(other).into_owned(),
        }
    };
    match dict.get(b"Filter") {
        Ok(Object::Name(n)) => short(n),
        Ok(Object::Array(a)) => a
            .iter()
            .filter_map(|o| o.as_name().ok())
            .map(short)
            .collect::<Vec<_>>()
            .join("+"),
        _ => "none".into(),
    }
}

/// Describe el /ColorSpace resuelto (1 salto de indirección) como string corto.
fn cs_desc(doc: &Document, dict: &lopdf::Dictionary) -> String {
    let resolved: Option<&Object> = match dict.get(b"ColorSpace") {
        Ok(Object::Reference(id)) => doc.get_object(*id).ok(),
        Ok(o) => Some(o),
        Err(_) => None,
    };
    match resolved {
        Some(Object::Name(n)) => String::from_utf8_lossy(n).into_owned(),
        Some(Object::Array(a)) => {
            let head = a
                .first()
                .and_then(|o| o.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_else(|| "?".into());
            // Para ICCBased añadimos el /N del stream del perfil.
            if head == "ICCBased" {
                let n = a
                    .get(1)
                    .and_then(|o| o.as_reference().ok())
                    .and_then(|id| doc.get_object(id).ok())
                    .and_then(|o| o.as_stream().ok())
                    .and_then(|s| s.dict.get(b"N").ok())
                    .and_then(|o| o.as_i64().ok());
                format!("ICC(N={})", n.map_or("?".into(), |v| v.to_string()))
            } else {
                head
            }
        }
        _ => "none".into(),
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("uso: diag_buckets <dpi> <quality> <pdf...>");
        std::process::exit(2);
    }
    let dpi: u32 = args.remove(0).parse().expect("dpi");
    let q: u8 = args.remove(0).parse().expect("quality");

    for path in &args {
        let name = Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let bytes = std::fs::read(path).expect("leer pdf");
        let doc = Document::load_mem(&bytes).expect("parsear pdf");
        let res = compress(
            &bytes,
            &CompressOptions {
                image_dpi: Some(dpi),
                jpeg_quality: Some(q),
                ..Default::default()
            },
        )
        .expect("compress");

        let mb = |b: u64| b as f64 / 1_048_576.0;
        println!(
            "\n### {name} @ {dpi}dpi/q{q}: {:.2} -> {:.2} MB ({:.1}%)",
            mb(bytes.len() as u64),
            mb(res.output.len() as u64),
            res.output.len() as f64 / bytes.len() as f64 * 100.0
        );

        // --- buckets con peso de entrada y salida ---
        let mut buck: HashMap<&'static str, (u32, u64, u64)> = HashMap::new();
        for st in &res.report.images {
            let key = match st.action {
                ImageAction::Recompressed => "recompressed",
                ImageAction::Downsampled => "downsampled",
                ImageAction::Kept => "kept",
                ImageAction::Skipped => "skipped",
                ImageAction::Preserved => "preserved",
            };
            let e = buck.entry(key).or_default();
            e.0 += 1;
            e.1 += st.original_bytes;
            e.2 += st.output_bytes;
        }
        println!(
            "  {:<14} {:>5} {:>10} {:>10}",
            "bucket", "n", "in_MB", "out_MB"
        );
        for k in [
            "recompressed",
            "downsampled",
            "kept",
            "skipped",
            "preserved",
        ] {
            if let Some((n, i, o)) = buck.get(k) {
                println!("  {:<14} {:>5} {:>10.2} {:>10.2}", k, n, mb(*i), mb(*o));
            }
        }

        // --- causas de skip, por peso ---
        let mut causes: HashMap<String, (u32, u64)> = HashMap::new();
        // --- kept: experimento de reintento a q-15 sobre los más grandes ---
        let mut kept: Vec<(u64, lopdf::ObjectId)> = Vec::new();

        for st in &res.report.images {
            let id = (st.object_id, 0u16);
            let Ok(stream) = doc.get_object(id).and_then(|o| o.as_stream()) else {
                continue;
            };
            match st.action {
                ImageAction::Skipped => {
                    let f = filter_desc(&stream.dict);
                    let cs = cs_desc(&doc, &stream.dict);
                    let smask = stream.dict.has(b"SMask");
                    let cause = if smask {
                        format!("SMask [{f}/{cs}]")
                    } else if f == "DCT" {
                        match image::load_from_memory(&stream.content) {
                            Ok(_) => format!("DCT abre OK pero skip [{cs}] (encode/otro)"),
                            Err(e) => {
                                let es = e.to_string();
                                let short = es.split(':').next_back().unwrap_or(&es).trim();
                                format!("DCT no abre [{cs}]: {short}")
                            }
                        }
                    } else {
                        format!("{f} [{cs}]")
                    };
                    let e = causes.entry(cause).or_default();
                    e.0 += 1;
                    e.1 += st.original_bytes;
                }
                ImageAction::Kept => kept.push((st.original_bytes, id)),
                _ => {}
            }
        }

        if !causes.is_empty() {
            println!("  -- causas de skip --");
            let mut v: Vec<_> = causes.into_iter().collect();
            v.sort_by_key(|(_, (_, b))| std::cmp::Reverse(*b));
            for (cause, (n, b)) in v {
                println!("    {:<48} {:>4} imgs {:>8.2} MB", cause, n, mb(b));
            }
        }

        // reintento: decodifica los 8 kept más pesados y prueba q y q-15
        kept.sort_by_key(|(b, _)| std::cmp::Reverse(*b));
        if !kept.is_empty() {
            println!(
                "  -- kept: reintento de encode (top {}) --",
                kept.len().min(8)
            );
            let (mut would_q, mut would_q15, mut stuck) = (0u64, 0u64, 0u64);
            for (orig, id) in kept.iter().take(8) {
                let Ok(stream) = doc.get_object(*id).and_then(|o| o.as_stream()) else {
                    continue;
                };
                let Ok(img) = image::load_from_memory(&stream.content) else {
                    continue;
                };
                let enc = |quality: u8| -> Option<usize> {
                    let mut out = Vec::new();
                    let rgb = img.to_rgb8();
                    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
                        .encode(
                            rgb.as_raw(),
                            rgb.width(),
                            rgb.height(),
                            image::ExtendedColorType::Rgb8,
                        )
                        .ok()?;
                    Some(out.len())
                };
                let at_q = enc(q).unwrap_or(usize::MAX) as u64;
                let at_q15 = enc(q.saturating_sub(15).max(1)).unwrap_or(usize::MAX) as u64;
                if at_q < *orig {
                    would_q += orig - at_q;
                } else if at_q15 < *orig {
                    would_q15 += orig - at_q15;
                } else {
                    stuck += *orig;
                }
            }
            println!(
                "    ganaría a q{q}: {:.2} MB | ganaría a q{}: {:.2} MB | atascado: {:.2} MB",
                mb(would_q),
                q.saturating_sub(15).max(1),
                mb(would_q15),
                mb(stuck)
            );
        }
    }
}
