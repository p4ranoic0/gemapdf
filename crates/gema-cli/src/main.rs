use clap::{Parser, Subcommand};
use gema_compress::{analyze, compress, CompressOptions, Profile, SignaturePolicy};
use gema_edit::{RemovalResult, RemovalStatus, TextRegion};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "gema", about = "Compresión PDF GemaPDF")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Comprime un PDF.
    Compress {
        input: String,
        output: String,
        #[arg(long, default_value = "ebook")]
        profile: String,
        /// Override del DPI objetivo de imagen (gana al perfil). Para calibración.
        #[arg(long)]
        image_dpi: Option<u32>,
        /// Override de la calidad JPEG 1-100 (gana al perfil). Para calibración.
        #[arg(long)]
        jpeg_quality: Option<u8>,
        /// Modo perceptual: SSIM2 objetivo por imagen (0-100). Requiere el
        /// feature `perceptual` (habilitado por default en el CLI).
        #[arg(long)]
        quality_target: Option<f32>,
        /// DPI objetivo sólo para escaneos que llegan sin pérdida (Flate) y se
        /// transcodifican a JPEG. Para calibración.
        #[arg(long)]
        transcode_dpi: Option<u32>,
        /// Calidad JPEG sólo para esos transcodificados. Para calibración.
        #[arg(long)]
        transcode_quality: Option<u8>,
        /// Presupuesto aproximado para trabajo de imágenes simultáneo, en MiB.
        #[arg(long)]
        max_memory_mib: Option<u64>,
        /// Máximo de imágenes preparadas simultáneamente.
        #[arg(long)]
        max_parallel_images: Option<usize>,
        /// Máximo estimado decodificado por imagen, en MiB; si se supera, se omite.
        #[arg(long)]
        max_image_mib: Option<u64>,
        /// Deduplica imágenes byte-idénticas con semántica de render equivalente.
        #[arg(long)]
        dedupe_images: bool,
        /// Política para PDFs firmados. `strict` conserva el archivo intacto;
        /// `flatten` preserva la apariencia visual pero invalida la firma.
        #[arg(long, default_value = "flatten", value_parser = ["strict", "ignore", "flatten"])]
        signatures: String,
        /// Emite el reporte como JSON en stdout en vez de texto legible.
        #[arg(long)]
        json: bool,
        /// Con `--json`, incluye una entrada por imagen. Implica `--json`.
        #[arg(long)]
        json_images: bool,
    },
    /// Analiza un PDF y muestra el reporte.
    Analyze {
        input: String,
        /// Emite el reporte como JSON en stdout en vez de texto legible.
        #[arg(long)]
        json: bool,
    },
    /// Remove text glyphs inside one or more page regions.
    RemoveText {
        input: PathBuf,
        output: PathBuf,
        /// `[<id>@]<page>:<x>,<y>,<width>,<height>` (page is 1-based). Repeatable.
        #[arg(long = "region", value_name = "[ID@]PAGE:X,Y,W,H")]
        regions: Vec<String>,
        /// Print the versioned JSON report on stdout instead of the summary.
        #[arg(long, action = clap::ArgAction::Count)]
        json: u8,
    },
}

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let json_requested = is_remove_text(args.clone()) && json_mode(args);
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            if json_requested && e.use_stderr() {
                println!(
                    "{}",
                    serde_json::json!({"schema_version": gema_edit::REPORT_SCHEMA_VERSION, "ok": false, "exit_code": 1, "error": e.to_string()})
                );
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    match cli.cmd {
        Cmd::Compress {
            input,
            output,
            profile,
            image_dpi,
            jpeg_quality,
            quality_target,
            transcode_dpi,
            transcode_quality,
            max_memory_mib,
            max_parallel_images,
            max_image_mib,
            dedupe_images,
            signatures,
            json,
            json_images,
        } => {
            let bytes = fs::read(&input).map_err(|e| e.to_string())?;
            // El mapeo nombre → enum vive en core (`FromStr`), así que la CLI y
            // el binding wasm no pueden divergir en los nombres aceptados.
            let profile: Profile = profile.parse()?;
            let signatures: SignaturePolicy = signatures.parse()?;
            let defaults = CompressOptions::default();
            let res = compress(
                &bytes,
                &CompressOptions {
                    profile,
                    image_dpi,
                    jpeg_quality,
                    quality_target,
                    transcode_dpi,
                    transcode_quality,
                    max_memory_bytes: max_memory_mib
                        .map(|mib| mib.saturating_mul(1024 * 1024))
                        .or(defaults.max_memory_bytes),
                    max_parallel_images,
                    max_image_bytes: max_image_mib.map(|mib| mib.saturating_mul(1024 * 1024)),
                    dedupe_images,
                    signatures,
                    ..defaults
                },
            )
            .map_err(|e| e.to_string())?;
            fs::write(&output, &res.output).map_err(|e| e.to_string())?;
            let r = res.report;
            if json || json_images {
                // stdout es JSON puro: cualquier aviso va a stderr.
                let view = if json_images {
                    gema_compress::ReportJson::from_report_with_images(&r, Some(signatures))
                } else {
                    gema_compress::ReportJson::from_report(&r, Some(signatures))
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&view).map_err(|e| e.to_string())?
                );
                return Ok(ExitCode::SUCCESS);
            }
            // ratio = output/input → "% del original" (más bajo = más comprimido).
            println!(
                "{} → {}  ({:.1}% del original, {} imágenes)",
                bytes.len(),
                res.output.len(),
                r.ratio.unwrap_or(1.0) * 100.0,
                r.images.len()
            );
            if r.is_signed {
                let message = match signatures {
                    SignaturePolicy::Strict => {
                        "documento firmado conservado intacto; no se aplicó compresión"
                    }
                    SignaturePolicy::Ignore => {
                        "el documento se modificó y su firma criptográfica perdió validez"
                    }
                    SignaturePolicy::Flatten => {
                        "la apariencia de la firma se conservó como contenido visual; su validez criptográfica se perdió"
                    }
                };
                println!("aviso: {message}");
            }
            if r.deduplicated_images > 0 {
                println!(
                    "deduplicación adicional: {} imágenes, {} bytes de streams redundantes",
                    r.deduplicated_images, r.deduplicated_image_bytes
                );
            }
            for skipped in &r.image_skip_summary {
                println!(
                    "omitidas [{}]: {} imágenes, {} bytes de entrada",
                    skipped.reason.as_str(),
                    skipped.images,
                    skipped.original_bytes
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Analyze { input, json } => {
            let bytes = fs::read(&input).map_err(|e| e.to_string())?;
            let r = analyze(&bytes).map_err(|e| e.to_string())?;
            if json {
                // `analyze` no aplica política de firmas: el campo va ausente.
                let view = gema_compress::ReportJson::from_report(&r, None);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&view).map_err(|e| e.to_string())?
                );
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "páginas: {}\nfirmado: {}\npáginas escaneadas (estimación): {}\ntamaño: {} bytes",
                r.pages, r.is_signed, r.has_scanned_pages, r.original_size
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::RemoveText {
            input,
            output,
            regions,
            json,
        } => remove_text(&input, &output, &regions, json > 0),
    }
}

fn parse_region(spec: &str, index: usize) -> Result<TextRegion, String> {
    let bad = |why: &str| format!("--region {spec}: {why}");
    let (id, rest) = match spec.split_once('@') {
        Some((id, rest)) => (id.to_string(), rest),
        None => (format!("r{}", index + 1), spec),
    };
    if id.is_empty() {
        return Err(bad("the id before '@' is empty"));
    }
    let (page, dims) = rest
        .split_once(':')
        .ok_or_else(|| bad("expected PAGE:X,Y,W,H"))?;
    let page: u32 = page
        .trim()
        .parse()
        .map_err(|_| bad("page must be an integer"))?;
    if page < 1 {
        return Err(bad("pages start at 1"));
    }
    let parts: Vec<&str> = dims.split(',').collect();
    if parts.len() != 4 {
        return Err(bad("expected four measures X,Y,W,H"));
    }
    let mut v = [0f64; 4];
    for (i, p) in parts.iter().enumerate() {
        v[i] = p
            .trim()
            .parse::<f64>()
            .map_err(|_| bad("measures must be numbers"))?;
        if !v[i].is_finite() {
            return Err(bad("measures must be finite"));
        }
    }
    if v[2] <= 0.0 || v[3] <= 0.0 {
        return Err(bad("width and height must be > 0"));
    }
    Ok(TextRegion {
        id,
        page: page - 1,
        x: v[0],
        y: v[1],
        width: v[2],
        height: v[3],
    })
}

fn parse_regions(specs: &[String]) -> Result<Vec<TextRegion>, String> {
    if specs.is_empty() {
        return Err("at least one --region is required".into());
    }
    let mut out = Vec::with_capacity(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        let r = parse_region(spec, i)?;
        if out.iter().any(|o: &TextRegion| o.id == r.id) {
            return Err(format!("--region {spec}: duplicate id '{}'", r.id));
        }
        out.push(r);
    }
    Ok(out)
}

fn json_mode<I>(args: I) -> bool
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg == "--json" {
            return true;
        }
    }
    false
}

fn is_remove_text<I>(args: I) -> bool
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    args.into_iter()
        .find(|a| !a.to_string_lossy().starts_with('-'))
        .is_some_and(|a| a == "remove-text")
}

fn exit_code_for(result: &RemovalResult) -> (u8, Vec<&'static str>) {
    let mut reasons = Vec::new();
    if result.regions.iter().any(|r| {
        !matches!(
            r.status,
            RemovalStatus::Removed | RemovalStatus::NothingFound
        )
    }) {
        reasons.push("region_status");
    }
    if !result.residual_risks.is_empty() {
        reasons.push("residual_risks");
    }
    if result.inspection_incomplete {
        reasons.push("inspection_incomplete");
    }
    if result.signature.any() && result.modified {
        reasons.push("signature_modified");
    }
    (if reasons.is_empty() { 0 } else { 2 }, reasons)
}

#[derive(serde::Serialize)]
struct CliReport<'a> {
    #[serde(flatten)]
    report: gema_edit::RemovalReport<'a>,
    ok: bool,
    output: String,
    exit_code: u8,
    not_guaranteed_because: Vec<&'static str>,
}

fn remove_text(
    input: &Path,
    output: &Path,
    specs: &[String],
    json: bool,
) -> Result<ExitCode, String> {
    let json_error = |msg: &str| {
        if json {
            println!(
                "{}",
                serde_json::json!({"schema_version": gema_edit::REPORT_SCHEMA_VERSION, "ok": false, "exit_code": 1, "error": msg})
            );
        }
        msg.to_string()
    };
    let regions = parse_regions(specs).map_err(|e| json_error(&e))?;
    let bytes = fs::read(input)
        .map_err(|e| json_error(&format!("cannot read {}: {e}", input.display())))?;
    let result =
        gema_edit::remove_text_glyphs(&bytes, &regions).map_err(|e| json_error(&e.to_string()))?;
    let (code, reasons) = exit_code_for(&result);
    let rendered = if json {
        serde_json::to_string_pretty(&CliReport {
            report: result.report(),
            ok: true,
            output: output.display().to_string(),
            exit_code: code,
            not_guaranteed_because: reasons.clone(),
        })
        .map_err(|e| json_error(&format!("cannot serialize the report: {e}")))?
    } else {
        human_summary(&result, output)
    };
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    use std::fs::OpenOptions;
    use std::io::Write;
    let mut temporary = None;
    for attempt in 0..16u32 {
        let candidate = parent.join(format!(
            ".{}.gema-tmp-{}-{attempt}",
            output
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("output"),
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(&result.output) {
                    let _ = fs::remove_file(&candidate);
                    return Err(json_error(&format!(
                        "cannot write {}: {e}",
                        output.display()
                    )));
                }
                temporary = Some(candidate);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(json_error(&format!("cannot create temporary output: {e}"))),
        }
    }
    let temporary =
        temporary.ok_or_else(|| json_error("cannot create a unique temporary output"))?;
    if let Err(e) = fs::rename(&temporary, output) {
        let _ = fs::remove_file(&temporary);
        return Err(json_error(&format!(
            "cannot rename temporary output to {}: {e}",
            output.display()
        )));
    }
    let mut stdout = std::io::stdout().lock();
    if let Err((code, message)) = report_exit(&mut stdout, &rendered, output) {
        eprintln!("{message}");
        return Ok(code);
    }
    Ok(if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(code)
    })
}

fn print_report(writer: &mut impl std::io::Write, rendered: &str) -> std::io::Result<()> {
    writeln!(writer, "{rendered}")?;
    writer.flush()
}

fn report_exit(
    writer: &mut impl std::io::Write,
    rendered: &str,
    output: &Path,
) -> Result<(), (ExitCode, String)> {
    print_report(writer, rendered).map_err(|e| {
        (
            ExitCode::from(3),
            format!(
                "error: the PDF was written to {} but the report could not be printed: {e}",
                output.display()
            ),
        )
    })
}

fn human_summary(result: &RemovalResult, output: &Path) -> String {
    let mut lines = Vec::new();
    for r in &result.regions {
        let mut line = format!(
            "region {} (page {}): {}",
            r.id,
            r.page + 1,
            r.status.as_str()
        );
        if r.removed_glyphs > 0 {
            line.push_str(&format!(", {} glyphs", r.removed_glyphs));
        }
        lines.push(line);
    }
    for risk in &result.residual_risks {
        lines.push(format!("risk: {} (page {})", risk.kind(), risk.page() + 1));
    }
    for gap in &result.inspection_gaps {
        let page = gap
            .page
            .map(|p| format!(" (page {})", p + 1))
            .unwrap_or_default();
        lines.push(format!(
            "inspection gap: {}{page} {}",
            gap.reason.as_str(),
            gap.detail
        ));
    }
    if result.signature.any() {
        let s = &result.signature;
        let mut which = Vec::new();
        if s.sig_flags {
            which.push("sig_flags");
        }
        if s.sig_field {
            which.push("sig_field");
        }
        if s.perms {
            which.push("perms");
        }
        let verb = if result.modified {
            "document modified"
        } else {
            "document untouched"
        };
        lines.push(format!(
            "signature: indicators found ({}), {verb}",
            which.join(", ")
        ));
    }
    let (_, reasons) = exit_code_for(result);
    if !reasons.is_empty() {
        lines.push(format!("not guaranteed: {}", reasons.join(", ")));
    }
    lines.push(format!("wrote {}", output.display()));
    lines.join("\n")
}

#[cfg(test)]
mod output_tests {
    use super::report_exit;
    use std::io::{self, Write};
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
    }
    #[test]
    fn report_print_failure_is_distinguishable_from_edit_failure() {
        let err =
            report_exit(&mut FailingWriter, "{}", std::path::Path::new("out.pdf")).unwrap_err();
        assert_eq!(err.0, std::process::ExitCode::from(3));
        assert_eq!(
            err.1,
            "error: the PDF was written to out.pdf but the report could not be printed: closed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_cli_defaults_to_flatten_signatures() {
        let cli = Cli::try_parse_from(["gema", "compress", "in.pdf", "out.pdf"]).unwrap();
        let Cmd::Compress { signatures, .. } = cli.cmd else {
            panic!("se esperaba el subcomando compress");
        };
        assert_eq!(signatures, "flatten");
    }

    #[test]
    fn compress_cli_accepts_all_signature_policies() {
        for policy in ["strict", "ignore", "flatten"] {
            let cli = Cli::try_parse_from([
                "gema",
                "compress",
                "in.pdf",
                "out.pdf",
                "--signatures",
                policy,
            ])
            .unwrap();
            let Cmd::Compress { signatures, .. } = cli.cmd else {
                panic!("se esperaba el subcomando compress");
            };
            assert_eq!(signatures, policy);
        }
    }

    #[test]
    fn compress_cli_rejects_unknown_signature_policy() {
        assert!(Cli::try_parse_from([
            "gema",
            "compress",
            "in.pdf",
            "out.pdf",
            "--signatures",
            "aggressive",
        ])
        .is_err());
    }

    #[test]
    fn compress_cli_accepts_memory_limits() {
        let cli = Cli::try_parse_from([
            "gema",
            "compress",
            "in.pdf",
            "out.pdf",
            "--max-memory-mib",
            "256",
            "--max-parallel-images",
            "2",
            "--max-image-mib",
            "128",
        ])
        .unwrap();
        let Cmd::Compress {
            max_memory_mib,
            max_parallel_images,
            max_image_mib,
            ..
        } = cli.cmd
        else {
            panic!("se esperaba el subcomando compress");
        };
        assert_eq!(max_memory_mib, Some(256));
        assert_eq!(max_parallel_images, Some(2));
        assert_eq!(max_image_mib, Some(128));
    }

    #[test]
    fn compress_cli_accepts_image_deduplication() {
        let cli = Cli::try_parse_from(["gema", "compress", "in.pdf", "out.pdf", "--dedupe-images"])
            .unwrap();
        let Cmd::Compress { dedupe_images, .. } = cli.cmd else {
            panic!("se esperaba el subcomando compress");
        };
        assert!(dedupe_images);
    }

    #[test]
    fn parses_id_page_and_measures_with_page_base_one() {
        let r = parse_region("hdr@2:10,20.5,100,30", 0).unwrap();
        assert_eq!(
            (r.id.as_str(), r.page, r.x, r.y, r.width, r.height),
            ("hdr", 1, 10.0, 20.5, 100.0, 30.0)
        );
        let r = parse_region("1:0,0,612,792", 4).unwrap();
        assert_eq!((r.id.as_str(), r.page), ("r5", 0));
    }

    #[test]
    fn rejects_malformed_regions() {
        for bad in [
            "@1:0,0,1,1",
            "0:0,0,1,1",
            "x:0,0,1,1",
            "1:0,0,1",
            "1:0,0,1,1,1",
            "1:0,0,0,1",
            "1:0,0,1,-1",
            "1:nan,0,1,1",
            "1:inf,0,1,1",
            "1:0,0,1,1e400",
        ] {
            assert!(parse_region(bad, 0).is_err(), "aceptó {bad}");
        }
    }

    #[test]
    fn rejects_empty_and_duplicate_ids() {
        assert!(parse_regions(&[]).is_err());
        assert!(parse_regions(&["a@1:0,0,1,1".into(), "a@1:5,5,1,1".into()]).is_err());
        assert_eq!(
            parse_regions(&["a@1:0,0,1,1".into(), "1:5,5,1,1".into()])
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn json_mode_stops_at_double_dash_and_accepts_repetition() {
        assert!(json_mode(
            ["gema", "--json", "--json"].into_iter().map(Into::into)
        ));
        assert!(json_mode(
            ["gema", "--region", "--json"].into_iter().map(Into::into)
        ));
        assert!(!json_mode(
            ["gema", "--", "--json"].into_iter().map(Into::into)
        ));
    }

    #[test]
    fn json_error_mode_is_only_for_remove_text() {
        let args = |v: &[&str]| {
            v.iter()
                .map(|s| std::ffi::OsString::from(*s))
                .collect::<Vec<_>>()
        };
        assert!(is_remove_text(args(&["remove-text", "in.pdf", "out.pdf"])));
        assert!(!is_remove_text(args(&[
            "compress", "in.pdf", "out.pdf", "--json"
        ])));
        assert!(!is_remove_text(args(&["--json"])));
    }
}
