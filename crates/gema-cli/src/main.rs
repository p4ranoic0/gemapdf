use clap::{Parser, Subcommand};
use gema_compress::{analyze, compress, CompressOptions, Profile, SignaturePolicy};
use std::fs;
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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
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
                return Ok(());
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
            Ok(())
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
                return Ok(());
            }
            println!(
                "páginas: {}\nfirmado: {}\npáginas escaneadas (estimación): {}\ntamaño: {} bytes",
                r.pages, r.is_signed, r.has_scanned_pages, r.original_size
            );
            Ok(())
        }
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
}
