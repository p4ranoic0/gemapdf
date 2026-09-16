use gema_core::{
    compress as core_compress, compress_with_progress, CompressOptions, Phase, Profile, Report,
    ReportJson, SignaturePolicy,
};
use gema_edit::EraseRegion;
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
///   transcode_quality?, max_memory_bytes?, max_parallel_images?,
///   max_image_bytes?, dedupe_images?,
///   signatures?: "strict"|"ignore"|"flatten" }`.
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

/// Borra de verdad el texto que cae dentro de unas regiones.
///
/// - `regions`: array de `{ id: string, page: number (base 0), x, y, width,
///   height }` en puntos, espacio de página PDF (origen abajo a la izquierda).
///
/// Devuelve `{ output: Uint8Array, report: { regions: [{ id, page,
/// erased_glyphs, status }] } }`. `status` ∈ "erased" | "erased_unverified" |
/// "nothing_found" | "skipped_encrypted" | "skipped_invalid_region" |
/// "skipped_page_geometry" | "skipped_content" | "skipped_unsupported_text" |
/// "skipped_verification". Sólo "erased" garantiza que en la región ya no queda
/// texto; en cualquier otro caso el llamador debe seguir tapando la región.
#[wasm_bindgen]
pub fn erase_text(input: &[u8], regions: JsValue) -> Result<JsValue, JsError> {
    let regions: Vec<EraseRegion> = serde_wasm_bindgen::from_value(regions)
        .map_err(|e| JsError::new(&format!("regiones inválidas: {e}")))?;
    let result =
        gema_edit::erase_text(input, &regions).map_err(|e| JsError::new(&e.to_string()))?;

    let report = js_sys::Object::new();
    js_sys::Reflect::set(
        &report,
        &JsValue::from_str("regions"),
        &to_js_object(&result.regions)?,
    )
    .map_err(|_| JsError::new("no se pudo construir el informe"))?;
    let out = js_sys::Object::new();
    js_sys::Reflect::set(
        &out,
        &JsValue::from_str("output"),
        &js_sys::Uint8Array::from(&result.output[..]),
    )
    .map_err(|_| JsError::new("no se pudo construir el objeto resultado"))?;
    js_sys::Reflect::set(&out, &JsValue::from_str("report"), &report)
        .map_err(|_| JsError::new("no se pudo construir el objeto resultado"))?;
    Ok(out.into())
}

fn to_js_report(r: &Report, policy: Option<SignaturePolicy>) -> ReportJson {
    ReportJson::from_report(r, policy)
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
    max_memory_bytes: Option<u64>,
    max_parallel_images: Option<usize>,
    max_image_bytes: Option<u64>,
    dedupe_images: Option<bool>,
    /// "strict" | "ignore" | "flatten" (default: flatten)
    signatures: Option<String>,
}

/// Mapea perfil + `JsOptions` a las `CompressOptions` de core. Los overrides
/// numéricos ganan al perfil (misma semántica que `CompressOptions::resolved`).
fn to_compress_options(profile: &str, o: &JsOptions) -> Result<CompressOptions, String> {
    // Nombres y mensajes de error los define core (`FromStr`); acá sólo se
    // decide qué significa la ausencia de la clave.
    let profile: Profile = profile.parse()?;
    let signatures = match o.signatures.as_deref() {
        Some(name) => name.parse()?,
        None => SignaturePolicy::Flatten,
    };
    let defaults = CompressOptions::default();
    Ok(CompressOptions {
        profile,
        image_dpi: o.image_dpi,
        jpeg_quality: o.jpeg_quality,
        transcode_dpi: o.transcode_dpi,
        transcode_quality: o.transcode_quality,
        // WASM es serial, pero preparar todos los outputs antes de confirmarlos
        // retenía el documento completo recomprimido. Un default de 256 MiB
        // forma lotes sin sacrificar paralelismo (no hay threads en el Beta).
        max_memory_bytes: Some(o.max_memory_bytes.unwrap_or(256 * 1024 * 1024)),
        max_parallel_images: o.max_parallel_images,
        max_image_bytes: o.max_image_bytes,
        dedupe_images: o.dedupe_images.unwrap_or(defaults.dedupe_images),
        signatures,
        ..defaults
    })
}

/// Nombre de fase que ve JS en `{ phase: ... }`.
fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Analyzing => "analyzing",
        Phase::OptimizingImages { .. } => "optimizing",
        Phase::Rewriting => "rewriting",
        Phase::Done => "done",
        // `Phase` es `#[non_exhaustive]`: una fase nueva llega como "unknown"
        // en vez de romper el build del binding. El worker ignora lo que no
        // reconoce, así que degrada a no mostrar progreso de esa etapa.
        _ => "unknown",
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
    use gema_core::{
        ImageAction, ImageSkipReason, ImageSkipSummary, ImageStat, Phase, Report, SignaturePolicy,
        Warning,
    };

    /// El binding no reimplementa el mapeo (vive en core), pero sí tiene que
    /// seguir aceptando los mismos nombres y rechazando lo desconocido en vez
    /// de caer a un default silencioso.
    #[test]
    fn maps_profile_strings() {
        for (name, expected) in [
            ("screen", Profile::Screen),
            ("ebook", Profile::Ebook),
            ("printer", Profile::Printer),
        ] {
            let opts = to_compress_options(name, &JsOptions::default()).unwrap();
            assert_eq!(opts.profile, expected);
        }
        assert!(to_compress_options("otro", &JsOptions::default()).is_err());
    }

    #[test]
    fn options_mapper_defaults_match_core_defaults() {
        let opts = to_compress_options("ebook", &JsOptions::default()).unwrap();
        assert_eq!(opts.profile, Profile::Ebook);
        assert_eq!(opts.image_dpi, None);
        assert_eq!(opts.jpeg_quality, None);
        assert_eq!(opts.max_memory_bytes, Some(256 * 1024 * 1024));
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
    fn options_mapper_carries_memory_limits() {
        let o = JsOptions {
            max_memory_bytes: Some(256 * 1024 * 1024),
            max_parallel_images: Some(2),
            max_image_bytes: Some(128 * 1024 * 1024),
            ..Default::default()
        };
        let opts = to_compress_options("ebook", &o).unwrap();
        assert_eq!(opts.max_memory_bytes, Some(256 * 1024 * 1024));
        assert_eq!(opts.max_parallel_images, Some(2));
        assert_eq!(opts.max_image_bytes, Some(128 * 1024 * 1024));
    }

    #[test]
    fn options_mapper_carries_image_deduplication() {
        let o = JsOptions {
            dedupe_images: Some(true),
            ..Default::default()
        };
        assert!(to_compress_options("ebook", &o).unwrap().dedupe_images);
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
        assert!(err.contains("aggressive"), "err={err}");
        assert!(err.contains("strict|ignore|flatten"), "err={err}");
    }

    #[test]
    fn report_mapper_aggregates_actions_and_warnings() {
        let stat = |action| ImageStat {
            object_id: 1,
            original_bytes: 10,
            output_bytes: 5,
            action,
            skip_reason: None,
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
            deduplicated_images: 2,
            deduplicated_image_bytes: 512,
            image_skip_summary: vec![ImageSkipSummary {
                reason: ImageSkipReason::Jpx,
                images: 1,
                original_bytes: 10,
            }],
            ..Default::default()
        };
        let js = to_js_report(&r, Some(SignaturePolicy::Flatten));
        assert_eq!(js.input.pages, 3);
        assert_eq!(js.input.bytes, 1000);
        let output = js.output.as_ref().unwrap();
        assert_eq!(output.bytes, 400);
        assert_eq!(output.ratio, Some(0.4));
        assert!(js.document.is_signed);
        assert!(!js.document.has_scanned_pages);
        assert_eq!(js.images.total, 5);
        assert_eq!(js.images.by_action.recompressed, 2);
        assert_eq!(js.images.by_action.downsampled, 1);
        assert_eq!(js.images.by_action.kept, 1);
        assert_eq!(js.images.by_action.skipped, 1);
        assert_eq!(js.document.flattened_signatures, 0);
        assert_eq!(js.images.deduplicated, 2);
        assert_eq!(js.images.deduplicated_bytes, 512);
        assert_eq!(js.images.skipped_by_reason.len(), 1);
        assert_eq!(js.images.skipped_by_reason[0].reason, "jpx");
        assert_eq!(js.document.signature_policy.as_deref(), Some("flatten"));
        assert!(js.document.visual_appearance_preserved);
        assert!(!js.document.cryptographic_validity_preserved);
        assert!(!js.document.operation_blocked);
        assert!(js.document.document_modified);
        assert_eq!(js.warnings.len(), 3);
        assert_eq!(js.warnings[0].kind, "signed_document");
        assert_eq!(js.warnings[0].object_id, None);
        assert!(js.warnings[0].message.contains("firmado"));
        assert_eq!(js.warnings[1].kind, "image_skipped");
        assert_eq!(js.warnings[1].object_id, Some(9));
        assert!(js.warnings[1].message.contains('9'));
        assert_eq!(js.warnings[2].kind, "other");
        assert_eq!(js.warnings[2].message, "x");
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
        assert_eq!(js.document.signature_policy.as_deref(), Some("strict"));
        assert!(js.document.visual_appearance_preserved);
        assert!(js.document.cryptographic_validity_preserved);
        assert!(js.document.operation_blocked);
        assert!(!js.document.document_modified);
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
