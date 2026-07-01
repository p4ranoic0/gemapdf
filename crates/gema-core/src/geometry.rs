//! Geometría del content stream: DPI efectivo de cada imagen a partir del CTM.
//!
//! Una imagen XObject se pinta sobre el cuadrado unitario `[0,1]×[0,1]`
//! transformado por la matriz de transformación actual (CTM). El tamaño en
//! puntos PDF con que se muestra es la escala del CTM; de ahí el DPI real:
//! `dpi = px / (pt / 72)`. Este módulo interpreta el content stream de cada
//! página (operadores `q`/`Q`/`cm`/`Do`) manteniendo una pila de CTM y acumula,
//! por imagen, el **DPI máximo** entre todos sus usos en el documento.

use std::collections::HashMap;

use lopdf::{Document, Object, ObjectId};

/// Matriz de transformación PDF `[a b c d e f]`, que representa
/// ```text
/// | a b 0 |
/// | c d 0 |
/// | e f 1 |
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Matrix {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Matrix {
    pub(crate) const IDENTITY: Matrix = Matrix { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    /// Multiplicación de matrices PDF: `self × other` (self a la izquierda).
    ///
    /// Con la convención de vectores fila `[x y 1] · M`, aplicar primero `self`
    /// y luego `other` equivale a `self × other`. El operador `cm` **pre-**
    /// multiplica el CTM actual: `nuevo_ctm = cm × ctm_actual`, es decir
    /// `cm_matrix.mul(&ctm)`.
    pub(crate) fn mul(&self, other: &Matrix) -> Matrix {
        Matrix {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }
}

/// DPI efectivo (mayor de los dos ejes) al pintar una imagen de `px_w`×`px_h`
/// píxeles con la matriz `ctm`. El tamaño mostrado en puntos es
/// `w_pt = hypot(a,b)`, `h_pt = hypot(c,d)`. Devuelve `None` si el CTM es
/// degenerado (escala cero en algún eje) para no dividir por cero.
pub(crate) fn effective_dpi(ctm: &Matrix, px_w: u32, px_h: u32) -> Option<f32> {
    let w_pt = ctm.a.hypot(ctm.b);
    let h_pt = ctm.c.hypot(ctm.d);
    if w_pt <= 0.0 || h_pt <= 0.0 || !w_pt.is_finite() || !h_pt.is_finite() {
        return None;
    }
    let dpi_w = px_w as f32 / (w_pt / 72.0);
    let dpi_h = px_h as f32 / (h_pt / 72.0);
    let dpi = dpi_w.max(dpi_h);
    if dpi.is_finite() && dpi > 0.0 {
        Some(dpi)
    } else {
        None
    }
}

/// Lee `Width` y `Height` (px) del dict de un stream de imagen XObject.
fn image_px_dims(doc: &Document, id: ObjectId) -> Option<(u32, u32)> {
    let stream = doc.get_object(id).ok()?.as_stream().ok()?;
    let w = stream.dict.get(b"Width").and_then(|o| o.as_i64()).ok()?;
    let h = stream.dict.get(b"Height").and_then(|o| o.as_i64()).ok()?;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some((w as u32, h as u32))
}

/// Construye el mapa nombre-de-recurso → ObjectId de las imágenes XObject
/// visibles desde una página (incluye los `/Resources` heredados del árbol de
/// páginas). Sólo incluye XObjects `/Subtype /Image`.
fn page_image_names(doc: &Document, page_id: ObjectId) -> HashMap<Vec<u8>, ObjectId> {
    let mut map = HashMap::new();
    let Ok((inline_dict, resource_ids)) = doc.get_page_resources(page_id) else {
        return map;
    };

    let mut collect = |resources: &lopdf::Dictionary| {
        let Ok(xobjects) = resources.get(b"XObject") else {
            return;
        };
        let xobj_dict = match xobjects {
            Object::Reference(id) => doc.get_object(*id).and_then(Object::as_dict).ok(),
            Object::Dictionary(d) => Some(d),
            _ => None,
        };
        if let Some(xobj_dict) = xobj_dict {
            for (name, value) in xobj_dict.iter() {
                if let Ok(obj_id) = value.as_reference() {
                    // sólo imágenes (evita Form XObjects, etc.)
                    let is_image = doc
                        .get_object(obj_id)
                        .and_then(Object::as_stream)
                        .ok()
                        .and_then(|s| s.dict.get(b"Subtype").and_then(|o| o.as_name()).ok())
                        .map(|st| st == b"Image")
                        .unwrap_or(false);
                    if is_image {
                        map.entry(name.clone()).or_insert(obj_id);
                    }
                }
            }
        }
    };

    if let Some(inline) = inline_dict {
        collect(inline);
    }
    for rid in resource_ids {
        if let Ok(resources) = doc.get_dictionary(rid) {
            collect(resources);
        }
    }
    map
}

/// Recorre el content stream de una página manteniendo la pila de CTM y, en
/// cada `Do` de una imagen conocida, acumula el DPI efectivo máximo por imagen.
fn accumulate_page(doc: &Document, page_id: ObjectId, out: &mut HashMap<ObjectId, f32>) {
    let names = page_image_names(doc, page_id);
    if names.is_empty() {
        return;
    }
    let Ok(content) = doc.get_and_decode_page_content(page_id) else {
        return;
    };

    let mut ctm = Matrix::IDENTITY;
    let mut stack: Vec<Matrix> = Vec::new();

    for op in &content.operations {
        match op.operator.as_str() {
            "q" => stack.push(ctm),
            "Q" => {
                if let Some(prev) = stack.pop() {
                    ctm = prev;
                }
            }
            "cm" => {
                if op.operands.len() >= 6 {
                    let vals: Option<Vec<f32>> =
                        op.operands[..6].iter().map(|o| o.as_float().ok()).collect();
                    if let Some(v) = vals {
                        let cm = Matrix { a: v[0], b: v[1], c: v[2], d: v[3], e: v[4], f: v[5] };
                        // cm pre-multiplica: nuevo = cm × ctm
                        ctm = cm.mul(&ctm);
                    }
                }
            }
            "Do" => {
                if let Some(name) = op.operands.first().and_then(|o| o.as_name().ok()) {
                    if let Some(&img_id) = names.get(name) {
                        if let Some((px_w, px_h)) = image_px_dims(doc, img_id) {
                            if let Some(dpi) = effective_dpi(&ctm, px_w, px_h) {
                                let entry = out.entry(img_id).or_insert(0.0);
                                if dpi > *entry {
                                    *entry = dpi;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Calcula, para todo el documento, el DPI efectivo **máximo** de cada imagen
/// XObject a partir del CTM de sus usos en los content streams de las páginas.
/// Las imágenes que nunca se encuentran pintadas no aparecen en el mapa.
pub(crate) fn effective_dpi_map(doc: &Document) -> HashMap<ObjectId, f32> {
    let mut out = HashMap::new();
    for (_, page_id) in doc.get_pages() {
        accumulate_page(doc, page_id, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_mul_is_noop() {
        let m = Matrix { a: 2.0, b: 3.0, c: 4.0, d: 5.0, e: 6.0, f: 7.0 };
        assert_eq!(Matrix::IDENTITY.mul(&m), m);
        assert_eq!(m.mul(&Matrix::IDENTITY), m);
    }

    #[test]
    fn pure_scale_gives_expected_pt() {
        // cm = 2 0 0 2 0 0 sobre identidad → escala x2
        let cm = Matrix { a: 2.0, b: 0.0, c: 0.0, d: 2.0, e: 0.0, f: 0.0 };
        let ctm = cm.mul(&Matrix::IDENTITY);
        assert_eq!(ctm, cm);
        // una imagen de 200px pintada con escala 2 → 2pt de lado.
        // Espera... el CTM escala el cuadrado unitario a 2pt. Con 200px eso da
        // 200 / (2/72) = 7200 DPI. Verificamos el número exacto abajo.
        let dpi = effective_dpi(&ctm, 200, 200).unwrap();
        assert!((dpi - 7200.0).abs() < 0.001, "dpi={dpi}");
    }

    #[test]
    fn scale_200pt_box_144dpi() {
        // Enunciado: escala 2 0 0 2 ... aplicada de forma que un 200px caiga en
        // 100pt ⇒ 144 DPI. 100pt de caja: escala = 100.
        let cm = Matrix { a: 100.0, b: 0.0, c: 0.0, d: 100.0, e: 0.0, f: 0.0 };
        let ctm = cm.mul(&Matrix::IDENTITY);
        let dpi = effective_dpi(&ctm, 200, 200).unwrap();
        assert!((dpi - 144.0).abs() < 0.001, "dpi={dpi}");
    }

    #[test]
    fn cm_premultiplies_scale_then_translate() {
        // Primero escala x2 (cm1), luego traslada (cm2). El CTM resultante debe
        // escalar y trasladar. Con pre-multiplicación: ctm = cm2 × cm1.
        let cm1 = Matrix { a: 2.0, b: 0.0, c: 0.0, d: 2.0, e: 0.0, f: 0.0 };
        let ctm1 = cm1.mul(&Matrix::IDENTITY);
        let cm2 = Matrix { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 10.0, f: 20.0 };
        let ctm2 = cm2.mul(&ctm1);
        // la escala (a,d) sigue siendo 2; la traslación se compone.
        assert_eq!((ctm2.a, ctm2.d), (2.0, 2.0));
        assert_eq!((ctm2.e, ctm2.f), (20.0, 40.0));
    }

    #[test]
    fn rotated_ctm_uses_hypot() {
        // rotación 90°: a=0 b=s c=-s d=0, con s la escala. La escala efectiva por
        // eje es hypot(a,b)=s y hypot(c,d)=s.
        let s = 72.0_f32; // 72pt = 1in
        let ctm = Matrix { a: 0.0, b: s, c: -s, d: 0.0, e: 0.0, f: 0.0 };
        // 72px en 1in = 72 DPI
        let dpi = effective_dpi(&ctm, 72, 72).unwrap();
        assert!((dpi - 72.0).abs() < 0.001, "dpi={dpi}");
    }

    #[test]
    fn degenerate_ctm_returns_none() {
        let zero = Matrix { a: 0.0, b: 0.0, c: 0.0, d: 0.0, e: 0.0, f: 0.0 };
        assert_eq!(effective_dpi(&zero, 100, 100), None);
    }

    #[test]
    fn effective_dpi_max_over_axes() {
        // caja no cuadrada: 100pt de ancho, 200pt de alto; imagen 300x300.
        // dpi_w = 300/(100/72)=216 ; dpi_h = 300/(200/72)=108 → max 216.
        let ctm = Matrix { a: 100.0, b: 0.0, c: 0.0, d: 200.0, e: 0.0, f: 0.0 };
        let dpi = effective_dpi(&ctm, 300, 300).unwrap();
        assert!((dpi - 216.0).abs() < 0.001, "dpi={dpi}");
    }

    /// Construye un PDF de 1 página que pinta una imagen `side`×`side` px con el
    /// content stream `content`. Devuelve (bytes-doc no serializado como Document,
    /// img_id). Trabajamos sobre el `Document` en memoria (no serializamos) para
    /// probar `effective_dpi_map` directamente.
    fn doc_with_image(side: i64, content: &[u8], mediabox: i64) -> (Document, ObjectId) {
        use lopdf::{dictionary, Stream};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        // stream de imagen mínimo (no hace falta que decodifique; sólo Width/Height)
        let img_stream = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => side, "Height" => side,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB", "Filter" => "DCTDecode",
            },
            vec![0u8; 16],
        );
        let img_id = doc.add_object(img_stream);
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
        let resources_id =
            doc.add_object(dictionary! { "XObject" => dictionary! { "Im0" => img_id } });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), mediabox.into(), mediabox.into()],
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
    fn map_computes_144_dpi_for_200px_in_100pt_box() {
        // 200px pintada con `100 0 0 100 0 0 cm` = caja de 100pt = 144 DPI.
        let (doc, img_id) = doc_with_image(200, b"q 100 0 0 100 0 0 cm /Im0 Do Q", 200);
        let map = effective_dpi_map(&doc);
        let dpi = *map.get(&img_id).expect("la imagen debe estar en el mapa");
        assert!((dpi - 144.0).abs() < 0.01, "dpi={dpi}");
    }

    #[test]
    fn map_keeps_maximum_over_multiple_uses() {
        // dos usos de la misma imagen: uno a 72 DPI (200pt) y otro a 144 DPI (100pt).
        // Debe quedar el máximo (144).
        let content = b"q 200 0 0 200 0 0 cm /Im0 Do Q q 100 0 0 100 0 0 cm /Im0 Do Q";
        let (doc, img_id) = doc_with_image(200, content, 400);
        let map = effective_dpi_map(&doc);
        let dpi = *map.get(&img_id).unwrap();
        assert!((dpi - 144.0).abs() < 0.01, "dpi={dpi}");
    }

    #[test]
    fn map_respects_q_Q_nesting() {
        // el `cm` dentro de q/Q no debe filtrarse al Do posterior fuera del bloque.
        // Bloque 1: escala 50 (288 DPI para 200px) dentro de q/Q.
        // Tras Q, el CTM vuelve a identidad; el Do exterior usa cm 100 (144 DPI).
        let content =
            b"q q 50 0 0 50 0 0 cm Q 100 0 0 100 0 0 cm /Im0 Do Q";
        let (doc, img_id) = doc_with_image(200, content, 400);
        let map = effective_dpi_map(&doc);
        let dpi = *map.get(&img_id).unwrap();
        assert!((dpi - 144.0).abs() < 0.01, "dpi={dpi}");
    }
}
