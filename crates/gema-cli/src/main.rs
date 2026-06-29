use clap::{Parser, Subcommand};
use gema_core::{analyze, compress, CompressOptions, Profile};
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
    },
    /// Analiza un PDF y muestra el reporte.
    Analyze { input: String },
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
        Cmd::Compress { input, output, profile } => {
            let bytes = fs::read(&input).map_err(|e| e.to_string())?;
            let profile = match profile.as_str() {
                "screen" => Profile::Screen,
                "ebook" => Profile::Ebook,
                "printer" => Profile::Printer,
                other => return Err(format!("perfil desconocido: {other} (usa screen|ebook|printer)")),
            };
            let res = compress(&bytes, &CompressOptions { profile, ..Default::default() })
                .map_err(|e| e.to_string())?;
            fs::write(&output, &res.output).map_err(|e| e.to_string())?;
            let r = res.report;
            // ratio = output/input → "% del original" (más bajo = más comprimido).
            println!(
                "{} → {}  ({:.1}% del original, {} imágenes)",
                bytes.len(),
                res.output.len(),
                r.ratio.unwrap_or(1.0) * 100.0,
                r.images.len()
            );
            Ok(())
        }
        Cmd::Analyze { input } => {
            let bytes = fs::read(&input).map_err(|e| e.to_string())?;
            let r = analyze(&bytes).map_err(|e| e.to_string())?;
            println!("páginas: {}\nfirmado: {}\ntamaño: {} bytes", r.pages, r.is_signed, r.original_size);
            Ok(())
        }
    }
}
