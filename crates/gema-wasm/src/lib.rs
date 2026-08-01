use gema_core::{
    compress as core_compress, compress_with_progress, CompressOptions, ImageAction, Phase,
    Profile, Report, SignaturePolicy,
};
use serde_wasm_bindgen::Serializer;
use wasm_bindgen::prelude::*;

/// Serializa a `JsValue` como objeto JS plano (`{ key: value }`), no como
/// `Map`. `serde_wasm_bindgen::to_value` por defecto produce `Map` para
/// structs, lo cual rompe el acceso `obj.campo` esperado por JS/TS
/// consumidores; `json_compatible()` fuerza la forma "objeto llano" que
/// coincide con `JSON.parse(JSON.stringify(x))`.
fn to_js_object<T: serde::Serialize + ?Sized>(value: &T) -> Result<JsValue, JsError> {
    value
        .serialize(&Serializer::json_compatible())
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Comprime un PDF. `profile`: "screen" | "ebook" | "printer".
/// Devuelve los bytes del PDF optimizado. (API v1: se mantiene sin cambios;
/// para reporte/opciones/progreso usa `compress_with_report`.)
#[wasm_bindgen]
pub fn compress(input: &[u8], profile: &str) -> Result<Vec<u8>, JsError> {
    // Un solo sitio de construcción de `CompressOptions` (compartido con
    // `compress_with_report`) para que ambos caminos no puedan divergir.
    let opts = to_compress_options(profile, &JsOptions::default()).map_err(|e| JsError::new(&e))?;
    let res = core_compress(input, &opts).map_err(|e| JsError::new(&e.to_string()))?;
    Ok(res.output)
}

/// Analiza un PDF sin comprimirlo. Devuelve el reporte como objeto JS con la
/// misma forma que `report` en `compress_with_report`; `output_size` y `ratio`
/// vienen undefined porque no hubo compresión, y los contadores de imágenes
/// vienen a 0 (el análisis no recorre imágenes en v1).
#[wasm_bindgen]
pub fn analyze(input: &[u8]) -> Result<JsValue, JsError> {
    let report = gema_core::analyze(input).map_err(|e| JsError::new(&e.to_string()))?;
    to_js_object(&to_js_report(&report, None))
}

/// Comprime un PDF devolviendo `{ output: Uint8Array, report: {...} }`.
///
/// - `profile`: "screen" | "ebook" | "printer".
/// - `options`: objeto `{ image_dpi?, jpeg_quality?, transcode_dpi?,
///   transcode_quality?, signatures?: "strict"|"ignore"|"flatten" }`.
///   Las `transcode_*` sólo afectan a escaneos que llegan sin pérdida y salen
///   como JPEG (ver ROADMAP §2.b).
///   o undefined/null para usar los defaults del perfil. Claves desconocidas se
///   ignoran; valores inválidos son un error.
/// - `on_phase`: función opcional que recibe `{ phase, done?, total? }` con
///   `phase` ∈ "analyzing" | "optimizing" | "rewriting" | "done" (`done`/`total`
///   sólo en "optimizing"). Si el callback lanza, el error se ignora: un fallo
///   de UI nunca aborta la compresión. Los eventos "optimizing" se limitan a
///   ~100 llamadas JS (siempre el primero, el último, y cada ~1% del total)
///   para no saturar el borde wasm en PDFs con miles de imágenes; el resto de
///   fases (analyzing/rewriting/done) siempre se reenvían.
#[wasm_bindgen]
pub fn compress_with_report(
    input: &[u8],
    profile: &str,
    options: JsValue,
    on_phase: Option<js_sys::Function>,
) -> Result<JsValue, JsError> {
    let js_opts: JsOptions = if options.is_undefined() || options.is_null() {
        JsOptions::default()
    } else {
        serde_wasm_bindgen::from_value(options)
            .map_err(|e| JsError::new(&format!("opciones inválidas: {e}")))?
    };
    let opts = to_compress_options(profile, &js_opts).map_err(|e| JsError::new(&e))?;

    let mut emit = |phase: Phase| {
        if let Some(cb) = on_phase.as_ref() {
            // El core emite `OptimizingImages` una vez POR IMAGEN (exacto a
            // propósito, ver progress.rs) — correcto para el CLI, pero un PDF
            // de ~1900 imágenes dispararía ~1900 llamadas JS a través del
            // límite wasm-bindgen, lo cual es caro (marshalling + reflow de
            // UI) sin aportar granularidad útil. Acá, en el borde wasm,
            // reducimos a ~100 cruces: siempre el primero (done == 0), el
            // último (done == total), y cada `step`-ésimo entremedio. El
            // core en sí queda intacto/exacto; esto es puramente una
            // decisión de la capa de bindings.
            let should_forward = match phase {
                Phase::OptimizingImages { done, total } => {
                    let step = (total / 100).max(1);
                    done == 0 || done == total || done % step == 0
                }
                // Analyzing/Rewriting/Done no llevan progreso granular:
                // siempre se reenvían.
                _ => true,
            };
            if should_forward {
                // Si el callback JS lanza, lo ignoramos deliberadamente.
                let _ = cb.call1(&JsValue::UNDEFINED, &phase_to_js(phase));
            }
        }
    };
    let res = compress_with_progress(input, &opts, &mut emit)
        .map_err(|e| JsError::new(&e.to_string()))?;

    let report = to_js_object(&to_js_report(&res.report, Some(opts.signatures)))?;
    let out = js_sys::Object::new();
    js_sys::Reflect::set(
        &out,
        &JsValue::from_str("output"),
        &js_sys::Uint8Array::from(&res.output[..]),
    )
    .map_err(|_| JsError::new("no se pudo construir el objeto resultado"))?;
    js_sys::Reflect::set(&out, &JsValue::from_str("report"), &report)
        .map_err(|_| JsError::new("no se pudo construir el objeto resultado"))?;
    Ok(out.into())
}

/// Reporte plano serializable hacia JS. Espejo de `gema_core::Report` con las
/// stats por-imagen agregadas en contadores por acción y las warnings rendidas
/// como texto (su `Display`).
#[derive(Debug, serde::Serialize)]
struct JsReport {
    pages: usize,
    original_size: u64,
    output_size: Option<u64>,
    ratio: Option<f32>,
    is_signed: bool,
    images_total: usize,
    images_recompressed: usize,
    images_downsampled: usize,
    images_kept: usize,
    images_skipped: usize,
    /// Firmas/sellos preservados byte-idénticos (no recomprimidos).
    images_preserved: usize,
    /// Firmas/sellos aplanados al contenido de página (política flatten).
    flattened_signatures: usize,
    /// Política aplicada por la operación. Ausente en `analyze`, que no modifica.
    signature_policy: Option<&'static str>,
    /// `true` cuando Flatten integró la apariencia al contenido de página.
    visual_appearance_preserved: bool,
    /// Sólo es `true` para un PDF firmado si Strict dejó el archivo intacto.
    cryptographic_validity_preserved: bool,
    /// Indica que Strict impidió la transformación solicitada.
    operation_blocked: bool,
    /// Indica si una operación sobre un PDF firmado produjo contenido nuevo.
    document_modified: bool,
    warnings: Vec<String>,
}

fn signature_policy_name(policy: SignaturePolicy) -> &'static str {
    match policy {
        SignaturePolicy::Strict => "strict",
        SignaturePolicy::Ignore => "ignore",
        SignaturePolicy::Flatten => "flatten",
    }
}

fn to_js_report(r: &Report, policy: Option<SignaturePolicy>) -> JsReport {
    let mut recompressed = 0;
    let mut downsampled = 0;
    let mut kept = 0;
    let mut skipped = 0;
    let mut preserved = 0;
    for s in &r.images {
        match s.action {
            ImageAction::Recompressed => recompressed += 1,
            ImageAction::Downsampled => downsampled += 1,
            ImageAction::Kept => kept += 1,
            ImageAction::Skipped => skipped += 1,
            ImageAction::Preserved => preserved += 1,
        }
    }
    let strict_blocked = r.is_signed && policy == Some(SignaturePolicy::Strict);
    let flattened = r.is_signed && policy == Some(SignaturePolicy::Flatten);
    JsReport {
        pages: r.pages,
        original_size: r.original_size,
        output_size: r.output_size,
        ratio: r.ratio,
        is_signed: r.is_signed,
        images_total: r.images.len(),
        images_recompressed: recompressed,
        images_downsampled: downsampled,
        images_kept: kept,
        images_skipped: skipped,
        images_preserved: preserved,
        flattened_signatures: r.flattened_signatures,
        signature_policy: policy.map(signature_policy_name),
        visual_appearance_preserved: !r.is_signed || flattened || strict_blocked,
        cryptographic_validity_preserved: !r.is_signed || strict_blocked,
        operation_blocked: strict_blocked,
        document_modified: r.is_signed && policy.is_some() && !strict_blocked,
        warnings: r.warnings.iter().map(|w| w.to_string()).collect(),
    }
}

/// Opciones opcionales desde JS. Campos ausentes → defaults del perfil / de
/// `CompressOptions::default()`. Claves desconocidas se ignoran (serde).
#[derive(Debug, Default, serde::Deserialize)]
struct JsOptions {
    image_dpi: Option<u32>,
    jpeg_quality: Option<u8>,
    /// Perillas sólo para escaneos que llegan sin pérdida (raster en Flate) y
    /// se transcodifican a JPEG. Calibradas para `ebook` (110/30); en `screen`
    /// y `printer` no aplican — ver ROADMAP §2.b.
    transcode_dpi: Option<u32>,
    transcode_quality: Option<u8>,
    /// "strict" | "ignore" | "flatten" (default: flatten)
    signatures: Option<String>,
}

fn parse_profile(profile: &str) -> Result<Profile, String> {
    match profile {
        "screen" => Ok(Profile::Screen),
        "ebook" => Ok(Profile::Ebook),
        "printer" => Ok(Profile::Printer),
        other => Err(format!(
            "perfil desconocido: {other} (usa screen|ebook|printer)"
        )),
    }
}

/// Mapea perfil + `JsOptions` a las `CompressOptions` de core. Los overrides
/// numéricos ganan al perfil (misma semántica que `CompressOptions::resolved`).
fn to_compress_options(profile: &str, o: &JsOptions) -> Result<CompressOptions, String> {
    let profile = parse_profile(profile)?;
    let signatures = match o.signatures.as_deref() {
        Some("strict") => SignaturePolicy::Strict,
        Some("ignore") => SignaturePolicy::Ignore,
        Some("flatten") | None => SignaturePolicy::Flatten,
        Some(other) => return Err(format!("signatures desconocido: {other}")),
    };
    Ok(CompressOptions {
        profile,
        image_dpi: o.image_dpi,
        jpeg_quality: o.jpeg_quality,
        transcode_dpi: o.transcode_dpi,
        transcode_quality: o.transcode_quality,
        signatures,
        ..Default::default()
    })
}

/// Nombre de fase que ve JS en `{ phase: ... }`.
fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Analyzing => "analyzing",
        Phase::OptimizingImages { .. } => "optimizing",
        Phase::Rewriting => "rewriting",
        Phase::Done => "done",
    }
}

/// `(done, total)` sólo para la fase de imágenes; el resto no lleva progreso.
fn phase_progress(p: Phase) -> Option<(usize, usize)> {
    match p {
        Phase::OptimizingImages { done, total } => Some((done, total)),
        _ => None,
    }
}

/// Construye el objeto JS `{ phase, done?, total? }` para el callback.
fn phase_to_js(p: Phase) -> JsValue {
    let obj = js_sys::Object::new();
    // Reflect::set sólo puede fallar sobre no-objetos; `obj` siempre es Object.
    let _ = js_sys::Reflect::set(
        &obj,
        &JsValue::from_str("phase"),
        &JsValue::from_str(phase_name(p)),
    );
    if let Some((done, total)) = phase_progress(p) {
        let _ = js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("done"),
            &JsValue::from_f64(done as f64),
        );
        let _ = js_sys::Reflect::set(
            &obj,
            &JsValue::from_str("total"),
            &JsValue::from_f64(total as f64),
        );
    }
    obj.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gema_core::{ImageAction, ImageStat, Phase, Report, SignaturePolicy, Warning};

    #[test]
    fn maps_profile_strings() {
        assert_eq!(parse_profile("screen"), Ok(Profile::Screen));
        assert_eq!(parse_profile("ebook"), Ok(Profile::Ebook));
        assert_eq!(parse_profile("printer"), Ok(Profile::Printer));
        // valor desconocido es un error explícito (no cae en un default)
        assert!(parse_profile("otro").is_err());
    }

    #[test]
    fn options_mapper_defaults_match_core_defaults() {
        let opts = to_compress_options("ebook", &JsOptions::default()).unwrap();
        assert_eq!(opts.profile, Profile::Ebook);
        assert_eq!(opts.image_dpi, None);
        assert_eq!(opts.jpeg_quality, None);
        assert_eq!(opts.signatures, SignaturePolicy::Flatten);
        // el resto de flags conserva los defaults de core
        assert!(opts.downsample && opts.recompress_streams && opts.remove_metadata);
    }

    /// Las perillas de transcodificado tienen que CRUZAR el binding. serde
    /// ignora las claves que no conoce, así que sin este mapeo el worker las
    /// pasaría y se perderían en silencio — el Beta comprimiría como antes y
    /// nada avisaría.
    #[test]
    fn options_mapper_carries_transcode_knobs() {
        let o = JsOptions {
            transcode_dpi: Some(110),
            transcode_quality: Some(30),
            ..Default::default()
        };
        let opts = to_compress_options("ebook", &o).unwrap();
        assert_eq!(opts.transcode_dpi, Some(110));
        assert_eq!(opts.transcode_quality, Some(30));
    }

    #[test]
    fn options_mapper_applies_overrides_and_signatures() {
        let o = JsOptions {
            image_dpi: Some(96),
            jpeg_quality: Some(55),
            signatures: Some("ignore".into()),
            ..Default::default()
        };
        let opts = to_compress_options("screen", &o).unwrap();
        assert_eq!(opts.profile, Profile::Screen);
        assert_eq!(opts.image_dpi, Some(96));
        assert_eq!(opts.jpeg_quality, Some(55));
        assert_eq!(opts.signatures, SignaturePolicy::Ignore);

        let o = JsOptions {
            signatures: Some("strict".into()),
            ..Default::default()
        };
        assert_eq!(
            to_compress_options("ebook", &o).unwrap().signatures,
            SignaturePolicy::Strict
        );
    }

    #[test]
    fn options_mapper_rejects_unknown_values() {
        // perfil desconocido → error (mismo comportamiento que `compress` v1)
        assert!(to_compress_options("otro", &JsOptions::default()).is_err());
        // política de firmas desconocida → error explícito
        let bad = JsOptions {
            signatures: Some("aggressive".into()),
            ..Default::default()
        };
        let err = to_compress_options("ebook", &bad).unwrap_err();
        assert!(err.contains("signatures desconocido"), "err={err}");
    }

    #[test]
    fn report_mapper_aggregates_actions_and_warnings() {
        let stat = |action| ImageStat {
            object_id: 1,
            original_bytes: 10,
            output_bytes: 5,
            action,
        };
        let r = Report {
            pages: 3,
            original_size: 1000,
            output_size: Some(400),
            ratio: Some(0.4),
            is_signed: true,
            images: vec![
                stat(ImageAction::Recompressed),
                stat(ImageAction::Recompressed),
                stat(ImageAction::Downsampled),
                stat(ImageAction::Kept),
                stat(ImageAction::Skipped),
            ],
            warnings: vec![
                Warning::SignedDocument,
                Warning::ImageSkipped(9),
                Warning::Other("x".into()),
            ],
            ..Default::default()
        };
        let js = to_js_report(&r, Some(SignaturePolicy::Flatten));
        assert_eq!(js.pages, 3);
        assert_eq!(js.original_size, 1000);
        assert_eq!(js.output_size, Some(400));
        assert_eq!(js.ratio, Some(0.4));
        assert!(js.is_signed);
        assert_eq!(js.images_total, 5);
        assert_eq!(js.images_recompressed, 2);
        assert_eq!(js.images_downsampled, 1);
        assert_eq!(js.images_kept, 1);
        assert_eq!(js.images_skipped, 1);
        assert_eq!(js.flattened_signatures, 0);
        assert_eq!(js.signature_policy, Some("flatten"));
        assert!(js.visual_appearance_preserved);
        assert!(!js.cryptographic_validity_preserved);
        assert!(!js.operation_blocked);
        assert!(js.document_modified);
        assert_eq!(js.warnings.len(), 3);
        assert!(js.warnings[0].contains("firmado"));
        assert!(js.warnings[1].contains('9'));
        assert_eq!(js.warnings[2], "x");
    }

    #[test]
    fn strict_report_makes_the_block_explicit() {
        let r = Report {
            is_signed: true,
            original_size: 100,
            output_size: Some(100),
            ..Default::default()
        };
        let js = to_js_report(&r, Some(SignaturePolicy::Strict));
        assert_eq!(js.signature_policy, Some("strict"));
        assert!(js.visual_appearance_preserved);
        assert!(js.cryptographic_validity_preserved);
        assert!(js.operation_blocked);
        assert!(!js.document_modified);
    }

    #[test]
    fn phase_maps_to_js_names_and_progress() {
        assert_eq!(phase_name(Phase::Analyzing), "analyzing");
        assert_eq!(
            phase_name(Phase::OptimizingImages { done: 1, total: 2 }),
            "optimizing"
        );
        assert_eq!(phase_name(Phase::Rewriting), "rewriting");
        assert_eq!(phase_name(Phase::Done), "done");
        assert_eq!(
            phase_progress(Phase::OptimizingImages { done: 1, total: 2 }),
            Some((1, 2))
        );
        assert_eq!(phase_progress(Phase::Analyzing), None);
        assert_eq!(phase_progress(Phase::Done), None);
    }
}
