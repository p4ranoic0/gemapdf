use crate::error::GemaError;
use crate::report::Report;
use lopdf::Document;

/// Detecta firma criptográfica buscando `/ByteRange` en cualquier objeto.
fn detect_signed(doc: &Document) -> bool {
    doc.objects.values().any(|obj| {
        obj.as_dict()
            .map(|d| d.has(b"ByteRange") || d.has(b"Sig"))
            .unwrap_or(false)
    })
}

pub fn analyze(input: &[u8]) -> Result<Report, GemaError> {
    let doc = Document::load_mem(input).map_err(|e| GemaError::Parse(e.to_string()))?;
    if doc.is_encrypted() {
        return Err(GemaError::Encrypted);
    }
    let pages = doc.get_pages().len();
    let is_signed = detect_signed(&doc);

    let mut report = Report {
        pages,
        original_size: input.len() as u64,
        is_signed,
        ..Default::default()
    };
    if is_signed {
        report.warnings.push(crate::report::Warning::SignedDocument);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PDF mínimo válido de 1 página, sin imágenes, escrito a bytes con lopdf.
    fn minimal_pdf() -> Vec<u8> {
        use lopdf::{dictionary, Document, Object, Stream};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(pages_id, Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }));
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn analyzes_page_count_and_size() {
        let bytes = minimal_pdf();
        let report = analyze(&bytes).unwrap();
        assert_eq!(report.pages, 1);
        assert_eq!(report.original_size, bytes.len() as u64);
        assert!(!report.is_signed);
    }

    #[test]
    fn rejects_garbage() {
        let err = analyze(b"not a pdf").unwrap_err();
        assert!(matches!(err, GemaError::Parse(_)));
    }
}
