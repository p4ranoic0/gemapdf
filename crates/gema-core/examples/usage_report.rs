//! Arnés de análisis de uso real.
//!
//! Corre `analyze` + `compress` sobre cada PDF pasado como argumento y emite una
//! línea CSV con métricas agregadas (sin contenido). Uso:
//!
//! ```sh
//! cargo run -p gema-core --features telemetry --example usage_report -- ebook a.pdf b.pdf
//! ```
//!
//! Primer argumento = perfil (screen|ebook|printer). El resto = rutas a PDFs.
//! No imprime nombres completos: solo el basename, para no filtrar rutas.

use gema_core::{
    compress,
    telemetry::{structural_telemetry, StructuralTelemetry},
    CompressOptions, ImageAction, ImageSkipReason, Profile,
};
use std::collections::BTreeMap;
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
    println!(
        "file,status,pages,signed,orig,out,ratio_pct,imgs,recompressed,downsampled,kept,skipped,preserved,warnings,inline_images_count,inline_images_data_bytes,inline_images_pages,images_only_in_forms_count,images_only_in_forms_encoded_bytes,images_only_in_forms_max_depth,extgstate_soft_masks_count,extgstate_soft_masks_pages,uninspectable_resources"
    );

    let mut tot_orig: u64 = 0;
    let mut tot_out: u64 = 0;
    let (mut ok, mut signed_n, mut encrypted_n, mut parse_err, mut telemetry_err, mut grew) =
        (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
    let (mut a_recomp, mut a_down, mut a_kept, mut a_skip, mut a_preserved) =
        (0u32, 0u32, 0u32, 0u32, 0u32);
    let mut skip_totals: BTreeMap<ImageSkipReason, (usize, u64)> = BTreeMap::new();
    let mut telemetry_totals = StructuralTelemetry::default();

    for path in &args {
        let name = Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "?".into());
        let safe = name.replace(',', "_");

        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                print_error(&safe, &format!("io_error:{e}"));
                continue;
            }
        };

        match compress(
            &bytes,
            &CompressOptions {
                profile,
                ..Default::default()
            },
        ) {
            Ok(res) => {
                let telemetry = match structural_telemetry(&bytes) {
                    Ok(telemetry) => telemetry,
                    Err(e) => {
                        telemetry_err = telemetry_err.saturating_add(1);
                        print_error(&safe, &format!("telemetry_error:{e}"));
                        continue;
                    }
                };
                let r = &res.report;
                let (mut rc, mut dn, mut kp, mut sk, mut pv) = (0u32, 0u32, 0u32, 0u32, 0u32);
                for st in &r.images {
                    match st.action {
                        ImageAction::Recompressed => rc = rc.saturating_add(1),
                        ImageAction::Downsampled => dn = dn.saturating_add(1),
                        ImageAction::Kept => kp = kp.saturating_add(1),
                        ImageAction::Skipped => sk = sk.saturating_add(1),
                        ImageAction::Preserved => pv = pv.saturating_add(1),
                        // `ImageAction` es `#[non_exhaustive]`.
                        _ => {}
                    }
                }
                let orig = r.original_size;
                let out = r.output_size.unwrap_or(orig);
                let ratio = if orig > 0 {
                    (out as f64 / orig as f64) * 100.0
                } else {
                    100.0
                };
                if out > orig {
                    grew = grew.saturating_add(1);
                }
                if r.is_signed {
                    signed_n = signed_n.saturating_add(1);
                }
                ok = ok.saturating_add(1);
                tot_orig = tot_orig.saturating_add(orig);
                tot_out = tot_out.saturating_add(out);
                a_recomp = a_recomp.saturating_add(rc);
                a_down = a_down.saturating_add(dn);
                a_kept = a_kept.saturating_add(kp);
                a_skip = a_skip.saturating_add(sk);
                a_preserved = a_preserved.saturating_add(pv);
                telemetry_totals.inline_images.count = telemetry_totals
                    .inline_images
                    .count
                    .saturating_add(telemetry.inline_images.count);
                telemetry_totals.inline_images.data_bytes = telemetry_totals
                    .inline_images
                    .data_bytes
                    .saturating_add(telemetry.inline_images.data_bytes);
                telemetry_totals.inline_images.pages = telemetry_totals
                    .inline_images
                    .pages
                    .saturating_add(telemetry.inline_images.pages);
                telemetry_totals.images_only_in_forms.count = telemetry_totals
                    .images_only_in_forms
                    .count
                    .saturating_add(telemetry.images_only_in_forms.count);
                telemetry_totals.images_only_in_forms.encoded_bytes = telemetry_totals
                    .images_only_in_forms
                    .encoded_bytes
                    .saturating_add(telemetry.images_only_in_forms.encoded_bytes);
                telemetry_totals.images_only_in_forms.max_depth = telemetry_totals
                    .images_only_in_forms
                    .max_depth
                    .max(telemetry.images_only_in_forms.max_depth);
                telemetry_totals.extgstate_soft_masks.count = telemetry_totals
                    .extgstate_soft_masks
                    .count
                    .saturating_add(telemetry.extgstate_soft_masks.count);
                telemetry_totals.extgstate_soft_masks.pages = telemetry_totals
                    .extgstate_soft_masks
                    .pages
                    .saturating_add(telemetry.extgstate_soft_masks.pages);
                telemetry_totals.uninspectable_resources = telemetry_totals
                    .uninspectable_resources
                    .saturating_add(telemetry.uninspectable_resources);
                for summary in &r.image_skip_summary {
                    let entry = skip_totals.entry(summary.reason).or_insert((0, 0));
                    entry.0 = entry.0.saturating_add(summary.images);
                    entry.1 = entry.1.saturating_add(summary.original_bytes);
                }
                println!(
                    "{safe},ok,{},{},{},{},{:.1},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                    r.pages,
                    r.is_signed,
                    orig,
                    out,
                    ratio,
                    r.images.len(),
                    rc,
                    dn,
                    kp,
                    sk,
                    pv,
                    r.warnings.len(),
                    telemetry.inline_images.count,
                    telemetry.inline_images.data_bytes,
                    telemetry.inline_images.pages,
                    telemetry.images_only_in_forms.count,
                    telemetry.images_only_in_forms.encoded_bytes,
                    telemetry.images_only_in_forms.max_depth,
                    telemetry.extgstate_soft_masks.count,
                    telemetry.extgstate_soft_masks.pages,
                    telemetry.uninspectable_resources
                );
            }
            Err(e) => {
                let kind = format!("{e}");
                let tag = if kind.contains("contraseña") {
                    encrypted_n = encrypted_n.saturating_add(1);
                    "encrypted"
                } else {
                    parse_err = parse_err.saturating_add(1);
                    "parse_error"
                };
                print_error(&safe, tag);
            }
        }
    }

    let agg_ratio = if tot_orig > 0 {
        (tot_out as f64 / tot_orig as f64) * 100.0
    } else {
        100.0
    };
    eprintln!("\n===== RESUMEN =====");
    eprintln!("archivos:        {}", args.len());
    eprintln!("comprimidos ok:  {ok}");
    eprintln!("firmados:        {signed_n}  (Strict → se conserva original)");
    eprintln!("cifrados:        {encrypted_n}");
    eprintln!("parse_error:     {parse_err}");
    eprintln!("telemetry_error: {telemetry_err}");
    eprintln!("crecieron(piso): {grew}  (se devolvió el original)");
    eprintln!("bytes orig:      {tot_orig}");
    eprintln!("bytes out:       {tot_out}");
    eprintln!("ratio global:    {agg_ratio:.1}% del original");
    eprintln!("--- acciones de imagen (totales) ---");
    eprintln!("recomprimidas:   {a_recomp}");
    eprintln!("downsampled:     {a_down}   <-- clave para validar el caveat de DPI");
    eprintln!("kept (sin gano): {a_kept}");
    eprintln!("skipped:         {a_skip}");
    eprintln!("preservadas:     {a_preserved}   <-- firmas/sellos preservados byte-idénticos");
    eprintln!("--- oportunidades omitidas por motivo ---");
    for (reason, (images, bytes)) in skip_totals {
        eprintln!(
            "{:<28} {:>6} imágenes  {:>12} bytes",
            reason.as_str(),
            images,
            bytes
        );
    }
    eprintln!("--- telemetría estructural (totales) ---");
    eprintln!(
        "inline images:       {}  {} bytes  {} páginas",
        telemetry_totals.inline_images.count,
        telemetry_totals.inline_images.data_bytes,
        telemetry_totals.inline_images.pages
    );
    eprintln!(
        "images only forms:   {}  {} bytes  profundidad máxima {}",
        telemetry_totals.images_only_in_forms.count,
        telemetry_totals.images_only_in_forms.encoded_bytes,
        telemetry_totals.images_only_in_forms.max_depth
    );
    eprintln!(
        "ExtGState SMask:     {}  {} páginas",
        telemetry_totals.extgstate_soft_masks.count, telemetry_totals.extgstate_soft_masks.pages
    );
    eprintln!(
        "recursos no inspeccionables: {}",
        telemetry_totals.uninspectable_resources
    );
}

fn print_error(file: &str, status: &str) {
    const EMPTY_COLUMNS: &str = ",,,,,,,,,,,,,,,,,,,,,";
    println!("{file},{status}{EMPTY_COLUMNS}");
}
