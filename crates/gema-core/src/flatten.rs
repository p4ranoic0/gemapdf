//! Aplanado de firmas/sellos visibles al contenido de página (política Flatten).
//!
//! Convierte los widgets de firma (`FT=Sig` con `/AP/N`) en contenido horneado:
//! pinta la apariencia en el content stream con la matriz que mapea el `/BBox`
//! (tras su `/Matrix`) al `/Rect` del widget (PDF 32000 §12.5.5), quita el widget
//! de `/Annots` y elimina `/AcroForm` (con él, `/NeedAppearances`). Así el
//! resultado es visible en Acrobat, que si no regeneraría los campos en blanco.
//! Las imágenes de la firma NO se tocan: el skip-set de `signatures.rs` las cubre.
//!
//! No entra en pánico sobre datos del documento (`.ok()`/`.and_then`).

use std::collections::HashMap;

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, ObjectId};

use crate::geometry::Matrix;
use crate::signatures::{is_signature_widget, page_annotations};

/// Un aplanado pendiente: qué apariencia pintar en qué página con qué matriz, y
/// qué anotación remover después.
struct FlattenOp {
    page_id: ObjectId,
    annot_id: ObjectId,
    form_id: ObjectId,
    a: Matrix,
}

/// Aplana todas las firmas visibles del documento. Devuelve cuántas aplanó.
/// Fase 1 (solo lectura) recolecta; fase 2 muta.
pub(crate) fn flatten_signatures(doc: &mut Document) -> usize {
    let ops = collect_flatten_ops(doc);
    if ops.is_empty() {
        return 0;
    }
    let mut per_page_remove: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let mut counter: usize = 0;
    for op in &ops {
        let name = format!("GemaFlat{counter}");
        counter += 1;
        if doc
            .add_xobject(op.page_id, name.clone().into_bytes(), op.form_id)
            .is_err()
        {
            continue;
        }
        let m = &op.a;
        let ops_content = vec![
            Operation::new("q", vec![]),
            Operation::new(
                "cm",
                vec![
                    Object::Real(m.a),
                    Object::Real(m.b),
                    Object::Real(m.c),
                    Object::Real(m.d),
                    Object::Real(m.e),
                    Object::Real(m.f),
                ],
            ),
            Operation::new("Do", vec![Object::Name(name.into_bytes())]),
            Operation::new("Q", vec![]),
        ];
        if doc
            .add_to_page_content(op.page_id, Content { operations: ops_content })
            .is_ok()
        {
            per_page_remove
                .entry(op.page_id)
                .or_default()
                .push(op.annot_id);
        }
    }
    let flattened: usize = per_page_remove.values().map(|v| v.len()).sum();
    for (page_id, remove_ids) in per_page_remove {
        remove_annots(doc, page_id, &remove_ids);
    }
    if flattened > 0 {
        // Elimina el AcroForm ENTERO (no sólo los campos aplanados): con él se van
        // /Fields, /SigFlags y sobre todo /NeedAppearances (el flag que hacía que
        // Acrobat regenerara los campos en blanco). Es intencional para este uso
        // —expedientes firmados finales que se recomprimen para archivo/visualización,
        // sin campos interactivos que conservar—; coincide con lo que produce el
        // motor clásico (salida pdf-lib con Form: none).
        if let Ok(catalog) = doc.catalog_mut() {
            catalog.remove(b"AcroForm");
        }
    }
    flattened
}

/// Fase 1: recorre páginas/anotaciones (solo lectura) y arma la lista de
/// aplanados con su matriz ya calculada.
fn collect_flatten_ops(doc: &Document) -> Vec<FlattenOp> {
    let mut out = Vec::new();
    for (_, page_id) in doc.get_pages() {
        let Some(annots) = page_annotations(doc, page_id) else {
            continue;
        };
        for annot_obj in annots {
            let Ok(annot_id) = annot_obj.as_reference() else {
                continue;
            };
            let Ok(annot) = doc.get_dictionary(annot_id) else {
                continue;
            };
            if !is_signature_widget(doc, annot) {
                continue;
            }
            let Some((form_id, bbox, matrix)) = appearance_form(doc, annot) else {
                continue;
            };
            let Some(rect) = read_num_array4(annot.get(b"Rect").ok(), doc) else {
                continue;
            };
            let a = compute_flatten_matrix(&bbox, &matrix, &rect);
            out.push(FlattenOp {
                page_id,
                annot_id,
                form_id,
                a,
            });
        }
    }
    out
}

/// Resuelve `/AP → /N` a un Form XObject: (id, BBox, Matrix). Maneja el caso
/// directo (stream) y el dict de estados (toma el primer stream).
fn appearance_form(
    doc: &Document,
    annot: &lopdf::Dictionary,
) -> Option<(ObjectId, [f32; 4], Matrix)> {
    let ap = annot.get(b"AP").ok()?;
    let (_, ap) = doc.dereference(ap).ok()?;
    let ap = ap.as_dict().ok()?;
    let n = ap.get(b"N").ok()?;
    let (n_id, n_resolved) = doc.dereference(n).ok()?;
    let form_id = match n_resolved {
        Object::Stream(_) => n_id?,
        Object::Dictionary(states) => {
            let mut found = None;
            for (_, v) in states.iter() {
                if let Ok((Some(id), Object::Stream(_))) = doc.dereference(v) {
                    found = Some(id);
                    break;
                }
            }
            found?
        }
        _ => return None,
    };
    let stream = doc.get_object(form_id).ok()?.as_stream().ok()?;
    let bbox = read_num_array4(stream.dict.get(b"BBox").ok(), doc)?;
    let matrix = match stream.dict.get(b"Matrix").ok() {
        Some(m) => read_matrix(m, doc).unwrap_or(Matrix::IDENTITY),
        None => Matrix::IDENTITY,
    };
    Some((form_id, bbox, matrix))
}

fn num(o: &Object) -> Option<f32> {
    o.as_f32().ok().or_else(|| o.as_i64().ok().map(|i| i as f32))
}

fn read_num_array4(o: Option<&Object>, doc: &Document) -> Option<[f32; 4]> {
    let (_, resolved) = doc.dereference(o?).ok()?;
    let arr = resolved.as_array().ok()?;
    if arr.len() < 4 {
        return None;
    }
    Some([num(&arr[0])?, num(&arr[1])?, num(&arr[2])?, num(&arr[3])?])
}

fn read_matrix(o: &Object, doc: &Document) -> Option<Matrix> {
    let (_, resolved) = doc.dereference(o).ok()?;
    let arr = resolved.as_array().ok()?;
    if arr.len() < 6 {
        return None;
    }
    Some(Matrix {
        a: num(&arr[0])?,
        b: num(&arr[1])?,
        c: num(&arr[2])?,
        d: num(&arr[3])?,
        e: num(&arr[4])?,
        f: num(&arr[5])?,
    })
}

/// Aplica una Matrix (convención fila `[x y 1]·M`) a un punto.
fn apply(m: &Matrix, x: f32, y: f32) -> (f32, f32) {
    (m.a * x + m.c * y + m.e, m.b * x + m.d * y + m.f)
}

/// Matriz que mapea el BBox (tras Matrix) al Rect (PDF 32000 §12.5.5).
fn compute_flatten_matrix(bbox: &[f32; 4], matrix: &Matrix, rect: &[f32; 4]) -> Matrix {
    let corners = [
        apply(matrix, bbox[0], bbox[1]),
        apply(matrix, bbox[2], bbox[1]),
        apply(matrix, bbox[2], bbox[3]),
        apply(matrix, bbox[0], bbox[3]),
    ];
    let tx0 = corners.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
    let tx1 = corners.iter().map(|p| p.0).fold(f32::NEG_INFINITY, f32::max);
    let ty0 = corners.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
    let ty1 = corners.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max);
    let rx0 = rect[0].min(rect[2]);
    let rx1 = rect[0].max(rect[2]);
    let ry0 = rect[1].min(rect[3]);
    let ry1 = rect[1].max(rect[3]);
    let tw = tx1 - tx0;
    let th = ty1 - ty0;
    let sx = if tw.abs() > f32::EPSILON {
        (rx1 - rx0) / tw
    } else {
        1.0
    };
    let sy = if th.abs() > f32::EPSILON {
        (ry1 - ry0) / th
    } else {
        1.0
    };
    Matrix {
        a: sx,
        b: 0.0,
        c: 0.0,
        d: sy,
        e: rx0 - sx * tx0,
        f: ry0 - sy * ty0,
    }
}

/// Quita del array `/Annots` de la página las anotaciones con id en `remove`.
fn remove_annots(doc: &mut Document, page_id: ObjectId, remove: &[ObjectId]) {
    let annots = match doc
        .get_dictionary(page_id)
        .ok()
        .and_then(|p| p.get(b"Annots").ok().cloned())
    {
        Some(x) => x,
        None => return,
    };
    match annots {
        Object::Array(arr) => {
            let filtered = filter_annots(arr, remove);
            if let Ok(page) = doc.get_dictionary_mut(page_id) {
                page.set("Annots", Object::Array(filtered));
            }
        }
        Object::Reference(arr_id) => {
            if let Ok(Object::Array(arr)) = doc.get_object(arr_id).cloned() {
                let filtered = filter_annots(arr, remove);
                if let Ok(obj) = doc.get_object_mut(arr_id) {
                    *obj = Object::Array(filtered);
                }
            }
        }
        _ => {}
    }
}

fn filter_annots(arr: Vec<Object>, remove: &[ObjectId]) -> Vec<Object> {
    arr.into_iter()
        .filter(|o| o.as_reference().map(|id| !remove.contains(&id)).unwrap_or(true))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    /// Doc con 1 página + 1 widget de firma (FT=Sig) cuyo `/AP/N` es un Form
    /// XObject (BBox [0 0 100 50]) con una imagen. AcroForm con NeedAppearances.
    /// Devuelve (doc, page_id, img_id).
    fn doc_with_signature() -> (Document, ObjectId, ObjectId) {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let img_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => 10, "Height" => 10,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceGray",
            },
            vec![0u8; 100],
        ));
        let form_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 100.into(), 50.into()],
                "Resources" => dictionary! { "XObject" => dictionary! { "Im0" => img_id } },
            },
            b"q /Im0 Do Q".to_vec(),
        ));
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Sig",
            "Rect" => vec![100.into(), 700.into(), 300.into(), 750.into()],
            "AP" => dictionary! { "N" => form_id },
        });
        let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Annots" => vec![annot_id.into()],
            "Resources" => dictionary! {},
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let acro_id = doc.add_object(dictionary! {
            "Fields" => vec![annot_id.into()], "NeedAppearances" => true, "SigFlags" => 3,
        });
        let catalog_id = doc.add_object(
            dictionary! { "Type" => "Catalog", "Pages" => pages_id, "AcroForm" => acro_id },
        );
        doc.trailer.set("Root", catalog_id);
        (doc, page_id, img_id)
    }

    #[test]
    fn flattens_signature_into_page_content() {
        let (mut doc, page_id, img_id) = doc_with_signature();
        let n = flatten_signatures(&mut doc);
        assert_eq!(n, 1, "debe aplanar 1 firma");

        let content = doc.get_page_content(page_id).unwrap();
        let s = String::from_utf8_lossy(&content);
        assert!(s.contains("Do"), "la página debe pintar la apariencia: {s}");

        let page = doc.get_dictionary(page_id).unwrap();
        let annots_empty = page
            .get(b"Annots")
            .ok()
            .and_then(|o| o.as_array().ok())
            .map(|a| a.is_empty())
            .unwrap_or(true);
        assert!(annots_empty, "el widget debe salir de /Annots");

        assert!(
            doc.catalog().unwrap().get(b"AcroForm").is_err(),
            "/AcroForm debe eliminarse"
        );
        assert!(
            doc.get_object(img_id).is_ok(),
            "la imagen de firma debe seguir existiendo (preservable)"
        );
    }

    #[test]
    fn no_signatures_is_noop() {
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
        assert_eq!(flatten_signatures(&mut doc), 0);
    }

    #[test]
    fn flatten_matrix_maps_bbox_to_rect() {
        // BBox [0 0 100 50], Matrix identidad, Rect [100 700 300 750]
        // → sx = 200/100 = 2, sy = 50/50 = 1, e = 100, f = 700.
        let a = compute_flatten_matrix(
            &[0.0, 0.0, 100.0, 50.0],
            &Matrix::IDENTITY,
            &[100.0, 700.0, 300.0, 750.0],
        );
        assert!((a.a - 2.0).abs() < 1e-4, "sx: {}", a.a);
        assert!((a.d - 1.0).abs() < 1e-4, "sy: {}", a.d);
        assert!((a.e - 100.0).abs() < 1e-4, "e: {}", a.e);
        assert!((a.f - 700.0).abs() < 1e-4, "f: {}", a.f);
    }

    #[test]
    fn flatten_matrix_accounts_for_form_matrix() {
        // /Matrix con traslación (e=10, f=20). El BBox [0 0 100 50] transformado
        // → esquinas (10,20)..(110,70), bbox transformado [10 20 110 70] (tw=100,
        // th=50). Rect [100 700 300 750] → sx=200/100=2, sy=50/50=1,
        // e=100-2*10=80, f=700-1*20=680. Si el código usara el BBox CRUDO en vez
        // del transformado, e daría 100 (no 80): este test lo detecta.
        let m = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 10.0,
            f: 20.0,
        };
        let a = compute_flatten_matrix(&[0.0, 0.0, 100.0, 50.0], &m, &[100.0, 700.0, 300.0, 750.0]);
        assert!((a.a - 2.0).abs() < 1e-4, "sx: {}", a.a);
        assert!((a.d - 1.0).abs() < 1e-4, "sy: {}", a.d);
        assert!((a.e - 80.0).abs() < 1e-4, "e: {}", a.e);
        assert!((a.f - 680.0).abs() < 1e-4, "f: {}", a.f);
    }
}
