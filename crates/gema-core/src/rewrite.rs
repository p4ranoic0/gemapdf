use crate::error::GemaError;
use lopdf::Document;

/// Quita /Metadata del catálogo y /Info del trailer si `remove_metadata`.
pub fn strip_metadata(doc: &mut Document) {
    if let Ok(catalog) = doc.catalog_mut() {
        catalog.remove(b"Metadata");
    }
    doc.trailer.remove(b"Info");
}

/// Recomprime todos los streams (Flate) y elimina objetos huérfanos.
pub fn cleanup_and_compress(doc: &mut Document, recompress_streams: bool) {
    doc.prune_objects();
    if recompress_streams {
        doc.compress();
    }
}

/// Serializa el documento a bytes con object streams + xref streams.
pub fn serialize(doc: &mut Document) -> Result<Vec<u8>, GemaError> {
    let mut buf = Vec::new();
    // save_modern = object streams + xref streams. lopdf sube doc.version a
    // "1.5" por sí mismo si hace falta (los object streams lo exigen).
    doc.save_modern(&mut buf)
        .map_err(|e| GemaError::Io(e.to_string()))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Document, Object, Stream};

    fn doc_with_uncompressed_content() -> Document {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        // contenido grande y repetitivo → comprime bien
        let content = b"BT /F1 12 Tf (AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA) Tj ET".repeat(50);
        let content_id = doc.add_object(Stream::new(dictionary! {}, content));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        doc
    }

    #[test]
    fn compression_shrinks_repetitive_content() {
        let mut doc = doc_with_uncompressed_content();
        let mut before = Vec::new();
        doc.save_to(&mut before).unwrap();

        let mut doc2 = doc_with_uncompressed_content();
        cleanup_and_compress(&mut doc2, true);
        let after = serialize(&mut doc2).unwrap();

        assert!(
            after.len() < before.len(),
            "after={} before={}",
            after.len(),
            before.len()
        );
        // sigue siendo un PDF parseable
        assert!(Document::load_mem(&after).is_ok());
    }
}
