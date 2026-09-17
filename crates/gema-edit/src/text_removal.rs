//! Borrado real de texto por regiones.
//!
//! Un editor que "reemplaza" texto tapándolo con un rectángulo deja el texto
//! original dentro del archivo: se copia, se busca y se indexa. Este módulo
//! retira los glifos del content stream y los sustituye por el desplazamiento
//! equivalente dentro de un `TJ`, de modo que el resto de la línea no se mueve.
//!
//! **Principio:** nunca se borra un glifo que no esté dentro de una región. Ante
//! cualquier duda (geometría de página no trivial, imágenes en línea, fuentes sin
//! métricas, texto girado o usado como recorte) la región no se toca y el
//! informe dice por qué. Tras reescribir, la página se vuelve a interpretar y, si
//! algún glifo de fuera cambió de código o se movió, se restaura el original.

use std::collections::{BTreeMap, HashMap, HashSet};

use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, ObjectId, Stream, StringFormat};

use crate::error::{EditError, LimitKind};
use crate::inspect::{GapReason, InspectionGap, Inspector, ResidualRisk};
use crate::options::{BudgetMeter, EditOptions};
use crate::signature::SignatureIndicators;
use crate::stream_read::{read_page_content_bounded, StreamReadError};
use crate::text_geometry::{interpret_content, Glyph, PageText};

/// Tolerancia de posición al verificar que los glifos no borrados siguen en su
/// sitio (puntos PDF). El intérprete trabaja con matrices `f32`.
const POSITION_TOLERANCE: f64 = 0.01;
/// Tolerancia para considerar que una caja de página empieza en el origen.
const BOX_TOLERANCE: f64 = 0.01;
/// Rectángulo cuyo texto debe desaparecer, en espacio de página PDF (origen
/// abajo a la izquierda, puntos).
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
pub struct TextRegion {
    /// Identificador del llamador; se devuelve tal cual en el informe.
    pub id: String,
    /// Página, base 0.
    pub page: u32,
    /// Borde izquierdo.
    pub x: f64,
    /// Borde inferior.
    pub y: f64,
    /// Ancho, mayor que cero.
    pub width: f64,
    /// Alto, mayor que cero.
    pub height: f64,
}

impl TextRegion {
    fn is_valid(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|value| value.is_finite())
            && self.width > 0.0
            && self.height > 0.0
    }

    fn contains(&self, (x, y): (f64, f64)) -> bool {
        x >= self.x && x <= self.x + self.width && y >= self.y && y <= self.y + self.height
    }
}

/// Resultado de una región.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum RemovalStatus {
    /// Los códigos de glifo seleccionados dejaron de ser emitidos por el
    /// content stream directo de esa página. **No afirma que la región quedó
    /// limpia**: el texto puede sobrevivir en streams compartidos, Form
    /// XObjects, anotaciones, contenido opcional o `ActualText`; ver
    /// `RemovalResult::residual_risks`.
    ///
    /// Alcance real: glifos upright, sin recorte y reescribibles. El guard
    /// rechaza `render_mode >= 4`, glifos girados y texto sin ajuste `TJ`.
    /// No se eliminan operadores: se reescriben operandos y cada glifo se
    /// sustituye por el desplazamiento `TJ` equivalente.
    Removed,
    /// Se borraron glifos, pero en la página hay texto sin medir (otra fuente,
    /// un Form XObject…) que podría caer dentro de la región.
    RemovedUnverified,
    /// La página se entendió entera y en la región no había texto.
    NothingFound,
    /// El documento está cifrado.
    SkippedEncrypted,
    /// Región con medidas no finitas o nulas, o página inexistente.
    SkippedInvalidRegion,
    /// `/Rotate`, `/UserUnit` o una caja de página que no empieza en el origen.
    SkippedPageGeometry,
    /// El contenido no se pudo decodificar, es demasiado grande o trae imágenes
    /// en línea (lopdf no las reescribe sin corromperlas).
    SkippedContent,
    /// En la región hay texto girado, usado como recorte o sin medidas fiables.
    SkippedUnsupportedText,
    /// La reescritura movió o cambió algún glifo de fuera: se restauró la página.
    SkippedVerification,
}

impl RemovalStatus {
    /// Nombre estable de la variante, idéntico al que produce serde.
    ///
    /// Es lo que el CLI imprime y lo que un consumidor puede comparar; `Debug`
    /// no forma parte del contrato.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Removed => "removed",
            Self::RemovedUnverified => "removed_unverified",
            Self::NothingFound => "nothing_found",
            Self::SkippedEncrypted => "skipped_encrypted",
            Self::SkippedInvalidRegion => "skipped_invalid_region",
            Self::SkippedPageGeometry => "skipped_page_geometry",
            Self::SkippedContent => "skipped_content",
            Self::SkippedUnsupportedText => "skipped_unsupported_text",
            Self::SkippedVerification => "skipped_verification",
        }
    }
}

/// Informe por región, en el mismo orden en que se pidieron.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct RegionReport {
    /// El `id` de la región.
    pub id: String,
    /// Página, base 0.
    pub page: u32,
    /// Glifos retirados del content stream.
    pub removed_glyphs: usize,
    /// Qué pasó.
    pub status: RemovalStatus,
}

/// Salida de [`remove_text_glyphs`].
#[derive(Debug, Clone)]
pub struct RemovalResult {
    /// PDF resultante. Idéntico byte a byte a la entrada si no se borró nada.
    pub output: Vec<u8>,
    /// Un informe por región.
    pub regions: Vec<RegionReport>,
    /// Superficies donde puede sobrevivir contenido textual.
    pub residual_risks: Vec<ResidualRisk>,
    /// `true` si alguna superficie no pudo inspeccionarse completamente.
    pub inspection_incomplete: bool,
    /// Motivos y ubicaciones que no pudieron inspeccionarse.
    pub inspection_gaps: Vec<InspectionGap>,
    /// `true` si el documento fue reescrito.
    pub modified: bool,
    /// Indicios de firma digital.
    pub signature: SignatureIndicators,
}

fn finish(
    output: Vec<u8>,
    regions: Vec<RegionReport>,
    modified: bool,
    mut risks: Vec<ResidualRisk>,
    mut gaps: Vec<InspectionGap>,
    signature: SignatureIndicators,
) -> RemovalResult {
    risks.sort();
    risks.dedup();
    gaps.sort();
    gaps.dedup();
    RemovalResult {
        output,
        regions,
        inspection_incomplete: !gaps.is_empty(),
        residual_risks: risks,
        inspection_gaps: gaps,
        modified,
        signature,
    }
}

/// Borra del content stream los glifos cuyo centro cae dentro de alguna región.
///
/// El centro de un glifo es el punto a media anchura de su avance y a 0.3 veces
/// su tamaño por encima de la línea base: un glifo del renglón vecino queda a un
/// interlineado y no entra.
///
/// Un documento cifrado no es un error: se devuelve intacto con
/// [`RemovalStatus::SkippedEncrypted`].
///
/// Equivale a [`remove_text_glyphs_with`] con [`EditOptions::default`].
pub fn remove_text_glyphs(
    input: &[u8],
    regions: &[TextRegion],
) -> Result<RemovalResult, EditError> {
    remove_text_glyphs_with(input, regions, &EditOptions::default())
}

/// Elimina los glifos que caen dentro de `regions`, acotado por `opts`.
///
/// Los límites de entrada y de regiones se verifican **antes** de parsear el
/// PDF. Los del content stream se verifican mientras se lee; superarlos
/// aborta con [`EditError::LimitExceeded`]. Los de la inspección residual no
/// abortan: dejan `inspection_incomplete`.
pub fn remove_text_glyphs_with(
    input: &[u8],
    regions: &[TextRegion],
    opts: &EditOptions,
) -> Result<RemovalResult, EditError> {
    if input.len() > opts.max_input_bytes {
        return Err(EditError::LimitExceeded(LimitKind::InputBytes));
    }
    if regions.len() > opts.max_regions {
        return Err(EditError::LimitExceeded(LimitKind::Regions));
    }
    let mut meter = BudgetMeter::new(&opts.budget);
    remove_text_glyphs_inner(input, regions, opts, &mut meter)
}

fn remove_text_glyphs_inner(
    input: &[u8],
    regions: &[TextRegion],
    opts: &EditOptions,
    meter: &mut BudgetMeter,
) -> Result<RemovalResult, EditError> {
    let mut reports: Vec<RegionReport> = regions
        .iter()
        .map(|region| RegionReport {
            id: region.id.clone(),
            page: region.page,
            removed_glyphs: 0,
            status: RemovalStatus::NothingFound,
        })
        .collect();
    if regions.is_empty() {
        return Ok(finish(
            input.to_vec(),
            reports,
            false,
            vec![],
            vec![InspectionGap {
                reason: GapReason::NotInspected,
                page: None,
                detail: "no regions requested".into(),
            }],
            SignatureIndicators::default(),
        ));
    }

    let mut doc = Document::load_mem(input).map_err(|e| EditError::Parse(e.to_string()))?;
    if doc.is_encrypted() {
        for report in &mut reports {
            report.status = RemovalStatus::SkippedEncrypted;
        }
        return Ok(finish(
            input.to_vec(),
            reports,
            false,
            vec![],
            vec![InspectionGap {
                reason: GapReason::Encrypted,
                page: None,
                detail: "encrypted".into(),
            }],
            SignatureIndicators::default(),
        ));
    }

    let pages = doc.get_pages();
    let mut by_page: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (index, region) in regions.iter().enumerate() {
        let exists = region
            .page
            .checked_add(1)
            .is_some_and(|number| pages.contains_key(&number));
        if region.is_valid() && exists {
            by_page.entry(region.page).or_default().push(index);
        } else {
            reports[index].status = RemovalStatus::SkippedInvalidRegion;
        }
    }

    let mut changed = false;
    let mut inspected_pages = Vec::new();
    for (page, indices) in by_page {
        let page_id = pages[&(page + 1)];
        let original_contents = doc.get_page_contents(page_id);
        let mut outcome = erase_on_page(&mut doc, page_id, regions, &indices, opts, meter)?;
        for (index, removed_glyphs, status) in outcome.regions {
            reports[index].removed_glyphs = removed_glyphs;
            reports[index].status = status;
        }
        changed |= outcome.changed;
        inspected_pages.push((
            page,
            page_id,
            original_contents,
            outcome.content.take(),
            outcome.read_error.take(),
        ));
    }
    let mut inspector = Inspector::new(&doc, meter);
    let mut read_gaps = Vec::new();
    for (page, page_id, original, content, read_error) in &mut inspected_pages {
        if let Some(error) = read_error.take() {
            let (reason, detail) = match &error {
                StreamReadError::UnsupportedFilter(filter) => {
                    (GapReason::UnsupportedFilter, filter.clone())
                }
                StreamReadError::Corrupt => (GapReason::CorruptStream, "page content".into()),
                StreamReadError::Missing => (GapReason::BrokenReference, "/Contents".into()),
                StreamReadError::NotAStream => (
                    GapReason::MalformedObject,
                    "/Contents is not a stream".into(),
                ),
                StreamReadError::Budget(_) | StreamReadError::TooLarge => {
                    unreachable!("erase_on_page converts budget errors to LimitExceeded")
                }
            };
            read_gaps.push(InspectionGap {
                reason,
                page: Some(*page),
                detail,
            });
        }
        let resources = inspector.resources(*page_id, *page);
        if let Some(content) = content {
            inspector.inspect_marked_content(content, &resources, *page);
            inspector.inspect_form_xobjects(content, &resources, *page);
        } else {
            inspector.gap(
                GapReason::NotInspected,
                Some(*page),
                "page content was not read",
            );
        }
        let page_dict = doc.get_dictionary(*page_id).ok();
        if let Some(dict) = page_dict {
            inspector.inspect_annotations(
                dict,
                *page,
                &regions
                    .iter()
                    .filter(|r| r.page == *page)
                    .collect::<Vec<_>>(),
            );
        } else {
            inspector.gap(GapReason::BrokenReference, Some(*page), "page dictionary");
        }
        inspector.inspect_shared_content(original, *page, &pages);
    }
    let signature = crate::signature::detect(&mut inspector);
    let mut gaps = inspector.gaps;
    gaps.extend(read_gaps);
    let risks = inspector.risks;
    if !changed {
        return Ok(finish(
            input.to_vec(),
            reports,
            false,
            risks,
            gaps,
            signature,
        ));
    }
    let mut output = Vec::new();
    doc.save_to(&mut output)
        .map_err(|e| EditError::Io(e.to_string()))?;
    Ok(finish(output, reports, true, risks, gaps, signature))
}

struct PageOutcome {
    regions: Vec<(usize, usize, RemovalStatus)>,
    changed: bool,
    read_error: Option<StreamReadError>,
    content: Option<Content>,
}

/// `(índice de operación, índice de operando, offset del código)`.
type GlyphKey = (usize, usize, usize);

fn key(glyph: &Glyph) -> GlyphKey {
    (glyph.op_index, glyph.operand_index, glyph.byte_offset)
}

fn erase_on_page(
    doc: &mut Document,
    page_id: ObjectId,
    regions: &[TextRegion],
    indices: &[usize],
    opts: &EditOptions,
    meter: &mut BudgetMeter,
) -> Result<PageOutcome, EditError> {
    let untouched = |status| PageOutcome {
        regions: indices.iter().map(|&index| (index, 0, status)).collect(),
        changed: false,
        read_error: None,
        content: None,
    };
    let untouched_with = |status, read_error| PageOutcome {
        regions: indices.iter().map(|&index| (index, 0, status)).collect(),
        changed: false,
        read_error: Some(read_error),
        content: None,
    };
    if !page_geometry_is_plain(doc, page_id) {
        return Ok(untouched(RemovalStatus::SkippedPageGeometry));
    }
    let bytes = match read_page_content_bounded(doc, page_id, opts, meter) {
        Ok(bytes) => bytes,
        Err(StreamReadError::Budget(kind)) => return Err(EditError::LimitExceeded(kind)),
        Err(StreamReadError::TooLarge) => {
            unreachable!("read_page_content_bounded lo traduce a Budget")
        }
        Err(
            e @ (StreamReadError::UnsupportedFilter(_)
            | StreamReadError::Corrupt
            | StreamReadError::Missing
            | StreamReadError::NotAStream),
        ) => {
            return Ok(untouched_with(RemovalStatus::SkippedContent, e));
        }
    };
    let Ok(content) = Content::decode(&bytes) else {
        return Ok(untouched(RemovalStatus::SkippedContent));
    };
    if content.operations.len() > opts.max_content_operations {
        // Sin content: clonar más de max_content_operations operadores no aporta a la inspección.
        return Ok(untouched(RemovalStatus::SkippedContent));
    }
    if content
        .operations
        .iter()
        .any(|operation| operation.operator == "BI")
    {
        return Ok(PageOutcome {
            regions: indices
                .iter()
                .map(|&index| (index, 0, RemovalStatus::SkippedContent))
                .collect(),
            changed: false,
            read_error: None,
            content: Some(content.clone()),
        });
    }
    let Ok(before) = interpret_content(doc, page_id, &content) else {
        return Ok(PageOutcome {
            regions: indices
                .iter()
                .map(|&index| (index, 0, RemovalStatus::SkippedContent))
                .collect(),
            changed: false,
            read_error: None,
            content: Some(content.clone()),
        });
    };
    let page_has_unsupported = !before.unsupported.is_empty();

    let mut hits: Vec<Vec<usize>> = vec![Vec::new(); indices.len()];
    let mut blocked = vec![false; indices.len()];
    for (glyph_index, glyph) in before.glyphs.iter().enumerate() {
        let probe = probe_point(glyph);
        for (slot, &region_index) in indices.iter().enumerate() {
            let region = &regions[region_index];
            let upright = is_upright(glyph);
            // En texto recto manda el centro: un glifo cuyo origen roza el borde
            // pero cuyo centro queda fuera es del texto vecino. En texto girado el
            // centro no basta para descartarlo, así que también cuenta el origen.
            if !region.contains(probe) && (upright || !region.contains(glyph.origin)) {
                continue;
            }
            if !upright || glyph.render_mode >= 4 || glyph.tj_adjustment.is_none() {
                // Un glifo girado, de recorte o sin número equivalente cerca de la
                // región bloquea la región entera: no se borra a medias.
                blocked[slot] = true;
            } else {
                hits[slot].push(glyph_index);
            }
            break;
        }
    }

    let mut erase: HashSet<GlyphKey> = HashSet::new();
    let mut statuses = Vec::with_capacity(indices.len());
    for (slot, &region_index) in indices.iter().enumerate() {
        if blocked[slot] {
            statuses.push((region_index, 0, RemovalStatus::SkippedUnsupportedText));
            continue;
        }
        let count = hits[slot].len();
        if count == 0 {
            let status = if page_has_unsupported {
                RemovalStatus::SkippedUnsupportedText
            } else {
                RemovalStatus::NothingFound
            };
            statuses.push((region_index, 0, status));
            continue;
        }
        erase.extend(hits[slot].iter().map(|&index| key(&before.glyphs[index])));
        let status = if page_has_unsupported {
            RemovalStatus::RemovedUnverified
        } else {
            RemovalStatus::Removed
        };
        statuses.push((region_index, count, status));
    }
    if erase.is_empty() {
        return Ok(PageOutcome {
            regions: statuses,
            changed: false,
            read_error: None,
            content: Some(content.clone()),
        });
    }

    let failed = |statuses: Vec<(usize, usize, RemovalStatus)>, status| PageOutcome {
        regions: statuses
            .into_iter()
            .map(|(index, count, previous)| {
                if count > 0 {
                    (index, 0, status)
                } else {
                    (index, 0, previous)
                }
            })
            .collect(),
        changed: false,
        read_error: None,
        content: Some(content.clone()),
    };

    let Some(operations) = rewrite_operations(&content.operations, &before, &erase) else {
        return Ok(failed(statuses, RemovalStatus::SkippedContent));
    };
    let Ok(bytes) = (Content { operations }).encode() else {
        return Ok(failed(statuses, RemovalStatus::SkippedContent));
    };

    let old_streams = doc.get_page_contents(page_id);
    let Some(previous_contents) = doc
        .get_dictionary(page_id)
        .ok()
        .and_then(|page| page.get(b"Contents").ok())
        .cloned()
    else {
        return Ok(failed(statuses, RemovalStatus::SkippedContent));
    };
    let mut stream = Stream::new(dictionary! {}, bytes.clone());
    let _ = stream.compress();
    let new_stream = doc.add_object(stream);
    set_contents(doc, page_id, Object::Reference(new_stream));

    let verified = Content::decode(&bytes)
        .ok()
        .and_then(|after| interpret_content(doc, page_id, &after).ok())
        .is_some_and(|after| unremoved_glyphs_unchanged(&before, &after, &erase));
    if !verified {
        set_contents(doc, page_id, previous_contents);
        doc.objects.remove(&new_stream);
        return Ok(failed(statuses, RemovalStatus::SkippedVerification));
    }

    // El stream viejo sigue conteniendo el texto borrado. Si ninguna otra página
    // lo usa, se elimina: si no, el dato seguiría recuperable dentro del archivo.
    for old in old_streams {
        if !is_referenced(doc, old) {
            doc.objects.remove(&old);
        }
    }
    Ok(PageOutcome {
        regions: statuses,
        changed: true,
        read_error: None,
        content: Some(content),
    })
}

fn set_contents(doc: &mut Document, page_id: ObjectId, contents: Object) {
    if let Ok(page) = doc.get_dictionary_mut(page_id) {
        page.set("Contents", contents);
    }
}

fn probe_point(glyph: &Glyph) -> (f64, f64) {
    let (bx, by) = glyph.baseline_dir;
    let (ux, uy) = glyph.ascent_dir;
    let along = glyph.advance / 2.0;
    let up = 0.3 * glyph.font_size_eff;
    (
        glyph.origin.0 + bx * along + ux * up,
        glyph.origin.1 + by * along + uy * up,
    )
}

fn is_upright(glyph: &Glyph) -> bool {
    const EPSILON: f64 = 1e-3;
    (glyph.baseline_dir.0 - 1.0).abs() < EPSILON
        && glyph.baseline_dir.1.abs() < EPSILON
        && glyph.ascent_dir.0.abs() < EPSILON
        && (glyph.ascent_dir.1 - 1.0).abs() < EPSILON
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
            .and_then(|parent| parent.as_reference().ok());
    }
    None
}

fn lower_left(doc: &Document, object: &Object) -> Option<(f64, f64)> {
    let values = object.as_array().ok()?;
    if values.len() != 4 {
        return None;
    }
    let mut numbers = [0.0_f64; 4];
    for (slot, value) in numbers.iter_mut().zip(values) {
        let value = doc.dereference(value).ok()?.1;
        *slot = f64::from(value.as_float().ok()?);
    }
    Some((numbers[0].min(numbers[2]), numbers[1].min(numbers[3])))
}

/// El llamador calcula las regiones en el espacio de página sin rotar y con
/// origen en la esquina de la caja visible. Sólo cuando esa caja empieza en
/// (0, 0) y no hay rotación ni `/UserUnit` coinciden con el espacio de usuario.
fn page_geometry_is_plain(doc: &Document, page_id: ObjectId) -> bool {
    let at_origin = |(x, y): (f64, f64)| x.abs() <= BOX_TOLERANCE && y.abs() <= BOX_TOLERANCE;
    let rotate = inherited(doc, page_id, b"Rotate")
        .map(|value| value.as_i64().ok())
        .unwrap_or(Some(0));
    if rotate.is_none_or(|degrees| degrees.rem_euclid(360) != 0) {
        return false;
    }
    if let Some(unit) = inherited(doc, page_id, b"UserUnit") {
        if unit
            .as_float()
            .map_or(true, |value| (value - 1.0).abs() > f32::EPSILON)
        {
            return false;
        }
    }
    let Some(media) = inherited(doc, page_id, b"MediaBox").and_then(|b| lower_left(doc, b)) else {
        return false;
    };
    if !at_origin(media) {
        return false;
    }
    match inherited(doc, page_id, b"CropBox") {
        None => true,
        Some(crop) => lower_left(doc, crop).is_some_and(at_origin),
    }
}

/// Una ranura por código dentro de una cadena que tiene al menos un borrado.
struct CodeSlot {
    offset: usize,
    len: usize,
    erase: bool,
    adjustment: f64,
}

fn rewrite_operations(
    operations: &[Operation],
    before: &PageText,
    erase: &HashSet<GlyphKey>,
) -> Option<Vec<Operation>> {
    let mut strings: HashMap<(usize, usize), Vec<CodeSlot>> = HashMap::new();
    let touched: HashSet<(usize, usize)> = erase
        .iter()
        .map(|&(op, operand, _)| (op, operand))
        .collect();
    for glyph in &before.glyphs {
        if !touched.contains(&(glyph.op_index, glyph.operand_index)) {
            continue;
        }
        let erase_it = erase.contains(&key(glyph));
        strings
            .entry((glyph.op_index, glyph.operand_index))
            .or_default()
            .push(CodeSlot {
                offset: glyph.byte_offset,
                len: usize::from(glyph.code_len),
                erase: erase_it,
                adjustment: if erase_it { glyph.tj_adjustment? } else { 0.0 },
            });
    }

    let mut output = Vec::with_capacity(operations.len());
    for (op_index, operation) in operations.iter().enumerate() {
        let operand_of = |index: usize| strings.get(&(op_index, index));
        match operation.operator.as_str() {
            "Tj" | "'" | "\"" => {
                let string_operand = if operation.operator == "\"" { 2 } else { 0 };
                let Some(slots) = operand_of(string_operand) else {
                    output.push(operation.clone());
                    continue;
                };
                let Some(Object::String(bytes, format)) = operation.operands.get(string_operand)
                else {
                    return None;
                };
                let mut array = Vec::new();
                rebuild_string(bytes, *format, slots, &mut array)?;
                if operation.operator == "\"" {
                    output.push(Operation::new(
                        "Tw",
                        vec![operation.operands.first()?.clone()],
                    ));
                    output.push(Operation::new(
                        "Tc",
                        vec![operation.operands.get(1)?.clone()],
                    ));
                }
                if operation.operator != "Tj" {
                    output.push(Operation::new("T*", vec![]));
                }
                output.push(Operation::new("TJ", vec![Object::Array(array)]));
            }
            "TJ" => {
                let Some(Object::Array(items)) = operation.operands.first() else {
                    output.push(operation.clone());
                    continue;
                };
                if !items
                    .iter()
                    .enumerate()
                    .any(|(index, _)| operand_of(index).is_some())
                {
                    output.push(operation.clone());
                    continue;
                }
                let mut array = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    match (operand_of(index), item) {
                        (Some(slots), Object::String(bytes, format)) => {
                            rebuild_string(bytes, *format, slots, &mut array)?;
                        }
                        (Some(_), _) => return None,
                        (None, item) => array.push(item.clone()),
                    }
                }
                output.push(Operation::new("TJ", vec![Object::Array(array)]));
            }
            _ => output.push(operation.clone()),
        }
    }
    Some(output)
}

/// Parte una cadena en tramos conservados y números equivalentes a los glifos
/// retirados. Exige que las ranuras cubran la cadena entera, contiguas y en
/// orden: si el intérprete y los bytes no coinciden, no se reescribe nada.
fn rebuild_string(
    bytes: &[u8],
    format: StringFormat,
    slots: &[CodeSlot],
    array: &mut Vec<Object>,
) -> Option<()> {
    let mut expected_offset = 0;
    let mut run = Vec::new();
    let mut pending = 0.0_f64;
    for slot in slots {
        if slot.offset != expected_offset {
            return None;
        }
        let code = bytes.get(slot.offset..slot.offset + slot.len)?;
        expected_offset += slot.len;
        if slot.erase {
            if !run.is_empty() {
                array.push(Object::String(std::mem::take(&mut run), format));
            }
            pending += slot.adjustment;
        } else {
            if pending != 0.0 {
                array.push(Object::Real(pending as f32));
                pending = 0.0;
            }
            run.extend_from_slice(code);
        }
    }
    if expected_offset != bytes.len() {
        return None;
    }
    if !run.is_empty() {
        array.push(Object::String(run, format));
    }
    if pending != 0.0 {
        array.push(Object::Real(pending as f32));
    }
    Some(())
}

fn unremoved_glyphs_unchanged(
    before: &PageText,
    after: &PageText,
    erase: &HashSet<GlyphKey>,
) -> bool {
    let kept: Vec<&Glyph> = before
        .glyphs
        .iter()
        .filter(|glyph| !erase.contains(&key(glyph)))
        .collect();
    kept.len() == after.glyphs.len()
        && before.unsupported.len() == after.unsupported.len()
        && kept.iter().zip(&after.glyphs).all(|(old, new)| {
            old.code == new.code
                && old.code_len == new.code_len
                && old.font_res_name == new.font_res_name
                && old.render_mode == new.render_mode
                && (old.origin.0 - new.origin.0).abs() <= POSITION_TOLERANCE
                && (old.origin.1 - new.origin.1).abs() <= POSITION_TOLERANCE
        })
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

fn is_referenced(doc: &Document, target: ObjectId) -> bool {
    doc.trailer
        .iter()
        .any(|(_, value)| references(value, target))
        || doc
            .objects
            .iter()
            .any(|(id, object)| *id != target && references(object, target))
}

#[cfg(test)]
mod tests {
    use lopdf::{dictionary, Dictionary, Document, Object, Stream};

    use super::{finish, remove_text_glyphs, RemovalStatus, SignatureIndicators, TextRegion};
    use crate::inspect::{GapReason, InspectionGap, ResidualRisk};
    use crate::text_geometry::{interpret_page_text, Glyph};

    fn close(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 0.01
    }

    /// TrueType simple: espacio 250, el resto de ASCII imprimible 500.
    fn simple_font() -> Object {
        let widths = (32..=126)
            .map(|code| Object::Integer(if code == 32 { 250 } else { 500 }))
            .collect::<Vec<_>>();
        Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "TrueType", "BaseFont" => "Synthetic",
            "FirstChar" => 32, "LastChar" => 126, "Widths" => widths,
        })
    }

    fn build(
        contents: &[&[u8]],
        page_extra: Option<(&str, Object)>,
        extra_fonts: Vec<(&str, Object)>,
    ) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let mut fonts = Dictionary::new();
        fonts.set("F1", doc.add_object(simple_font()));
        for (name, font) in extra_fonts {
            let id = doc.add_object(font);
            fonts.set(name, id);
        }
        let mut kids = Vec::new();
        for content in contents {
            let content_id = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
            let mut page = dictionary! {
                "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
                "Resources" => dictionary! { "Font" => fonts.clone() },
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            };
            if let Some((name, value)) = page_extra.clone() {
                page.set(name, value);
            }
            kids.push(Object::Reference(doc.add_object(page)));
        }
        let count = kids.len() as i64;
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn region(page: u32, x: f64, y: f64, width: f64, height: f64) -> TextRegion {
        TextRegion {
            id: format!("r{page}-{x}-{y}"),
            page,
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn with_options_rejects_too_many_regions_before_parsing() {
        let opts = crate::EditOptions {
            max_regions: 1,
            ..crate::EditOptions::default()
        };
        let regions = [
            region(0, 0.0, 0.0, 10.0, 10.0),
            region(0, 20.0, 0.0, 10.0, 10.0),
        ];
        let err = super::remove_text_glyphs_with(b"", &regions, &opts).unwrap_err();
        assert!(matches!(
            err,
            crate::EditError::LimitExceeded(crate::LimitKind::Regions)
        ));
    }

    #[test]
    fn with_options_rejects_oversized_input_before_parsing() {
        let opts = crate::EditOptions {
            max_input_bytes: 3,
            ..crate::EditOptions::default()
        };
        let err = super::remove_text_glyphs_with(b"%PDF", &[], &opts).unwrap_err();
        assert!(matches!(
            err,
            crate::EditError::LimitExceeded(crate::LimitKind::InputBytes)
        ));
    }

    #[test]
    fn operation_limit_skips_the_page_like_today() {
        let pdf = build(&[b"BT /F1 12 Tf 10 10 Td (hola) Tj ET"], None, vec![]);
        let opts = crate::EditOptions {
            max_content_operations: 3,
            ..crate::EditOptions::default()
        };
        let r = super::remove_text_glyphs_with(&pdf, &[region(0, 0.0, 0.0, 612.0, 792.0)], &opts)
            .unwrap();
        assert_eq!(r.regions[0].status, RemovalStatus::SkippedContent);
    }

    #[test]
    fn page_byte_limit_aborts_the_call() {
        let pdf = build(&[b"BT /F1 12 Tf 10 10 Td (hola) Tj ET"], None, vec![]);
        let opts = crate::EditOptions {
            max_decompressed_bytes: 8,
            ..crate::EditOptions::default()
        };
        let err = super::remove_text_glyphs_with(&pdf, &[region(0, 0.0, 0.0, 612.0, 792.0)], &opts)
            .unwrap_err();
        assert!(matches!(
            err,
            crate::EditError::LimitExceeded(crate::LimitKind::DecompressedBytes)
        ));
    }

    fn glyphs(bytes: &[u8], page: u32) -> Vec<Glyph> {
        let doc = Document::load_mem(bytes).unwrap();
        let page_id = doc.get_pages()[&(page + 1)];
        interpret_page_text(&doc, page_id).unwrap().glyphs
    }

    /// Todo el contenido de todos los streams, descomprimido cuando se puede.
    fn every_stream(bytes: &[u8]) -> Vec<u8> {
        let doc = Document::load_mem(bytes).unwrap();
        let mut all = Vec::new();
        for object in doc.objects.values() {
            if let Object::Stream(stream) = object {
                match stream.decompressed_content() {
                    Ok(content) => all.extend(content),
                    Err(_) => all.extend(&stream.content),
                }
            }
        }
        all
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn truncated_content_stream_is_skipped_and_the_original_survives() {
        use crate::test_support::Fixture;
        use std::io::Write;
        let ops = b"BT /F1 12 Tf 10 10 Td (hola) Tj ET ".repeat(2_000);
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&ops).unwrap();
        let packed = enc.finish().unwrap();
        let cut = packed[..packed.len() / 2].to_vec();
        let mut fx = Fixture::new();
        let c = fx.doc.add_object(lopdf::Stream::new(
            lopdf::dictionary! { "Filter" => "FlateDecode" },
            cut,
        ));
        let page = fx.add_page(c, Some(Fixture::default_resources()), vec![]);
        let prefix = fx.doc.get_and_decode_page_content(page).unwrap();
        assert!(
            !prefix.operations.is_empty()
                && fx.doc.get_page_content(page).unwrap().len() < ops.len()
        );
        let pdf = fx.bytes();
        let r = remove_text_glyphs(&pdf, &[region(0, 0.0, 0.0, 612.0, 792.0)]).unwrap();
        assert_eq!(r.regions[0].status, RemovalStatus::SkippedContent);
        assert_eq!(r.output, pdf, "sin cambios, el output es el input");
    }

    #[test]
    fn erases_one_word_and_keeps_the_rest_of_the_line_in_place() {
        // A@100 B@105 espacio@110 (2.5) C@112.5 D@117.5; centro de A = (102.5, 703).
        let input = build(
            &[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB CD) Tj ET"],
            None,
            vec![],
        );
        let result = remove_text_glyphs(&input, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();

        assert_eq!(result.regions[0].status, RemovalStatus::Removed);
        assert_eq!(result.regions[0].removed_glyphs, 2);
        let after = glyphs(&result.output, 0);
        let codes: Vec<u32> = after.iter().map(|glyph| glyph.code).collect();
        assert_eq!(codes, vec![32, 67, 68]);
        assert!(close(after[1].origin.0, 112.5) && close(after[2].origin.0, 117.5));
        // El texto viejo no sobrevive en ningún stream del archivo.
        assert!(!contains(&every_stream(&result.output), b"AB"));
        assert!(contains(&every_stream(&result.output), b" CD"));
    }

    #[test]
    fn quote_operators_are_rewritten_without_moving_what_follows() {
        let input = build(
            &[b"BT /F1 10 Tf 12 TL 1 0 0 1 100 700 Tm (XX) Tj (AB) ' 1 2 (CD) \" (EF) Tj ET"],
            None,
            vec![],
        );
        let before = glyphs(&input, 0);
        // AB está en y=688 (primer '), CD en y=676 con Tc=2 que persiste en EF.
        let result = remove_text_glyphs(&input, &[region(0, 95.0, 684.0, 20.0, 10.0)]).unwrap();
        assert_eq!(result.regions[0].status, RemovalStatus::Removed);
        assert_eq!(result.regions[0].removed_glyphs, 2);

        let after = glyphs(&result.output, 0);
        let kept: Vec<&Glyph> = before
            .iter()
            .filter(|g| !close(g.origin.1, 688.0))
            .collect();
        assert_eq!(kept.len(), after.len());
        for (old, new) in kept.iter().zip(&after) {
            assert_eq!(old.code, new.code);
            assert!(close(old.origin.0, new.origin.0) && close(old.origin.1, new.origin.1));
        }
    }

    #[test]
    fn fixture_erases_one_phrase_of_a_shared_tj_and_a_type0_line() {
        let input = include_bytes!("../tests/fixtures/editor-fuentes.pdf");
        // Línea B: «Frase que se cambia» termina en 190.718; «Frase que se queda»
        // empieza en 225.718. Línea A (Type0) en y=720. Línea C no se toca.
        let regions = [
            region(0, 58.0, 615.0, 134.0, 14.0),
            region(0, 58.0, 715.0, 400.0, 15.0),
        ];
        let before = glyphs(input, 0);
        let result = remove_text_glyphs(input, &regions).unwrap();
        assert_eq!(result.regions[0].status, RemovalStatus::Removed);
        assert_eq!(
            result.regions[0].removed_glyphs,
            "Frase que se cambia".len()
        );
        assert_eq!(result.regions[1].status, RemovalStatus::Removed);

        let after = glyphs(&result.output, 0);
        let line = |set: &[Glyph], y: f64| -> Vec<(u32, f64)> {
            set.iter()
                .filter(|g| close(g.origin.1, y))
                .map(|g| (g.code, g.origin.0))
                .collect()
        };
        assert!(line(&after, 720.0).is_empty());
        let line_b = line(&after, 620.0);
        assert_eq!(line_b.len(), "Frase que se queda".len());
        assert!(close(line_b[0].1, 225.718), "{}", line_b[0].1);
        assert_eq!(line(&after, 520.0), line(&before, 520.0));

        let streams = every_stream(&result.output);
        assert!(!contains(&streams, b"Frase que se cambia"));
        assert!(contains(&streams, b"Frase que se queda"));
    }

    #[test]
    fn leaves_the_document_byte_identical_when_nothing_is_erased() {
        let input = build(
            &[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            None,
            vec![],
        );
        let result = remove_text_glyphs(&input, &[region(0, 300.0, 300.0, 50.0, 20.0)]).unwrap();
        assert_eq!(result.regions[0].status, RemovalStatus::NothingFound);
        assert_eq!(result.output, input);
    }

    #[test]
    fn skips_rotated_pages_inline_images_and_invalid_regions() {
        let rotated = build(
            &[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            Some(("Rotate", Object::Integer(90))),
            vec![],
        );
        let result = remove_text_glyphs(&rotated, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();
        assert_eq!(result.regions[0].status, RemovalStatus::SkippedPageGeometry);
        assert_eq!(result.output, rotated);

        let inline = build(
            &[b"q 10 0 0 10 0 0 cm BI /W 1 /H 1 /BPC 8 /CS /G ID \x80 EI Q BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            None,
            vec![],
        );
        let result = remove_text_glyphs(&inline, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();
        assert_eq!(result.regions[0].status, RemovalStatus::SkippedContent);
        assert_eq!(result.output, inline);

        let plain = build(
            &[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            None,
            vec![],
        );
        let result = remove_text_glyphs(
            &plain,
            &[
                region(3, 99.0, 695.0, 11.0, 17.0),
                region(0, 1.0, 1.0, 0.0, 5.0),
            ],
        )
        .unwrap();
        assert!(result
            .regions
            .iter()
            .all(|report| report.status == RemovalStatus::SkippedInvalidRegion));
        assert_eq!(result.output, plain);
    }

    #[test]
    fn rotated_page_still_inspects_annotations() {
        let input = build(
            &[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            Some(("Rotate", Object::Integer(90))),
            vec![],
        );
        let mut doc = Document::load_mem(&input).unwrap();
        let page_id = doc.get_pages()[&1];
        let annot = doc.add_object(dictionary! {
            "Subtype" => "FreeText",
            "Rect" => vec![90.into(), 690.into(), 120.into(), 730.into()],
        });
        doc.get_dictionary_mut(page_id)
            .unwrap()
            .set("Annots", vec![annot.into()]);
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();
        let result = remove_text_glyphs(&pdf, &[region(0, 90.0, 690.0, 40.0, 40.0)]).unwrap();
        assert!(result
            .inspection_gaps
            .iter()
            .any(|gap| gap.reason == GapReason::NotInspected));
        assert!(result
            .residual_risks
            .iter()
            .any(|risk| matches!(risk, crate::inspect::ResidualRisk::Annotation { .. })));
    }

    #[test]
    fn read_error_details_are_stable() {
        let input = build(&[b"BT /F1 10 Tf (AB) Tj ET"], None, vec![]);
        let mut doc = Document::load_mem(&input).unwrap();
        let page_id = doc.get_pages()[&1];
        let stream = doc.add_object(Stream::new(
            dictionary! { "Filter" => "LZWDecode" },
            b"not lzw".to_vec(),
        ));
        doc.get_dictionary_mut(page_id)
            .unwrap()
            .set("Contents", stream);
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();
        let result = remove_text_glyphs(&pdf, &[region(0, 0.0, 0.0, 10.0, 10.0)]).unwrap();
        assert!(result.inspection_gaps.iter().any(|gap| {
            gap.reason == GapReason::UnsupportedFilter && gap.detail == "LZWDecode"
        }));
    }

    #[test]
    fn clipping_text_and_positions_after_unmeasurable_text_are_never_erased() {
        let clip = build(
            &[b"BT 7 Tr /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            None,
            vec![],
        );
        let result = remove_text_glyphs(&clip, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();
        assert_eq!(
            result.regions[0].status,
            RemovalStatus::SkippedUnsupportedText
        );
        assert_eq!(result.output, clip);

        // ZZ no tiene métricas: AB, en el mismo objeto de texto y sin Tm nuevo, ya
        // no tiene posición fiable aunque su fuente sí la tenga.
        let custom = Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "CustomFont",
        });
        let tainted = build(
            &[b"BT /NM 10 Tf 1 0 0 1 100 700 Tm (ZZ) Tj /F1 10 Tf (AB) Tj ET"],
            None,
            vec![("NM", custom)],
        );
        let result = remove_text_glyphs(&tainted, &[region(0, 90.0, 690.0, 300.0, 30.0)]).unwrap();
        assert_eq!(
            result.regions[0].status,
            RemovalStatus::SkippedUnsupportedText
        );
        assert_eq!(result.output, tainted);
    }

    #[test]
    fn unmeasurable_text_elsewhere_downgrades_to_unverified() {
        let custom = Object::Dictionary(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "CustomFont",
        });
        let input = build(
            &[b"BT /NM 10 Tf 1 0 0 1 100 500 Tm (ZZ) Tj ET BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            None,
            vec![("NM", custom)],
        );
        let result = remove_text_glyphs(&input, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();
        assert_eq!(result.regions[0].status, RemovalStatus::RemovedUnverified);
        assert_eq!(result.regions[0].removed_glyphs, 2);
    }

    #[test]
    fn a_content_stream_shared_by_two_pages_is_not_altered_for_the_other() {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font = doc.add_object(simple_font());
        let content = doc.add_object(Stream::new(
            dictionary! {},
            b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET".to_vec(),
        ));
        let mut kids = Vec::new();
        for _ in 0..2 {
            kids.push(Object::Reference(doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => pages_id, "Contents" => content,
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            })));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => 2 }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut input = Vec::new();
        doc.save_to(&mut input).unwrap();

        let result = remove_text_glyphs(&input, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();
        assert_eq!(result.regions[0].status, RemovalStatus::Removed);
        assert!(glyphs(&result.output, 0).is_empty());
        assert_eq!(glyphs(&result.output, 1).len(), 2);
    }

    #[test]
    fn status_as_str_matches_serde_names() {
        let all = [
            (RemovalStatus::Removed, "removed"),
            (RemovalStatus::RemovedUnverified, "removed_unverified"),
            (RemovalStatus::NothingFound, "nothing_found"),
            (RemovalStatus::SkippedEncrypted, "skipped_encrypted"),
            (
                RemovalStatus::SkippedInvalidRegion,
                "skipped_invalid_region",
            ),
            (RemovalStatus::SkippedPageGeometry, "skipped_page_geometry"),
            (RemovalStatus::SkippedContent, "skipped_content"),
            (
                RemovalStatus::SkippedUnsupportedText,
                "skipped_unsupported_text",
            ),
            (RemovalStatus::SkippedVerification, "skipped_verification"),
        ];
        for (status, expected) in all {
            assert_eq!(status.as_str(), expected);
            #[cfg(feature = "serde")]
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(expected.to_string())
            );
        }
    }

    #[test]
    fn clean_page_reports_no_risks_and_complete_inspection() {
        let input = build(&[b"BT /F1 12 Tf (hola) Tj ET"], None, vec![]);
        let result = remove_text_glyphs(&input, &[region(0, 500.0, 700.0, 10.0, 10.0)]).unwrap();
        assert!(!result.inspection_incomplete);
        assert!(result.residual_risks.is_empty());
        assert!(result.inspection_gaps.is_empty());
    }

    #[test]
    fn shared_stream_and_form_xobject_surface_in_the_result() {
        let input = build(&[b"BT /F1 12 Tf (hola) Tj ET"], None, vec![]);
        let result = remove_text_glyphs(&input, &[region(0, 0.0, 0.0, 612.0, 792.0)]).unwrap();
        assert!(result.residual_risks.iter().all(|r| !r.kind().is_empty()));
    }

    #[test]
    fn broken_annotation_reference_marks_the_inspection_incomplete() {
        let input = build(
            &[b"BT /F1 12 Tf (hola) Tj ET"],
            Some(("Annots", vec![Object::Reference((999, 0))].into())),
            vec![],
        );
        let result = remove_text_glyphs(&input, &[region(0, 500.0, 700.0, 10.0, 10.0)]).unwrap();
        assert!(result.inspection_incomplete);
    }

    #[test]
    fn unchanged_document_is_still_inspected() {
        let input = build(&[b"BT /F1 12 Tf (hola) Tj ET"], None, vec![]);
        let result = remove_text_glyphs(&input, &[region(0, 500.0, 700.0, 10.0, 10.0)]).unwrap();
        assert_eq!(result.output, input);
    }

    #[test]
    fn zero_regions_is_declared_not_inspected() {
        let result = remove_text_glyphs(b"not a pdf", &[]).unwrap();
        assert!(result.inspection_incomplete);
        assert_eq!(result.inspection_gaps[0].reason, GapReason::NotInspected);
    }

    #[test]
    fn risks_and_gaps_come_out_sorted_and_deduplicated() {
        let risk = ResidualRisk::ActualText { page: 1 };
        let gap = InspectionGap {
            reason: GapReason::BrokenReference,
            page: Some(2),
            detail: "x".into(),
        };
        let result = finish(
            vec![],
            vec![],
            false,
            vec![risk.clone(), risk],
            vec![gap.clone(), gap],
            SignatureIndicators::default(),
        );
        assert_eq!(result.residual_risks.len(), 1);
        assert_eq!(result.inspection_gaps.len(), 1);
    }

    #[test]
    fn modified_is_true_only_when_the_output_was_rewritten() {
        let pdf = build(&[b"BT /F1 12 Tf 10 10 Td (hola) Tj ET"], None, vec![]);
        let hit = remove_text_glyphs(&pdf, &[region(0, 0.0, 0.0, 612.0, 792.0)]).unwrap();
        assert!(hit.modified);
        assert_ne!(hit.output, pdf);
        let miss = remove_text_glyphs(&pdf, &[region(0, 500.0, 700.0, 1.0, 1.0)]).unwrap();
        assert_eq!(miss.regions[0].status, RemovalStatus::NothingFound);
        assert!(!miss.modified);
        assert_eq!(miss.output, pdf);
        assert!(!hit.signature.any());
    }

    #[test]
    fn signed_document_is_still_written_and_flagged() {
        use crate::test_support::{Fixture, HOLA};
        let mut fx = Fixture::new();
        fx.text_page(HOLA);
        fx.set_catalog("AcroForm", lopdf::dictionary! { "SigFlags" => 1 });
        let pdf = fx.bytes();
        let r = remove_text_glyphs(&pdf, &[region(0, 0.0, 0.0, 612.0, 792.0)]).unwrap();
        assert_eq!(r.regions[0].status, RemovalStatus::Removed);
        assert!(r.signature.sig_flags);
        assert!(r.modified);
        assert_ne!(r.output, pdf);
    }
}
