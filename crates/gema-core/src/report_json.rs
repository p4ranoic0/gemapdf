//! Esquema JSON del reporte, compartido por la CLI y el binding WASM.
//!
//! Es una vista estable de [`Report`]: los contadores por acción van en un
//! objeto (una acción nueva agrega una clave, no rompe la raíz) y las warnings
//! llevan un `kind` estable además del texto legible. El detalle por imagen es
//! opcional: publicarlo por default congelaría la semántica de `object_id`, que
//! es el número de objeto en la *entrada*.

use crate::options::SignaturePolicy;
use crate::report::{ImageAction, Report, Warning};

/// Versión del esquema JSON. Sube sólo cuando el JSON deja de ser
/// retrocompatible: una clave renombrada, borrada o de otro tipo. Agregar una
/// clave nueva **no** la sube — los consumidores deben ignorar lo que no
/// conocen.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Reporte en la forma que se serializa a JSON.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ReportJson {
    /// Ver [`REPORT_SCHEMA_VERSION`].
    pub report_schema_version: u32,
    /// Datos del PDF de entrada.
    pub input: InputJson,
    /// Datos de la salida; `None` en `analyze`, que no produce ninguna.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub output: Option<OutputJson>,
    /// Propiedades del documento y efecto de la política de firmas.
    pub document: DocumentJson,
    /// Agregados de imágenes.
    pub images: ImagesJson,
    /// Avisos no fatales.
    pub warnings: Vec<WarningJson>,
}

/// Datos del PDF de entrada.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct InputJson {
    /// Bytes del archivo de entrada.
    pub bytes: u64,
    /// Páginas del documento.
    pub pages: usize,
}

/// Datos del PDF de salida.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OutputJson {
    /// Bytes del archivo de salida.
    pub bytes: u64,
    /// `bytes` sobre los de entrada: menor es más comprimido.
    pub ratio: Option<f32>,
}

/// Propiedades del documento y efecto de la política de firmas aplicada.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct DocumentJson {
    /// El documento trae una firma criptográfica.
    pub is_signed: bool,
    /// Estimación conservadora de páginas escaneadas.
    pub has_scanned_pages: bool,
    /// Política aplicada; `None` en `analyze`, que no aplica ninguna.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub signature_policy: Option<String>,
    /// Firmas o sellos aplanados al contenido de página por la política
    /// `Flatten`. Cero con cualquier otra política.
    pub flattened_signatures: usize,
    /// La apariencia visible de firmas y sellos se conserva.
    pub visual_appearance_preserved: bool,
    /// La validez criptográfica se conserva; sólo si Strict no tocó nada.
    pub cryptographic_validity_preserved: bool,
    /// Strict impidió la transformación pedida.
    pub operation_blocked: bool,
    /// La operación produjo contenido nuevo sobre un documento firmado.
    pub document_modified: bool,
}

/// Agregados por imagen.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ImagesJson {
    /// Imágenes procesadas.
    pub total: usize,
    /// Cuántas recibieron cada acción.
    pub by_action: ByActionJson,
    /// Imágenes redundantes eliminadas por el modo opt-in.
    pub deduplicated: usize,
    /// Bytes de stream de esas imágenes redundantes.
    pub deduplicated_bytes: u64,
    /// Oportunidades no procesadas, agrupadas por motivo estable.
    pub skipped_by_reason: Vec<SkipSummaryJson>,
    /// Detalle por imagen; presente sólo si se pidió explícitamente.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub detail: Option<Vec<ImageStatJson>>,
}

/// Contadores por acción. Una acción nueva agrega una clave acá, en vez de un
/// campo nuevo en la raíz del documento.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ByActionJson {
    /// Recomprimidas a la misma resolución.
    pub recompressed: usize,
    /// Recomprimidas a menor resolución.
    pub downsampled: usize,
    /// Reescritas sin ganancia, codificación original conservada.
    pub kept: usize,
    /// No procesadas; ver `skipped_by_reason`.
    pub skipped: usize,
    /// Preservadas byte-idénticas por ser firma o sello.
    pub preserved: usize,
}

/// Un grupo de imágenes omitidas por el mismo motivo.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct SkipSummaryJson {
    /// Identificador estable del motivo.
    pub reason: &'static str,
    /// Cuántas imágenes.
    pub images: usize,
    /// Bytes codificados de entrada de esas imágenes.
    pub input_bytes: u64,
}

/// Detalle de una imagen. Sólo aparece si el llamador lo pide.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ImageStatJson {
    /// Número de objeto en el PDF de **entrada**.
    pub object_id: u32,
    /// Bytes codificados en la entrada.
    pub input_bytes: u64,
    /// Bytes codificados en la salida.
    pub output_bytes: u64,
    /// Acción aplicada, en `snake_case`.
    pub action: &'static str,
    /// Motivo estable cuando `action == "skipped"`.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub skip_reason: Option<&'static str>,
}

/// Aviso no fatal con discriminante estable.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct WarningJson {
    /// Discriminante estable: `signed_document`, `image_skipped`,
    /// `streams_skipped` u `other`.
    pub kind: &'static str,
    /// Objeto afectado, cuando el aviso es sobre una imagen.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub object_id: Option<u32>,
    /// Texto legible. **No es superficie estable**: puede cambiar de redacción.
    pub message: String,
}

/// Nombre estable de una acción.
///
/// El `match` es exhaustivo **a propósito**: `#[non_exhaustive]` no aplica
/// dentro del crate que lo define, así que agregar una variante a
/// [`ImageAction`] rompe la compilación acá y obliga a decidir su nombre
/// estable en el JSON, en vez de dejarla caer en un `"unknown"` silencioso.
fn action_name(action: &ImageAction) -> &'static str {
    match action {
        ImageAction::Kept => "kept",
        ImageAction::Recompressed => "recompressed",
        ImageAction::Downsampled => "downsampled",
        ImageAction::Skipped => "skipped",
        ImageAction::Preserved => "preserved",
    }
}

impl ReportJson {
    /// Vista JSON sin el detalle por imagen.
    pub fn from_report(report: &Report, policy: Option<SignaturePolicy>) -> Self {
        Self::build(report, policy, false)
    }

    /// Igual que [`ReportJson::from_report`], más una entrada por imagen.
    pub fn from_report_with_images(report: &Report, policy: Option<SignaturePolicy>) -> Self {
        Self::build(report, policy, true)
    }

    fn build(report: &Report, policy: Option<SignaturePolicy>, detail: bool) -> Self {
        let mut by_action = ByActionJson::default();
        for stat in &report.images {
            let slot = match stat.action {
                ImageAction::Kept => &mut by_action.kept,
                ImageAction::Recompressed => &mut by_action.recompressed,
                ImageAction::Downsampled => &mut by_action.downsampled,
                ImageAction::Skipped => &mut by_action.skipped,
                ImageAction::Preserved => &mut by_action.preserved,
            };
            *slot += 1;
        }

        let strict_blocked = report.is_signed && policy == Some(SignaturePolicy::Strict);
        let flattened = report.is_signed && policy == Some(SignaturePolicy::Flatten);

        Self {
            report_schema_version: REPORT_SCHEMA_VERSION,
            input: InputJson {
                bytes: report.original_size,
                pages: report.pages,
            },
            output: report.output_size.map(|bytes| OutputJson {
                bytes,
                ratio: report.ratio,
            }),
            document: DocumentJson {
                is_signed: report.is_signed,
                has_scanned_pages: report.has_scanned_pages,
                signature_policy: policy.map(|p| p.to_string()),
                flattened_signatures: if flattened {
                    report.flattened_signatures
                } else {
                    0
                },
                visual_appearance_preserved: !report.is_signed || flattened || strict_blocked,
                cryptographic_validity_preserved: !report.is_signed || strict_blocked,
                operation_blocked: strict_blocked,
                document_modified: report.is_signed && policy.is_some() && !strict_blocked,
            },
            images: ImagesJson {
                total: report.images.len(),
                by_action,
                deduplicated: report.deduplicated_images,
                deduplicated_bytes: report.deduplicated_image_bytes,
                skipped_by_reason: report
                    .image_skip_summary
                    .iter()
                    .map(|s| SkipSummaryJson {
                        reason: s.reason.as_str(),
                        images: s.images,
                        input_bytes: s.original_bytes,
                    })
                    .collect(),
                detail: detail.then(|| {
                    report
                        .images
                        .iter()
                        .map(|s| ImageStatJson {
                            object_id: s.object_id,
                            input_bytes: s.original_bytes,
                            output_bytes: s.output_bytes,
                            action: action_name(&s.action),
                            skip_reason: s.skip_reason.map(|r| r.as_str()),
                        })
                        .collect()
                }),
            },
            warnings: report
                .warnings
                .iter()
                .map(|w| WarningJson {
                    // Exhaustivo a propósito, como `action_name`: una variante
                    // nueva de `Warning` debe elegir su `kind` estable.
                    kind: match w {
                        Warning::SignedDocument => "signed_document",
                        Warning::ImageSkipped(_) => "image_skipped",
                        Warning::StreamsSkipped { .. } => "streams_skipped",
                        Warning::Other(_) => "other",
                    },
                    object_id: match w {
                        Warning::ImageSkipped(id) => Some(*id),
                        _ => None,
                    },
                    message: w.to_string(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{ImageSkipReason, ImageSkipSummary, ImageStat};

    fn sample_report() -> Report {
        Report {
            pages: 2,
            original_size: 1000,
            output_size: Some(400),
            ratio: Some(0.4),
            images: vec![
                ImageStat {
                    object_id: 7,
                    original_bytes: 500,
                    output_bytes: 200,
                    action: ImageAction::Recompressed,
                    skip_reason: None,
                },
                ImageStat {
                    object_id: 9,
                    original_bytes: 300,
                    output_bytes: 300,
                    action: ImageAction::Skipped,
                    skip_reason: Some(ImageSkipReason::Ccit),
                },
            ],
            image_skip_summary: vec![ImageSkipSummary {
                reason: ImageSkipReason::Ccit,
                images: 1,
                original_bytes: 300,
            }],
            preserved_images: 0,
            flattened_signatures: 1,
            deduplicated_images: 0,
            deduplicated_image_bytes: 0,
            is_signed: true,
            has_scanned_pages: true,
            warnings: vec![Warning::ImageSkipped(9), Warning::SignedDocument],
        }
    }

    #[test]
    fn counts_images_by_action() {
        let j = ReportJson::from_report(&sample_report(), Some(SignaturePolicy::Flatten));
        assert_eq!(j.images.total, 2);
        assert_eq!(j.images.by_action.recompressed, 1);
        assert_eq!(j.images.by_action.skipped, 1);
        assert_eq!(j.images.by_action.kept, 0);
    }

    #[test]
    fn image_detail_is_omitted_unless_requested() {
        let r = sample_report();
        assert!(ReportJson::from_report(&r, None).images.detail.is_none());
        let with = ReportJson::from_report_with_images(&r, None);
        let detail = with.images.detail.expect("detalle pedido");
        assert_eq!(detail.len(), 2);
        assert_eq!(detail[1].object_id, 9);
        assert_eq!(detail[1].action, "skipped");
        assert_eq!(detail[1].skip_reason, Some("ccitt"));
    }

    #[test]
    fn warnings_carry_a_stable_kind_not_only_prose() {
        let mut report = sample_report();
        report.warnings = vec![
            Warning::ImageSkipped(9),
            Warning::SignedDocument,
            Warning::StreamsSkipped {
                count: 3,
                reason: crate::LimitKind::StreamBytes,
            },
            Warning::Other("aviso adicional".into()),
        ];
        let j = ReportJson::from_report(&report, None);

        assert_eq!(j.warnings[0].kind, "image_skipped");
        assert_eq!(j.warnings[0].object_id, Some(9));
        assert!(!j.warnings[0].message.is_empty());

        assert_eq!(j.warnings[1].kind, "signed_document");
        assert_eq!(j.warnings[1].object_id, None);
        assert!(!j.warnings[1].message.is_empty());

        assert_eq!(j.warnings[2].kind, "streams_skipped");
        assert_eq!(j.warnings[2].object_id, None);
        assert!(!j.warnings[2].message.is_empty());
        // `WarningJson` no tiene un campo para `count`: el `Display` y, por lo
        // tanto, `message` son la única vía por la que llega al JSON.
        assert!(j.warnings[2].message.contains('3'));

        assert_eq!(j.warnings[3].kind, "other");
        assert_eq!(j.warnings[3].object_id, None);
        assert!(!j.warnings[3].message.is_empty());
    }

    #[test]
    fn signature_flags_match_the_policy_applied() {
        let r = sample_report();
        let flatten = ReportJson::from_report(&r, Some(SignaturePolicy::Flatten));
        assert_eq!(flatten.document.flattened_signatures, 1);
        assert!(flatten.document.visual_appearance_preserved);
        assert!(!flatten.document.cryptographic_validity_preserved);
        assert!(!flatten.document.operation_blocked);
        assert!(flatten.document.document_modified);

        let strict = ReportJson::from_report(&r, Some(SignaturePolicy::Strict));
        assert_eq!(strict.document.flattened_signatures, 0);
        assert!(strict.document.operation_blocked);
        assert!(strict.document.cryptographic_validity_preserved);
        assert!(!strict.document.document_modified);
    }

    #[test]
    fn analyze_shape_has_no_output_and_no_policy() {
        let mut r = sample_report();
        r.output_size = None;
        r.ratio = None;
        let j = ReportJson::from_report(&r, None);
        assert!(j.output.is_none());
        assert!(j.document.signature_policy.is_none());
        assert_eq!(j.report_schema_version, REPORT_SCHEMA_VERSION);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serializes_with_the_documented_shape() {
        let j = ReportJson::from_report(&sample_report(), Some(SignaturePolicy::Flatten));
        let v: serde_json::Value = serde_json::to_value(&j).unwrap();

        assert_eq!(v["report_schema_version"], 1);
        assert_eq!(v["input"]["bytes"], 1000);
        assert_eq!(v["output"]["bytes"], 400);
        assert_eq!(v["document"]["signature_policy"], "flatten");
        assert_eq!(v["document"]["flattened_signatures"], 1);
        assert_eq!(v["images"]["by_action"]["recompressed"], 1);
        assert_eq!(v["images"]["skipped_by_reason"][0]["reason"], "ccitt");
        assert_eq!(v["warnings"][0]["kind"], "image_skipped");
        // las claves ausentes NO se serializan como null
        assert!(v["images"].get("detail").is_none());

        let strict = ReportJson::from_report(&sample_report(), Some(SignaturePolicy::Strict));
        let strict_value = serde_json::to_value(strict).unwrap();
        assert_eq!(strict_value["document"]["flattened_signatures"], 0);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serialization_is_deterministic() {
        let j = ReportJson::from_report(&sample_report(), None);
        let a = serde_json::to_string(&j).unwrap();
        let b = serde_json::to_string(&j).unwrap();
        assert_eq!(a, b, "el mismo reporte debe dar los mismos bytes");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn analyze_omits_output_instead_of_emitting_null() {
        let mut r = sample_report();
        r.output_size = None;
        r.ratio = None;
        let v = serde_json::to_value(ReportJson::from_report(&r, None)).unwrap();
        assert!(v.get("output").is_none(), "`output` debe estar ausente");
        assert!(v["document"].get("signature_policy").is_none());
    }
}
