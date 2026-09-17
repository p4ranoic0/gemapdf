//! Inspección de las superficies donde el texto sobrevive al borrado.
//!
//! Todo lo que hay acá **informa, no bloquea**. Cada resolución de objeto
//! pasa por [`Inspector::deref`], que sigue referencias con tope de
//! profundidad, detecta ciclos y cobra al presupuesto. Cuando algo no se
//! pudo mirar, queda un [`InspectionGap`] y el resultado lleva
//! `inspection_incomplete = true`: no saber no es lo mismo que no haber.

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::BTreeMap;

use crate::options::BudgetMeter;

/// Superficie de la página donde el texto puede seguir existiendo aunque el
/// content stream directo ya no lo emita. `page` es índice base 0.
///
/// `Ord` existe para que el resultado salga ordenado y deduplicado (Task 7);
/// el orden concreto no es contrato, que sea determinista sí.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "kind", rename_all = "snake_case"))]
#[non_exhaustive]
pub enum ResidualRisk {
    /// El content stream de esta página también lo usan otras: reescribirlo
    /// las afecta a todas, y el texto que otras dibujan encima no se tocó.
    SharedContentStream {
        /// Página inspeccionada.
        page: u32,
        /// Otras páginas (base 0) que comparten al menos un stream.
        shared_with: Vec<u32>,
    },
    /// La página dibuja un Form XObject. Su contenido **no se reescribe ni se
    /// inspecciona**: el texto que haya adentro sobrevive.
    #[cfg_attr(feature = "serde", serde(rename = "form_xobject"))]
    FormXObject {
        /// Página inspeccionada.
        page: u32,
        /// Nombre del recurso (`/XObject /<name>`).
        name: String,
    },
    /// Una anotación cuyo `/Rect` toca la región (o no se pudo leer).
    Annotation {
        /// Página inspeccionada.
        page: u32,
        /// `/Subtype` de la anotación, o `unknown`.
        subtype: String,
    },
    /// Contenido opcional (`/OC`, OCG/OCMD): el texto puede estar en una capa
    /// que el visor oculta o muestra a voluntad.
    OptionalContent {
        /// Página inspeccionada.
        page: u32,
    },
    /// `/ActualText` o `/Alt` en contenido marcado: el texto sigue en el
    /// diccionario de propiedades aunque los glifos no se emitan.
    ActualText {
        /// Página inspeccionada.
        page: u32,
    },
}

impl ResidualRisk {
    /// Nombre estable del tipo de riesgo, idéntico al `kind` que produce serde.
    pub fn kind(&self) -> &'static str {
        match self {
            ResidualRisk::SharedContentStream { .. } => "shared_content_stream",
            ResidualRisk::FormXObject { .. } => "form_xobject",
            ResidualRisk::Annotation { .. } => "annotation",
            ResidualRisk::OptionalContent { .. } => "optional_content",
            ResidualRisk::ActualText { .. } => "actual_text",
        }
    }

    /// Página (base 0) a la que refiere el riesgo.
    pub fn page(&self) -> u32 {
        match self {
            ResidualRisk::SharedContentStream { page, .. }
            | ResidualRisk::FormXObject { page, .. }
            | ResidualRisk::Annotation { page, .. }
            | ResidualRisk::OptionalContent { page }
            | ResidualRisk::ActualText { page } => *page,
        }
    }
}

/// Por qué una parte de la inspección no se pudo completar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum GapReason {
    /// Una referencia indirecta no resuelve.
    BrokenReference,
    /// Las referencias forman un ciclo.
    ReferenceCycle,
    /// Se superó `ObjectBudget::max_reference_depth`.
    ReferenceDepth,
    /// Se agotó el presupuesto de objetos o de streams.
    BudgetExhausted,
    /// Un stream usa un filtro distinto de `FlateDecode`.
    UnsupportedFilter,
    /// Un stream no se pudo descomprimir.
    CorruptStream,
    /// `/Annots` no es un array o contiene algo que no es un diccionario.
    MalformedAnnots,
    /// `/Rect` ausente, malformado o no finito.
    MalformedRect,
    /// Un nombre de recurso no existe en `/Resources` propio ni heredado.
    MissingResource,
    /// El objeto existe y resuelve, pero no tiene el tipo esperado (un
    /// `Integer` donde va un diccionario, un stream donde va un array…).
    MalformedObject,
    /// Documento cifrado: no se inspecciona nada.
    Encrypted,
    /// La llamada no pidió ninguna región: no se parseó ni se inspeccionó.
    NotInspected,
}

impl GapReason {
    /// Nombre estable, idéntico al que produce serde.
    pub fn as_str(&self) -> &'static str {
        match self {
            GapReason::BrokenReference => "broken_reference",
            GapReason::ReferenceCycle => "reference_cycle",
            GapReason::ReferenceDepth => "reference_depth",
            GapReason::BudgetExhausted => "budget_exhausted",
            GapReason::UnsupportedFilter => "unsupported_filter",
            GapReason::CorruptStream => "corrupt_stream",
            GapReason::MalformedAnnots => "malformed_annots",
            GapReason::MalformedRect => "malformed_rect",
            GapReason::MissingResource => "missing_resource",
            GapReason::MalformedObject => "malformed_object",
            GapReason::Encrypted => "encrypted",
            GapReason::NotInspected => "not_inspected",
        }
    }
}

/// Algo que la inspección no pudo mirar.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct InspectionGap {
    /// Por qué.
    pub reason: GapReason,
    /// Página (base 0), si aplica.
    pub page: Option<u32>,
    /// Detalle legible: objeto, nombre de recurso, filtro.
    pub detail: String,
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `/Rect` como `[x_min, y_min, x_max, y_max]`.
fn parse_rect(obj: &Object) -> Option<[f64; 4]> {
    let items = obj.as_array().ok()?;
    if items.len() != 4 {
        return None;
    }
    let mut v = [0.0f64; 4];
    for (i, item) in items.iter().enumerate() {
        v[i] = match item {
            Object::Integer(n) => *n as f64,
            Object::Real(x) => f64::from(*x),
            _ => return None,
        };
        if !v[i].is_finite() {
            return None;
        }
    }
    Some([
        v[0].min(v[2]),
        v[1].min(v[3]),
        v[0].max(v[2]),
        v[1].max(v[3]),
    ])
}

/// Intersección estricta: bordes que se tocan y rectángulos de área cero no cuentan.
fn intersects(rect: [f64; 4], region: &crate::TextRegion) -> bool {
    let (rx1, ry1) = (region.x, region.y);
    let (rx2, ry2) = (region.x + region.width, region.y + region.height);
    let has_area = rect[0] < rect[2] && rect[1] < rect[3];
    has_area && rect[0] < rx2 && rx1 < rect[2] && rect[1] < ry2 && ry1 < rect[3]
}

/// Recorre el documento cobrando cada resolución al presupuesto.
pub(crate) struct Inspector<'a> {
    doc: &'a Document,
    meter: &'a mut BudgetMeter,
    pub(crate) risks: Vec<ResidualRisk>,
    pub(crate) gaps: Vec<InspectionGap>,
    contents_index: Option<Vec<(u32, Vec<ObjectId>)>>,
}

impl<'a> Inspector<'a> {
    pub(crate) fn doc(&self) -> &'a Document {
        self.doc
    }

    pub(crate) fn check_depth(&self, depth: usize) -> Result<(), crate::LimitKind> {
        self.meter.check_depth(depth)
    }

    pub(crate) fn new(doc: &'a Document, meter: &'a mut BudgetMeter) -> Self {
        Inspector {
            doc,
            meter,
            risks: Vec::new(),
            gaps: Vec::new(),
            contents_index: None,
        }
    }

    pub(crate) fn gap(&mut self, reason: GapReason, page: Option<u32>, detail: impl Into<String>) {
        self.gaps.push(InspectionGap {
            reason,
            page,
            detail: detail.into(),
        });
    }

    /// Sigue referencias hasta un objeto directo. Cada salto cobra un objeto
    /// y cuenta profundidad; un ciclo o una referencia rota dejan gap y `None`.
    pub(crate) fn deref(&mut self, obj: &'a Object, page: Option<u32>) -> Option<&'a Object> {
        let mut seen: Vec<ObjectId> = Vec::new();
        let mut cur = obj;
        loop {
            let Object::Reference(id) = cur else {
                return Some(cur);
            };
            let label = format!("{} {} R", id.0, id.1);
            if seen.contains(id) {
                self.gap(GapReason::ReferenceCycle, page, label);
                return None;
            }
            if self.meter.check_depth(seen.len() + 1).is_err() {
                self.gap(GapReason::ReferenceDepth, page, label);
                return None;
            }
            if let Err(kind) = self.meter.touch_object() {
                self.gap(GapReason::BudgetExhausted, page, kind.as_str());
                return None;
            }
            seen.push(*id);
            // `doc.objects.get`, NO `doc.get_object`: `get_object` sigue la
            // cadena de referencias por su cuenta (hasta 128 saltos, sin cobrar)
            // y este bucle nunca vería los saltos intermedios. Verificado en
            // lopdf 0.43 `document.rs:162`.
            match self.doc.objects.get(id) {
                Some(o) => cur = o,
                None => {
                    self.gap(GapReason::BrokenReference, page, label);
                    return None;
                }
            }
        }
    }

    /// Como [`deref`](Self::deref), pero exige **diccionario**. Si el objeto
    /// resuelve a otra cosa —incluido un stream: ningún uso de este método
    /// acepta uno— deja gap `MalformedObject` y devuelve `None`. Así un
    /// `None` siempre tiene su gap: ausencia y malformación no se confunden.
    /// Quien necesite un stream usa `deref` y hace su propio `match`.
    pub(crate) fn deref_dict(
        &mut self,
        obj: &'a Object,
        page: Option<u32>,
    ) -> Option<&'a Dictionary> {
        match self.deref(obj, page)? {
            Object::Dictionary(d) => Some(d),
            other => {
                self.gap(
                    GapReason::MalformedObject,
                    page,
                    format!("expected a dictionary, found {}", other.enum_variant()),
                );
                None
            }
        }
    }

    /// Diccionarios de recursos de la página y de sus ancestros, en ese
    /// orden (el más cercano primero). Se sube por `/Parent` a mano:
    /// `lopdf::Document::get_page_resources` sólo junta los `/Resources` que
    /// son **referencias** y descarta los inline de los nodos `/Pages`
    /// (verificado en lopdf 0.43 `document.rs:605`). Cada salto a un padre
    /// cobra un objeto y cuenta profundidad.
    pub(crate) fn resources(&mut self, page_id: ObjectId, page: u32) -> Vec<&'a Dictionary> {
        let mut out = Vec::new();
        let Some(Object::Dictionary(first)) = self.doc.objects.get(&page_id) else {
            self.gap(GapReason::BrokenReference, Some(page), "page dictionary");
            return out;
        };
        let mut node: &'a Dictionary = first;
        let mut seen = vec![page_id];
        loop {
            if let Ok(res) = node.get(b"Resources") {
                // `deref_dict` cobra si es referencia y deja gap si no resuelve.
                if let Some(d) = self.deref_dict(res, Some(page)) {
                    out.push(d);
                }
            }
            let Ok(parent) = node.get(b"Parent") else {
                break;
            };
            let Object::Reference(parent_id) = parent else {
                self.gap(
                    GapReason::MalformedObject,
                    Some(page),
                    format!("/Parent is {}", parent.enum_variant()),
                );
                break;
            };
            if seen.contains(parent_id) {
                self.gap(GapReason::ReferenceCycle, Some(page), "/Parent");
                break;
            }
            if self.meter.check_depth(seen.len()).is_err() {
                self.gap(GapReason::ReferenceDepth, Some(page), "/Parent");
                break;
            }
            if let Err(kind) = self.meter.touch_object() {
                self.gap(GapReason::BudgetExhausted, Some(page), kind.as_str());
                break;
            }
            seen.push(*parent_id);
            match self.doc.objects.get(parent_id) {
                Some(Object::Dictionary(d)) => node = d,
                Some(other) => {
                    self.gap(
                        GapReason::MalformedObject,
                        Some(page),
                        format!("/Parent is {}", other.enum_variant()),
                    );
                    break;
                }
                None => {
                    self.gap(GapReason::BrokenReference, Some(page), "/Parent");
                    break;
                }
            }
        }
        out
    }

    /// Busca `/category /name` en los recursos, propio antes que heredado.
    pub(crate) fn resource(
        &mut self,
        resources: &[&'a Dictionary],
        category: &[u8],
        name: &[u8],
        page: u32,
    ) -> Option<&'a Object> {
        for res in resources {
            let res: &'a Dictionary = res;
            let Ok(cat) = res.get(category) else { continue };
            let cat = self.deref_dict(cat, Some(page))?;
            if let Ok(entry) = cat.get(name) {
                return self.deref(entry, Some(page));
            }
        }
        self.gap(
            GapReason::MissingResource,
            Some(page),
            format!("/{} /{}", lossy(category), lossy(name)),
        );
        None
    }

    /// `BDC` con propiedades inline o por nombre: `OptionalContent` si el tag
    /// es `/OC` o el diccionario es OCG/OCMD; `ActualText` si trae
    /// `/ActualText` o `/Alt`. Un riesgo por tipo y por página, como máximo.
    pub(crate) fn inspect_marked_content(
        &mut self,
        content: &Content,
        resources: &[&'a Dictionary],
        page: u32,
    ) {
        let mut optional = false;
        let mut actual = false;
        for op in &content.operations {
            if op.operator != "BDC" {
                continue;
            }
            let Some(Object::Name(tag)) = op.operands.first() else {
                continue;
            };
            if tag == b"OC" {
                optional = true;
            }
            let props: Option<Dictionary> = match op.operands.get(1) {
                Some(Object::Dictionary(d)) => Some(d.clone()),
                Some(Object::Name(n)) => match self.resource(resources, b"Properties", n, page) {
                    Some(Object::Dictionary(d)) => Some(d.clone()),
                    // `resource` ya dejó gap si no resolvió.
                    None => None,
                    Some(other) => {
                        self.gap(
                            GapReason::MalformedObject,
                            Some(page),
                            format!("/Properties /{} is {}", lossy(n), other.enum_variant()),
                        );
                        None
                    }
                },
                Some(other) => {
                    self.gap(
                        GapReason::MalformedObject,
                        Some(page),
                        format!("BDC operand is {}", other.enum_variant()),
                    );
                    None
                }
                // `BDC` con un solo operando es inválido, pero no esconde texto.
                None => None,
            };
            if let Some(d) = props {
                if d.has(b"ActualText") || d.has(b"Alt") {
                    actual = true;
                }
                if matches!(d.get(b"Type"), Ok(Object::Name(t)) if t == b"OCG" || t == b"OCMD") {
                    optional = true;
                }
            }
        }
        if optional {
            self.risks.push(ResidualRisk::OptionalContent { page });
        }
        if actual {
            self.risks.push(ResidualRisk::ActualText { page });
        }
    }

    /// Recorre `/Annots` a mano, distinguiendo ausencia de referencias rotas.
    pub(crate) fn inspect_annotations(
        &mut self,
        page_dict: &'a Dictionary,
        page: u32,
        regions: &[&crate::TextRegion],
    ) {
        let annots = match page_dict.get(b"Annots") {
            Err(_) => return,
            Ok(o) => o,
        };
        let Some(annots) = self.deref(annots, Some(page)) else {
            return;
        };
        let Object::Array(items) = annots else {
            self.gap(
                GapReason::MalformedAnnots,
                Some(page),
                "/Annots is not an array",
            );
            return;
        };
        for (i, item) in items.iter().enumerate() {
            let Some(obj) = self.deref(item, Some(page)) else {
                continue;
            };
            let Object::Dictionary(dict) = obj else {
                self.gap(
                    GapReason::MalformedAnnots,
                    Some(page),
                    format!("/Annots[{i}] is not a dictionary"),
                );
                continue;
            };
            let subtype = match dict.get(b"Subtype") {
                Ok(Object::Name(n)) => lossy(n),
                _ => "unknown".to_string(),
            };
            let rect = dict
                .get(b"Rect")
                .ok()
                .and_then(|r| self.deref(r, Some(page)))
                .and_then(parse_rect);
            match rect {
                Some(rect) => {
                    if regions.iter().any(|g| intersects(rect, g)) {
                        self.risks.push(ResidualRisk::Annotation { page, subtype });
                    }
                }
                None => {
                    self.gap(
                        GapReason::MalformedRect,
                        Some(page),
                        format!("/Annots[{i}] /Subtype /{subtype}"),
                    );
                    self.risks.push(ResidualRisk::Annotation { page, subtype });
                }
            }
        }
    }

    pub(crate) fn inspect_shared_content(
        &mut self,
        original: &[ObjectId],
        page: u32,
        pages: &BTreeMap<u32, ObjectId>,
    ) {
        if original.is_empty() {
            return;
        }
        let doc = self.doc;
        let index = self.contents_index.get_or_insert_with(|| {
            pages
                .iter()
                .map(|(number, id)| (number.saturating_sub(1), doc.get_page_contents(*id)))
                .collect()
        });
        let shared_with = index
            .iter()
            .filter(|(other, ids)| *other != page && ids.iter().any(|id| original.contains(id)))
            .map(|(other, _)| *other)
            .collect::<Vec<_>>();
        if !shared_with.is_empty() {
            self.risks
                .push(ResidualRisk::SharedContentStream { page, shared_with });
        }
    }

    pub(crate) fn inspect_form_xobjects(
        &mut self,
        content: &Content,
        resources: &[&'a Dictionary],
        page: u32,
    ) {
        let mut seen = std::collections::BTreeSet::new();
        for op in &content.operations {
            if op.operator != "Do" {
                continue;
            }
            let Some(Object::Name(name)) = op.operands.first() else {
                continue;
            };
            if !seen.insert(name.clone()) {
                continue;
            }
            match self.resource(resources, b"XObject", name, page) {
                Some(Object::Stream(stream)) => {
                    if matches!(stream.dict.get(b"Subtype"), Ok(Object::Name(kind)) if kind == b"Form")
                    {
                        self.risks.push(ResidualRisk::FormXObject {
                            page,
                            name: lossy(name),
                        });
                    }
                }
                Some(other) => self.gap(
                    GapReason::MalformedObject,
                    Some(page),
                    format!("/XObject /{} is {}", lossy(name), other.enum_variant()),
                ),
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{BudgetMeter, ObjectBudget};
    use crate::test_support::Fixture;
    use crate::TextRegion;
    use lopdf::content::Content;
    use lopdf::{dictionary, Object};

    fn run_marked_content(
        fx: &Fixture,
        page_id: lopdf::ObjectId,
        ops: &[u8],
        budget: ObjectBudget,
    ) -> (Vec<ResidualRisk>, Vec<InspectionGap>) {
        let mut meter = BudgetMeter::new(&budget);
        let mut insp = Inspector::new(&fx.doc, &mut meter);
        let resources = insp.resources(page_id, 0);
        let content = Content::decode(ops).unwrap();
        insp.inspect_marked_content(&content, &resources, 0);
        (insp.risks, insp.gaps)
    }

    #[test]
    fn inline_actual_text_is_a_risk() {
        let ops = b"BT /F1 12 Tf /Span <</ActualText (hola)>> BDC (x) Tj EMC ET";
        let mut fx = Fixture::new();
        let page = fx.text_page(ops);
        let (risks, gaps) = run_marked_content(&fx, page, ops, ObjectBudget::default());
        assert_eq!(risks, vec![ResidualRisk::ActualText { page: 0 }]);
        assert!(gaps.is_empty());
    }

    #[test]
    fn named_properties_resolve_through_a_valid_indirect_reference() {
        let ops = b"/OC /P1 BDC BT /F1 12 Tf (x) Tj ET EMC";
        let mut fx = Fixture::new();
        let ocg = fx
            .doc
            .add_object(dictionary! { "Type" => "OCG", "Name" => "capa" });
        let mut res = Fixture::default_resources();
        res.set("Properties", dictionary! { "P1" => ocg });
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, Some(res), vec![]);
        let (risks, gaps) = run_marked_content(&fx, page, ops, ObjectBudget::default());
        assert_eq!(risks, vec![ResidualRisk::OptionalContent { page: 0 }]);
        assert!(gaps.is_empty(), "{gaps:?}");
    }

    #[test]
    fn named_properties_with_a_broken_reference_is_a_gap_and_still_a_risk() {
        let ops = b"/OC /P1 BDC BT /F1 12 Tf (x) Tj ET EMC";
        let mut fx = Fixture::new();
        let mut res = Fixture::default_resources();
        res.set(
            "Properties",
            dictionary! { "P1" => Object::Reference((999, 0)) },
        );
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, Some(res), vec![]);
        let (risks, gaps) = run_marked_content(&fx, page, ops, ObjectBudget::default());
        assert_eq!(risks, vec![ResidualRisk::OptionalContent { page: 0 }]);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].reason, GapReason::BrokenReference);
        assert_eq!(gaps[0].page, Some(0));
    }

    #[test]
    fn inherited_properties_are_found_through_the_pages_node() {
        let ops = b"/Span /P1 BDC BT /F1 12 Tf (x) Tj ET EMC";
        let mut fx = Fixture::new();
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, None, vec![]);
        let mut res = Fixture::default_resources();
        res.set(
            "Properties",
            dictionary! { "P1" => dictionary! { "ActualText" => "hola" } },
        );
        fx.set_pages("Resources", res);
        let (risks, gaps) = run_marked_content(&fx, page, ops, ObjectBudget::default());
        assert_eq!(risks, vec![ResidualRisk::ActualText { page: 0 }]);
        assert!(gaps.is_empty(), "{gaps:?}");
    }

    #[test]
    fn reference_cycle_is_a_gap_not_a_hang() {
        let ops = b"/Span /P1 BDC BT /F1 12 Tf (x) Tj ET EMC";
        let mut fx = Fixture::new();
        let a = fx.doc.new_object_id();
        let b = fx.doc.new_object_id();
        fx.doc.objects.insert(a, Object::Reference(b));
        fx.doc.objects.insert(b, Object::Reference(a));
        let mut res = Fixture::default_resources();
        res.set("Properties", dictionary! { "P1" => a });
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, Some(res), vec![]);
        let (risks, gaps) = run_marked_content(&fx, page, ops, ObjectBudget::default());
        assert!(risks.is_empty());
        assert_eq!(gaps[0].reason, GapReason::ReferenceCycle);
    }

    #[test]
    fn reference_depth_is_capped() {
        let ops = b"/Span /P1 BDC BT /F1 12 Tf (x) Tj ET EMC";
        let mut fx = Fixture::new();
        let target = fx.doc.add_object(dictionary! { "ActualText" => "hola" });
        let hop2 = fx.doc.add_object(Object::Reference(target));
        let hop1 = fx.doc.add_object(Object::Reference(hop2));
        let mut res = Fixture::default_resources();
        res.set("Properties", dictionary! { "P1" => hop1 });
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, Some(res), vec![]);
        let tight = ObjectBudget {
            max_reference_depth: 2,
            ..ObjectBudget::default()
        };
        let (risks, gaps) = run_marked_content(&fx, page, ops, tight);
        assert!(risks.is_empty());
        assert_eq!(gaps[0].reason, GapReason::ReferenceDepth);
        let (risks, gaps) = run_marked_content(&fx, page, ops, ObjectBudget::default());
        assert_eq!(risks, vec![ResidualRisk::ActualText { page: 0 }]);
        assert!(gaps.is_empty());
    }

    #[test]
    fn missing_named_property_is_a_gap() {
        let ops = b"/Span /Nope BDC BT /F1 12 Tf (x) Tj ET EMC";
        let mut fx = Fixture::new();
        let page = fx.text_page(ops);
        let (risks, gaps) = run_marked_content(&fx, page, ops, ObjectBudget::default());
        assert!(risks.is_empty());
        assert_eq!(gaps[0].reason, GapReason::MissingResource);
    }

    #[test]
    fn property_that_resolves_to_a_non_dictionary_is_a_gap_not_silence() {
        // "Existe pero no se pudo interpretar" no es "no existe".
        let ops = b"/Span /P1 BDC BT /F1 12 Tf (x) Tj ET EMC";
        let mut fx = Fixture::new();
        let not_a_dict = fx.doc.add_object(Object::Integer(7));
        let mut res = Fixture::default_resources();
        res.set("Properties", dictionary! { "P1" => not_a_dict });
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, Some(res), vec![]);
        let (risks, gaps) = run_marked_content(&fx, page, ops, ObjectBudget::default());
        assert!(risks.is_empty());
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert_eq!(gaps[0].reason, GapReason::MalformedObject);
    }

    #[test]
    fn deref_dict_rejects_streams_and_other_types_with_a_gap() {
        let mut fx = Fixture::new();
        let stream = fx
            .doc
            .add_object(lopdf::Stream::new(dictionary! {}, vec![]));
        let name = fx.doc.add_object(Object::Name(b"X".to_vec()));
        let dict = fx.doc.add_object(dictionary! { "A" => 1 });
        let mut meter = BudgetMeter::new(&ObjectBudget::default());
        let mut insp = Inspector::new(&fx.doc, &mut meter);
        let (s, n, d) = (
            Object::Reference(stream),
            Object::Reference(name),
            Object::Reference(dict),
        );
        assert!(insp.deref_dict(&s, Some(0)).is_none());
        assert!(insp.deref_dict(&n, Some(0)).is_none());
        assert!(insp.deref_dict(&d, Some(0)).is_some());
        let reasons: Vec<GapReason> = insp.gaps.iter().map(|g| g.reason).collect();
        assert_eq!(
            reasons,
            vec![GapReason::MalformedObject, GapReason::MalformedObject]
        );
    }

    #[test]
    fn inherited_resources_by_reference_are_found_too() {
        let ops = b"/Span /P1 BDC BT /F1 12 Tf (x) Tj ET EMC";
        let mut fx = Fixture::new();
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, None, vec![]);
        let mut res = Fixture::default_resources();
        res.set(
            "Properties",
            dictionary! { "P1" => dictionary! { "Alt" => "hola" } },
        );
        let res_id = fx.doc.add_object(res);
        fx.set_pages("Resources", res_id);
        let (risks, gaps) = run_marked_content(&fx, page, ops, ObjectBudget::default());
        assert_eq!(risks, vec![ResidualRisk::ActualText { page: 0 }]);
        assert!(gaps.is_empty(), "{gaps:?}");
    }

    #[test]
    fn lopdf_drops_inline_inherited_resources_so_we_walk_parents_ourselves() {
        // El hecho que motiva `resources()` a mano.
        let mut fx = Fixture::new();
        let c = fx.content_stream(dictionary! {}, b"");
        let page = fx.add_page(c, None, vec![]);
        fx.set_pages("Resources", Fixture::default_resources());
        let (inline, refs) = fx.doc.get_page_resources(page).unwrap();
        assert!(inline.is_none() && refs.is_empty());
        let mut meter = BudgetMeter::new(&ObjectBudget::default());
        let mut insp = Inspector::new(&fx.doc, &mut meter);
        assert_eq!(insp.resources(page, 0).len(), 1);
    }

    #[test]
    fn risk_kind_and_gap_reason_have_stable_names() {
        assert_eq!(ResidualRisk::ActualText { page: 0 }.kind(), "actual_text");
        assert_eq!(
            ResidualRisk::SharedContentStream {
                page: 0,
                shared_with: vec![]
            }
            .kind(),
            "shared_content_stream"
        );
        assert_eq!(GapReason::BrokenReference.as_str(), "broken_reference");
        #[cfg(feature = "serde")]
        {
            let v = serde_json::to_value(ResidualRisk::OptionalContent { page: 3 }).unwrap();
            assert_eq!(
                v,
                serde_json::json!({ "kind": "optional_content", "page": 3 })
            );
        }
    }
    fn run_annots(
        fx: &Fixture,
        page_id: lopdf::ObjectId,
        regions: &[TextRegion],
    ) -> (Vec<ResidualRisk>, Vec<InspectionGap>) {
        let mut meter = BudgetMeter::new(&ObjectBudget::default());
        let mut insp = Inspector::new(&fx.doc, &mut meter);
        let page_dict = fx.doc.get_dictionary(page_id).unwrap();
        let refs: Vec<&TextRegion> = regions.iter().collect();
        insp.inspect_annotations(page_dict, 0, &refs);
        (insp.risks, insp.gaps)
    }

    fn region(x: f64, y: f64, width: f64, height: f64) -> TextRegion {
        TextRegion {
            id: "r".into(),
            page: 0,
            x,
            y,
            width,
            height,
        }
    }

    fn annot(subtype: &str, rect: Vec<Object>) -> lopdf::Dictionary {
        dictionary! { "Type" => "Annot", "Subtype" => subtype, "Rect" => rect }
    }

    #[test]
    fn lopdf_swallows_broken_annotation_refs_so_we_walk_annots_ourselves() {
        let mut fx = Fixture::new();
        let c = fx.content_stream(dictionary! {}, crate::test_support::HOLA);
        let page = fx.add_page(
            c,
            Some(Fixture::default_resources()),
            vec![("Annots", vec![Object::Reference((999, 0))].into())],
        );
        // El hecho que motiva la tarea: lopdf no distingue "rota" de "no hay".
        assert!(fx.doc.get_page_annotations(page).unwrap().is_empty());
        let (risks, gaps) = run_annots(&fx, page, &[region(0.0, 0.0, 612.0, 792.0)]);
        assert!(risks.is_empty());
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].reason, GapReason::BrokenReference);
    }

    #[test]
    fn page_without_annots_has_no_risk_and_no_gap() {
        let mut fx = Fixture::new();
        let page = fx.text_page(crate::test_support::HOLA);
        let (risks, gaps) = run_annots(&fx, page, &[region(0.0, 0.0, 612.0, 792.0)]);
        assert!(risks.is_empty() && gaps.is_empty());
    }

    #[test]
    fn intersecting_annotation_is_a_risk_and_non_intersecting_is_not() {
        let mut fx = Fixture::new();
        // Esquinas invertidas a propósito: [x2 y2 x1 y1] es válido y se normaliza.
        let a = fx.doc.add_object(annot(
            "FreeText",
            vec![50.into(), 50.into(), 10.into(), 10.into()],
        ));
        let c = fx.content_stream(dictionary! {}, crate::test_support::HOLA);
        let page = fx.add_page(
            c,
            Some(Fixture::default_resources()),
            vec![("Annots", vec![Object::Reference(a)].into())],
        );
        let (risks, gaps) = run_annots(&fx, page, &[region(40.0, 40.0, 100.0, 100.0)]);
        assert_eq!(
            risks,
            vec![ResidualRisk::Annotation {
                page: 0,
                subtype: "FreeText".into()
            }]
        );
        assert!(gaps.is_empty());
        let (risks, gaps) = run_annots(&fx, page, &[region(200.0, 200.0, 10.0, 10.0)]);
        assert!(risks.is_empty() && gaps.is_empty());
    }

    #[test]
    fn inline_annotation_dictionaries_are_accepted() {
        let mut fx = Fixture::new();
        let inline = annot("Square", vec![0.into(), 0.into(), 20.into(), 20.into()]);
        let c = fx.content_stream(dictionary! {}, crate::test_support::HOLA);
        let page = fx.add_page(
            c,
            Some(Fixture::default_resources()),
            vec![("Annots", vec![inline.into()].into())],
        );
        let (risks, _) = run_annots(&fx, page, &[region(5.0, 5.0, 5.0, 5.0)]);
        assert_eq!(risks.len(), 1);
    }

    #[test]
    fn malformed_rect_is_a_gap_and_a_conservative_risk() {
        let mut fx = Fixture::new();
        let short = fx
            .doc
            .add_object(annot("Text", vec![1.into(), 2.into(), 3.into()]));
        let nan = fx.doc.add_object(annot(
            "Text",
            vec![0.into(), 0.into(), Object::Real(f32::NAN), 5.into()],
        ));
        let missing = fx
            .doc
            .add_object(dictionary! { "Type" => "Annot", "Subtype" => "Text" });
        let c = fx.content_stream(dictionary! {}, crate::test_support::HOLA);
        let annots: Vec<Object> = [short, nan, missing]
            .iter()
            .map(|id| Object::Reference(*id))
            .collect();
        let page = fx.add_page(
            c,
            Some(Fixture::default_resources()),
            vec![("Annots", annots.into())],
        );
        let (risks, gaps) = run_annots(&fx, page, &[region(500.0, 700.0, 1.0, 1.0)]);
        assert_eq!(risks.len(), 3);
        assert_eq!(gaps.len(), 3);
        assert!(gaps.iter().all(|g| g.reason == GapReason::MalformedRect));
        let details: Vec<&str> = gaps.iter().map(|g| g.detail.as_str()).collect();
        assert_eq!(
            details,
            [
                "/Annots[0] /Subtype /Text",
                "/Annots[1] /Subtype /Text",
                "/Annots[2] /Subtype /Text"
            ]
        );
    }

    #[test]
    fn annots_that_is_not_an_array_is_a_gap() {
        let mut fx = Fixture::new();
        let c = fx.content_stream(dictionary! {}, crate::test_support::HOLA);
        let page = fx.add_page(
            c,
            Some(Fixture::default_resources()),
            vec![("Annots", 7.into())],
        );
        let (risks, gaps) = run_annots(&fx, page, &[region(0.0, 0.0, 1.0, 1.0)]);
        assert!(risks.is_empty());
        assert_eq!(gaps[0].reason, GapReason::MalformedAnnots);
    }

    #[test]
    fn zero_area_rect_never_intersects() {
        assert!(!intersects(
            [10.0, 10.0, 10.0, 50.0],
            &region(0.0, 0.0, 100.0, 100.0)
        ));
        assert!(!intersects(
            [10.0, 10.0, 50.0, 10.0],
            &region(0.0, 0.0, 100.0, 100.0)
        ));
        // Y el caso con área sí intersecta: el test no pasa por devolver siempre false.
        assert!(intersects(
            [10.0, 10.0, 50.0, 50.0],
            &region(0.0, 0.0, 100.0, 100.0)
        ));
        // Bordes que sólo se tocan no cuentan.
        assert!(!intersects(
            [100.0, 0.0, 150.0, 50.0],
            &region(0.0, 0.0, 100.0, 100.0)
        ));
    }
    #[test]
    fn shared_content_stream_names_the_other_pages() {
        let mut fx = Fixture::new();
        let shared = fx.content_stream(dictionary! {}, crate::test_support::HOLA);
        let p0 = fx.add_page(shared, Some(Fixture::default_resources()), vec![]);
        let _p1 = fx.add_page(shared, Some(Fixture::default_resources()), vec![]);
        let _p2 = fx.text_page(crate::test_support::HOLA);
        let mut meter = BudgetMeter::new(&ObjectBudget::default());
        let mut insp = Inspector::new(&fx.doc, &mut meter);
        let pages = fx.doc.get_pages();
        let original = fx.doc.get_page_contents(p0);
        insp.inspect_shared_content(&original, 0, &pages);
        assert_eq!(
            insp.risks,
            vec![ResidualRisk::SharedContentStream {
                page: 0,
                shared_with: vec![1]
            }]
        );
        assert!(insp.gaps.is_empty());
    }

    #[test]
    fn shared_stream_disappears_when_both_pages_are_rewritten() {
        let mut fx = Fixture::new();
        let shared = fx.content_stream(dictionary! {}, crate::test_support::HOLA);
        let p0 = fx.add_page(shared, Some(Fixture::default_resources()), vec![]);
        let p1 = fx.add_page(shared, Some(Fixture::default_resources()), vec![]);
        let pdf = fx.bytes();
        let regions = [
            TextRegion {
                id: "r0".into(),
                page: 0,
                x: 0.0,
                y: 0.0,
                width: 612.0,
                height: 792.0,
            },
            TextRegion {
                id: "r1".into(),
                page: 1,
                x: 0.0,
                y: 0.0,
                width: 612.0,
                height: 792.0,
            },
        ];
        let result = crate::remove_text_glyphs(&pdf, &regions).unwrap();
        assert!(!result
            .residual_risks
            .iter()
            .any(|r| matches!(r, ResidualRisk::SharedContentStream { .. })));
        let _ = (p0, p1); // IDs documentan las dos páginas reescritas.
    }

    #[test]
    fn form_xobject_drawn_by_the_page_is_a_risk_but_images_are_not() {
        let ops = b"q /Fx1 Do Q q /Im1 Do Q q /Fx1 Do Q";
        let mut fx = Fixture::new();
        let form = fx.doc.add_object(lopdf::Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()] },
            b"BT /F1 12 Tf (oculto) Tj ET".to_vec(),
        ));
        let image = fx.doc.add_object(lopdf::Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Image", "Width" => 1, "Height" => 1 },
            vec![0],
        ));
        let mut res = Fixture::default_resources();
        res.set("XObject", dictionary! { "Fx1" => form, "Im1" => image });
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, Some(res), vec![]);
        let mut meter = BudgetMeter::new(&ObjectBudget::default());
        let mut insp = Inspector::new(&fx.doc, &mut meter);
        let resources = insp.resources(page, 0);
        insp.inspect_form_xobjects(&Content::decode(ops).unwrap(), &resources, 0);
        // Un solo riesgo aunque /Fx1 se dibuje dos veces.
        assert_eq!(
            insp.risks,
            vec![ResidualRisk::FormXObject {
                page: 0,
                name: "Fx1".into()
            }]
        );
        assert!(insp.gaps.is_empty());
    }

    #[test]
    fn actual_text_inside_a_form_xobject_is_not_claimed_as_inspected() {
        // El form trae /ActualText adentro. No se entra: el riesgo es
        // FormXObject, nunca ActualText, y `not_inspected` (Task 9) declara
        // que el contenido de los forms no se miró.
        let ops = b"q /Fx1 Do Q";
        let mut fx = Fixture::new();
        let form = fx.doc.add_object(lopdf::Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()] },
            b"/Span <</ActualText (secreto)>> BDC BT /F1 12 Tf (x) Tj ET EMC".to_vec(),
        ));
        let mut res = Fixture::default_resources();
        res.set("XObject", dictionary! { "Fx1" => form });
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, Some(res), vec![]);
        let mut meter = BudgetMeter::new(&ObjectBudget::default());
        let mut insp = Inspector::new(&fx.doc, &mut meter);
        let resources = insp.resources(page, 0);
        let content = Content::decode(ops).unwrap();
        insp.inspect_marked_content(&content, &resources, 0);
        insp.inspect_form_xobjects(&content, &resources, 0);
        assert_eq!(
            insp.risks,
            vec![ResidualRisk::FormXObject {
                page: 0,
                name: "Fx1".into()
            }]
        );
    }

    #[test]
    fn xobject_that_is_not_a_stream_is_a_gap() {
        let ops = b"q /Fx1 Do Q";
        let mut fx = Fixture::new();
        let bogus = fx.doc.add_object(dictionary! { "Subtype" => "Form" });
        let mut res = Fixture::default_resources();
        res.set("XObject", dictionary! { "Fx1" => bogus });
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, Some(res), vec![]);
        let mut meter = BudgetMeter::new(&ObjectBudget::default());
        let mut insp = Inspector::new(&fx.doc, &mut meter);
        let resources = insp.resources(page, 0);
        insp.inspect_form_xobjects(&Content::decode(ops).unwrap(), &resources, 0);
        assert!(insp.risks.is_empty());
        assert_eq!(insp.gaps[0].reason, GapReason::MalformedObject);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn every_risk_kind_matches_its_serde_tag() {
        let all = [
            ResidualRisk::SharedContentStream {
                page: 0,
                shared_with: vec![],
            },
            ResidualRisk::FormXObject {
                page: 0,
                name: "Fx".into(),
            },
            ResidualRisk::Annotation {
                page: 0,
                subtype: "Text".into(),
            },
            ResidualRisk::OptionalContent { page: 0 },
            ResidualRisk::ActualText { page: 0 },
        ];
        for risk in all {
            let v = serde_json::to_value(&risk).unwrap();
            assert_eq!(
                v["kind"],
                serde_json::Value::String(risk.kind().to_string()),
                "{}",
                risk.kind()
            );
        }
        for reason in [
            GapReason::BrokenReference,
            GapReason::ReferenceCycle,
            GapReason::ReferenceDepth,
            GapReason::BudgetExhausted,
            GapReason::UnsupportedFilter,
            GapReason::CorruptStream,
            GapReason::MalformedAnnots,
            GapReason::MalformedRect,
            GapReason::MissingResource,
            GapReason::MalformedObject,
            GapReason::Encrypted,
            GapReason::NotInspected,
        ] {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::Value::String(reason.as_str().to_string())
            );
        }
    }
}
