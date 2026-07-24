//! Pre-pasada de máscaras de transparencia: qué XObjects de imagen se usan
//! como `/SMask` de otra imagen. El bucle del pipeline fuerza esos objetos a
//! re-encode sin pérdida (nunca JPEG) y sin downsample, para que la
//! transparencia quede bit-exacta (lever C, spec 2026-07-10).

use lopdf::{Document, Object, ObjectId};
use std::collections::HashSet;

/// Devuelve el set de ObjectIds referenciados como `/SMask` por algún XObject
/// de imagen del documento. No muta el doc; corre una vez antes del bucle de
/// imágenes (misma mecánica que `signatures::collect_preserved_images`).
pub(crate) fn collect_smask_ids(doc: &Document) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    for obj in doc.objects.values() {
        let Ok(stream) = obj.as_stream() else {
            continue;
        };
        if stream.dict.get(b"Subtype").and_then(|o| o.as_name()).ok() != Some(b"Image".as_slice()) {
            continue;
        }
        if let Ok(Object::Reference(id)) = stream.dict.get(b"SMask") {
            out.insert(*id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    #[test]
    fn collects_only_referenced_smasks() {
        let mut doc = Document::with_version("1.5");
        // máscara: imagen gris suelta
        let mask_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 8, "Height" => 8,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceGray",
                "Filter" => "FlateDecode",
            },
            vec![0u8; 4],
        ));
        // base que la referencia como /SMask
        let base_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 8, "Height" => 8,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
                "SMask" => mask_id,
            },
            vec![0u8; 4],
        ));
        // otra imagen sin máscara
        let plain_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 8, "Height" => 8,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
            },
            vec![0u8; 4],
        ));

        let set = collect_smask_ids(&doc);
        assert!(set.contains(&mask_id), "la máscara referenciada debe estar");
        assert!(!set.contains(&base_id), "la base no es máscara");
        assert!(!set.contains(&plain_id), "una imagen suelta no es máscara");
        assert_eq!(set.len(), 1);
    }
}
