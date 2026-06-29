use crate::error::GemaError;
use crate::report::Report;
use lopdf::Document;

/// Detecta firma criptográfica.
///
/// Un diccionario cuenta como firma si tiene `/ByteRange` (presente en todo
/// objeto de firma real), O bien tiene `/Sig` Y su `/Type` es el nombre `Sig`.
/// La condición sobre `/Type` evita falsos positivos por una clave `/Sig`
/// suelta en un dict que no es una firma (que bajo SignaturePolicy::Strict
/// bloquearía erróneamente la compresión).
fn detect_signed(doc: &Document) -> bool {
    doc.objects.values().any(|obj| {
        obj.as_dict()
            .map(|d| {
                d.has(b"ByteRange")
                    || (d.has(b"Sig")
                        && d.get(b"Type").and_then(|o| o.as_name()).is_ok_and(|n| n == b"Sig"))
            })
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

    /// Construye un PDF mínimo cuyo page dict lleva las claves extra dadas.
    fn pdf_with_page_extras(extras: lopdf::Dictionary) -> Vec<u8> {
        use lopdf::{dictionary, Document, Object, Stream};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
        let mut page = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        };
        for (k, v) in extras.iter() {
            page.set(k.clone(), v.clone());
        }
        let page_id = doc.add_object(page);
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
    fn stray_sig_key_is_not_detected_as_signed() {
        use lopdf::dictionary;
        // page dict con una clave /Sig suelta, sin /ByteRange y sin /Type=Sig
        let bytes = pdf_with_page_extras(dictionary! { "Sig" => 1 });
        let report = analyze(&bytes).unwrap();
        assert!(!report.is_signed, "una clave /Sig suelta no debe marcar firma");
    }

    #[test]
    fn byterange_is_detected_as_signed() {
        use lopdf::dictionary;
        let bytes = pdf_with_page_extras(
            dictionary! { "ByteRange" => vec![0.into(), 100.into(), 200.into(), 50.into()] },
        );
        let report = analyze(&bytes).unwrap();
        assert!(report.is_signed, "/ByteRange debe marcar el documento como firmado");
    }
}
