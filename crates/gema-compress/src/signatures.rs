//! Detección de firmas/sellos para preservar sus bytes de imagen originales.
//!
//! Una pasada pre-flight de sólo-lectura sobre el documento que produce el
//! `HashSet<ObjectId>` de imágenes XObject que NO deben recomprimirse. Se cubren
//! dos casos, unidos al mismo set:
//!
//! - **Caso A — apariencias de widget de firma** (`FT=Sig`, `/AP/N`): detección
//!   estructural precisa (cero falsos positivos).
//! - **Caso B — sellos/logos aplanados**: heurística de tamaño (imagen pequeña),
//!   igual que el motor clásico.
//!
//! El módulo NUNCA muta el documento; sólo lo lee y devuelve IDs. Es robusto a
//! diccionarios malformados (usa `.ok()`/`.and_then`, nunca `unwrap`/panic sobre
//! datos del documento).

use std::collections::HashSet;

use lopdf::{Document, Object, ObjectId};

/// Sellos/logos aplanados: lado máximo (px) por debajo del cual una imagen se
/// considera candidata a sello. Umbral del motor clásico (`_selectSmallImages`).
const MAX_STAMP_DIM: i64 = 300;
/// Sellos/logos aplanados: tamaño máximo (bytes del stream) de un sello.
const MAX_STAMP_BYTES: usize = 50 * 1024;
/// Presupuesto TOTAL de bytes que case B puede preservar en un documento. Case B
/// es una heurística para un PUÑADO de sellos/logos institucionales; si las
/// candidatas suman más que esto, el documento es image-heavy (p.ej. un escaneo
/// troceado en cientos de fragmentos) y la heurística se disparó de más — se deja
/// que compriman. El case A (apariencias de firma FT=Sig) es preciso y NO se ve
/// afectado por este tope.
const MAX_STAMP_TOTAL_BYTES: usize = 1024 * 1024;

/// Tope de saltos al subir la cadena `/Parent` de una anotación buscando `FT`.
/// Guarda anti-ciclo: una cadena maliciosa/malformada no debe colgar.
const MAX_PARENT_HOPS: usize = 8;
/// Tope de profundidad al recursar Form XObjects anidados dentro de una
/// apariencia de firma.
const MAX_FORM_DEPTH: usize = 3;

/// IDs de imágenes XObject que deben preservarse sin recomprimir (apariencias de
/// firma + sellos/logos pequeños). No muta el documento.
pub(crate) fn collect_preserved_images(doc: &Document) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    collect_signature_appearance_images(doc, &mut out);
    collect_small_flattened_images(doc, &mut out);
    out
}

/// Caso A: recolecta las imágenes embebidas en las apariencias (`/AP/N`) de los
/// widgets de firma (`Subtype=Widget`, `FT=Sig`).
fn collect_signature_appearance_images(doc: &Document, out: &mut HashSet<ObjectId>) {
    for (_, page_id) in doc.get_pages() {
        let annots = match page_annotations(doc, page_id) {
            Some(a) => a,
            None => continue,
        };
        for annot_obj in annots {
            // Resolver la anotación (puede ser referencia indirecta).
            let Ok((_, resolved)) = doc.dereference(&annot_obj) else {
                continue;
            };
            let Ok(annot) = resolved.as_dict() else {
                continue;
            };
            if !is_signature_widget(doc, annot) {
                continue;
            }
            collect_appearance_images(doc, annot, out);
        }
    }
}

/// Devuelve el array `/Annots` de una página (resuelto a un `Vec<Object>` de sus
/// elementos, sin desreferenciar cada uno todavía). `None` si no hay `/Annots`.
pub(crate) fn page_annotations(doc: &Document, page_id: ObjectId) -> Option<Vec<Object>> {
    let page = doc.get_dictionary(page_id).ok()?;
    let annots_obj = page.get(b"Annots").ok()?;
    let (_, resolved) = doc.dereference(annots_obj).ok()?;
    let arr = resolved.as_array().ok()?;
    Some(arr.to_vec())
}

/// Una anotación es firma si `Subtype=Widget` y su `FT` (directo o heredado por
/// la cadena `/Parent`) es `Sig`. Sube `/Parent` con tope duro de saltos.
pub(crate) fn is_signature_widget(doc: &Document, annot: &lopdf::Dictionary) -> bool {
    let is_widget = annot
        .get(b"Subtype")
        .and_then(|o| o.as_name())
        .map(|n| n == b"Widget")
        .unwrap_or(false);
    if !is_widget {
        return false;
    }
    field_type_is_sig(doc, annot)
}

/// Busca `FT=Sig` en el dict dado, subiendo por `/Parent` con tope de saltos.
fn field_type_is_sig(doc: &Document, start: &lopdf::Dictionary) -> bool {
    // Trabajamos con dicts propios (clonados al desreferenciar) para poder subir
    // la cadena sin problemas de préstamo.
    let mut current = start.clone();
    for _ in 0..=MAX_PARENT_HOPS {
        if let Ok(ft) = current.get(b"FT").and_then(|o| o.as_name()) {
            if ft == b"Sig" {
                return true;
            }
        }
        // Subir al padre, si existe.
        let Some(parent_obj) = current.get(b"Parent").ok() else {
            return false;
        };
        let Ok((_, resolved)) = doc.dereference(parent_obj) else {
            return false;
        };
        let Ok(parent) = resolved.as_dict() else {
            return false;
        };
        current = parent.clone();
    }
    false
}

/// Recolecta las imágenes de la apariencia normal (`/AP /N`) de una anotación.
/// `/N` puede ser un Form XObject directo o un diccionario de estados de
/// apariencia (subdiccionario nombre→Form); se toman todas las apariencias.
fn collect_appearance_images(
    doc: &Document,
    annot: &lopdf::Dictionary,
    out: &mut HashSet<ObjectId>,
) {
    let Some(ap_obj) = annot.get(b"AP").ok() else {
        return;
    };
    let Ok((_, ap_resolved)) = doc.dereference(ap_obj) else {
        return;
    };
    let Ok(ap) = ap_resolved.as_dict() else {
        return;
    };
    let Some(n_obj) = ap.get(b"N").ok() else {
        return;
    };
    // `/N` puede ser una referencia a un Form stream, o un dict de estados.
    let Ok((n_id, n_resolved)) = doc.dereference(n_obj) else {
        return;
    };
    match n_resolved {
        // Form XObject directo (stream).
        Object::Stream(_) => {
            if let Some(id) = n_id {
                collect_form_images(doc, id, 0, out, &mut HashSet::new());
            }
        }
        // Diccionario de estados de apariencia: cada valor es un Form XObject.
        Object::Dictionary(states) => {
            for (_, state_val) in states.iter() {
                if let Ok((Some(state_id), Object::Stream(_))) = doc.dereference(state_val) {
                    collect_form_images(doc, state_id, 0, out, &mut HashSet::new());
                }
            }
        }
        _ => {}
    }
}

/// Recorre los `/Resources /XObject` de un Form XObject (por ObjectId),
/// añadiendo las imágenes al set y recursando en Forms anidados con tope de
/// profundidad. `visited` evita ciclos entre forms.
fn collect_form_images(
    doc: &Document,
    form_id: ObjectId,
    depth: usize,
    out: &mut HashSet<ObjectId>,
    visited: &mut HashSet<ObjectId>,
) {
    if depth > MAX_FORM_DEPTH || !visited.insert(form_id) {
        return;
    }
    let Ok(stream) = doc.get_object(form_id).and_then(Object::as_stream) else {
        return;
    };
    // /Resources (puede ser referencia indirecta o dict directo).
    let Some(res_obj) = stream.dict.get(b"Resources").ok() else {
        return;
    };
    let Ok((_, res_resolved)) = doc.dereference(res_obj) else {
        return;
    };
    let Ok(resources) = res_resolved.as_dict() else {
        return;
    };
    let Some(xobj_obj) = resources.get(b"XObject").ok() else {
        return;
    };
    let Ok((_, xobj_resolved)) = doc.dereference(xobj_obj) else {
        return;
    };
    let Ok(xobjects) = xobj_resolved.as_dict() else {
        return;
    };
    for (_, value) in xobjects.iter() {
        let Ok(obj_id) = value.as_reference() else {
            continue;
        };
        let Ok(child) = doc.get_object(obj_id).and_then(Object::as_stream) else {
            continue;
        };
        match child.dict.get(b"Subtype").and_then(|o| o.as_name()) {
            Ok(st) if st == b"Image" => {
                out.insert(obj_id);
            }
            Ok(st) if st == b"Form" => {
                collect_form_images(doc, obj_id, depth + 1, out, visited);
            }
            _ => {}
        }
    }
}

/// Caso B: recolecta las imágenes XObject "pequeñas" (candidatas a sello/logo
/// aplanado): `Width <= MAX_STAMP_DIM && Height <= MAX_STAMP_DIM` y bytes del
/// stream `<= MAX_STAMP_BYTES`.
///
/// Guard anti-over-preservation (lever #2): si el total de bytes de las
/// candidatas supera `MAX_STAMP_TOTAL_BYTES`, el documento es image-heavy y la
/// heurística se disparó de más → no se preserva ninguna por case B (se dejan
/// comprimir). Los sellos legítimos de un doc normal suman poco y pasan; un
/// escaneo con cientos de fragmentos chicos no.
fn collect_small_flattened_images(doc: &Document, out: &mut HashSet<ObjectId>) {
    let mut candidates: Vec<ObjectId> = Vec::new();
    let mut total_bytes: usize = 0;
    for (id, obj) in doc.objects.iter() {
        let Ok(stream) = obj.as_stream() else {
            continue;
        };
        let is_image = stream
            .dict
            .get(b"Subtype")
            .and_then(|o| o.as_name())
            .map(|n| n == b"Image")
            .unwrap_or(false);
        if !is_image {
            continue;
        }
        let Ok(w) = stream.dict.get(b"Width").and_then(|o| o.as_i64()) else {
            continue;
        };
        let Ok(h) = stream.dict.get(b"Height").and_then(|o| o.as_i64()) else {
            continue;
        };
        if w <= 0 || h <= 0 || w > MAX_STAMP_DIM || h > MAX_STAMP_DIM {
            continue;
        }
        if stream.content.len() <= MAX_STAMP_BYTES {
            candidates.push(*id);
            total_bytes = total_bytes.saturating_add(stream.content.len());
        }
    }
    // Doc image-heavy → la heurística de sellos se disparó de más: no preservar.
    if total_bytes > MAX_STAMP_TOTAL_BYTES {
        return;
    }
    out.extend(candidates);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    /// JPEG pequeño de `side`×`side` px con ruido suave.
    fn small_jpeg(side: u32) -> Vec<u8> {
        use image::codecs::jpeg::JpegEncoder;
        use image::{ImageEncoder, RgbImage};
        let mut rgb = RgbImage::new(side, side);
        for (x, y, px) in rgb.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 90)
            .write_image(rgb.as_raw(), side, side, image::ExtendedColorType::Rgb8)
            .unwrap();
        jpeg
    }

    fn image_stream(side: i64, content: Vec<u8>) -> Stream {
        Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => side, "Height" => side,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
            },
            content,
        )
    }

    /// Documento con una firma widget cuyo `/AP/N` es un Form XObject con una
    /// imagen en sus recursos. Devuelve (doc, image_id).
    fn doc_signature_appearance(inherit_via_parent: bool) -> (Document, ObjectId) {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();

        // imagen embebida en la apariencia (grande, para probar que se preserva
        // por firma y no por tamaño pequeño).
        let img_id = doc.add_object(image_stream(400, small_jpeg(400)));

        // Form XObject de apariencia con /Resources /XObject /Frm0 -> img
        let form_stream = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                "Resources" => dictionary! {
                    "XObject" => dictionary! { "SImg" => img_id },
                },
            },
            b"q /SImg Do Q".to_vec(),
        );
        let form_id = doc.add_object(form_stream);

        // Anotación widget de firma con /AP /N -> form.
        let annot_dict = if inherit_via_parent {
            // FT=Sig lo aporta el /Parent; el widget hijo no lo tiene.
            let parent_id = doc.add_object(dictionary! { "FT" => "Sig" });
            dictionary! {
                "Type" => "Annot", "Subtype" => "Widget",
                "Parent" => parent_id,
                "AP" => dictionary! { "N" => form_id },
            }
        } else {
            dictionary! {
                "Type" => "Annot", "Subtype" => "Widget",
                "FT" => "Sig",
                "AP" => dictionary! { "N" => form_id },
            }
        };
        let annot_id = doc.add_object(annot_dict);

        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Annots" => vec![annot_id.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        (doc, img_id)
    }

    #[test]
    fn collects_signature_appearance_image() {
        let (doc, img_id) = doc_signature_appearance(false);
        let set = collect_preserved_images(&doc);
        assert!(
            set.contains(&img_id),
            "la imagen de la apariencia de firma debe recolectarse"
        );
    }

    #[test]
    fn collects_signature_appearance_image_via_parent() {
        let (doc, img_id) = doc_signature_appearance(true);
        let set = collect_preserved_images(&doc);
        assert!(
            set.contains(&img_id),
            "FT=Sig heredado por /Parent debe detectarse"
        );
    }

    #[test]
    fn parent_cycle_terminates() {
        // Cadena /Parent cíclica: a -> b -> a, sin FT=Sig. No debe colgar ni
        // desbordar la pila; simplemente devuelve false / termina.
        let mut doc = Document::with_version("1.5");
        let a_id = doc.new_object_id();
        let b_id = doc.new_object_id();
        doc.objects.insert(
            a_id,
            Object::Dictionary(dictionary! { "Subtype" => "Widget", "Parent" => b_id }),
        );
        doc.objects.insert(
            b_id,
            Object::Dictionary(dictionary! { "Subtype" => "Widget", "Parent" => a_id }),
        );
        let annot = doc.get_dictionary(a_id).unwrap().clone();
        // No debe colgar; sin FT=Sig en la cadena → false.
        assert!(!is_signature_widget(&doc, &annot));
    }

    #[test]
    fn small_stamp_is_collected() {
        let mut doc = Document::with_version("1.5");
        // sello pequeño 150x150 con pocos bytes.
        let stamp_id = doc.add_object(image_stream(150, small_jpeg(150)));
        let set = collect_preserved_images(&doc);
        assert!(
            set.contains(&stamp_id),
            "un sello pequeño (<=300px, <=50KB) debe recolectarse"
        );
    }

    #[test]
    fn large_image_is_not_collected_as_stamp() {
        let mut doc = Document::with_version("1.5");
        // imagen grande 800x800: fuera del umbral de sello.
        let big = small_jpeg(800);
        let big_id = doc.add_object(image_stream(800, big));
        let set = collect_preserved_images(&doc);
        assert!(
            !set.contains(&big_id),
            "una imagen grande no debe tratarse como sello"
        );
    }

    #[test]
    fn small_dims_but_large_bytes_not_collected() {
        // dimensiones pequeñas pero stream > 50KB: no es sello (falla el guard
        // de bytes).
        let mut doc = Document::with_version("1.5");
        let content = vec![0u8; MAX_STAMP_BYTES + 1];
        let id = doc.add_object(image_stream(100, content));
        let set = collect_preserved_images(&doc);
        assert!(
            !set.contains(&id),
            "imagen pequeña en px pero grande en bytes no debe preservarse"
        );
    }

    #[test]
    fn over_preservation_guard_skips_case_b_when_over_budget() {
        // Muchas imágenes chicas que SUMAN más que el presupuesto total: el guard
        // del lever #2 apaga case B por completo (doc image-heavy, no de sellos).
        let mut doc = Document::with_version("1.5");
        let per = MAX_STAMP_BYTES; // 50 KB c/u (bajo el tope por-imagen)
        let n = MAX_STAMP_TOTAL_BYTES / per + 2; // supera el presupuesto total
        for _ in 0..n {
            doc.add_object(image_stream(100, vec![0u8; per]));
        }
        let set = collect_preserved_images(&doc);
        assert!(
            set.is_empty(),
            "doc image-heavy: el guard debe apagar case B; preservadas={}",
            set.len()
        );
    }

    #[test]
    fn under_budget_stamps_still_preserved() {
        // Pocos sellos que suman poco (bajo presupuesto): se preservan normal.
        let mut doc = Document::with_version("1.5");
        let a = doc.add_object(image_stream(150, small_jpeg(150)));
        let b = doc.add_object(image_stream(120, small_jpeg(120)));
        let set = collect_preserved_images(&doc);
        assert!(
            set.contains(&a) && set.contains(&b),
            "sellos bajo presupuesto deben preservarse"
        );
    }
}
