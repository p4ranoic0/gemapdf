//! Indicios de firma digital.
//!
//! Se detectan e informan; nunca bloquean. Un documento firmado que se
//! reescribe pierde la firma o la invalida: el llamador decide qué hacer.

use lopdf::Object;

use crate::inspect::{GapReason, Inspector};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
/// Indicios de que el documento está o estuvo firmado.
pub struct SignatureIndicators {
    /// `/AcroForm /SigFlags` con el bit 1 encendido.
    pub sig_flags: bool,
    /// Algún campo `/FT /Sig`, incluso anidado en `/Kids`.
    pub sig_field: bool,
    /// `/Perms` presente en el catálogo.
    pub perms: bool,
}

impl SignatureIndicators {
    /// `true` si hay al menos un indicio.
    pub fn any(&self) -> bool {
        self.sig_flags || self.sig_field || self.perms
    }
}

pub(crate) fn detect<'a>(insp: &mut Inspector<'a>) -> SignatureIndicators {
    let mut out = SignatureIndicators::default();
    let catalog = match insp.doc().catalog() {
        Ok(c) => c,
        Err(_) => {
            insp.gap(GapReason::BrokenReference, None, "/Root");
            return out;
        }
    };
    if catalog.has(b"Perms") {
        out.perms = true;
    }
    let Ok(acro) = catalog.get(b"AcroForm") else {
        return out;
    };
    let Some(acro) = insp.deref_dict(acro, None) else {
        return out;
    };
    match acro.get(b"SigFlags").ok().map(|o| insp.deref(o, None)) {
        None => {}
        Some(None) => {}
        Some(Some(Object::Integer(flags))) => out.sig_flags = flags & 1 == 1,
        Some(Some(other)) => insp.gap(
            GapReason::MalformedObject,
            None,
            format!("/SigFlags is {}", other.enum_variant()),
        ),
    }
    if let Ok(fields) = acro.get(b"Fields") {
        match insp.deref(fields, None) {
            None => {}
            Some(Object::Array(items)) => out.sig_field = has_sig_field(insp, items, 0),
            Some(other) => insp.gap(
                GapReason::MalformedObject,
                None,
                format!("/Fields is {}", other.enum_variant()),
            ),
        }
    }
    out
}

fn has_sig_field<'a>(insp: &mut Inspector<'a>, items: &'a [Object], depth: usize) -> bool {
    if insp.check_depth(depth).is_err() {
        insp.gap(GapReason::ReferenceDepth, None, "/Fields /Kids");
        return false;
    }
    for item in items {
        let Some(field) = insp.deref_dict(item, None) else {
            continue;
        };
        if matches!(field.get(b"FT"), Ok(Object::Name(t)) if t == b"Sig") {
            return true;
        }
        if let Ok(kids) = field.get(b"Kids") {
            match insp.deref(kids, None) {
                None => {}
                Some(Object::Array(kids)) => {
                    if has_sig_field(insp, kids, depth + 1) {
                        return true;
                    }
                }
                Some(other) => insp.gap(
                    GapReason::MalformedObject,
                    None,
                    format!("/Kids is {}", other.enum_variant()),
                ),
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::Inspector;
    use crate::options::{BudgetMeter, ObjectBudget};
    use crate::test_support::{Fixture, HOLA};
    use lopdf::{dictionary, Object};

    fn detect_in(fx: &Fixture) -> SignatureIndicators {
        let mut meter = BudgetMeter::new(&ObjectBudget::default());
        let mut insp = Inspector::new(&fx.doc, &mut meter);
        let out = detect(&mut insp);
        assert!(insp.gaps.is_empty(), "{:?}", insp.gaps);
        out
    }

    #[test]
    fn plain_document_has_no_indicators() {
        let mut fx = Fixture::new();
        fx.text_page(HOLA);
        let s = detect_in(&fx);
        assert_eq!(s, SignatureIndicators::default());
        assert!(!s.any());
    }
    #[test]
    fn sig_flags_bit_one_is_detected() {
        let mut fx = Fixture::new();
        fx.text_page(HOLA);
        fx.set_catalog(
            "AcroForm",
            dictionary! { "SigFlags" => 3, "Fields" => Vec::<Object>::new() },
        );
        let s = detect_in(&fx);
        assert!(s.sig_flags && !s.sig_field && !s.perms);
        fx.set_catalog("AcroForm", dictionary! { "SigFlags" => 2 });
        assert!(!detect_in(&fx).sig_flags);
    }
    #[test]
    fn sig_field_is_found_even_nested_in_kids_through_an_indirect_acroform() {
        let mut fx = Fixture::new();
        fx.text_page(HOLA);
        let leaf = fx
            .doc
            .add_object(dictionary! { "FT" => "Sig", "T" => "firma" });
        let parent = fx
            .doc
            .add_object(dictionary! { "T" => "grupo", "Kids" => vec![Object::Reference(leaf)] });
        let acro = fx
            .doc
            .add_object(dictionary! { "Fields" => vec![Object::Reference(parent)] });
        fx.set_catalog("AcroForm", acro);
        let s = detect_in(&fx);
        assert!(s.sig_field && !s.sig_flags);
    }
    #[test]
    fn perms_in_the_catalog_is_detected() {
        let mut fx = Fixture::new();
        fx.text_page(HOLA);
        fx.set_catalog(
            "Perms",
            dictionary! { "DocMDP" => Object::Reference((999, 0)) },
        );
        assert!(detect_in(&fx).perms);
    }
    #[test]
    fn broken_fields_reference_leaves_a_gap_not_a_panic() {
        let mut fx = Fixture::new();
        fx.text_page(HOLA);
        fx.set_catalog(
            "AcroForm",
            dictionary! { "Fields" => vec![Object::Reference((999, 0))] },
        );
        let mut meter = BudgetMeter::new(&ObjectBudget::default());
        let mut insp = Inspector::new(&fx.doc, &mut meter);
        assert!(!detect(&mut insp).sig_field);
        assert_eq!(insp.gaps[0].reason, crate::GapReason::BrokenReference);
    }
    #[test]
    fn perms_counts_by_presence_even_indirect_broken_or_malformed() {
        for value in [
            Object::Reference((999, 0)),
            Object::Integer(7),
            dictionary! { "UR3" => 1 }.into(),
        ] {
            let mut fx = Fixture::new();
            fx.text_page(HOLA);
            fx.set_catalog("Perms", value);
            assert!(detect_in(&fx).perms);
        }
        let mut fx = Fixture::new();
        fx.text_page(HOLA);
        let perms = fx.doc.add_object(dictionary! { "DocMDP" => 1 });
        fx.set_catalog("Perms", perms);
        assert!(detect_in(&fx).perms);
    }
    #[test]
    fn indirect_acroform_without_signatures_is_a_form_not_a_signature() {
        let mut fx = Fixture::new();
        fx.text_page(HOLA);
        let text_field = fx
            .doc
            .add_object(dictionary! { "FT" => "Tx", "T" => "nombre" });
        let acro = fx.doc.add_object(
            dictionary! { "Fields" => vec![Object::Reference(text_field)], "SigFlags" => 0 },
        );
        fx.set_catalog("AcroForm", acro);
        assert_eq!(detect_in(&fx), SignatureIndicators::default());
    }
    #[test]
    fn malformed_acroform_pieces_leave_gaps_not_silence() {
        let cases: [(Object, &str); 3] = [
            (Object::Integer(1), "AcroForm is not a dictionary"),
            (
                dictionary! { "SigFlags" => "uno" }.into(),
                "SigFlags is not an integer",
            ),
            (
                dictionary! { "Fields" => 7 }.into(),
                "Fields is not an array",
            ),
        ];
        for (acro, why) in cases {
            let mut fx = Fixture::new();
            fx.text_page(HOLA);
            fx.set_catalog("AcroForm", acro);
            let mut meter = BudgetMeter::new(&ObjectBudget::default());
            let mut insp = Inspector::new(&fx.doc, &mut meter);
            detect(&mut insp);
            assert_eq!(insp.gaps.len(), 1, "{why}: {:?}", insp.gaps);
            assert_eq!(
                insp.gaps[0].reason,
                crate::GapReason::MalformedObject,
                "{why}"
            );
        }
    }

    fn one_indirection_per_surface() -> (Fixture, lopdf::ObjectId, &'static [u8]) {
        let ops: &'static [u8] = b"/OC /P1 BDC BT /F1 12 Tf 10 10 Td (hola) Tj ET EMC";
        let mut fx = Fixture::new();
        let ocg = fx
            .doc
            .add_object(dictionary! { "Type" => "OCG", "Name" => "capa" });
        let annot = fx.doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Text", "Rect" => vec![0.into(), 0.into(), 1.into(), 1.into()] });
        let annots = fx.doc.add_object(vec![Object::Reference(annot)]);
        let mut res = Fixture::default_resources();
        res.set("Properties", dictionary! { "P1" => ocg });
        let c = fx.content_stream(dictionary! {}, ops);
        let page = fx.add_page(c, Some(res), vec![("Annots", Object::Reference(annots))]);
        let leaf = fx.doc.add_object(dictionary! { "FT" => "Sig" });
        let acro = fx
            .doc
            .add_object(dictionary! { "Fields" => vec![Object::Reference(leaf)] });
        fx.set_catalog("AcroForm", acro);
        (fx, page, ops)
    }
    fn inspect_everything<'a>(
        fx: &'a Fixture,
        page: lopdf::ObjectId,
        ops: &[u8],
        meter: &'a mut BudgetMeter,
    ) -> Inspector<'a> {
        let region = crate::TextRegion {
            id: "r".into(),
            page: 0,
            x: 0.0,
            y: 0.0,
            width: 612.0,
            height: 792.0,
        };
        let mut insp = Inspector::new(&fx.doc, meter);
        let content = lopdf::content::Content::decode(ops).unwrap();
        let resources = insp.resources(page, 0);
        insp.inspect_marked_content(&content, &resources, 0);
        insp.inspect_form_xobjects(&content, &resources, 0);
        insp.inspect_annotations(fx.doc.get_dictionary(page).unwrap(), 0, &[&region]);
        let original = fx.doc.get_page_contents(page);
        insp.inspect_shared_content(&original, 0, &fx.doc.get_pages());
        let _ = detect(&mut insp);
        insp
    }
    #[test]
    fn every_resolution_path_charges_the_budget_exactly_once() {
        let (fx, page, ops) = one_indirection_per_surface();
        let mut meter = BudgetMeter::new(&ObjectBudget::default());
        let insp = inspect_everything(&fx, page, ops, &mut meter);
        assert!(insp.gaps.is_empty(), "{:?}", insp.gaps);
        drop(insp);
        assert_eq!(meter.objects_touched(), 6);
    }
    #[test]
    fn zero_object_budget_leaves_a_gap_on_every_indirect_surface() {
        let (fx, page, ops) = one_indirection_per_surface();
        let budget = ObjectBudget {
            max_inspected_objects: 0,
            ..ObjectBudget::default()
        };
        let mut meter = BudgetMeter::new(&budget);
        let insp = inspect_everything(&fx, page, ops, &mut meter);
        let exhausted = insp
            .gaps
            .iter()
            .filter(|g| g.reason == crate::GapReason::BudgetExhausted)
            .count();
        assert_eq!(exhausted, 4, "{:?}", insp.gaps);
    }
}
