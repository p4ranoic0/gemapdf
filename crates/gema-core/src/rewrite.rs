use crate::error::GemaError;
use lopdf::{dictionary, Document, Object, ObjectId, Stream, StringFormat};
use std::collections::HashMap;

/// Quita /Metadata del catálogo y /Info del trailer si `remove_metadata`.
pub fn strip_metadata(doc: &mut Document) {
    if let Ok(catalog) = doc.catalog_mut() {
        catalog.remove(b"Metadata");
    }
    doc.trailer.remove(b"Info");
}

/// Estampa la marca gemaPDF: `/Info` (Producer + Creator) y un `/Metadata` XMP
/// mínimo en el catálogo. Debe llamarse DESPUÉS de `cleanup_and_compress` para
/// que el stream XMP no se recomprima (los lectores XMP esperan texto plano).
pub fn brand_metadata(doc: &mut Document) {
    let producer = format!("gemaPDF {}", env!("CARGO_PKG_VERSION"));

    let info = dictionary! {
        "Producer" => Object::String(producer.clone().into_bytes(), StringFormat::Literal),
        "Creator" => Object::String(producer.clone().into_bytes(), StringFormat::Literal),
    };
    let info_id = doc.add_object(Object::Dictionary(info));
    doc.trailer.set("Info", Object::Reference(info_id));

    let xmp = build_xmp(&producer);
    let meta_id = doc.add_object(Object::Stream(Stream::new(
        dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
        xmp,
    )));
    if let Ok(catalog) = doc.catalog_mut() {
        catalog.set("Metadata", Object::Reference(meta_id));
    }
}

/// Paquete XMP mínimo y bien formado con la marca gemaPDF.
fn build_xmp(producer: &str) -> Vec<u8> {
    format!(
        r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:pdf="http://ns.adobe.com/pdf/1.3/"
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
   <pdf:Producer>{producer}</pdf:Producer>
   <xmp:CreatorTool>{producer}</xmp:CreatorTool>
   <dc:format>application/pdf</dc:format>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#
    )
    .into_bytes()
}

/// Reemplaza recursivamente cada `Reference(id)` según `map` (dup → canónico).
fn remap_refs(obj: &mut Object, map: &HashMap<ObjectId, ObjectId>) {
    match obj {
        Object::Reference(id) => {
            if let Some(&canon) = map.get(id) {
                *id = canon;
            }
        }
        Object::Array(items) => {
            for it in items.iter_mut() {
                remap_refs(it, map);
            }
        }
        Object::Dictionary(d) => {
            for (_, v) in d.iter_mut() {
                remap_refs(v, map);
            }
        }
        Object::Stream(s) => {
            for (_, v) in s.dict.iter_mut() {
                remap_refs(v, map);
            }
        }
        _ => {}
    }
}

/// Deduplica objetos-stream INTERCAMBIABLES (mismo contenido byte-idéntico Y mismo
/// dict): repunta todas las referencias a un único canónico; los duplicados quedan
/// huérfanos y los borra el `prune_objects` posterior. Típico en merges que
/// embeben la misma fuente/imagen muchas veces. Seguro: sólo colapsa objetos
/// realmente equivalentes, así que no cambia el render.
pub fn dedupe_streams(doc: &mut Document) {
    // Agrupar por contenido byte-idéntico (los duplicados reales son pocos).
    let mut by_content: HashMap<&[u8], Vec<ObjectId>> = HashMap::new();
    for (id, obj) in doc.objects.iter() {
        if let Object::Stream(s) = obj {
            by_content
                .entry(s.content.as_slice())
                .or_default()
                .push(*id);
        }
    }
    // dup_id → canónico, sólo entre streams cuyo DICT también es idéntico.
    let mut remap: HashMap<ObjectId, ObjectId> = HashMap::new();
    for ids in by_content.values() {
        if ids.len() < 2 {
            continue;
        }
        // Representantes por dict distinto (los grupos son chicos → lineal ok).
        let mut canon: Vec<ObjectId> = Vec::new();
        for &id in ids {
            let Some(Object::Stream(s)) = doc.objects.get(&id) else {
                continue;
            };
            let mut matched = None;
            for &c in &canon {
                if let Some(Object::Stream(cs)) = doc.objects.get(&c) {
                    if cs.dict == s.dict {
                        matched = Some(c);
                        break;
                    }
                }
            }
            match matched {
                Some(c) => {
                    remap.insert(id, c);
                }
                None => canon.push(id),
            }
        }
    }
    if remap.is_empty() {
        return;
    }
    // Repuntar todas las referencias del documento y del trailer.
    for obj in doc.objects.values_mut() {
        remap_refs(obj, &remap);
    }
    for (_, v) in doc.trailer.iter_mut() {
        remap_refs(v, &remap);
    }
}

/// Deduplica streams idénticos, recomprime todos los streams (Flate) y elimina
/// objetos huérfanos. El dedup va ANTES del prune para que los duplicados
/// repuntados queden sin referencias y se borren.
pub fn cleanup_and_compress(doc: &mut Document, recompress_streams: bool) {
    dedupe_streams(doc);
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

    #[test]
    fn dedupe_merges_identical_streams() {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        // dos "fuentes" byte-idénticas (mismo dict + contenido)
        let font_bytes = b"FONTPROGRAMDATA".repeat(100);
        let f1 = doc.add_object(Stream::new(
            dictionary! { "Length1" => 1500 },
            font_bytes.clone(),
        ));
        let f2 = doc.add_object(Stream::new(
            dictionary! { "Length1" => 1500 },
            font_bytes.clone(),
        ));
        let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            // ambas fuentes referenciadas desde recursos
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => f1, "F2" => f2 } },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);

        dedupe_streams(&mut doc);
        doc.prune_objects();

        // los dos streams idénticos colapsan a uno (el otro queda huérfano y prune lo borra)
        let remaining = [f1, f2]
            .iter()
            .filter(|id| doc.objects.contains_key(id))
            .count();
        assert_eq!(remaining, 1, "dos streams idénticos deben colapsar a uno");

        // el doc sigue parseando tras repuntar referencias
        let out = serialize(&mut doc).unwrap();
        assert!(
            Document::load_mem(&out).is_ok(),
            "el output debe re-parsear"
        );
    }

    #[test]
    fn dedupe_keeps_distinct_streams() {
        // streams con MISMO contenido pero DICT distinto no deben colapsar.
        let mut doc = Document::with_version("1.5");
        let body = b"SAMEBYTES".repeat(50);
        let a = doc.add_object(Stream::new(dictionary! { "Length1" => 1 }, body.clone()));
        let b = doc.add_object(Stream::new(dictionary! { "Length1" => 2 }, body.clone()));
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "A" => a, "B" => b });
        doc.trailer.set("Root", catalog_id);
        dedupe_streams(&mut doc);
        assert!(
            doc.objects.contains_key(&a) && doc.objects.contains_key(&b),
            "dicts distintos no deben deduplicarse"
        );
    }

    #[test]
    fn brand_metadata_stamps_gemapdf() {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1 },
            ),
        );
        let cat = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", cat);

        brand_metadata(&mut doc);

        // /Info con Producer que contiene gemaPDF
        let info_ref = doc.trailer.get(b"Info").unwrap();
        let (_, info) = doc.dereference(info_ref).unwrap();
        let producer = info.as_dict().unwrap().get(b"Producer").unwrap();
        if let Object::String(b, _) = producer {
            assert!(
                String::from_utf8_lossy(b).contains("gemaPDF"),
                "Producer debe llevar gemaPDF"
            );
        } else {
            panic!("Producer debe ser string literal");
        }

        // /Metadata XMP en el catálogo, con gemaPDF en el texto
        let meta_ref = doc.catalog().unwrap().get(b"Metadata").unwrap();
        let (_, meta) = doc.dereference(meta_ref).unwrap();
        let xmp = &meta.as_stream().unwrap().content;
        assert!(
            String::from_utf8_lossy(xmp).contains("gemaPDF"),
            "el XMP debe llevar gemaPDF"
        );

        // el doc re-parsea
        let out = serialize(&mut doc).unwrap();
        assert!(Document::load_mem(&out).is_ok());
    }
}
