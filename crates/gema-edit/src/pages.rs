//! Seleccionar, ordenar y rotar las páginas de un documento.
//!
//! [`arrange_pages`] no crea un documento nuevo: reemplaza el árbol de
//! páginas del original. Todo lo que cuelga del catálogo —marcadores,
//! destinos, capas, XMP, Info, formulario— se conserva por construcción.
//! Lo que deja de tener sentido al quitar páginas se limpia y **se cuenta**
//! en [`ArrangeReport`]; nada se pierde sin aparecer ahí.
//!
//! Fuera de alcance: varios documentos, páginas repetidas e insertar
//! páginas de otro archivo. Esos planes se rechazan con
//! [`PagePlanError`].

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::inspect::Inspector;
use crate::options::BudgetMeter;
use crate::signature::{self, SignatureIndicators};
use crate::{EditError, EditOptions, LimitKind};

/// Versión del esquema de [`ArrangeReport`].
pub const PAGES_REPORT_SCHEMA_VERSION: u32 = 1;

/// Profundidad máxima del árbol de páginas que se recorre.
const MAX_TREE_DEPTH: usize = 64;
/// Saltos máximos al resolver una referencia encadenada.
const MAX_DEREF: usize = 16;

/// Rotación que se **suma** a la que la página ya tiene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
#[non_exhaustive]
pub enum Rotation {
    /// Sin cambio.
    #[default]
    R0,
    /// 90° en sentido horario.
    R90,
    /// 180°.
    R180,
    /// 270° en sentido horario.
    R270,
}

impl Rotation {
    /// Grados en sentido horario.
    pub fn degrees(self) -> i64 {
        match self {
            Rotation::R0 => 0,
            Rotation::R90 => 90,
            Rotation::R180 => 180,
            Rotation::R270 => 270,
        }
    }
}

/// Una página de la salida: qué página del original (0-based) y cuánto rotarla.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PageSpec {
    /// Índice 0-based de la página en el documento de entrada.
    pub source: u32,
    /// Rotación adicional.
    #[cfg_attr(feature = "serde", serde(default))]
    pub rotate: Rotation,
}

/// Por qué un plan de páginas no se puede aplicar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PagePlanError {
    /// El plan no tiene páginas.
    Empty,
    /// Un índice no existe en el documento.
    OutOfRange {
        /// Índice pedido (0-based).
        index: u32,
        /// Páginas del documento.
        count: u32,
    },
    /// Una página aparece dos veces (duplicar está fuera del piloto).
    Duplicate {
        /// Índice repetido (0-based).
        index: u32,
    },
}

impl std::fmt::Display for PagePlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PagePlanError::Empty => f.write_str("the page plan is empty"),
            PagePlanError::OutOfRange { index, count } => {
                write!(f, "page {index} is out of range (document has {count})")
            }
            PagePlanError::Duplicate { index } => write!(f, "page {index} appears twice"),
        }
    }
}

/// Qué pasó con una entrada del catálogo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum CatalogEntry {
    /// El documento no la tenía.
    Absent,
    /// Se conserva.
    Kept,
    /// Se descartó (ya no corresponde a las páginas de salida).
    Dropped,
}

/// Lo que hizo [`arrange_pages`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ArrangeReport {
    /// [`PAGES_REPORT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Páginas de la salida.
    pub pages_out: u32,
    /// Índices 0-based del original que no salen, en orden.
    pub dropped: Vec<u32>,
    /// Marcadores que apuntaban a una página quitada: siguen, sin destino.
    pub outline_items_cleared: u32,
    /// Destinos con nombre que apuntaban a una página quitada.
    pub named_dests_removed: u32,
    /// Enlaces de páginas que quedan, hacia páginas quitadas: siguen, sin destino.
    pub links_cleared: u32,
    /// Campos de formulario que se quedaron sin widgets.
    pub form_fields_removed: u32,
    /// Otras referencias a páginas quitadas, puestas en null.
    pub other_refs_cleared: u32,
    /// `/PageLabels`: se descartan si cambia el orden o la cantidad.
    pub page_labels: CatalogEntry,
    /// `/StructTreeRoot` y `/MarkInfo`: se descartan si se quitan páginas.
    pub struct_tree: CatalogEntry,
    /// Reordenar invalida una firma digital; se informa, no se bloquea.
    pub signatures: SignatureIndicators,
}

/// Salida de [`arrange_pages`].
#[derive(Debug, Clone)]
pub struct ArrangeResult {
    /// El PDF resultante.
    pub output: Vec<u8>,
    /// Qué se hizo.
    pub report: ArrangeReport,
}

/// Selecciona, ordena y rota páginas de `input` según `plan`.
///
/// La salida tiene exactamente las páginas del plan, en ese orden. Sólo
/// aplica `opts.max_input_bytes`.
pub fn arrange_pages(
    input: &[u8],
    plan: &[PageSpec],
    opts: &EditOptions,
) -> Result<ArrangeResult, EditError> {
    if input.len() > opts.max_input_bytes {
        return Err(EditError::LimitExceeded(LimitKind::InputBytes));
    }
    let mut doc = Document::load_mem(input).map_err(|e| EditError::Parse(e.to_string()))?;
    // lopdf descifra solo con contraseña vacía; guardar escribiría el PDF sin cifrar.
    if doc.is_encrypted() || doc.was_encrypted() {
        return Err(EditError::Encrypted);
    }
    let root_id = pages_root(&doc)?;
    let leaves = collect_leaves(&doc, root_id)?;
    validate(plan, leaves.len())?;

    let signatures = {
        let mut meter = BudgetMeter::new(&opts.budget);
        let mut insp = Inspector::new(&doc, &mut meter);
        signature::detect(&mut insp)
    };

    let identity = plan.len() == leaves.len()
        && plan
            .iter()
            .enumerate()
            .all(|(i, p)| p.source as usize == i && p.rotate == Rotation::R0);
    let kept: HashSet<u32> = plan.iter().map(|p| p.source).collect();
    let dropped_idx: Vec<u32> = (0..leaves.len() as u32)
        .filter(|i| !kept.contains(i))
        .collect();
    let dropped: HashSet<ObjectId> = dropped_idx.iter().map(|&i| leaves[i as usize].id).collect();

    rebuild_tree(&mut doc, root_id, &leaves, plan)?;

    let mut report = ArrangeReport {
        schema_version: PAGES_REPORT_SCHEMA_VERSION,
        pages_out: plan.len() as u32,
        dropped: dropped_idx,
        outline_items_cleared: 0,
        named_dests_removed: 0,
        links_cleared: 0,
        form_fields_removed: 0,
        other_refs_cleared: 0,
        page_labels: CatalogEntry::Absent,
        struct_tree: CatalogEntry::Absent,
        signatures,
    };

    let kept_pages: Vec<ObjectId> = plan.iter().map(|p| leaves[p.source as usize].id).collect();
    if !dropped.is_empty() {
        let removed_names = clean_named_dests(&mut doc, &dropped, &mut report)?;
        let targets = Targets {
            pages: &dropped,
            names: &removed_names,
        };
        report.outline_items_cleared = clean_outlines(&mut doc, &targets);
        report.links_cleared = clean_links(&mut doc, &kept_pages, &targets);
        let dropped_ids: Vec<ObjectId> = dropped.iter().copied().collect();
        report.form_fields_removed = clean_form(&mut doc, &dropped_ids);
    }
    let catalog = doc
        .catalog_mut()
        .map_err(|e| EditError::Parse(e.to_string()))?;
    if catalog.has(b"PageLabels") {
        report.page_labels = if identity {
            CatalogEntry::Kept
        } else {
            catalog.remove(b"PageLabels");
            CatalogEntry::Dropped
        };
    }
    if catalog.has(b"StructTreeRoot") {
        report.struct_tree = if dropped.is_empty() {
            CatalogEntry::Kept
        } else {
            catalog.remove(b"StructTreeRoot");
            catalog.remove(b"MarkInfo");
            CatalogEntry::Dropped
        };
    }
    if !dropped.is_empty() {
        report.other_refs_cleared = sweep_references(&mut doc, &dropped);
    }
    doc.prune_objects();

    let mut output = Vec::new();
    doc.save_modern(&mut output)
        .map_err(|e| EditError::Io(e.to_string()))?;
    Ok(ArrangeResult { output, report })
}

fn validate(plan: &[PageSpec], count: usize) -> Result<(), EditError> {
    if plan.is_empty() {
        return Err(EditError::PagePlan(PagePlanError::Empty));
    }
    let mut seen = HashSet::new();
    for spec in plan {
        if spec.source as usize >= count {
            return Err(EditError::PagePlan(PagePlanError::OutOfRange {
                index: spec.source,
                count: count as u32,
            }));
        }
        if !seen.insert(spec.source) {
            return Err(EditError::PagePlan(PagePlanError::Duplicate {
                index: spec.source,
            }));
        }
    }
    Ok(())
}

// ---------- árbol de páginas ----------

const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

struct Leaf {
    id: ObjectId,
    /// Atributos heredables vigentes en el padre (los propios de la hoja ganan).
    inherited: Vec<(&'static [u8], Object)>,
}

fn pages_root(doc: &Document) -> Result<ObjectId, EditError> {
    doc.catalog()
        .and_then(|c| c.get(b"Pages"))
        .and_then(Object::as_reference)
        .map_err(|_| EditError::Parse("the catalog has no /Pages reference".into()))
}

fn collect_leaves(doc: &Document, root: ObjectId) -> Result<Vec<Leaf>, EditError> {
    let mut leaves = Vec::new();
    let mut visited = HashSet::new();
    walk(doc, root, Vec::new(), 0, &mut visited, &mut leaves)?;
    Ok(leaves)
}

fn walk(
    doc: &Document,
    id: ObjectId,
    mut inherited: Vec<(&'static [u8], Object)>,
    depth: usize,
    visited: &mut HashSet<ObjectId>,
    leaves: &mut Vec<Leaf>,
) -> Result<(), EditError> {
    if depth > MAX_TREE_DEPTH {
        return Err(EditError::Parse("the page tree is too deep".into()));
    }
    if !visited.insert(id) {
        return Err(EditError::Parse("the page tree has a cycle".into()));
    }
    let node = doc.get_dictionary(id).map_err(|_| {
        EditError::Parse(format!(
            "page tree node {} {} is not a dictionary",
            id.0, id.1
        ))
    })?;
    let is_page = node.get(b"Type").and_then(Object::as_name).ok() == Some(b"Page");
    let kids = node.get(b"Kids").ok();
    if is_page || kids.is_none() {
        leaves.push(Leaf { id, inherited });
        return Ok(());
    }
    for key in INHERITABLE {
        if let Ok(value) = node.get(key) {
            inherited.retain(|(k, _)| *k != key);
            inherited.push((key, value.clone()));
        }
    }
    let kids = deref(doc, kids.unwrap_or(&Object::Null))
        .as_array()
        .map_err(|_| EditError::Parse("/Kids is not an array".into()))?;
    for kid in kids {
        let kid = kid
            .as_reference()
            .map_err(|_| EditError::Parse("/Kids holds a direct object".into()))?;
        walk(doc, kid, inherited.clone(), depth + 1, visited, leaves)?;
    }
    Ok(())
}

fn normalized_rotation(value: Option<&Object>) -> i64 {
    let raw = match value {
        Some(Object::Integer(i)) => *i,
        Some(Object::Real(r)) => *r as i64,
        _ => 0,
    };
    // Un /Rotate que no es múltiplo de 90 es inválido; se redondea al más cercano.
    let quarter = (raw as f64 / 90.0).round() as i64;
    (quarter * 90).rem_euclid(360)
}

fn rebuild_tree(
    doc: &mut Document,
    root: ObjectId,
    leaves: &[Leaf],
    plan: &[PageSpec],
) -> Result<(), EditError> {
    for spec in plan {
        let leaf = &leaves[spec.source as usize];
        let page = doc
            .get_dictionary_mut(leaf.id)
            .map_err(|e| EditError::Parse(e.to_string()))?;
        for (key, value) in &leaf.inherited {
            if !page.has(key) {
                page.set(key.to_vec(), value.clone());
            }
        }
        let rotation =
            (normalized_rotation(page.get(b"Rotate").ok()) + spec.rotate.degrees()).rem_euclid(360);
        if rotation == 0 {
            page.remove(b"Rotate");
        } else {
            page.set("Rotate", rotation);
        }
        page.set("Parent", root);
    }
    let kids: Vec<Object> = plan
        .iter()
        .map(|p| Object::Reference(leaves[p.source as usize].id))
        .collect();
    let node = doc
        .get_dictionary_mut(root)
        .map_err(|e| EditError::Parse(e.to_string()))?;
    for key in INHERITABLE {
        node.remove(key);
    }
    node.set("Type", Object::Name(b"Pages".to_vec()));
    node.set("Kids", kids);
    node.set("Count", plan.len() as i64);
    node.remove(b"Parent");
    Ok(())
}

// ---------- referencias a páginas quitadas ----------

fn deref<'a>(doc: &'a Document, mut object: &'a Object) -> &'a Object {
    for _ in 0..MAX_DEREF {
        match object {
            Object::Reference(id) => match doc.get_object(*id) {
                Ok(next) => object = next,
                Err(_) => return &Object::Null,
            },
            _ => return object,
        }
    }
    &Object::Null
}

struct Targets<'a> {
    pages: &'a HashSet<ObjectId>,
    names: &'a HashSet<Vec<u8>>,
}

impl Targets<'_> {
    /// ¿El destino (explícito, con nombre o `<< /D … >>`) va a una página quitada?
    fn dest_hits(&self, doc: &Document, dest: &Object) -> bool {
        match deref(doc, dest) {
            Object::Array(items) => items
                .first()
                .and_then(|o| o.as_reference().ok())
                .is_some_and(|id| self.pages.contains(&id)),
            Object::Name(name) | Object::String(name, _) => self.names.contains(name),
            Object::Dictionary(d) => {
                d.get(b"D")
                    .ok()
                    .is_some_and(|inner| match deref(doc, inner) {
                        Object::Dictionary(_) => false,
                        other => self.dest_hits(doc, other),
                    })
            }
            _ => false,
        }
    }

    /// ¿La acción es un GoTo hacia una página quitada?
    fn action_hits(&self, doc: &Document, action: &Object) -> bool {
        let Object::Dictionary(action) = deref(doc, action) else {
            return false;
        };
        action.get(b"S").and_then(Object::as_name).ok() == Some(b"GoTo")
            && action
                .get(b"D")
                .ok()
                .is_some_and(|d| self.dest_hits(doc, d))
    }

    /// Copia de `dict` sin `/Dest` ni `/A` si apuntan a una página quitada.
    fn cleaned(&self, doc: &Document, dict: &Dictionary) -> Option<Dictionary> {
        let dest = dict
            .get(b"Dest")
            .ok()
            .is_some_and(|d| self.dest_hits(doc, d));
        let action = dict
            .get(b"A")
            .ok()
            .is_some_and(|a| self.action_hits(doc, a));
        if !dest && !action {
            return None;
        }
        let mut out = dict.clone();
        if dest {
            out.remove(b"Dest");
        }
        if action {
            out.remove(b"A");
        }
        Some(out)
    }
}

/// Convierte en indirecto el valor `key` de `owner` si es directo; devuelve su id.
fn indirect(doc: &mut Document, owner: ObjectId, key: &[u8]) -> Option<ObjectId> {
    let value = doc.get_dictionary(owner).ok()?.get(key).ok()?.clone();
    match value {
        Object::Reference(id) => Some(id),
        Object::Dictionary(_) => {
            let id = doc.add_object(value);
            doc.get_dictionary_mut(owner).ok()?.set(key.to_vec(), id);
            Some(id)
        }
        _ => None,
    }
}

fn catalog_id(doc: &Document) -> Result<ObjectId, EditError> {
    doc.trailer
        .get(b"Root")
        .and_then(Object::as_reference)
        .map_err(|_| EditError::Parse("the trailer has no /Root reference".into()))
}

/// Quita de `/Names /Dests` y de `/Dests` los destinos a páginas quitadas.
/// Devuelve los nombres quitados.
fn clean_named_dests(
    doc: &mut Document,
    dropped: &HashSet<ObjectId>,
    report: &mut ArrangeReport,
) -> Result<HashSet<Vec<u8>>, EditError> {
    let none = HashSet::new();
    let targets = Targets {
        pages: dropped,
        names: &none,
    };
    let catalog = catalog_id(doc)?;
    let mut removed = HashSet::new();

    // Árbol de nombres: nodos con /Names [clave valor …] y/o /Kids.
    if let Some(names) = indirect(doc, catalog, b"Names") {
        if let Some(tree) = indirect(doc, names, b"Dests") {
            let mut pending = vec![(tree, 0usize)];
            let mut visited = HashSet::new();
            while let Some((node_id, depth)) = pending.pop() {
                if depth > MAX_TREE_DEPTH || !visited.insert(node_id) {
                    continue;
                }
                let Ok(node) = doc.get_dictionary(node_id) else {
                    continue;
                };
                if let Ok(kids) = deref(doc, node.get(b"Kids").unwrap_or(&Object::Null)).as_array()
                {
                    pending.extend(
                        kids.iter()
                            .filter_map(|k| k.as_reference().ok())
                            .map(|k| (k, depth + 1)),
                    );
                }
                let Ok(pairs) = node.get(b"Names").and_then(Object::as_array) else {
                    continue;
                };
                let mut kept = Vec::with_capacity(pairs.len());
                for pair in pairs.chunks(2) {
                    let hit = pair.len() == 2 && targets.dest_hits(doc, &pair[1]);
                    if hit {
                        if let Object::String(name, _) = &pair[0] {
                            removed.insert(name.clone());
                        }
                        report.named_dests_removed += 1;
                    } else {
                        kept.extend(pair.iter().cloned());
                    }
                }
                if kept.len() != pairs.len() {
                    if let Ok(node) = doc.get_dictionary_mut(node_id) {
                        node.set("Names", kept);
                    }
                }
            }
        }
    }

    // Estilo PDF 1.1: /Dests en el catálogo, nombre → destino.
    if let Some(dests) = indirect(doc, catalog, b"Dests") {
        let hits: Vec<Vec<u8>> = doc
            .get_dictionary(dests)
            .map(|d| {
                d.iter()
                    .filter(|(_, v)| targets.dest_hits(doc, v))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default();
        if let Ok(dict) = doc.get_dictionary_mut(dests) {
            for key in hits {
                dict.remove(&key);
                report.named_dests_removed += 1;
                removed.insert(key);
            }
        }
    }
    Ok(removed)
}

fn clean_outlines(doc: &mut Document, targets: &Targets) -> u32 {
    let Some(catalog) = catalog_id(doc).ok() else {
        return 0;
    };
    let Some(root) = doc
        .get_dictionary(catalog)
        .ok()
        .and_then(|c| c.get(b"Outlines").ok())
        .and_then(|o| o.as_reference().ok())
    else {
        return 0;
    };
    let mut cleared = 0;
    let mut visited = HashSet::new();
    let first = |doc: &Document, id: ObjectId| {
        doc.get_dictionary(id)
            .ok()
            .and_then(|d| d.get(b"First").ok())
            .and_then(|o| o.as_reference().ok())
    };
    let mut pending: Vec<ObjectId> = first(doc, root).into_iter().collect();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) || visited.len() > 1_000_000 {
            continue;
        }
        let Ok(item) = doc.get_dictionary(id) else {
            continue;
        };
        if let Some(next) = item.get(b"Next").ok().and_then(|o| o.as_reference().ok()) {
            pending.push(next);
        }
        if let Some(child) = first(doc, id) {
            pending.push(child);
        }
        if let Some(clean) = targets.cleaned(doc, item) {
            if let Ok(item) = doc.get_dictionary_mut(id) {
                *item = clean;
                cleared += 1;
            }
        }
    }
    cleared
}

fn annots_of(doc: &Document, page: ObjectId) -> Vec<Object> {
    doc.get_dictionary(page)
        .ok()
        .and_then(|p| p.get(b"Annots").ok())
        .map(|a| deref(doc, a))
        .and_then(|a| a.as_array().ok())
        .cloned()
        .unwrap_or_default()
}

fn clean_links(doc: &mut Document, kept_pages: &[ObjectId], targets: &Targets) -> u32 {
    let mut cleared = 0;
    for &page in kept_pages {
        let annots = annots_of(doc, page);
        let mut direct_changed = false;
        let mut rewritten = annots.clone();
        for (index, annot) in annots.iter().enumerate() {
            match annot {
                Object::Reference(id) => {
                    let Some(clean) = doc
                        .get_dictionary(*id)
                        .ok()
                        .and_then(|d| targets.cleaned(doc, d))
                    else {
                        continue;
                    };
                    if let Ok(dict) = doc.get_dictionary_mut(*id) {
                        *dict = clean;
                        cleared += 1;
                    }
                }
                Object::Dictionary(dict) => {
                    if let Some(clean) = targets.cleaned(doc, dict) {
                        rewritten[index] = Object::Dictionary(clean);
                        direct_changed = true;
                        cleared += 1;
                    }
                }
                _ => {}
            }
        }
        if direct_changed {
            if let Ok(p) = doc.get_dictionary_mut(page) {
                p.set("Annots", rewritten);
            }
        }
    }
    cleared
}

/// Quita del formulario los widgets de páginas quitadas y los campos que se
/// quedan sin ninguno. Devuelve cuántos campos (nodos con `/T`) se fueron.
fn clean_form(doc: &mut Document, dropped_pages: &[ObjectId]) -> u32 {
    let Ok(catalog) = catalog_id(doc) else {
        return 0;
    };
    let Some(acro) = indirect(doc, catalog, b"AcroForm") else {
        return 0;
    };
    let dropped_set: HashSet<ObjectId> = dropped_pages.iter().copied().collect();
    let mut widgets: HashSet<ObjectId> = dropped_pages
        .iter()
        .flat_map(|&p| annots_of(doc, p))
        .filter_map(|a| a.as_reference().ok())
        .collect();
    // Widgets que declaran su página con /P aunque no estén en sus /Annots.
    for (id, object) in &doc.objects {
        if let Ok(dict) = object.as_dict() {
            let is_widget = dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Widget");
            let on_dropped = dict
                .get(b"P")
                .and_then(Object::as_reference)
                .is_ok_and(|p| dropped_set.contains(&p));
            if is_widget && on_dropped {
                widgets.insert(*id);
            }
        }
    }
    let Some(fields) = doc
        .get_dictionary(acro)
        .ok()
        .and_then(|a| a.get(b"Fields").ok())
        .map(|f| deref(doc, f).clone())
        .and_then(|f| f.as_array().ok().cloned())
    else {
        return 0;
    };
    let mut removed = 0;
    let mut memo = HashMap::new();
    let kept: Vec<Object> = fields
        .into_iter()
        .filter(|f| {
            f.as_reference()
                .map(|id| keep_field(doc, id, &widgets, 0, &mut memo, &mut removed))
                .unwrap_or(true)
        })
        .collect();
    if let Ok(acro) = doc.get_dictionary_mut(acro) {
        acro.set("Fields", kept);
    }
    removed
}

fn keep_field(
    doc: &mut Document,
    id: ObjectId,
    widgets: &HashSet<ObjectId>,
    depth: usize,
    memo: &mut HashMap<ObjectId, bool>,
    removed: &mut u32,
) -> bool {
    if let Some(&known) = memo.get(&id) {
        return known;
    }
    memo.insert(id, true); // corta ciclos
    let Ok(node) = doc.get_dictionary(id) else {
        return true;
    };
    let is_field = node.has(b"T");
    let keep = if widgets.contains(&id) {
        false
    } else if let Some(kids) = node
        .get(b"Kids")
        .ok()
        .and_then(|k| deref(doc, k).as_array().ok().cloned())
    {
        if depth > MAX_TREE_DEPTH || kids.is_empty() {
            true
        } else {
            let kept: Vec<Object> = kids
                .into_iter()
                .filter(|k| {
                    k.as_reference()
                        .map(|kid| keep_field(doc, kid, widgets, depth + 1, memo, removed))
                        .unwrap_or(true)
                })
                .collect();
            let any = !kept.is_empty();
            if let Ok(node) = doc.get_dictionary_mut(id) {
                node.set("Kids", kept);
            }
            any
        }
    } else {
        true
    };
    if !keep && is_field {
        *removed += 1;
    }
    memo.insert(id, keep);
    keep
}

/// Pone en null toda referencia a una página quitada que siga alcanzable
/// desde el trailer sin pasar por una página quitada. Devuelve cuántas.
fn sweep_references(doc: &mut Document, dropped: &HashSet<ObjectId>) -> u32 {
    let mut reached = HashSet::new();
    let mut pending: Vec<ObjectId> = doc
        .trailer
        .iter()
        .filter_map(|(_, v)| v.as_reference().ok())
        .collect();
    let mut to_fix = Vec::new();
    fn refs(object: &Object, out: &mut Vec<ObjectId>) {
        match object {
            Object::Reference(id) => out.push(*id),
            Object::Array(items) => items.iter().for_each(|o| refs(o, out)),
            Object::Dictionary(d) => d.iter().for_each(|(_, o)| refs(o, out)),
            Object::Stream(s) => s.dict.iter().for_each(|(_, o)| refs(o, out)),
            _ => {}
        }
    }
    while let Some(id) = pending.pop() {
        if dropped.contains(&id) || !reached.insert(id) {
            continue;
        }
        let Ok(object) = doc.get_object(id) else {
            continue;
        };
        let mut found = Vec::new();
        refs(object, &mut found);
        if found.iter().any(|r| dropped.contains(r)) {
            to_fix.push(id);
        }
        pending.extend(found);
    }
    fn null_out(object: &mut Object, dropped: &HashSet<ObjectId>, count: &mut u32) {
        match object {
            Object::Reference(id) if dropped.contains(id) => {
                *object = Object::Null;
                *count += 1;
            }
            Object::Array(items) => items.iter_mut().for_each(|o| null_out(o, dropped, count)),
            Object::Dictionary(d) => d.iter_mut().for_each(|(_, o)| null_out(o, dropped, count)),
            Object::Stream(s) => s
                .dict
                .iter_mut()
                .for_each(|(_, o)| null_out(o, dropped, count)),
            _ => {}
        }
    }
    let mut count = 0;
    for id in to_fix {
        if let Ok(object) = doc.get_object_mut(id) {
            null_out(object, dropped, &mut count);
        }
    }
    count
}

#[cfg(test)]
mod tests;
