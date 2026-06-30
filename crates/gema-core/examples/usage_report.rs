//! Arnés de análisis de uso real.
//!
//! Corre `analyze` + `compress` sobre cada PDF pasado como argumento y emite una
//! línea CSV con métricas agregadas (sin contenido). Uso:
//!
//! ```sh
//! cargo run -p gema-core --example usage_report -- ebook archivo1.pdf archivo2.pdf ...
//! ```
//!
//! Primer argumento = perfil (screen|ebook|printer). El resto = rutas a PDFs.
//! No imprime nombres completos: solo el basename, para no filtrar rutas.

use gema_core::report::ImageAction;
use gema_core::{compress, CompressOptions, Profile};
use std::path::Path;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("uso: usage_report <perfil> <pdf...>");
        std::process::exit(2);
    }
    let profile = match args.remove(0).as_str() {
        "screen" => Profile::Screen,
        "printer" => Profile::Printer,
        _ => Profile::Ebook,
    };

    // CSV header
    println!("file,status,pages,signed,orig,out,ratio_pct,imgs,recompressed,downsampled,kept,skipped,warnings");

    let mut tot_orig: u64 = 0;
    let mut tot_out: u64 = 0;
    let (mut ok, mut signed_n, mut encrypted_n, mut parse_err, mut grew) = (0u32, 0u32, 0u32, 0u32, 0u32);
    let (mut a_recomp, mut a_down, mut a_kept, mut a_skip) = (0u32, 0u32, 0u32, 0u32);

    for path in &args {
        let name = Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "?".into());
        let safe = name.replace(',', "_");

        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                println!("{safe},io_error:{e},,,,,,,,,,");
                continue;
            }
        };

        match compress(&bytes, &CompressOptions { profile, ..Default::default() }) {
            Ok(res) => {
                let r = &res.report;
                let (mut rc, mut dn, mut kp, mut sk) = (0u32, 0u32, 0u32, 0u32);
                for st in &r.images {
                    match st.action {
                        ImageAction::Recompressed => rc += 1,
                        ImageAction::Downsampled => dn += 1,
                        ImageAction::Kept => kp += 1,
                        ImageAction::Skipped => sk += 1,
                    }
                }
                let orig = r.original_size;
                let out = r.output_size.unwrap_or(orig);
                let ratio = if orig > 0 { (out as f64 / orig as f64) * 100.0 } else { 100.0 };
                if out > orig {
                    grew += 1;
                }
                if r.is_signed {
                    signed_n += 1;
                }
                ok += 1;
                tot_orig += orig;
                tot_out += out;
                a_recomp += rc;
                a_down += dn;
                a_kept += kp;
                a_skip += sk;
                println!(
                    "{safe},ok,{},{},{},{},{:.1},{},{},{},{},{},{}",
                    r.pages, r.is_signed, orig, out, ratio,
                    r.images.len(), rc, dn, kp, sk, r.warnings.len()
                );
            }
            Err(e) => {
                let kind = format!("{e}");
                let tag = if kind.contains("contraseña") {
                    encrypted_n += 1;
                    "encrypted"
                } else {
                    parse_err += 1;
                    "parse_error"
                };
                println!("{safe},{tag},,,,,,,,,,,");
            }
        }
    }

    let agg_ratio = if tot_orig > 0 { (tot_out as f64 / tot_orig as f64) * 100.0 } else { 100.0 };
    eprintln!("\n===== RESUMEN =====");
    eprintln!("archivos:        {}", args.len());
    eprintln!("comprimidos ok:  {ok}");
    eprintln!("firmados:        {signed_n}  (Strict → se conserva original)");
    eprintln!("cifrados:        {encrypted_n}");
    eprintln!("parse_error:     {parse_err}");
    eprintln!("crecieron(piso): {grew}  (se devolvió el original)");
    eprintln!("bytes orig:      {tot_orig}");
    eprintln!("bytes out:       {tot_out}");
    eprintln!("ratio global:    {agg_ratio:.1}% del original");
    eprintln!("--- acciones de imagen (totales) ---");
    eprintln!("recomprimidas:   {a_recomp}");
    eprintln!("downsampled:     {a_down}   <-- clave para validar el caveat de DPI");
    eprintln!("kept (sin gano): {a_kept}");
    eprintln!("skipped:         {a_skip}");
}
