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

use crate::error::GemaError;
use crate::text_geometry::{interpret_page_text, Glyph, PageText};

/// Tolerancia de posición al verificar que los glifos no borrados siguen en su
/// sitio (puntos PDF). El intérprete trabaja con matrices `f32`.
const POSITION_TOLERANCE: f64 = 0.01;
/// Tolerancia para considerar que una caja de página empieza en el origen.
const BOX_TOLERANCE: f64 = 0.01;
/// Páginas con más operadores se dejan intactas.
const MAX_PAGE_OPERATIONS: usize = 1_000_000;

/// Rectángulo cuyo texto debe desaparecer, en espacio de página PDF (origen
/// abajo a la izquierda, puntos).
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
pub struct EraseRegion {
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

impl EraseRegion {
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
pub enum EraseStatus {
    /// Se borraron glifos y la página no tiene texto que no se pudiera medir:
    /// dentro de la región ya no queda texto.
    Erased,
    /// Se borraron glifos, pero en la página hay texto sin medir (otra fuente,
    /// un Form XObject…) que podría caer dentro de la región.
    ErasedUnverified,
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

/// Informe por región, en el mismo orden en que se pidieron.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct RegionReport {
    /// El `id` de la región.
    pub id: String,
    /// Página, base 0.
    pub page: u32,
    /// Glifos retirados del content stream.
    pub erased_glyphs: usize,
    /// Qué pasó.
    pub status: EraseStatus,
}

/// Salida de [`erase_text`].
#[derive(Debug, Clone)]
pub struct EraseResult {
    /// PDF resultante. Idéntico byte a byte a la entrada si no se borró nada.
    pub output: Vec<u8>,
    /// Un informe por región.
    pub regions: Vec<RegionReport>,
}

/// Borra del content stream los glifos cuyo centro cae dentro de alguna región.
///
/// El centro de un glifo es el punto a media anchura de su avance y a 0.3 veces
/// su tamaño por encima de la línea base: un glifo del renglón vecino queda a un
/// interlineado y no entra.
///
/// Un documento cifrado no es un error: se devuelve intacto con
/// [`EraseStatus::SkippedEncrypted`].
pub fn erase_text(input: &[u8], regions: &[EraseRegion]) -> Result<EraseResult, GemaError> {
    let mut reports: Vec<RegionReport> = regions
        .iter()
        .map(|region| RegionReport {
            id: region.id.clone(),
            page: region.page,
            erased_glyphs: 0,
            status: EraseStatus::NothingFound,
        })
        .collect();
    let unchanged = |reports| EraseResult {
        output: input.to_vec(),
        regions: reports,
    };
    if regions.is_empty() {
        return Ok(unchanged(reports));
    }

    let mut doc = Document::load_mem(input).map_err(|e| GemaError::Parse(e.to_string()))?;
    if doc.is_encrypted() {
        for report in &mut reports {
            report.status = EraseStatus::SkippedEncrypted;
        }
        return Ok(unchanged(reports));
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
            reports[index].status = EraseStatus::SkippedInvalidRegion;
        }
    }

    let mut changed = false;
    for (page, indices) in by_page {
        let page_id = pages[&(page + 1)];
        let outcome = erase_on_page(&mut doc, page_id, regions, &indices);
        for (index, erased_glyphs, status) in outcome.regions {
            reports[index].erased_glyphs = erased_glyphs;
            reports[index].status = status;
        }
        changed |= outcome.changed;
    }

    if !changed {
        return Ok(unchanged(reports));
    }
    let mut output = Vec::new();
    doc.save_to(&mut output)
        .map_err(|e| GemaError::Io(e.to_string()))?;
    Ok(EraseResult {
        output,
        regions: reports,
    })
}

struct PageOutcome {
    regions: Vec<(usize, usize, EraseStatus)>,
    changed: bool,
}

/// `(índice de operación, índice de operando, offset del código)`.
type GlyphKey = (usize, usize, usize);

fn key(glyph: &Glyph) -> GlyphKey {
    (glyph.op_index, glyph.operand_index, glyph.byte_offset)
}

fn erase_on_page(
    doc: &mut Document,
    page_id: ObjectId,
    regions: &[EraseRegion],
    indices: &[usize],
) -> PageOutcome {
    let untouched = |status| PageOutcome {
        regions: indices.iter().map(|&index| (index, 0, status)).collect(),
        changed: false,
    };
    if !page_geometry_is_plain(doc, page_id) {
        return untouched(EraseStatus::SkippedPageGeometry);
    }
    let Ok(content) = doc.get_and_decode_page_content(page_id) else {
        return untouched(EraseStatus::SkippedContent);
    };
    if content.operations.len() > MAX_PAGE_OPERATIONS
        || content
            .operations
            .iter()
            .any(|operation| operation.operator == "BI")
    {
        return untouched(EraseStatus::SkippedContent);
    }
    let Ok(before) = interpret_page_text(doc, page_id) else {
        return untouched(EraseStatus::SkippedContent);
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
            statuses.push((region_index, 0, EraseStatus::SkippedUnsupportedText));
            continue;
        }
        let count = hits[slot].len();
        if count == 0 {
            let status = if page_has_unsupported {
                EraseStatus::SkippedUnsupportedText
            } else {
                EraseStatus::NothingFound
            };
            statuses.push((region_index, 0, status));
            continue;
        }
        erase.extend(hits[slot].iter().map(|&index| key(&before.glyphs[index])));
        let status = if page_has_unsupported {
            EraseStatus::ErasedUnverified
        } else {
            EraseStatus::Erased
        };
        statuses.push((region_index, count, status));
    }
    if erase.is_empty() {
        return PageOutcome {
            regions: statuses,
            changed: false,
        };
    }

    let failed = |statuses: Vec<(usize, usize, EraseStatus)>, status| PageOutcome {
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
    };

    let Some(operations) = rewrite_operations(&content.operations, &before, &erase) else {
        return failed(statuses, EraseStatus::SkippedContent);
    };
    let Ok(bytes) = (Content { operations }).encode() else {
        return failed(statuses, EraseStatus::SkippedContent);
    };

    let old_streams = doc.get_page_contents(page_id);
    let Some(previous_contents) = doc
        .get_dictionary(page_id)
        .ok()
        .and_then(|page| page.get(b"Contents").ok())
        .cloned()
    else {
        return failed(statuses, EraseStatus::SkippedContent);
    };
    let mut stream = Stream::new(dictionary! {}, bytes);
    let _ = stream.compress();
    let new_stream = doc.add_object(stream);
    set_contents(doc, page_id, Object::Reference(new_stream));

    let verified = interpret_page_text(doc, page_id)
        .is_ok_and(|after| unerased_glyphs_unchanged(&before, &after, &erase));
    if !verified {
        set_contents(doc, page_id, previous_contents);
        doc.objects.remove(&new_stream);
        return failed(statuses, EraseStatus::SkippedVerification);
    }

    // El stream viejo sigue conteniendo el texto borrado. Si ninguna otra página
    // lo usa, se elimina: si no, el dato seguiría recuperable dentro del archivo.
    for old in old_streams {
        if !is_referenced(doc, old) {
            doc.objects.remove(&old);
        }
    }
    PageOutcome {
        regions: statuses,
        changed: true,
    }
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

fn unerased_glyphs_unchanged(
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

    use super::{erase_text, EraseRegion, EraseStatus};
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

    fn region(page: u32, x: f64, y: f64, width: f64, height: f64) -> EraseRegion {
        EraseRegion {
            id: format!("r{page}-{x}-{y}"),
            page,
            x,
            y,
            width,
            height,
        }
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
    fn erases_one_word_and_keeps_the_rest_of_the_line_in_place() {
        // A@100 B@105 espacio@110 (2.5) C@112.5 D@117.5; centro de A = (102.5, 703).
        let input = build(
            &[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB CD) Tj ET"],
            None,
            vec![],
        );
        let result = erase_text(&input, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();

        assert_eq!(result.regions[0].status, EraseStatus::Erased);
        assert_eq!(result.regions[0].erased_glyphs, 2);
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
        let result = erase_text(&input, &[region(0, 95.0, 684.0, 20.0, 10.0)]).unwrap();
        assert_eq!(result.regions[0].status, EraseStatus::Erased);
        assert_eq!(result.regions[0].erased_glyphs, 2);

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
        let result = erase_text(input, &regions).unwrap();
        assert_eq!(result.regions[0].status, EraseStatus::Erased);
        assert_eq!(result.regions[0].erased_glyphs, "Frase que se cambia".len());
        assert_eq!(result.regions[1].status, EraseStatus::Erased);

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
        let result = erase_text(&input, &[region(0, 300.0, 300.0, 50.0, 20.0)]).unwrap();
        assert_eq!(result.regions[0].status, EraseStatus::NothingFound);
        assert_eq!(result.output, input);
    }

    #[test]
    fn skips_rotated_pages_inline_images_and_invalid_regions() {
        let rotated = build(
            &[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            Some(("Rotate", Object::Integer(90))),
            vec![],
        );
        let result = erase_text(&rotated, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();
        assert_eq!(result.regions[0].status, EraseStatus::SkippedPageGeometry);
        assert_eq!(result.output, rotated);

        let inline = build(
            &[b"q 10 0 0 10 0 0 cm BI /W 1 /H 1 /BPC 8 /CS /G ID \x80 EI Q BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            None,
            vec![],
        );
        let result = erase_text(&inline, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();
        assert_eq!(result.regions[0].status, EraseStatus::SkippedContent);
        assert_eq!(result.output, inline);

        let plain = build(
            &[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            None,
            vec![],
        );
        let result = erase_text(
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
            .all(|report| report.status == EraseStatus::SkippedInvalidRegion));
        assert_eq!(result.output, plain);
    }

    #[test]
    fn clipping_text_and_positions_after_unmeasurable_text_are_never_erased() {
        let clip = build(
            &[b"BT 7 Tr /F1 10 Tf 1 0 0 1 100 700 Tm (AB) Tj ET"],
            None,
            vec![],
        );
        let result = erase_text(&clip, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();
        assert_eq!(
            result.regions[0].status,
            EraseStatus::SkippedUnsupportedText
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
        let result = erase_text(&tainted, &[region(0, 90.0, 690.0, 300.0, 30.0)]).unwrap();
        assert_eq!(
            result.regions[0].status,
            EraseStatus::SkippedUnsupportedText
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
        let result = erase_text(&input, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();
        assert_eq!(result.regions[0].status, EraseStatus::ErasedUnverified);
        assert_eq!(result.regions[0].erased_glyphs, 2);
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

        let result = erase_text(&input, &[region(0, 99.0, 695.0, 11.0, 17.0)]).unwrap();
        assert_eq!(result.regions[0].status, EraseStatus::Erased);
        assert!(glyphs(&result.output, 0).is_empty());
        assert_eq!(glyphs(&result.output, 1).len(), 2);
    }
}
