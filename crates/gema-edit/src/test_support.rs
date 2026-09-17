//! Constructor de PDFs mínimos para tests. Sólo compila bajo `cfg(test)`.
//!
//! El `build` de `text_removal` sirve para páginas sueltas con fuentes; esto
//! sirve para lo que aquél no puede: varias páginas, recursos heredados,
//! anotaciones, catálogo con `/AcroForm` y objetos indirectos arbitrarios.
#![allow(dead_code)] // helpers de fixtures: varios los usan tareas posteriores

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};

/// `BT /F1 12 Tf 10 10 Td (hola) Tj ET`: cuatro glifos Helvetica en (10, 10).
pub(crate) const HOLA: &[u8] = b"BT /F1 12 Tf 10 10 Td (hola) Tj ET";

pub(crate) struct Fixture {
    pub doc: Document,
    pub catalog_id: ObjectId,
    pub pages_id: ObjectId,
    pub page_ids: Vec<ObjectId>,
}

impl Fixture {
    pub fn new() -> Self {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => Vec::<Object>::new(), "Count" => 0
            }),
        );
        doc.trailer.set("Root", catalog_id);
        Fixture {
            doc,
            catalog_id,
            pages_id,
            page_ids: Vec::new(),
        }
    }

    pub fn helvetica() -> Dictionary {
        dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" }
    }

    pub fn default_resources() -> Dictionary {
        dictionary! { "Font" => dictionary! { "F1" => Self::helvetica() } }
    }

    pub fn content_stream(&mut self, dict: Dictionary, bytes: &[u8]) -> ObjectId {
        self.doc.add_object(Stream::new(dict, bytes.to_vec()))
    }

    /// Página con `/Contents content_id`, `/Resources resources` (si `Some`)
    /// y entradas extra. Devuelve su `ObjectId`.
    pub fn add_page(
        &mut self,
        content_id: ObjectId,
        resources: Option<Dictionary>,
        extra: Vec<(&str, Object)>,
    ) -> ObjectId {
        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => self.pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content_id,
        };
        if let Some(r) = resources {
            page.set("Resources", r);
        }
        for (k, v) in extra {
            page.set(k, v);
        }
        let id = self.doc.add_object(page);
        self.page_ids.push(id);
        let kids: Vec<Object> = self
            .page_ids
            .iter()
            .map(|id| Object::Reference(*id))
            .collect();
        let pages = self
            .doc
            .get_object_mut(self.pages_id)
            .unwrap()
            .as_dict_mut()
            .unwrap();
        pages.set("Kids", kids);
        pages.set("Count", self.page_ids.len() as i64);
        id
    }

    /// Página de texto con recursos propios.
    pub fn text_page(&mut self, ops: &[u8]) -> ObjectId {
        let c = self.content_stream(dictionary! {}, ops);
        self.add_page(c, Some(Self::default_resources()), vec![])
    }

    pub fn set_catalog(&mut self, key: &str, value: impl Into<Object>) {
        self.doc
            .get_object_mut(self.catalog_id)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set(key, value);
    }

    pub fn set_pages(&mut self, key: &str, value: impl Into<Object>) {
        self.doc
            .get_object_mut(self.pages_id)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set(key, value);
    }

    pub fn bytes(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        self.doc.save_to(&mut out).unwrap();
        out
    }
}
