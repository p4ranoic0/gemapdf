//! Reemplazo acotado de texto reutilizando códigos que ya dibuja el PDF.
//!
//! Esta fase no inspecciona programas de fuente. Sólo reutiliza códigos
//! observados en runs visibles de la misma definición efectiva de fuente.

use std::collections::{HashMap, HashSet};

use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, ObjectId, Stream, StringFormat};

use crate::error::{EditError, LimitKind};
use crate::inspect::{GapReason, InspectionGap, Inspector, ResidualRisk};
use crate::options::{BudgetMeter, EditOptions};
use crate::signature::SignatureIndicators;
use crate::stream_read::{read_page_content_bounded, StreamReadError};
use crate::text_geometry::{interpret_content, Glyph, PageText};
use crate::text_removal::TextRegion;

/// Versión del informe de reemplazo.
pub const REPLACEMENT_REPORT_SCHEMA_VERSION: u32 = 1;

/// Solicitud de reemplazo de una región de texto.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
pub struct TextReplacement {
    /// Región que identifica el texto original.
    pub region: TextRegion,
    /// Texto nuevo. Cada carácter debe tener un código reutilizable.
    pub new_text: String,
    /// Texto esperado antes de editar, si el llamador quiere protegerse de una
    /// selección obsoleta.
    pub expected_text: Option<String>,
}

/// Resultado de un reemplazo, separado del resultado de borrado.
#[derive(Debug, Clone)]
pub struct ReplacementResult {
    /// PDF resultante.
    pub output: Vec<u8>,
    /// Un informe por reemplazo, en el mismo orden de entrada.
    pub replacements: Vec<ReplacementReport>,
    /// Riesgos residuales detectados.
    pub residual_risks: Vec<ResidualRisk>,
    /// Si quedó alguna superficie sin inspeccionar.
    pub inspection_incomplete: bool,
    /// Gaps de inspección.
    pub inspection_gaps: Vec<InspectionGap>,
    /// Si al menos un reemplazo modificó el PDF.
    pub modified: bool,
    /// Indicios de firma digital.
    pub signature: SignatureIndicators,
}

/// Vista serializable de un resultado, sin los bytes del PDF.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ReplacementDocumentReport<'a> {
    /// Versión del esquema propio del reemplazo.
    pub schema_version: u32,
    /// Si al menos un reemplazo modificó el PDF.
    pub modified: bool,
    /// Informes por reemplazo.
    pub replacements: &'a [ReplacementReport],
    /// Riesgos residuales.
    pub residual_risks: &'a [ResidualRisk],
    /// Si quedó alguna superficie sin inspeccionar.
    pub inspection_incomplete: bool,
    /// Gaps de inspección.
    pub inspection_gaps: &'a [InspectionGap],
    /// Indicios de firma digital.
    pub signature: SignatureIndicators,
}

impl ReplacementResult {
    /// Vista serializable del resultado, sin `output`.
    pub fn report(&self) -> ReplacementDocumentReport<'_> {
        ReplacementDocumentReport {
            schema_version: REPLACEMENT_REPORT_SCHEMA_VERSION,
            modified: self.modified,
            replacements: &self.replacements,
            residual_risks: &self.residual_risks,
            inspection_incomplete: self.inspection_incomplete,
            inspection_gaps: &self.inspection_gaps,
            signature: self.signature,
        }
    }
}

/// Resultado de una región de reemplazo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum ReplacementStatus {
    /// El texto nuevo fue emitido y la verificación pasó.
    Replaced,
    /// No había glifos directos en la región.
    NothingFound,
    /// No se pudo demostrar un código reutilizable sin leer el programa de fuente.
    /// No afirma que el glifo falte en la fuente: sólo que no se pudo demostrar
    /// que exista sin leer el programa de fuente.
    SkippedNoReusableCode,
    /// El mapping código-unicode no fue inequívoco.
    SkippedAmbiguousMapping,
    /// El tipo de fuente no tiene métricas que el intérprete pueda usar.
    SkippedUnsupportedFont,
    /// La operación podría dejar texto visible o semántico anterior.
    SkippedSemantics,
    /// El avance del texto nuevo supera el límite configurado.
    SkippedLayout,
    /// La selección no coincide con el texto esperado.
    SkippedStaleSelection,
    /// El documento está cifrado.
    SkippedEncrypted,
    /// La región es inválida.
    SkippedInvalidRegion,
    /// La geometría de página no es soportada.
    SkippedPageGeometry,
    /// El content stream no se pudo leer o parsear.
    SkippedContent,
    /// La secuencia no cumple el alcance estructural de Fase 1.
    SkippedUnsupportedText,
    /// La verificación posterior falló y la página fue restaurada.
    SkippedVerification,
}

impl ReplacementStatus {
    /// Nombre estable serializable.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Replaced => "replaced",
            Self::NothingFound => "nothing_found",
            Self::SkippedNoReusableCode => "skipped_no_reusable_code",
            Self::SkippedAmbiguousMapping => "skipped_ambiguous_mapping",
            Self::SkippedUnsupportedFont => "skipped_unsupported_font",
            Self::SkippedSemantics => "skipped_semantics",
            Self::SkippedLayout => "skipped_layout",
            Self::SkippedStaleSelection => "skipped_stale_selection",
            Self::SkippedEncrypted => "skipped_encrypted",
            Self::SkippedInvalidRegion => "skipped_invalid_region",
            Self::SkippedPageGeometry => "skipped_page_geometry",
            Self::SkippedContent => "skipped_content",
            Self::SkippedUnsupportedText => "skipped_unsupported_text",
            Self::SkippedVerification => "skipped_verification",
        }
    }
}

/// Informe de un reemplazo individual.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ReplacementReport {
    /// Identificador de la región.
    pub id: String,
    /// Página base 0.
    pub page: u32,
    /// Resultado de la operación.
    pub status: ReplacementStatus,
    /// Cantidad de glifos originales reemplazados.
    pub replaced_glyphs: usize,
    /// Texto reconstruido de la selección.
    pub original_text: Option<String>,
    /// Texto solicitado.
    pub new_text: String,
    /// Avance original en espacio de texto.
    pub original_advance: Option<f64>,
    /// Avance nuevo en espacio de texto.
    pub new_advance: Option<f64>,
    /// Ajuste numérico TJ aplicado.
    pub tj_delta: Option<f64>,
    /// Páginas inspeccionadas para buscar códigos.
    pub scanned_pages: usize,
    /// Si el scan terminó por el tope antes de cubrir el texto.
    pub scan_incomplete: bool,
}

impl ReplacementReport {
    fn new(input: &TextReplacement) -> Self {
        Self {
            id: input.region.id.clone(),
            page: input.region.page,
            status: ReplacementStatus::NothingFound,
            replaced_glyphs: 0,
            original_text: None,
            new_text: input.new_text.clone(),
            original_advance: None,
            new_advance: None,
            tj_delta: None,
            scanned_pages: 0,
            scan_incomplete: false,
        }
    }
}

#[derive(Debug, Clone)]
struct FontDefinition {
    /// Identidad del diccionario de fuente dentro del documento actual. La
    /// dirección distingue dos objetos con el mismo `/BaseFont` y los mismos
    /// valores aparentes, que no son reutilizables entre sí en Fase 1.
    key: usize,
    mapping: HashMap<Vec<u8>, String>,
    ambiguous: HashSet<char>,
}

#[derive(Debug, Clone)]
struct CodeCandidate {
    bytes: Vec<u8>,
    font_width: f64,
}

#[derive(Debug)]
struct PageInfo {
    content: Content,
    text: PageText,
    fonts: HashMap<Vec<u8>, FontDefinition>,
}

#[derive(Debug)]
struct Attempt {
    status: ReplacementStatus,
    report: AttemptReport,
    risks: Vec<ResidualRisk>,
    gaps: Vec<InspectionGap>,
}

#[derive(Debug, Default)]
struct AttemptReport {
    replaced_glyphs: usize,
    original_text: Option<String>,
    original_advance: Option<f64>,
    new_advance: Option<f64>,
    tj_delta: Option<f64>,
    scanned_pages: usize,
    scan_incomplete: bool,
}

#[derive(Debug, Default)]
struct ScanCache {
    fonts: HashMap<ObjectId, HashMap<Vec<u8>, FontDefinition>>,
    page_indices: HashMap<ObjectId, usize>,
    pages: Vec<Option<PageInfo>>,
}

#[derive(Debug)]
enum PageLoadError {
    Budget(LimitKind),
    Content,
}

/// Reemplaza texto reutilizando códigos ya observados en runs visibles.
pub fn replace_text_glyphs(
    input: &[u8],
    replacements: &[TextReplacement],
    opts: &EditOptions,
) -> Result<ReplacementResult, EditError> {
    if input.len() > opts.max_input_bytes {
        return Err(EditError::LimitExceeded(LimitKind::InputBytes));
    }
    if replacements.len() > opts.max_regions {
        return Err(EditError::LimitExceeded(LimitKind::Regions));
    }

    let mut doc = Document::load_mem(input).map_err(|e| EditError::Parse(e.to_string()))?;
    let mut reports: Vec<_> = replacements.iter().map(ReplacementReport::new).collect();
    let mut risks = Vec::new();
    let mut gaps = Vec::new();
    let mut modified = false;
    let mut meter = BudgetMeter::new(&opts.budget);
    let mut scan_cache = ScanCache::default();

    if doc.is_encrypted() {
        for report in &mut reports {
            report.status = ReplacementStatus::SkippedEncrypted;
        }
        return Ok(finish(
            input.to_vec(),
            reports,
            risks,
            gaps,
            false,
            SignatureIndicators::default(),
        ));
    }

    for (index, replacement) in replacements.iter().enumerate() {
        let result = attempt_replacement(&mut doc, replacement, opts, &mut meter, &mut scan_cache)?;
        reports[index].status = result.status;
        reports[index].replaced_glyphs = result.report.replaced_glyphs;
        reports[index].original_text = result.report.original_text;
        reports[index].original_advance = result.report.original_advance;
        reports[index].new_advance = result.report.new_advance;
        reports[index].tj_delta = result.report.tj_delta;
        reports[index].scanned_pages = result.report.scanned_pages;
        reports[index].scan_incomplete = result.report.scan_incomplete;
        risks.extend(result.risks);
        gaps.extend(result.gaps);
        if result.status == ReplacementStatus::Replaced {
            modified = true;
            if let Some(page_number) = replacement.region.page.checked_add(1) {
                if let Some(page_id) = doc.get_pages().get(&page_number).copied() {
                    if let Some(page_index) = scan_cache.page_indices.remove(&page_id) {
                        scan_cache.pages[page_index] = None;
                    }
                }
            }
        }
    }

    let mut final_meter = BudgetMeter::new(&opts.budget);
    let mut inspector = Inspector::new(&doc, &mut final_meter);
    let signature = crate::signature::detect(&mut inspector);
    risks.extend(inspector.risks);
    gaps.extend(inspector.gaps);
    dedup_sort(&mut risks);
    dedup_sort(&mut gaps);

    let output = if modified {
        let mut output = Vec::new();
        doc.save_to(&mut output)
            .map_err(|e| EditError::Io(e.to_string()))?;
        output
    } else {
        input.to_vec()
    };
    Ok(finish(output, reports, risks, gaps, modified, signature))
}

fn finish(
    output: Vec<u8>,
    replacements: Vec<ReplacementReport>,
    mut risks: Vec<ResidualRisk>,
    mut gaps: Vec<InspectionGap>,
    modified: bool,
    signature: SignatureIndicators,
) -> ReplacementResult {
    dedup_sort(&mut risks);
    dedup_sort(&mut gaps);
    ReplacementResult {
        output,
        replacements,
        residual_risks: risks,
        inspection_incomplete: !gaps.is_empty(),
        inspection_gaps: gaps,
        modified,
        signature,
    }
}

fn cached_fonts(
    doc: &Document,
    page_id: ObjectId,
    cache: &mut ScanCache,
) -> Result<HashMap<Vec<u8>, FontDefinition>, PageLoadError> {
    if let Some(fonts) = cache.fonts.get(&page_id) {
        return Ok(fonts.clone());
    }
    let fonts = font_definitions(doc, page_id).map_err(|_| PageLoadError::Content)?;
    cache.fonts.insert(page_id, fonts.clone());
    Ok(fonts)
}

fn cached_page_info(
    doc: &Document,
    page_id: ObjectId,
    opts: &EditOptions,
    meter: &mut BudgetMeter,
    cache: &mut ScanCache,
) -> Result<usize, PageLoadError> {
    if let Some(&page_index) = cache.page_indices.get(&page_id) {
        return Ok(page_index);
    }
    let fonts = cached_fonts(doc, page_id, cache)?;
    let content_bytes =
        read_page_content_bounded(doc, page_id, opts, meter).map_err(|error| match error {
            StreamReadError::Budget(kind) => PageLoadError::Budget(kind),
            _ => PageLoadError::Content,
        })?;
    let content = Content::decode(&content_bytes).map_err(|_| PageLoadError::Content)?;
    let text = interpret_content(doc, page_id, &content).map_err(|_| PageLoadError::Content)?;
    let page_index = cache.pages.len();
    cache.pages.push(Some(PageInfo {
        content,
        text,
        fonts,
    }));
    cache.page_indices.insert(page_id, page_index);
    Ok(page_index)
}

fn attempt_replacement(
    doc: &mut Document,
    replacement: &TextReplacement,
    opts: &EditOptions,
    meter: &mut BudgetMeter,
    cache: &mut ScanCache,
) -> Result<Attempt, EditError> {
    let invalid = !valid_region(&replacement.region) || replacement.new_text.is_empty();
    let Some(page_id) = replacement
        .region
        .page
        .checked_add(1)
        .and_then(|page| doc.get_pages().get(&page).copied())
    else {
        return Ok(rejected(
            ReplacementStatus::SkippedInvalidRegion,
            AttemptReport::default(),
        ));
    };
    if invalid {
        return Ok(rejected(
            ReplacementStatus::SkippedInvalidRegion,
            AttemptReport::default(),
        ));
    }
    if !plain_page_geometry(doc, page_id) {
        return Ok(rejected(
            ReplacementStatus::SkippedPageGeometry,
            AttemptReport::default(),
        ));
    }

    let page_index = match cached_page_info(doc, page_id, opts, meter, cache) {
        Ok(page_index) => page_index,
        Err(PageLoadError::Budget(kind)) => return Err(EditError::LimitExceeded(kind)),
        Err(PageLoadError::Content) => {
            return Ok(rejected(
                ReplacementStatus::SkippedContent,
                AttemptReport::default(),
            ))
        }
    };

    let (risks, gaps, semantic_risk, form_risk) = {
        let page = cache.pages[page_index]
            .as_ref()
            .expect("page was inserted or cached");
        let (risks, gaps) = inspect_page(doc, page_id, replacement, &page.content, meter);
        let semantic_risk = has_semantic_risk(doc, &page.content, &risks);
        let form_risk = risks
            .iter()
            .any(|risk| matches!(risk, ResidualRisk::FormXObject { .. }));
        (risks, gaps, semantic_risk, form_risk)
    };
    if semantic_risk {
        return Ok(Attempt {
            status: ReplacementStatus::SkippedSemantics,
            report: AttemptReport::default(),
            risks,
            gaps,
        });
    }
    if form_risk {
        return Ok(Attempt {
            status: ReplacementStatus::SkippedUnsupportedText,
            report: AttemptReport::default(),
            risks,
            gaps,
        });
    }

    let selected = match select_sequence(
        cache.pages[page_index]
            .as_ref()
            .expect("page was inserted or cached"),
        &replacement.region,
    ) {
        Ok(Some(selected)) => selected,
        Ok(None) => {
            return Ok(Attempt {
                status: ReplacementStatus::NothingFound,
                report: AttemptReport::default(),
                risks,
                gaps,
            })
        }
        Err(status) => {
            return Ok(Attempt {
                status,
                report: AttemptReport::default(),
                risks,
                gaps,
            })
        }
    };

    let original_text = selected.original_text.clone();
    if replacement
        .expected_text
        .as_ref()
        .is_some_and(|expected| Some(expected) != original_text.as_ref())
    {
        return Ok(Attempt {
            status: ReplacementStatus::SkippedStaleSelection,
            report: AttemptReport {
                original_text,
                ..AttemptReport::default()
            },
            risks,
            gaps,
        });
    }

    let mut scan = ScanResult::default();
    let order = scan_order(replacement.region.page, doc.get_pages().len() as u32);
    for page_number in order {
        if scan.scanned_pages >= opts.max_scan_pages {
            scan.incomplete = true;
            break;
        }
        let Some(other_id) = doc.get_pages().get(&(page_number + 1)).copied() else {
            continue;
        };
        let other_fonts = match cached_fonts(doc, other_id, cache) {
            Ok(fonts) => fonts,
            Err(PageLoadError::Budget(_)) => {
                scan.incomplete = true;
                break;
            }
            Err(PageLoadError::Content) => {
                scan.incomplete = true;
                continue;
            }
        };
        if !other_fonts
            .values()
            .any(|font| font.key == selected.font_key)
        {
            continue;
        }
        scan.scanned_pages += 1;
        let other_page_index = match cached_page_info(doc, other_id, opts, meter, cache) {
            Ok(page_index) => page_index,
            Err(PageLoadError::Budget(_)) => {
                scan.incomplete = true;
                break;
            }
            Err(PageLoadError::Content) => {
                scan.incomplete = true;
                continue;
            }
        };
        let other_page = cache.pages[other_page_index]
            .as_ref()
            .expect("page was inserted or cached");
        inventory_codes(
            &other_page.content,
            &other_page.text,
            &other_fonts,
            selected.font_key,
            &mut scan.codes,
        );
        if replacement
            .new_text
            .chars()
            .all(|ch| scan.codes.contains_key(&ch))
        {
            scan.complete = true;
            break;
        }
    }
    scan.incomplete |= !scan.complete;
    if !scan.complete {
        return Ok(Attempt {
            status: ReplacementStatus::SkippedNoReusableCode,
            report: AttemptReport {
                original_text,
                scanned_pages: scan.scanned_pages,
                scan_incomplete: scan.incomplete,
                ..AttemptReport::default()
            },
            risks,
            gaps,
        });
    }

    let mut chosen = Vec::new();
    for ch in replacement.new_text.chars() {
        let candidates = scan.codes.get(&ch).expect("scan completion checked");
        let unique: HashSet<Vec<u8>> = candidates.iter().map(|c| c.bytes.clone()).collect();
        if unique.len() > 1 {
            return Ok(Attempt {
                status: ReplacementStatus::SkippedAmbiguousMapping,
                report: AttemptReport {
                    original_text,
                    scanned_pages: scan.scanned_pages,
                    scan_incomplete: scan.incomplete,
                    ..AttemptReport::default()
                },
                risks,
                gaps,
            });
        }
        chosen.push(candidates[0].clone());
    }

    let original_advance: f64 = selected.glyphs.iter().map(|glyph| glyph.text_advance).sum();
    let Some(context) = selected.glyphs.first() else {
        return Ok(rejected(
            ReplacementStatus::NothingFound,
            AttemptReport::default(),
        ));
    };
    if context.font_size.abs() <= f64::EPSILON || context.horizontal_scale.abs() <= f64::EPSILON {
        return Ok(Attempt {
            status: ReplacementStatus::SkippedUnsupportedText,
            report: AttemptReport {
                original_text,
                scanned_pages: scan.scanned_pages,
                scan_incomplete: scan.incomplete,
                ..AttemptReport::default()
            },
            risks,
            gaps,
        });
    }
    let new_advance: f64 = chosen
        .iter()
        .zip(replacement.new_text.chars())
        .map(|(candidate, ch)| {
            (candidate.font_width / 1000.0 * context.font_size
                + context.char_spacing
                + if ch == ' ' { context.word_spacing } else { 0.0 })
                * context.horizontal_scale
        })
        .sum();
    let delta = new_advance - original_advance;
    if delta.abs() > opts.max_width_delta_em * context.font_size.abs() {
        return Ok(Attempt {
            status: ReplacementStatus::SkippedLayout,
            report: AttemptReport {
                original_text,
                original_advance: Some(original_advance),
                new_advance: Some(new_advance),
                scanned_pages: scan.scanned_pages,
                scan_incomplete: scan.incomplete,
                ..AttemptReport::default()
            },
            risks,
            gaps,
        });
    }
    let tj_delta = delta * 1000.0 / (context.font_size * context.horizontal_scale);
    let first = selected.glyphs.first().unwrap();
    let last = selected.glyphs.last().unwrap();
    let first_key = (first.op_index, first.operand_index);
    if selected
        .glyphs
        .iter()
        .any(|glyph| (glyph.op_index, glyph.operand_index) != first_key)
    {
        return Ok(Attempt {
            status: ReplacementStatus::SkippedUnsupportedText,
            report: AttemptReport {
                original_text,
                original_advance: Some(original_advance),
                new_advance: Some(new_advance),
                tj_delta: Some(tj_delta),
                scanned_pages: scan.scanned_pages,
                scan_incomplete: scan.incomplete,
                ..AttemptReport::default()
            },
            risks,
            gaps,
        });
    }
    let new_bytes = chosen
        .iter()
        .flat_map(|c| c.bytes.clone())
        .collect::<Vec<_>>();
    let Some(rewritten) = rewrite_selected(
        &cache.pages[page_index]
            .as_ref()
            .expect("page was inserted or cached")
            .content
            .operations,
        first.op_index,
        first.operand_index,
        first.byte_offset,
        last.byte_offset + usize::from(last.code_len),
        &new_bytes,
        tj_delta,
    ) else {
        return Ok(Attempt {
            status: ReplacementStatus::SkippedUnsupportedText,
            report: AttemptReport {
                original_text,
                original_advance: Some(original_advance),
                new_advance: Some(new_advance),
                tj_delta: Some(tj_delta),
                scanned_pages: scan.scanned_pages,
                scan_incomplete: scan.incomplete,
                ..AttemptReport::default()
            },
            risks,
            gaps,
        });
    };
    let encoded = Content {
        operations: rewritten,
    };
    let Ok(encoded_bytes) = encoded.encode() else {
        return Ok(rejected(
            ReplacementStatus::SkippedContent,
            AttemptReport::default(),
        ));
    };
    let old_contents = doc.get_page_contents(page_id);
    let old_contents_object = doc
        .get_dictionary(page_id)
        .ok()
        .and_then(|page| page.get(b"Contents").ok())
        .cloned();
    let new_stream = doc.add_object({
        let mut stream = Stream::new(dictionary! {}, encoded_bytes.clone());
        let _ = stream.compress();
        stream
    });
    set_contents(doc, page_id, Object::Reference(new_stream));

    let verified = Content::decode(&encoded_bytes)
        .ok()
        .and_then(|after_content| interpret_content(doc, page_id, &after_content).ok())
        .is_some_and(|after| {
            verify_suffix(
                &cache.pages[page_index]
                    .as_ref()
                    .expect("page was inserted or cached")
                    .text,
                &after,
                selected.start,
                selected.end,
                replacement.new_text.chars().count(),
            )
        });
    if !verified {
        if let Some(previous) = old_contents_object {
            set_contents(doc, page_id, previous);
        }
        doc.objects.remove(&new_stream);
        return Ok(Attempt {
            status: ReplacementStatus::SkippedVerification,
            report: AttemptReport {
                replaced_glyphs: selected.glyphs.len(),
                original_text,
                original_advance: Some(original_advance),
                new_advance: Some(new_advance),
                tj_delta: Some(tj_delta),
                scanned_pages: scan.scanned_pages,
                scan_incomplete: scan.incomplete,
            },
            risks,
            gaps,
        });
    }
    for old in old_contents {
        if !is_referenced(doc, old) {
            doc.objects.remove(&old);
        }
    }
    Ok(Attempt {
        status: ReplacementStatus::Replaced,
        report: AttemptReport {
            replaced_glyphs: selected.glyphs.len(),
            original_text,
            original_advance: Some(original_advance),
            new_advance: Some(new_advance),
            tj_delta: Some(tj_delta),
            scanned_pages: scan.scanned_pages,
            scan_incomplete: scan.incomplete,
        },
        risks,
        gaps,
    })
}

fn rejected(status: ReplacementStatus, report: AttemptReport) -> Attempt {
    Attempt {
        status,
        report,
        risks: Vec::new(),
        gaps: Vec::new(),
    }
}

#[derive(Debug)]
struct Selected {
    start: usize,
    end: usize,
    font_key: usize,
    glyphs: Vec<Glyph>,
    original_text: Option<String>,
}

fn select_sequence(
    page: &PageInfo,
    region: &TextRegion,
) -> Result<Option<Selected>, ReplacementStatus> {
    let hits: Vec<usize> = page
        .text
        .glyphs
        .iter()
        .enumerate()
        .filter(|(_, glyph)| probe_point(glyph, region))
        .map(|(index, _)| index)
        .collect();
    if hits.is_empty() {
        return Ok(None);
    }
    if hits.windows(2).any(|pair| pair[1] != pair[0] + 1) {
        return Err(ReplacementStatus::SkippedUnsupportedText);
    }
    let glyphs: Vec<Glyph> = hits
        .iter()
        .map(|&index| page.text.glyphs[index].clone())
        .collect();
    if glyphs
        .iter()
        .any(|glyph| !matches!(glyph.render_mode, 0..=2) || !is_upright(glyph))
    {
        return Err(ReplacementStatus::SkippedUnsupportedText);
    }
    let first = glyphs.first().unwrap();
    let last = glyphs.last().unwrap();
    if glyphs
        .iter()
        .any(|glyph| (glyph.origin.1 - first.origin.1).abs() > 0.01)
    {
        return Err(ReplacementStatus::SkippedUnsupportedText);
    }
    let Some(font) = page.fonts.get(&first.font_res_name) else {
        return Err(ReplacementStatus::SkippedUnsupportedFont);
    };
    if page.text.unsupported.iter().any(|unsupported| {
        unsupported.op_index >= first.op_index && unsupported.op_index <= last.op_index
    }) {
        return Err(ReplacementStatus::SkippedUnsupportedText);
    }
    if glyphs.iter().any(|glyph| {
        page.fonts
            .get(&glyph.font_res_name)
            .is_none_or(|other| other.key != font.key)
    }) {
        return Err(ReplacementStatus::SkippedUnsupportedText);
    }
    if glyphs
        .iter()
        .any(|glyph| glyph.op_index != first.op_index || glyph.operand_index != first.operand_index)
    {
        return Err(ReplacementStatus::SkippedUnsupportedText);
    }
    let mut text = String::new();
    for glyph in &glyphs {
        let bytes = glyph_bytes(&page.content, glyph).ok_or(ReplacementStatus::SkippedContent)?;
        let Some(unicode) = font.mapping.get(&bytes) else {
            return Err(ReplacementStatus::SkippedAmbiguousMapping);
        };
        let mut chars = unicode.chars();
        let Some(ch) = chars.next() else {
            return Err(ReplacementStatus::SkippedAmbiguousMapping);
        };
        if chars.next().is_some() || font.ambiguous.contains(&ch) {
            return Err(ReplacementStatus::SkippedAmbiguousMapping);
        }
        text.push(ch);
    }
    Ok(Some(Selected {
        start: hits[0],
        end: *hits.last().unwrap(),
        font_key: font.key,
        glyphs,
        original_text: Some(text),
    }))
}

#[derive(Debug, Default)]
struct ScanResult {
    codes: HashMap<char, Vec<CodeCandidate>>,
    scanned_pages: usize,
    complete: bool,
    incomplete: bool,
}

fn inventory_codes(
    content: &Content,
    text: &PageText,
    fonts: &HashMap<Vec<u8>, FontDefinition>,
    wanted_key: usize,
    inventory: &mut HashMap<char, Vec<CodeCandidate>>,
) {
    for glyph in &text.glyphs {
        if !matches!(glyph.render_mode, 0..=2) || !is_upright(glyph) {
            continue;
        }
        let Some(font) = fonts.get(&glyph.font_res_name) else {
            continue;
        };
        if font.key != wanted_key {
            continue;
        }
        let Some(bytes) = glyph_bytes(content, glyph) else {
            continue;
        };
        let Some(unicode) = font.mapping.get(&bytes) else {
            continue;
        };
        let mut chars = unicode.chars();
        let Some(ch) = chars.next() else {
            continue;
        };
        if chars.next().is_some() || font.ambiguous.contains(&ch) {
            continue;
        }
        let values = inventory.entry(ch).or_default();
        if !values.iter().any(|candidate| candidate.bytes == bytes) {
            values.push(CodeCandidate {
                bytes,
                font_width: glyph.font_width,
            });
        }
    }
}

fn rewrite_selected(
    operations: &[Operation],
    op_index: usize,
    operand_index: usize,
    start: usize,
    end: usize,
    new_bytes: &[u8],
    tj_delta: f64,
) -> Option<Vec<Operation>> {
    let mut output = Vec::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        if index != op_index {
            output.push(operation.clone());
            continue;
        }
        if matches!(operation.operator.as_str(), "'" | "\"") {
            return None;
        }
        match operation.operator.as_str() {
            "Tj" => {
                let Object::String(bytes, format) = operation.operands.first()? else {
                    return None;
                };
                if operand_index != 0 || end > bytes.len() {
                    return None;
                }
                let mut array = Vec::new();
                push_string(&mut array, &bytes[..start], *format);
                array.push(Object::String(new_bytes.to_vec(), *format));
                push_delta(&mut array, tj_delta);
                push_string(&mut array, &bytes[end..], *format);
                output.push(Operation::new("TJ", vec![Object::Array(array)]));
            }
            "TJ" => {
                let Object::Array(items) = operation.operands.first()? else {
                    return None;
                };
                let Object::String(bytes, format) = items.get(operand_index)? else {
                    return None;
                };
                if end > bytes.len() {
                    return None;
                }
                let mut replacement = Vec::new();
                push_string(&mut replacement, &bytes[..start], *format);
                replacement.push(Object::String(new_bytes.to_vec(), *format));
                push_delta(&mut replacement, tj_delta);
                push_string(&mut replacement, &bytes[end..], *format);
                let mut array = Vec::new();
                for (i, item) in items.iter().enumerate() {
                    if i == operand_index {
                        array.extend(replacement.iter().cloned());
                    } else {
                        array.push(item.clone());
                    }
                }
                output.push(Operation::new("TJ", vec![Object::Array(array)]));
            }
            _ => return None,
        }
    }
    Some(output)
}

fn push_string(array: &mut Vec<Object>, bytes: &[u8], format: StringFormat) {
    if !bytes.is_empty() {
        array.push(Object::String(bytes.to_vec(), format));
    }
}

fn push_delta(array: &mut Vec<Object>, delta: f64) {
    if delta.abs() > f64::EPSILON {
        array.push(Object::Real(delta as f32));
    }
}

fn glyph_bytes(content: &Content, glyph: &Glyph) -> Option<Vec<u8>> {
    let operation = content.operations.get(glyph.op_index)?;
    let item = if operation.operator == "TJ" {
        match operation.operands.first()? {
            Object::Array(items) => items.get(glyph.operand_index)?,
            _ => return None,
        }
    } else {
        operation.operands.get(glyph.operand_index)?
    };
    let Object::String(bytes, _) = item else {
        return None;
    };
    bytes
        .get(glyph.byte_offset..glyph.byte_offset + usize::from(glyph.code_len))
        .map(ToOwned::to_owned)
}

fn verify_suffix(
    before: &PageText,
    after: &PageText,
    start: usize,
    end: usize,
    new_count: usize,
) -> bool {
    if before.glyphs.len() < end + 1 || after.glyphs.len() < start + new_count {
        return true;
    }
    let suffix = &before.glyphs[end + 1..];
    let after_suffix_start = start + new_count;
    if after.glyphs.len() < after_suffix_start + suffix.len() {
        return false;
    }
    suffix
        .iter()
        .zip(&after.glyphs[after_suffix_start..])
        .all(|(old, new)| {
            old.code == new.code
                && old.code_len == new.code_len
                && old.font_res_name == new.font_res_name
                && old.render_mode == new.render_mode
                && (old.origin.0 - new.origin.0).abs() <= 0.01
                && (old.origin.1 - new.origin.1).abs() <= 0.01
        })
}

fn font_definitions(
    doc: &Document,
    page_id: ObjectId,
) -> Result<HashMap<Vec<u8>, FontDefinition>, EditError> {
    let fonts = doc
        .get_page_fonts(page_id)
        .map_err(|e| EditError::Parse(e.to_string()))?;
    let mut output = HashMap::new();
    for (name, dict) in fonts {
        let code_len = if dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .is_ok_and(|subtype| subtype == b"Type0")
        {
            2
        } else {
            1
        };
        let mut mapping = HashMap::new();
        if let Ok(tounicode) = dict.get(b"ToUnicode") {
            if let Ok((_, Object::Stream(stream))) = doc.dereference(tounicode) {
                mapping = parse_tounicode(&stream.content);
            }
        }
        let has_declared_encoding = dict.get(b"Encoding").is_ok();
        if mapping.is_empty()
            && code_len == 1
            && !(is_symbolic_font(doc, dict) && !has_declared_encoding)
        {
            mapping = simple_mapping(doc, dict);
        }
        let mut by_char: HashMap<char, Vec<Vec<u8>>> = HashMap::new();
        for (code, text) in &mapping {
            let mut chars = text.chars();
            if let Some(ch) = chars.next() {
                if chars.next().is_none() {
                    by_char.entry(ch).or_default().push(code.clone());
                }
            }
        }
        let ambiguous = by_char
            .into_iter()
            .filter_map(|(ch, codes)| (codes.len() > 1).then_some(ch))
            .collect();
        let key = std::ptr::from_ref(dict) as usize;
        output.insert(
            name,
            FontDefinition {
                key,
                mapping,
                ambiguous,
            },
        );
    }
    Ok(output)
}

fn is_symbolic_font(doc: &Document, dict: &lopdf::Dictionary) -> bool {
    let Some(descriptor) = dict
        .get(b"FontDescriptor")
        .ok()
        .and_then(|object| doc.dereference(object).ok().map(|(_, value)| value))
        .and_then(|object| object.as_dict().ok())
    else {
        return false;
    };
    descriptor
        .get(b"Flags")
        .ok()
        .and_then(|object| object.as_i64().ok())
        .is_some_and(|flags| flags & 4 != 0)
}

fn parse_tounicode(data: &[u8]) -> HashMap<Vec<u8>, String> {
    let tokens = cmap_tokens(data);
    let mut output = HashMap::new();
    let mut index = 0;
    while index < tokens.len() {
        let Some(Tok::Word(word)) = tokens.get(index) else {
            index += 1;
            continue;
        };
        if word == "beginbfchar" {
            index += 1;
            while index + 1 < tokens.len() && !is_word(&tokens[index], "endbfchar") {
                if let (Some(src), Some(dst)) = (hex(&tokens[index]), hex(&tokens[index + 1])) {
                    if let Some(text) = utf16(dst) {
                        output.insert(src, text);
                    }
                    index += 2;
                } else {
                    index += 1;
                }
            }
        } else if word == "beginbfrange" {
            index += 1;
            while index + 2 < tokens.len() && !is_word(&tokens[index], "endbfrange") {
                let (Some(low), Some(high)) = (hex(&tokens[index]), hex(&tokens[index + 1])) else {
                    index += 1;
                    continue;
                };
                let start = index + 2;
                if matches!(tokens.get(start), Some(Tok::LBracket)) {
                    let mut code = number_from_bytes(&low);
                    let end = number_from_bytes(&high);
                    let mut cursor = start + 1;
                    while cursor < tokens.len() && !matches!(tokens[cursor], Tok::RBracket) {
                        if let Some(dst) = hex(&tokens[cursor]) {
                            if code <= end {
                                if let Some(text) = utf16(dst) {
                                    output.insert(bytes_from_number(code, low.len()), text);
                                }
                                code += 1;
                            }
                        }
                        cursor += 1;
                    }
                    index = cursor + 1;
                } else if let Some(dst) = tokens.get(start).and_then(hex) {
                    let end = number_from_bytes(&high);
                    let mut code = number_from_bytes(&low);
                    let base = utf16(dst);
                    while code <= end {
                        if let Some(text) = base.as_ref().and_then(|text| {
                            increment_last_scalar(text, code - number_from_bytes(&low))
                        }) {
                            output.insert(bytes_from_number(code, low.len()), text);
                        }
                        code += 1;
                    }
                    index += 3;
                } else {
                    index += 1;
                }
            }
        } else {
            index += 1;
        }
    }
    output
}

#[derive(Debug)]
enum Tok {
    Word(String),
    Hex(Vec<u8>),
    LBracket,
    RBracket,
}

fn cmap_tokens(data: &[u8]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < data.len() {
        match data[index] {
            b'<' if data.get(index + 1) == Some(&b'<') => index += 2,
            b'<' if data.get(index + 1) != Some(&b'<') => {
                let start = index + 1;
                index = start;
                while index < data.len() && data[index] != b'>' {
                    index += 1;
                }
                if let Ok(bytes) = hex_decode(&data[start..index]) {
                    out.push(Tok::Hex(bytes));
                }
                index = index.saturating_add(1);
            }
            b'[' => {
                out.push(Tok::LBracket);
                index += 1;
            }
            b']' => {
                out.push(Tok::RBracket);
                index += 1;
            }
            byte if byte.is_ascii_whitespace() => index += 1,
            _ => {
                let start = index;
                while index < data.len()
                    && !data[index].is_ascii_whitespace()
                    && !matches!(data[index], b'[' | b']' | b'<')
                {
                    index += 1;
                }
                out.push(Tok::Word(
                    String::from_utf8_lossy(&data[start..index]).into_owned(),
                ));
            }
        }
    }
    out
}

fn hex_decode(bytes: &[u8]) -> Result<Vec<u8>, ()> {
    let mut nibbles = bytes
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if nibbles.len() % 2 == 1 {
        nibbles.push(b'0');
    }
    let mut out = Vec::with_capacity(nibbles.len() / 2);
    for pair in nibbles.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16).ok_or(())?;
        let lo = (pair[1] as char).to_digit(16).ok_or(())?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

fn hex(token: &Tok) -> Option<Vec<u8>> {
    match token {
        Tok::Hex(bytes) => Some(bytes.clone()),
        _ => None,
    }
}

fn is_word(token: &Tok, word: &str) -> bool {
    matches!(token, Tok::Word(value) if value == word)
}

fn utf16(bytes: Vec<u8>) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let values = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]));
    Some(char::decode_utf16(values).filter_map(Result::ok).collect())
}

fn increment_last_scalar(text: &str, delta: u32) -> Option<String> {
    let mut chars = text.chars().collect::<Vec<_>>();
    let last = chars.pop()? as u32;
    chars.push(char::from_u32(last.checked_add(delta)?)?);
    Some(chars.into_iter().collect())
}

fn number_from_bytes(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0u32, |value, byte| (value << 8) | u32::from(*byte))
}

fn bytes_from_number(mut number: u32, len: usize) -> Vec<u8> {
    let mut out = vec![0; len];
    for byte in out.iter_mut().rev() {
        *byte = number as u8;
        number >>= 8;
    }
    out
}

fn simple_mapping(doc: &Document, dict: &lopdf::Dictionary) -> HashMap<Vec<u8>, String> {
    let encoding = dict
        .get(b"Encoding")
        .ok()
        .and_then(|object| doc.dereference(object).ok().map(|(_, value)| value));
    let name: &[u8] = match encoding {
        Some(Object::Name(name)) => name,
        Some(Object::Dictionary(dict)) => dict
            .get(b"BaseEncoding")
            .ok()
            .and_then(|object| object.as_name().ok())
            .unwrap_or(b"StandardEncoding"),
        _ => b"StandardEncoding",
    };
    let mut mapping = HashMap::new();
    for code in 0u16..=255 {
        let ch = match name {
            b"WinAnsiEncoding" => winansi(code as u8),
            b"MacRomanEncoding" => macroman(code as u8),
            _ => standard(code as u8),
        };
        if let Some(ch) = ch {
            mapping.insert(vec![code as u8], ch.to_string());
        }
    }
    if let Some(Object::Dictionary(dict)) = encoding {
        if let Ok(Object::Array(items)) = dict.get(b"Differences") {
            let mut code = None;
            for item in items {
                match item {
                    Object::Integer(value) => code = (*value).try_into().ok(),
                    Object::Name(name) => {
                        if let Some(current) = code {
                            if let Some(ch) = glyph_name_char(name) {
                                mapping.insert(vec![current], ch.to_string());
                            }
                            code = current.checked_add(1);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    mapping
}

fn winansi(code: u8) -> Option<char> {
    if code.is_ascii() && code >= 0x20 {
        return Some(code as char);
    }
    const EXTRA: &[(u8, char)] = &[
        (0x80, '€'),
        (0x82, '‚'),
        (0x83, 'ƒ'),
        (0x84, '„'),
        (0x85, '…'),
        (0x86, '†'),
        (0x87, '‡'),
        (0x89, '‰'),
        (0x8a, 'Š'),
        (0x8b, '‹'),
        (0x8c, 'Œ'),
        (0x8e, 'Ž'),
        (0x91, '‘'),
        (0x92, '’'),
        (0x93, '“'),
        (0x94, '”'),
        (0x95, '•'),
        (0x96, '–'),
        (0x97, '—'),
        (0x98, '˜'),
        (0x99, '™'),
        (0x9a, 'š'),
        (0x9b, '›'),
        (0x9c, 'œ'),
        (0x9e, 'ž'),
        (0x9f, 'Ÿ'),
    ];
    EXTRA
        .iter()
        .find_map(|(value, ch)| (*value == code).then_some(*ch))
        .or_else(|| (code >= 0xa0).then_some(code as char))
}

fn macroman(code: u8) -> Option<char> {
    if code.is_ascii() && code >= 0x20 {
        return Some(code as char);
    }
    const MAC_ROMAN: &str =
        "ÄÅÇÉÑÖÜáàâäãåçéèêëíìîïñóòôöõúùûü†°¢£§•¶ß®©™´¨≠ÆØ∞±≤≥¥µ∂ΣΠπ∫ªºΩæø¿¡¬√ƒ≈∆«»… ÀÃÕŒœ–—“”‘’÷◊ÿŸ⁄€‹›ﬁﬂ‡·‚„‰ÂÊÁËÈÍÎÏÌÓÔÒÚÛÙıˆ˜¯˘˙˚¸˝˛ˇ";
    MAC_ROMAN
        .chars()
        .nth(usize::from(code.saturating_sub(0x80)))
}

fn standard(code: u8) -> Option<char> {
    if code.is_ascii() && code >= 0x20 {
        Some(code as char)
    } else {
        None
    }
}

fn glyph_name_char(name: &[u8]) -> Option<char> {
    let value = String::from_utf8_lossy(name);
    if value.len() == 1 {
        return value.chars().next();
    }
    match value.as_ref() {
        "space" => Some(' '),
        "zero" => Some('0'),
        "one" => Some('1'),
        "two" => Some('2'),
        "three" => Some('3'),
        "four" => Some('4'),
        "five" => Some('5'),
        "six" => Some('6'),
        "seven" => Some('7'),
        "eight" => Some('8'),
        "nine" => Some('9'),
        "hyphen" => Some('-'),
        "period" => Some('.'),
        "comma" => Some(','),
        "slash" => Some('/'),
        "colon" => Some(':'),
        "semicolon" => Some(';'),
        "question" => Some('?'),
        "exclam" => Some('!'),
        "Aacute" => Some('Á'),
        "aacute" => Some('á'),
        "Eacute" => Some('É'),
        "eacute" => Some('é'),
        "Iacute" => Some('Í'),
        "iacute" => Some('í'),
        "Oacute" => Some('Ó'),
        "oacute" => Some('ó'),
        "Uacute" => Some('Ú'),
        "uacute" => Some('ú'),
        "Ntilde" => Some('Ñ'),
        "ntilde" => Some('ñ'),
        _ => None,
    }
}

fn probe_point(glyph: &Glyph, region: &TextRegion) -> bool {
    let (bx, by) = glyph.baseline_dir;
    let (ux, uy) = glyph.ascent_dir;
    let along = glyph.advance / 2.0;
    let up = 0.3 * glyph.font_size_eff;
    let point = (
        glyph.origin.0 + bx * along + ux * up,
        glyph.origin.1 + by * along + uy * up,
    );
    point.0 >= region.x
        && point.0 <= region.x + region.width
        && point.1 >= region.y
        && point.1 <= region.y + region.height
}

fn is_upright(glyph: &Glyph) -> bool {
    const EPSILON: f64 = 1e-3;
    (glyph.baseline_dir.0 - 1.0).abs() < EPSILON
        && glyph.baseline_dir.1.abs() < EPSILON
        && glyph.ascent_dir.0.abs() < EPSILON
        && (glyph.ascent_dir.1 - 1.0).abs() < EPSILON
}

fn valid_region(region: &TextRegion) -> bool {
    !region.id.is_empty()
        && [region.x, region.y, region.width, region.height]
            .iter()
            .all(|value| value.is_finite())
        && region.width > 0.0
        && region.height > 0.0
}

fn inspect_page(
    doc: &Document,
    page_id: ObjectId,
    replacement: &TextReplacement,
    content: &Content,
    meter: &mut BudgetMeter,
) -> (Vec<ResidualRisk>, Vec<InspectionGap>) {
    let page = replacement.region.page;
    let mut inspector = Inspector::new(doc, meter);
    let resources = inspector.resources(page_id, page);
    inspector.inspect_marked_content(content, &resources, page);
    inspector.inspect_form_xobjects(content, &resources, page);
    if let Ok(page_dict) = doc.get_dictionary(page_id) {
        inspector.inspect_annotations(page_dict, page, &[&replacement.region]);
    } else {
        inspector.gap(GapReason::BrokenReference, Some(page), "page dictionary");
    }
    inspector.inspect_shared_content(
        doc.get_page_contents(page_id).as_slice(),
        page,
        &doc.get_pages(),
    );
    (inspector.risks, inspector.gaps)
}

fn has_semantic_risk(doc: &Document, content: &Content, risks: &[ResidualRisk]) -> bool {
    if risks.iter().any(|risk| {
        matches!(
            risk,
            ResidualRisk::ActualText { .. }
                | ResidualRisk::Annotation { .. }
                | ResidualRisk::OptionalContent { .. }
        )
    }) {
        return true;
    }
    if content
        .operations
        .iter()
        .any(|operation| matches!(operation.operator.as_str(), "BMC" | "BDC"))
    {
        return true;
    }
    let Some(root) = doc
        .trailer
        .get(b"Root")
        .ok()
        .and_then(|root| doc.dereference(root).ok().map(|(_, value)| value))
    else {
        return false;
    };
    root.as_dict()
        .ok()
        .is_some_and(|catalog| catalog.has(b"StructTreeRoot"))
}

fn scan_order(page: u32, page_count: u32) -> Vec<u32> {
    let mut order = Vec::with_capacity(page_count as usize);
    order.push(page);
    for distance in 1..page_count {
        if page + distance < page_count {
            order.push(page + distance);
        }
        if page >= distance {
            order.push(page - distance);
        }
    }
    order
}

fn plain_page_geometry(doc: &Document, page_id: ObjectId) -> bool {
    let rotate: i64 = inherited(doc, page_id, b"Rotate")
        .and_then(|object| object.as_i64().ok())
        .unwrap_or(0);
    if rotate.rem_euclid(360) != 0 {
        return false;
    }
    let Some(media) = inherited(doc, page_id, b"MediaBox").and_then(lower_left) else {
        return false;
    };
    if media.0.abs() > 0.01 || media.1.abs() > 0.01 {
        return false;
    }
    inherited(doc, page_id, b"CropBox")
        .map(|object| {
            lower_left(object).is_some_and(|point| point.0.abs() <= 0.01 && point.1.abs() <= 0.01)
        })
        .unwrap_or(true)
}

fn inherited<'a>(doc: &'a Document, page_id: ObjectId, name: &[u8]) -> Option<&'a Object> {
    let mut current = Some(page_id);
    let mut visited = HashSet::new();
    while let Some(id) = current {
        if !visited.insert(id) {
            return None;
        }
        let node = doc.get_dictionary(id).ok()?;
        if let Ok(value) = node.get(name) {
            return doc.dereference(value).ok().map(|(_, value)| value);
        }
        current = node
            .get(b"Parent")
            .ok()
            .and_then(|value| value.as_reference().ok());
    }
    None
}

fn lower_left(object: &Object) -> Option<(f64, f64)> {
    let values = object.as_array().ok()?;
    if values.len() != 4 {
        return None;
    }
    let number = |value: &Object| match value {
        Object::Integer(value) => Some(*value as f64),
        Object::Real(value) => Some(f64::from(*value)),
        _ => None,
    };
    Some((number(&values[0])?, number(&values[1])?))
}

fn set_contents(doc: &mut Document, page_id: ObjectId, contents: Object) {
    if let Ok(page) = doc.get_dictionary_mut(page_id) {
        page.set("Contents", contents);
    }
}

fn is_referenced(doc: &Document, target: ObjectId) -> bool {
    doc.objects
        .values()
        .any(|object| references(object, target))
}

fn references(object: &Object, target: ObjectId) -> bool {
    match object {
        Object::Reference(id) => *id == target,
        Object::Array(items) => items.iter().any(|item| references(item, target)),
        Object::Dictionary(dict) => dict.iter().any(|(_, value)| references(value, target)),
        Object::Stream(stream) => stream
            .dict
            .iter()
            .any(|(_, value)| references(value, target)),
        _ => false,
    }
}

fn dedup_sort<T: Ord>(values: &mut Vec<T>) {
    values.sort();
    values.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use lopdf::{dictionary, Object};

    fn region(id: &str, page: u32, x: f64, y: f64, width: f64, height: f64) -> TextRegion {
        TextRegion {
            id: id.into(),
            page,
            x,
            y,
            width,
            height,
        }
    }

    fn to_unicode(fx: &mut Fixture, entries: &[(&str, &str)]) -> Object {
        let mut body = format!("{} beginbfchar\n", entries.len()).into_bytes();
        for (code, unicode) in entries {
            body.extend_from_slice(format!(" <{code}> <{unicode}>\n").as_bytes());
        }
        body.extend_from_slice(b"endbfchar\n");
        fx.doc
            .add_object(lopdf::Stream::new(dictionary! {}, body))
            .into()
    }

    fn font(fx: &mut Fixture, base: &str, unicode: Object, width_count: usize) -> Object {
        let widths = vec![500.into(); width_count];
        fx.doc
            .add_object(dictionary! {
                "Type" => "Font",
                "Subtype" => "TrueType",
                "BaseFont" => base,
                "FirstChar" => 48,
                "Widths" => widths,
                "ToUnicode" => unicode,
            })
            .into()
    }

    fn scope_fixture() -> (Vec<u8>, Vec<u8>) {
        let mut fx = Fixture::new();
        let f1_unicode = to_unicode(&mut fx, &[("30", "0030"), ("31", "0031")]);
        let f1 = font(&mut fx, "SubsetFont", f1_unicode, 2);
        let f2_unicode = to_unicode(&mut fx, &[("30", "0030"), ("31", "0031"), ("32", "0032")]);
        let f2 = font(&mut fx, "SubsetFont", f2_unicode, 3);
        let safe_resources = Some(dictionary! { "Font" => dictionary! { "F1" => f1.clone() } });
        let shared = fx.content_stream(
            dictionary! {},
            b"BT /F1 10 Tf 1 0 0 1 100 700 Tm [(01) 40 (1)] TJ ET",
        );
        fx.add_page(shared, safe_resources.clone(), vec![]);
        fx.add_page(shared, safe_resources, vec![]);
        let other = fx.content_stream(
            dictionary! {},
            b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (02) Tj ET",
        );
        fx.add_page(
            other,
            Some(dictionary! { "Font" => dictionary! { "F1" => f2.clone() } }),
            vec![],
        );
        let semantics = fx.content_stream(
            dictionary! {},
            b"BT /F1 10 Tf 1 0 0 1 100 700 Tm /Span <</ActualText (01)>> BDC (01) Tj EMC ET",
        );
        fx.add_page(
            semantics,
            Some(dictionary! { "Font" => dictionary! { "F1" => f1.clone() } }),
            vec![],
        );
        let clipping = fx.content_stream(
            dictionary! {},
            b"BT 4 Tr /F1 10 Tf 1 0 0 1 100 700 Tm (01) Tj ET",
        );
        fx.add_page(
            clipping,
            Some(dictionary! { "Font" => dictionary! { "F1" => f1.clone() } }),
            vec![],
        );
        let mixed = fx.content_stream(
            dictionary! {},
            b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (0) Tj /F2 10 Tf (1) Tj ET",
        );
        fx.add_page(
            mixed,
            Some(dictionary! { "Font" => dictionary! { "F1" => f1, "F2" => f2 } }),
            vec![],
        );
        let page_one_before = fx.doc.get_page_content(fx.page_ids[1]).unwrap();
        (fx.bytes(), page_one_before)
    }

    fn replacement(id: &str, page: u32, new_text: &str) -> TextReplacement {
        TextReplacement {
            region: region(id, page, 99.0, 695.0, 12.0, 17.0),
            new_text: new_text.into(),
            expected_text: Some("01".into()),
        }
    }

    fn varying_width_fixture() -> Vec<u8> {
        let mut fx = Fixture::new();
        let unicode = to_unicode(&mut fx, &[("30", "0030"), ("31", "0031")]);
        let font = fx.doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "TrueType",
            "BaseFont" => "SubsetFont",
            "FirstChar" => 48,
            "Widths" => vec![50.into(), 500.into()],
            "ToUnicode" => unicode,
        });
        let content = fx.content_stream(
            dictionary! {},
            b"BT /F1 10 Tf 1 0 0 1 100 700 Tm [(01) 40 (1)] TJ ET",
        );
        fx.add_page(
            content,
            Some(dictionary! { "Font" => dictionary! { "F1" => font } }),
            vec![],
        );
        fx.bytes()
    }

    fn varying_replacement(new_text: &str) -> TextReplacement {
        TextReplacement {
            region: region("varying", 0, 99.0, 695.0, 5.0, 17.0),
            new_text: new_text.into(),
            expected_text: Some("01".into()),
        }
    }

    fn fontless_scan_fixture() -> Vec<u8> {
        let mut fx = Fixture::new();
        let large_content = vec![b' '; 128 * 1024];
        for _ in 0..3 {
            let content = fx.content_stream(dictionary! {}, &large_content);
            fx.add_page(content, None, vec![]);
        }

        let unicode = to_unicode(&mut fx, &[("30", "0030"), ("31", "0031")]);
        let target_font = font(&mut fx, "SubsetFont", unicode, 2);
        let target_content = fx.content_stream(
            dictionary! {},
            b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (01) Tj ET",
        );
        fx.add_page(
            target_content,
            Some(dictionary! { "Font" => dictionary! { "F1" => target_font } }),
            vec![],
        );

        for _ in 0..3 {
            let content = fx.content_stream(dictionary! {}, &large_content);
            fx.add_page(content, None, vec![]);
        }
        fx.bytes()
    }

    fn first_page_text(bytes: &[u8]) -> (Document, PageText) {
        let doc = Document::load_mem(bytes).unwrap();
        let page = doc.get_pages()[&1];
        let content = Content::decode(&doc.get_page_content(page).unwrap()).unwrap();
        let text = interpret_content(&doc, page, &content).unwrap();
        (doc, text)
    }

    fn tj_numbers(content: &Content) -> Vec<f64> {
        content
            .operations
            .iter()
            .filter(|operation| operation.operator == "TJ")
            .flat_map(|operation| operation.operands.iter())
            .filter_map(|operand| match operand {
                Object::Array(items) => Some(items),
                _ => None,
            })
            .flat_map(|items| items.iter())
            .filter_map(|item| match item {
                Object::Integer(value) => Some(*value as f64),
                Object::Real(value) => Some(f64::from(*value)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn cmap_parser_reads_bfchar() {
        let map = parse_tounicode(b"1 beginbfchar <0001> <0031> endbfchar");
        assert_eq!(map.get(&vec![0, 1]), Some(&"1".to_string()));
    }

    #[test]
    fn cmap_parser_skips_dictionary_delimiters() {
        let map = parse_tounicode(
            b"1 beginbfchar << /Registry (Adobe) /Ordering (UCS) >> <0001> <0031> endbfchar",
        );
        assert_eq!(map.get(&vec![0, 1]), Some(&"1".to_string()));
    }

    #[test]
    fn replacement_emits_negative_tj_for_shorter_text() {
        let input = varying_width_fixture();
        let result =
            replace_text_glyphs(&input, &[varying_replacement("1")], &EditOptions::default())
                .unwrap();
        assert_eq!(result.replacements[0].status, ReplacementStatus::Replaced);
        assert_eq!(result.replacements[0].original_advance, Some(5.5));
        assert_eq!(result.replacements[0].new_advance, Some(5.0));
        assert_eq!(result.replacements[0].tj_delta, Some(-50.0));

        let (_, before) = first_page_text(&input);
        let (output_doc, after) = first_page_text(&result.output);
        assert!((before.glyphs[2].origin.0 - after.glyphs[1].origin.0).abs() <= 0.01);
        let output_content = Content::decode(
            &output_doc
                .get_page_content(output_doc.get_pages()[&1])
                .unwrap(),
        )
        .unwrap();
        let numbers = tj_numbers(&output_content);
        assert!(numbers
            .iter()
            .any(|value| (*value + 50.0).abs() <= f64::EPSILON));
        assert!(numbers
            .iter()
            .any(|value| (*value - 40.0).abs() <= f64::EPSILON));
    }

    #[test]
    fn replacement_emits_positive_tj_for_longer_text() {
        let input = varying_width_fixture();
        let result = replace_text_glyphs(
            &input,
            &[varying_replacement("010")],
            &EditOptions::default(),
        )
        .unwrap();
        assert_eq!(result.replacements[0].status, ReplacementStatus::Replaced);
        assert_eq!(result.replacements[0].original_advance, Some(5.5));
        assert_eq!(result.replacements[0].new_advance, Some(6.0));
        assert_eq!(result.replacements[0].tj_delta, Some(50.0));
        let output_doc = Document::load_mem(&result.output).unwrap();
        let output_content = Content::decode(
            &output_doc
                .get_page_content(output_doc.get_pages()[&1])
                .unwrap(),
        )
        .unwrap();
        let numbers = tj_numbers(&output_content);
        assert!(numbers
            .iter()
            .any(|value| (*value - 50.0).abs() <= f64::EPSILON));
    }

    #[test]
    fn layout_limit_rejects_over_threshold() {
        let input = varying_width_fixture();
        let options = EditOptions {
            max_width_delta_em: 0.04,
            ..EditOptions::default()
        };
        let result = replace_text_glyphs(&input, &[varying_replacement("1")], &options).unwrap();
        assert_eq!(
            result.replacements[0].status,
            ReplacementStatus::SkippedLayout
        );
        assert_eq!(result.output, input);
        assert!(!result.modified);
    }

    #[test]
    fn layout_limit_accepts_just_below_threshold() {
        let input = varying_width_fixture();
        let options = EditOptions {
            max_width_delta_em: 0.06,
            ..EditOptions::default()
        };
        let result = replace_text_glyphs(&input, &[varying_replacement("1")], &options).unwrap();
        assert_eq!(result.replacements[0].status, ReplacementStatus::Replaced);
        assert_eq!(result.replacements[0].tj_delta, Some(-50.0));
        assert!(result.modified);
    }

    #[test]
    fn stale_selection_is_byte_identical() {
        let input = varying_width_fixture();
        let replacement = TextReplacement {
            expected_text: Some("00".into()),
            ..varying_replacement("1")
        };
        let result = replace_text_glyphs(&input, &[replacement], &EditOptions::default()).unwrap();
        assert_eq!(
            result.replacements[0].status,
            ReplacementStatus::SkippedStaleSelection
        );
        assert_eq!(result.replacements[0].original_text, Some("01".into()));
        assert_eq!(result.output, input);
        assert!(!result.modified);
    }

    #[test]
    fn scan_skips_large_pages_without_the_selected_font_and_reuses_cache() {
        let input = fontless_scan_fixture();
        let replacements = (0..3)
            .map(|index| TextReplacement {
                region: region(&format!("missing-{index}"), 3, 99.0, 695.0, 12.0, 17.0),
                new_text: "2".into(),
                expected_text: Some("01".into()),
            })
            .collect::<Vec<_>>();
        let options = EditOptions {
            budget: crate::options::ObjectBudget {
                max_streams: 1,
                max_total_decompressed_bytes: 1024,
                ..crate::options::ObjectBudget::default()
            },
            ..EditOptions::default()
        };

        let result = replace_text_glyphs(&input, &replacements, &options).unwrap();

        assert_eq!(result.output, input);
        assert!(!result.modified);
        for report in &result.replacements {
            assert_eq!(report.status, ReplacementStatus::SkippedNoReusableCode);
            assert_eq!(report.scanned_pages, 1);
            assert!(report.scan_incomplete);
        }
    }

    #[test]
    fn scope_fence_covers_the_nine_required_assertions() {
        let (input, page_one_before) = scope_fixture();
        let replacements = vec![
            replacement("safe", 0, "10"),
            replacement("clip", 4, "10"),
            TextReplacement {
                expected_text: Some("10".into()),
                ..replacement("missing", 0, "2")
            },
            replacement("semantics", 3, "10"),
            replacement("mixed", 5, "10"),
        ];
        let result = replace_text_glyphs(&input, &replacements, &EditOptions::default()).unwrap();

        assert_eq!(result.replacements[0].status, ReplacementStatus::Replaced);
        assert_eq!(
            result.replacements[1].status,
            ReplacementStatus::SkippedUnsupportedText
        );
        assert_eq!(
            result.replacements[2].status,
            ReplacementStatus::SkippedNoReusableCode
        );
        assert_ne!(result.replacements[2].status, ReplacementStatus::Replaced);
        assert_eq!(
            result.replacements[3].status,
            ReplacementStatus::SkippedSemantics
        );
        assert_eq!(
            result.replacements[4].status,
            ReplacementStatus::SkippedUnsupportedText
        );

        let output_doc = Document::load_mem(&result.output).unwrap();
        let input_doc = Document::load_mem(&input).unwrap();
        assert_eq!(
            input_doc
                .get_page_content(input_doc.get_pages()[&(2)])
                .unwrap(),
            output_doc
                .get_page_content(output_doc.get_pages()[&(2)])
                .unwrap()
        );
        let before = input_doc
            .get_page_content(input_doc.get_pages()[&(1)])
            .unwrap();
        let after = output_doc
            .get_page_content(output_doc.get_pages()[&(1)])
            .unwrap();
        assert_ne!(before, after);
        assert_eq!(
            page_one_before,
            output_doc
                .get_page_content(output_doc.get_pages()[&(2)])
                .unwrap()
        );
        let other_definition = input_doc
            .get_page_content(input_doc.get_pages()[&(3)])
            .unwrap();
        assert!(other_definition
            .windows(4)
            .any(|bytes| bytes == b"(02)".as_slice()));
        let mixed_before = input_doc
            .get_page_content(input_doc.get_pages()[&(6)])
            .unwrap();
        let mixed_after = output_doc
            .get_page_content(output_doc.get_pages()[&(6)])
            .unwrap();
        assert_eq!(mixed_before, mixed_after);

        let before_text = interpret_content(
            &input_doc,
            input_doc.get_pages()[&(1)],
            &Content::decode(&before).unwrap(),
        )
        .unwrap();
        let after_text = interpret_content(
            &output_doc,
            output_doc.get_pages()[&(1)],
            &Content::decode(&after).unwrap(),
        )
        .unwrap();
        assert!((before_text.glyphs[2].origin.0 - after_text.glyphs[2].origin.0).abs() <= 0.01);

        let operations = Content::decode(&after).unwrap().operations;
        assert!(operations.iter().any(|operation| {
            operation.operator == "TJ"
                && operation.operands.iter().any(|operand| {
                    matches!(operand, Object::Array(items) if items.iter().any(|item| match item {
                        Object::Integer(value) => *value == 40,
                        Object::Real(value) => f64::from(*value) == 40.0,
                        _ => false,
                    }))
                })
        }));
    }

    #[test]
    fn shared_stream_replacement_isolated_to_one_page() {
        let (input, _) = scope_fixture();
        let input_doc = Document::load_mem(&input).unwrap();
        let before_page_one = input_doc
            .get_page_content(input_doc.get_pages()[&(2)])
            .unwrap();
        let result = replace_text_glyphs(
            &input,
            &[replacement("safe", 0, "10")],
            &EditOptions::default(),
        )
        .unwrap();
        assert_eq!(result.replacements[0].status, ReplacementStatus::Replaced);
        assert!(result
            .residual_risks
            .iter()
            .any(|risk| risk.kind() == "shared_content_stream"));
        let output_doc = Document::load_mem(&result.output).unwrap();
        assert_eq!(
            before_page_one,
            output_doc
                .get_page_content(output_doc.get_pages()[&(2)])
                .unwrap()
        );
        assert_ne!(
            input_doc
                .get_page_content(input_doc.get_pages()[&(1)])
                .unwrap(),
            output_doc
                .get_page_content(output_doc.get_pages()[&(1)])
                .unwrap()
        );
    }

    #[test]
    fn rejected_replacement_is_byte_identical() {
        let (input, _) = scope_fixture();
        let result = replace_text_glyphs(
            &input,
            &[replacement("missing", 0, "2")],
            &EditOptions::default(),
        )
        .unwrap();
        assert_eq!(
            result.replacements[0].status,
            ReplacementStatus::SkippedNoReusableCode
        );
        assert_eq!(result.output, input);
        assert!(!result.modified);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn replacement_report_golden() {
        let result = ReplacementResult {
            output: Vec::new(),
            replacements: vec![
                ReplacementReport {
                    id: "ok".into(),
                    page: 0,
                    status: ReplacementStatus::Replaced,
                    replaced_glyphs: 2,
                    original_text: Some("01".into()),
                    new_text: "10".into(),
                    original_advance: Some(10.0),
                    new_advance: Some(10.0),
                    tj_delta: Some(0.0),
                    scanned_pages: 1,
                    scan_incomplete: false,
                },
                ReplacementReport {
                    id: "missing".into(),
                    page: 0,
                    status: ReplacementStatus::SkippedNoReusableCode,
                    replaced_glyphs: 0,
                    original_text: Some("01".into()),
                    new_text: "2".into(),
                    original_advance: None,
                    new_advance: None,
                    tj_delta: None,
                    scanned_pages: 8,
                    scan_incomplete: true,
                },
                ReplacementReport {
                    id: "semantic".into(),
                    page: 3,
                    status: ReplacementStatus::SkippedSemantics,
                    replaced_glyphs: 0,
                    original_text: None,
                    new_text: "10".into(),
                    original_advance: None,
                    new_advance: None,
                    tj_delta: None,
                    scanned_pages: 0,
                    scan_incomplete: false,
                },
            ],
            residual_risks: Vec::new(),
            inspection_incomplete: false,
            inspection_gaps: Vec::new(),
            modified: true,
            signature: SignatureIndicators::default(),
        };
        let path = format!(
            "{}/tests/golden/replacement-v1.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let actual = serde_json::to_string_pretty(&result.report()).unwrap() + "\n";
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("falta {path}; generar con UPDATE_GOLDEN=1"));
        assert_eq!(
            actual,
            expected,
            "el esquema v{REPLACEMENT_REPORT_SCHEMA_VERSION} cambió; si es intencional, subir la versión y regenerar"
        );
    }
}
