//! Informe serializable y versionado de una eliminación.

use crate::inspect::{InspectionGap, ResidualRisk};
use crate::signature::SignatureIndicators;
use crate::text_removal::{RegionReport, RemovalResult};

/// Versión actual del esquema del informe.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Superficies que esta operación nunca inspecciona.
pub const NOT_INSPECTED_SURFACES: &[&str] = &[
    "document_info",
    "xmp_metadata",
    "form_xobject_content",
    "annotation_appearance",
    "structure_tree",
    "embedded_files",
    "outlines",
];

/// Vista serializable de un resultado, sin los bytes del PDF.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct RemovalReport<'a> {
    /// Versión del esquema.
    pub schema_version: u32,
    /// Si el PDF fue reescrito.
    pub modified: bool,
    /// Informes por región.
    pub regions: &'a [RegionReport],
    /// Riesgos residuales.
    pub residual_risks: &'a [ResidualRisk],
    /// Si quedó algo sin inspeccionar.
    pub inspection_incomplete: bool,
    /// Gaps de inspección.
    pub inspection_gaps: &'a [InspectionGap],
    /// Indicios de firma.
    pub signature: SignatureIndicators,
    /// Superficies deliberadamente fuera de alcance.
    pub not_inspected: &'static [&'static str],
}

impl RemovalResult {
    /// Vista serializable del resultado, sin `output`.
    pub fn report(&self) -> RemovalReport<'_> {
        RemovalReport {
            schema_version: REPORT_SCHEMA_VERSION,
            modified: self.modified,
            regions: &self.regions,
            residual_risks: &self.residual_risks,
            inspection_incomplete: self.inspection_incomplete,
            inspection_gaps: &self.inspection_gaps,
            signature: self.signature,
            not_inspected: NOT_INSPECTED_SURFACES,
        }
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;
    use crate::test_support::{Fixture, HOLA};
    use crate::{remove_text_glyphs, TextRegion};
    use lopdf::{dictionary, Object};

    fn full_page(id: &str) -> TextRegion {
        TextRegion {
            id: id.into(),
            page: 0,
            x: 0.0,
            y: 0.0,
            width: 612.0,
            height: 792.0,
        }
    }

    fn check_golden(name: &str, report: &RemovalReport<'_>) {
        let path = format!("{}/tests/golden/{name}.json", env!("CARGO_MANIFEST_DIR"));
        let actual = serde_json::to_string_pretty(report).unwrap() + "\n";
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &actual).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("falta {path}; generar con UPDATE_GOLDEN=1"));
        assert_eq!(actual, expected, "el esquema v{REPORT_SCHEMA_VERSION} cambió; si es intencional, subir la versión y regenerar");
    }

    #[test]
    fn golden_clean() {
        let mut fx = Fixture::new();
        fx.text_page(HOLA);
        let r = remove_text_glyphs(&fx.bytes(), &[full_page("r1")]).unwrap();
        check_golden("report-v1-clean", &r.report());
    }

    #[test]
    fn golden_risks_and_signature() {
        let mut fx = Fixture::new();
        let form = fx.doc.add_object(lopdf::Stream::new(dictionary! { "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 1.into(), 1.into()] }, Vec::new()));
        let mut res = Fixture::default_resources();
        res.set("XObject", dictionary! { "Fx1" => form });
        let shared = fx.content_stream(
            dictionary! {},
            b"BT /F1 12 Tf 10 10 Td (hola) Tj ET q /Fx1 Do Q",
        );
        fx.add_page(
            shared,
            Some(res.clone()),
            vec![("Annots", vec![Object::Reference((999, 0))].into())],
        );
        fx.add_page(shared, Some(res), vec![]);
        fx.set_catalog("AcroForm", dictionary! { "SigFlags" => 1 });
        let r = remove_text_glyphs(
            &fx.bytes(),
            &[
                full_page("r1"),
                TextRegion {
                    id: "r2".into(),
                    page: 0,
                    x: 500.0,
                    y: 700.0,
                    width: 1.0,
                    height: 1.0,
                },
            ],
        )
        .unwrap();
        check_golden("report-v1-risks", &r.report());
    }

    #[test]
    fn xmp_is_explicitly_not_inspected() {
        let mut fx = Fixture::new();
        fx.text_page(HOLA);
        let metadata = fx.doc.add_object(lopdf::Stream::new(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            b"<x:xmpmeta>hola</x:xmpmeta>".to_vec(),
        ));
        fx.set_catalog("Metadata", metadata);
        let r = remove_text_glyphs(&fx.bytes(), &[full_page("r1")]).unwrap();
        assert!(r.residual_risks.is_empty());
        assert!(r.report().not_inspected.contains(&"xmp_metadata"));
    }
}
