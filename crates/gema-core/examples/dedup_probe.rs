//! Sonda: mide cuánto recuperaría deduplicar streams byte-idénticos en un PDF.
//! Uso: cargo run --release -p gema-core --example dedup_probe -- <pdf...>
//!
//! Un merge de documentos suele embeber la MISMA fuente (o imagen) muchas veces
//! como objetos separados. Si su contenido de stream es byte-idéntico, se pueden
//! colapsar a un solo objeto. Esta sonda cuantifica ese potencial SIN mutar nada.

use lopdf::{Document, Object};
use std::collections::HashMap;

fn is_font_stream(s: &lopdf::Stream) -> bool {
    // Un programa de fuente embebido: dict con /Length1 (Type1) o /Subtype de
    // fuente (Type1C/CIDFontType0C/OpenType), típico de FontFile/2/3.
    if s.dict.has(b"Length1") {
        return true;
    }
    matches!(
        s.dict.get(b"Subtype").and_then(|o| o.as_name()),
        Ok(b"Type1C") | Ok(b"CIDFontType0C") | Ok(b"OpenType")
    )
}

fn probe(path: &str) {
    let doc = match Document::load(path) {
        Ok(d) => d,
        Err(e) => {
            println!("{path}: error al cargar: {e}");
            return;
        }
    };
    // content byte-idéntico -> (count, size, algún_es_fuente)
    let mut map: HashMap<Vec<u8>, (usize, usize, bool)> = HashMap::new();
    let mut n_streams = 0usize;
    let mut total_bytes = 0usize;
    for (_, obj) in doc.objects.iter() {
        if let Object::Stream(s) = obj {
            n_streams += 1;
            let size = s.content.len();
            total_bytes += size;
            let is_font = is_font_stream(s);
            let e = map.entry(s.content.clone()).or_insert((0, size, false));
            e.0 += 1;
            e.2 = e.2 || is_font;
        }
    }
    let (mut dup_groups, mut redundant, mut redundant_font) = (0usize, 0usize, 0usize);
    for (count, size, is_font) in map.values() {
        if *count > 1 {
            dup_groups += 1;
            let r = (count - 1) * size;
            redundant += r;
            if *is_font {
                redundant_font += r;
            }
        }
    }
    let mb = |b: usize| b as f64 / 1_048_576.0;
    let name = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    println!("=== {} ===", &name[..name.len().min(40)]);
    println!(
        "  streams: {n_streams} ({:.2} MB)  ·  grupos duplicados: {dup_groups}",
        mb(total_bytes)
    );
    println!(
        "  REDUNDANTE (dedup recuperaría): {:.2} MB  (de fuentes: {:.2} MB)",
        mb(redundant),
        mb(redundant_font)
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("uso: dedup_probe <pdf...>");
        std::process::exit(2);
    }
    for p in &args {
        probe(p);
    }
}
