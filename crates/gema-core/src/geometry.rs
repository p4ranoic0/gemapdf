//! Geometría del content stream: DPI efectivo de cada imagen a partir del CTM.
//!
//! Una imagen XObject se pinta sobre el cuadrado unitario `[0,1]×[0,1]`
//! transformado por la matriz de transformación actual (CTM). El tamaño en
//! puntos PDF con que se muestra es la escala del CTM; de ahí el DPI real:
//! `dpi = px / (pt / 72)`. Este módulo interpreta el content stream de cada
//! página (operadores `q`/`Q`/`cm`/`Do`) manteniendo una pila de CTM y acumula,
//! por imagen, el **DPI máximo** entre todos sus usos en el documento.
//!
//! El recorrido entra también en los Form XObjects (`/Subtype /Form`): un `Do`
//! sobre un form compone su `/Matrix` con el CTM del llamador y sigue con los
//! `/Resources` del form (o, si no los declara, con los del contexto que lo
//! pinta, según PDF 32000 §8.10.1). Sin esa recursión las imágenes anidadas en
//! forms quedaban con DPI desconocido y nunca se reducían.

use std::collections::{HashMap, HashSet};

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

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
    pub(crate) const IDENTITY: Matrix = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

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

/// Qué clase de XObject es un nombre de recurso. Los demás subtipos (grupos de
/// PostScript, por ejemplo) no participan del recorrido.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XObjectKind {
    Image,
    Form,
}

/// Nombre de recurso → XObject al que apunta.
type XObjectNames = HashMap<Vec<u8>, (ObjectId, XObjectKind)>;

/// Agrega a `map` los XObjects declarados por un diccionario `/Resources`.
/// El primer nombre gana, que es el orden de precedencia con que se consultan
/// los recursos propios antes que los heredados.
fn collect_xobjects(doc: &Document, resources: &Dictionary, map: &mut XObjectNames) {
    let Ok(xobjects) = resources.get(b"XObject") else {
        return;
    };
    let xobj_dict = match xobjects {
        Object::Reference(id) => doc.get_object(*id).and_then(Object::as_dict).ok(),
        Object::Dictionary(d) => Some(d),
        _ => None,
    };
    let Some(xobj_dict) = xobj_dict else {
        return;
    };
    for (name, value) in xobj_dict.iter() {
        let Ok(obj_id) = value.as_reference() else {
            continue;
        };
        let subtype = doc
            .get_object(obj_id)
            .and_then(Object::as_stream)
            .ok()
            .and_then(|s| s.dict.get(b"Subtype").and_then(|o| o.as_name()).ok());
        let kind = match subtype {
            Some(st) if st == b"Image" => XObjectKind::Image,
            Some(st) if st == b"Form" => XObjectKind::Form,
            _ => continue,
        };
        map.entry(name.clone()).or_insert((obj_id, kind));
    }
}

/// XObjects visibles desde una página, incluidos los `/Resources` heredados del
/// árbol de páginas.
fn page_xobject_names(doc: &Document, page_id: ObjectId) -> XObjectNames {
    let mut map = XObjectNames::new();
    let Ok((inline_dict, resource_ids)) = doc.get_page_resources(page_id) else {
        return map;
    };
    if let Some(inline) = inline_dict {
        collect_xobjects(doc, inline, &mut map);
    }
    for rid in resource_ids {
        if let Ok(resources) = doc.get_dictionary(rid) {
            collect_xobjects(doc, resources, &mut map);
        }
    }
    map
}

/// XObjects declarados por el `/Resources` propio de un form. `None` cuando el
/// form no declara recursos: en ese caso hereda los del contexto que lo pinta.
fn form_xobject_names(doc: &Document, form: &Stream) -> Option<XObjectNames> {
    let resources = form.dict.get(b"Resources").ok()?;
    let (_, resolved) = doc.dereference(resources).ok()?;
    let dict = resolved.as_dict().ok()?;
    let mut map = XObjectNames::new();
    collect_xobjects(doc, dict, &mut map);
    Some(map)
}

/// Bytes ya desfiltrados del content stream de un form.
fn stream_content_bytes(form: &Stream) -> Vec<u8> {
    form.decompressed_content()
        .unwrap_or_else(|_| form.content.clone())
}

/// `/Matrix` del form; identidad si falta o está malformada.
fn form_matrix(doc: &Document, form: &Stream) -> Matrix {
    let Ok(value) = form.dict.get(b"Matrix") else {
        return Matrix::IDENTITY;
    };
    let Ok((_, resolved)) = doc.dereference(value) else {
        return Matrix::IDENTITY;
    };
    let Ok(arr) = resolved.as_array() else {
        return Matrix::IDENTITY;
    };
    if arr.len() < 6 {
        return Matrix::IDENTITY;
    }
    let vals: Option<Vec<f32>> = arr[..6].iter().map(|o| o.as_float().ok()).collect();
    match vals {
        Some(v) => Matrix {
            a: v[0],
            b: v[1],
            c: v[2],
            d: v[3],
            e: v[4],
            f: v[5],
        },
        None => Matrix::IDENTITY,
    }
}

/// Anidado máximo de Form XObjects que se recorre. Los PDFs reales rara vez
/// pasan de dos o tres niveles; el tope evita gastar tiempo en documentos
/// patológicos.
const MAX_FORM_DEPTH: usize = 8;

/// Techo de operadores interpretados por página, contando los de los forms
/// anidados. Un form pintado muchas veces se recorre una vez por `Do` (cada uso
/// tiene su propio CTM), así que el producto puede crecer rápido; el
/// presupuesto lo acota sin cambiar el resultado en documentos normales.
const MAX_OPS_PER_PAGE: u32 = 200_000;

const MIN_SCAN_COVERAGE: f32 = 0.80;
const MIN_SCAN_RASTER_SIDE: u32 = 400;
const MAX_SCAN_TEXT_BYTES: usize = 32;

/// Estado compartido por el recorrido de una página y de los forms que anida.
struct GeometryWalk<'a> {
    doc: &'a Document,
    dpi: &'a mut HashMap<ObjectId, f32>,
    max_raster_area: f32,
    text_bytes: usize,
    ops_left: u32,
}

impl GeometryWalk<'_> {
    /// Interpreta una secuencia de operadores con `ctm` como CTM inicial.
    /// `names` son los XObjects visibles en este contexto y `active` los forms
    /// que están en la pila de recursión (corta ciclos `/Resources`).
    fn walk(
        &mut self,
        content: &Content,
        names: &XObjectNames,
        ctm: Matrix,
        depth: usize,
        active: &mut Vec<ObjectId>,
    ) {
        let mut ctm = ctm;
        let mut stack: Vec<Matrix> = Vec::new();

        for op in &content.operations {
            if self.ops_left == 0 {
                return;
            }
            self.ops_left -= 1;

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
                            let cm = Matrix {
                                a: v[0],
                                b: v[1],
                                c: v[2],
                                d: v[3],
                                e: v[4],
                                f: v[5],
                            };
                            // cm pre-multiplica: nuevo = cm × ctm
                            ctm = cm.mul(&ctm);
                        }
                    }
                }
                "Do" => {
                    let Some(name) = op.operands.first().and_then(|o| o.as_name().ok()) else {
                        continue;
                    };
                    match names.get(name) {
                        Some(&(img_id, XObjectKind::Image)) => self.paint_image(img_id, &ctm),
                        Some(&(form_id, XObjectKind::Form)) => {
                            self.enter_form(form_id, names, &ctm, depth, active)
                        }
                        None => {}
                    }
                }
                "Tj" | "'" | "\"" => {
                    if let Some(Object::String(bytes, _)) = op.operands.last() {
                        self.text_bytes = self.text_bytes.saturating_add(bytes.len());
                    }
                }
                "TJ" => {
                    if let Some(Object::Array(items)) = op.operands.first() {
                        for item in items {
                            if let Object::String(bytes, _) = item {
                                self.text_bytes = self.text_bytes.saturating_add(bytes.len());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Registra un uso de imagen: DPI efectivo máximo y evidencia de escaneo.
    fn paint_image(&mut self, img_id: ObjectId, ctm: &Matrix) {
        let Some((px_w, px_h)) = image_px_dims(self.doc, img_id) else {
            return;
        };
        if let Some(dpi) = effective_dpi(ctm, px_w, px_h) {
            let entry = self.dpi.entry(img_id).or_insert(0.0);
            if dpi > *entry {
                *entry = dpi;
            }
        }
        if px_w >= MIN_SCAN_RASTER_SIDE && px_h >= MIN_SCAN_RASTER_SIDE {
            let painted_area = (ctm.a * ctm.d - ctm.b * ctm.c).abs();
            if painted_area.is_finite() {
                self.max_raster_area = self.max_raster_area.max(painted_area);
            }
        }
    }

    /// Recorre el contenido de un Form XObject con el CTM del `Do` compuesto
    /// con su `/Matrix`. Si el form no declara `/Resources`, hereda los del
    /// contexto que lo pinta.
    fn enter_form(
        &mut self,
        form_id: ObjectId,
        outer_names: &XObjectNames,
        ctm: &Matrix,
        depth: usize,
        active: &mut Vec<ObjectId>,
    ) {
        if depth >= MAX_FORM_DEPTH || active.contains(&form_id) {
            return;
        }
        let Ok(form) = self.doc.get_object(form_id).and_then(Object::as_stream) else {
            return;
        };
        let Ok(content) = Content::decode(&stream_content_bytes(form)) else {
            return;
        };
        let inner_ctm = form_matrix(self.doc, form).mul(ctm);
        let own_names = form_xobject_names(self.doc, form);
        let names = own_names.as_ref().unwrap_or(outer_names);

        active.push(form_id);
        self.walk(&content, names, inner_ctm, depth + 1, active);
        active.pop();
    }
}

/// Recorre el content stream de una página manteniendo la pila de CTM y, en
/// cada `Do` de una imagen conocida, acumula el DPI efectivo máximo por imagen.
/// Devuelve si la página aporta evidencia conservadora de escaneo.
fn accumulate_page(doc: &Document, page_id: ObjectId, out: &mut HashMap<ObjectId, f32>) -> bool {
    let names = page_xobject_names(doc, page_id);
    if names.is_empty() {
        return false;
    }
    let Ok(content) = doc.get_and_decode_page_content(page_id) else {
        return false;
    };

    let page_area = inherited_page_area(doc, page_id);
    let mut walk = GeometryWalk {
        doc,
        dpi: out,
        max_raster_area: 0.0,
        text_bytes: 0,
        ops_left: MAX_OPS_PER_PAGE,
    };
    walk.walk(&content, &names, Matrix::IDENTITY, 0, &mut Vec::new());

    let (max_raster_area, text_bytes) = (walk.max_raster_area, walk.text_bytes);
    page_area.is_some_and(|area| {
        text_bytes <= MAX_SCAN_TEXT_BYTES && max_raster_area / area >= MIN_SCAN_COVERAGE
    })
}

/// Calcula, para todo el documento, el DPI efectivo **máximo** de cada imagen
/// XObject a partir del CTM de sus usos en los content streams de las páginas.
/// Las imágenes que nunca se encuentran pintadas no aparecen en el mapa.
#[cfg(test)]
pub(crate) fn effective_dpi_map(doc: &Document) -> HashMap<ObjectId, f32> {
    document_geometry(doc).0
}

/// Área de `/CropBox` o `/MediaBox`, buscando también en ancestros `/Pages`.
fn inherited_page_area(doc: &Document, page_id: ObjectId) -> Option<f32> {
    for key in [b"CropBox".as_slice(), b"MediaBox"] {
        let mut current = page_id;
        let mut seen = HashSet::new();
        while seen.insert(current) {
            let dict = doc.get_dictionary(current).ok()?;
            if let Ok(value) = dict.get(key) {
                let (_, resolved) = doc.dereference(value).ok()?;
                let values = resolved.as_array().ok()?;
                if values.len() < 4 {
                    return None;
                }
                let coords: Option<Vec<f32>> =
                    values[..4].iter().map(|o| o.as_float().ok()).collect();
                let coords = coords?;
                let area = ((coords[2] - coords[0]) * (coords[3] - coords[1])).abs();
                return (area.is_finite() && area > 0.0).then_some(area);
            }
            let Ok(parent) = dict.get(b"Parent").and_then(Object::as_reference) else {
                break;
            };
            current = parent;
        }
    }
    None
}

/// `true` si al menos una página aporta evidencia conservadora de escaneo.
pub(crate) fn has_scanned_pages(doc: &Document) -> bool {
    document_geometry(doc).1
}

/// Calcula DPI efectivo y evidencia de escaneo en una sola lectura de cada
/// content stream. El pipeline consume ambos resultados; `analyze()` usa el
/// segundo y descarta el mapa.
pub(crate) fn document_geometry(doc: &Document) -> (HashMap<ObjectId, f32>, bool) {
    let mut dpi = HashMap::new();
    let mut has_scans = false;
    for page_id in doc.get_pages().values() {
        has_scans |= accumulate_page(doc, *page_id, &mut dpi);
    }
    (dpi, has_scans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    #[test]
    fn identity_mul_is_noop() {
        let m = Matrix {
            a: 2.0,
            b: 3.0,
            c: 4.0,
            d: 5.0,
            e: 6.0,
            f: 7.0,
        };
        assert_eq!(Matrix::IDENTITY.mul(&m), m);
        assert_eq!(m.mul(&Matrix::IDENTITY), m);
    }

    #[test]
    fn pure_scale_gives_expected_pt() {
        // cm = 2 0 0 2 0 0 sobre identidad → escala x2
        let cm = Matrix {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 2.0,
            e: 0.0,
            f: 0.0,
        };
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
        let cm = Matrix {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            d: 100.0,
            e: 0.0,
            f: 0.0,
        };
        let ctm = cm.mul(&Matrix::IDENTITY);
        let dpi = effective_dpi(&ctm, 200, 200).unwrap();
        assert!((dpi - 144.0).abs() < 0.001, "dpi={dpi}");
    }

    #[test]
    fn cm_premultiplies_scale_then_translate() {
        // Primero escala x2 (cm1), luego traslada (cm2). El CTM resultante debe
        // escalar y trasladar. Con pre-multiplicación: ctm = cm2 × cm1.
        let cm1 = Matrix {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 2.0,
            e: 0.0,
            f: 0.0,
        };
        let ctm1 = cm1.mul(&Matrix::IDENTITY);
        let cm2 = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 10.0,
            f: 20.0,
        };
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
        let ctm = Matrix {
            a: 0.0,
            b: s,
            c: -s,
            d: 0.0,
            e: 0.0,
            f: 0.0,
        };
        // 72px en 1in = 72 DPI
        let dpi = effective_dpi(&ctm, 72, 72).unwrap();
        assert!((dpi - 72.0).abs() < 0.001, "dpi={dpi}");
    }

    #[test]
    fn degenerate_ctm_returns_none() {
        let zero = Matrix {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
        };
        assert_eq!(effective_dpi(&zero, 100, 100), None);
    }

    #[test]
    fn effective_dpi_max_over_axes() {
        // caja no cuadrada: 100pt de ancho, 200pt de alto; imagen 300x300.
        // dpi_w = 300/(100/72)=216 ; dpi_h = 300/(200/72)=108 → max 216.
        let ctm = Matrix {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            d: 200.0,
            e: 0.0,
            f: 0.0,
        };
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
    fn contents_array_preserves_ctm_across_split_streams() {
        use lopdf::{dictionary, Stream};
        let (mut doc, image_id) = doc_with_image(200, b"", 200);
        let page_id = doc.get_pages()[&1];
        let first = doc.add_object(Stream::new(
            dictionary! {},
            b"q 100 0 0 100 0 0 cm".to_vec(),
        ));
        let second = doc.add_object(Stream::new(dictionary! {}, b"/Im0 Do Q".to_vec()));
        doc.get_dictionary_mut(page_id)
            .unwrap()
            .set("Contents", vec![first.into(), second.into()]);

        let dpi = effective_dpi_map(&doc);
        assert!((dpi[&image_id] - 144.0).abs() < 0.01);
    }

    #[test]
    fn map_respects_q_q_nesting() {
        // el `cm` dentro de q/Q no debe filtrarse al Do posterior fuera del bloque.
        // Bloque 1: escala 50 (288 DPI para 200px) dentro de q/Q.
        // Tras Q, el CTM vuelve a identidad; el Do exterior usa cm 100 (144 DPI).
        let content = b"q q 50 0 0 50 0 0 cm Q 100 0 0 100 0 0 cm /Im0 Do Q";
        let (doc, img_id) = doc_with_image(200, content, 400);
        let map = effective_dpi_map(&doc);
        let dpi = *map.get(&img_id).unwrap();
        assert!((dpi - 144.0).abs() < 0.01, "dpi={dpi}");
    }

    /// Añade un Form XObject y lo declara como `/Fm0` en los `/Resources` de la
    /// primera página. `extra` permite fijar `/Matrix` o `/Resources` propios.
    fn add_form(doc: &mut Document, content: &[u8], extra: lopdf::Dictionary) -> ObjectId {
        use lopdf::{dictionary, Stream};
        let mut dict = dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 1000.into(), 1000.into()],
        };
        for (key, value) in extra.iter() {
            dict.set(key.clone(), value.clone());
        }
        let form_id = doc.add_object(Stream::new(dict, content.to_vec()));

        let page_id = doc.get_pages()[&1];
        let resources_id = doc
            .get_dictionary(page_id)
            .unwrap()
            .get(b"Resources")
            .unwrap()
            .as_reference()
            .unwrap();
        let resources = doc.get_dictionary_mut(resources_id).unwrap();
        let xobjects = resources
            .get_mut(b"XObject")
            .unwrap()
            .as_dict_mut()
            .unwrap();
        xobjects.set("Fm0", form_id);
        form_id
    }

    #[test]
    fn image_inside_a_form_gets_its_dpi_derived() {
        // La imagen se pinta DENTRO del form; sin recursión quedaba sin DPI y
        // por lo tanto nunca se reducía (TODO-v2 #15).
        let (mut doc, image_id) = doc_with_image(200, b"q /Fm0 Do Q", 200);
        add_form(
            &mut doc,
            b"q 100 0 0 100 0 0 cm /Im0 Do Q",
            dictionary! {
                "Resources" => dictionary! {
                    "XObject" => dictionary! { "Im0" => image_id },
                },
            },
        );

        let map = effective_dpi_map(&doc);
        let dpi = *map
            .get(&image_id)
            .expect("la imagen anidada debe tener DPI");
        assert!((dpi - 144.0).abs() < 0.01, "dpi={dpi}");
    }

    #[test]
    fn form_matrix_composes_with_the_outer_ctm() {
        // /Matrix escala 0.5 y el `cm` interno escala 100 ⇒ caja de 50pt.
        // 200px en 50pt = 288 DPI. Si el /Matrix se ignorara darían 144.
        let (mut doc, image_id) = doc_with_image(200, b"q /Fm0 Do Q", 200);
        add_form(
            &mut doc,
            b"q 100 0 0 100 0 0 cm /Im0 Do Q",
            dictionary! {
                "Matrix" => vec![0.5.into(), 0.into(), 0.into(), 0.5.into(), 0.into(), 0.into()],
                "Resources" => dictionary! {
                    "XObject" => dictionary! { "Im0" => image_id },
                },
            },
        );

        let map = effective_dpi_map(&doc);
        let dpi = *map.get(&image_id).unwrap();
        assert!((dpi - 288.0).abs() < 0.01, "dpi={dpi}");
    }

    #[test]
    fn outer_cm_scales_the_form_content() {
        // El `cm` de la página multiplica lo que pinta el form: cm 2 fuera y
        // cm 100 dentro ⇒ caja de 200pt ⇒ 72 DPI para 200px.
        let (mut doc, image_id) = doc_with_image(200, b"q 2 0 0 2 0 0 cm /Fm0 Do Q", 400);
        add_form(
            &mut doc,
            b"q 100 0 0 100 0 0 cm /Im0 Do Q",
            dictionary! {
                "Resources" => dictionary! {
                    "XObject" => dictionary! { "Im0" => image_id },
                },
            },
        );

        let map = effective_dpi_map(&doc);
        let dpi = *map.get(&image_id).unwrap();
        assert!((dpi - 72.0).abs() < 0.01, "dpi={dpi}");
    }

    #[test]
    fn form_without_resources_inherits_them_from_the_page() {
        // PDF 32000 §8.10.1: sin /Resources propios, el form ve los del
        // contexto que lo pinta — ahí sigue estando /Im0.
        let (mut doc, image_id) = doc_with_image(200, b"q /Fm0 Do Q", 200);
        add_form(
            &mut doc,
            b"q 100 0 0 100 0 0 cm /Im0 Do Q",
            lopdf::Dictionary::new(),
        );

        let map = effective_dpi_map(&doc);
        let dpi = *map.get(&image_id).expect("debe heredar /Im0 de la página");
        assert!((dpi - 144.0).abs() < 0.01, "dpi={dpi}");
    }

    #[test]
    fn self_referencing_form_terminates() {
        // Un form que se pinta a sí mismo no debe colgar el recorrido; el guard
        // de ciclo lo corta y la imagen igual se registra.
        let (mut doc, image_id) = doc_with_image(200, b"q /Fm0 Do Q", 200);
        let form_id = add_form(
            &mut doc,
            b"q 100 0 0 100 0 0 cm /Im0 Do /Fm0 Do Q",
            dictionary! {
                "Resources" => dictionary! {
                    "XObject" => dictionary! { "Im0" => image_id },
                },
            },
        );
        // el form se declara a sí mismo dentro de sus propios recursos
        let resources_id = doc
            .get_object(form_id)
            .unwrap()
            .as_stream()
            .unwrap()
            .dict
            .get(b"Resources")
            .unwrap()
            .clone();
        let mut resources = resources_id.as_dict().unwrap().clone();
        resources
            .get_mut(b"XObject")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Fm0", form_id);
        doc.get_object_mut(form_id)
            .unwrap()
            .as_stream_mut()
            .unwrap()
            .dict
            .set("Resources", resources);

        let map = effective_dpi_map(&doc);
        assert!((map[&image_id] - 144.0).abs() < 0.01);
    }

    #[test]
    fn full_page_raster_painted_through_a_form_looks_scanned() {
        // La evidencia de escaneo también tiene que ver a través del form.
        let (mut doc, image_id) = doc_with_image(1000, b"q /Fm0 Do Q", 600);
        add_form(
            &mut doc,
            b"q 600 0 0 600 0 0 cm /Im0 Do Q",
            dictionary! {
                "Resources" => dictionary! {
                    "XObject" => dictionary! { "Im0" => image_id },
                },
            },
        );
        assert!(has_scanned_pages(&doc));
    }

    #[test]
    fn full_page_raster_with_little_text_looks_scanned() {
        let content = b"q 600 0 0 600 0 0 cm /Im0 Do Q BT (page 1) Tj ET";
        let (doc, _) = doc_with_image(1000, content, 600);
        assert!(has_scanned_pages(&doc));
    }

    #[test]
    fn full_page_raster_with_substantial_text_is_not_classified_as_scan() {
        let content = b"q 600 0 0 600 0 0 cm /Im0 Do Q BT (This page has substantial born-digital text over its background image.) Tj ET";
        let (doc, _) = doc_with_image(1000, content, 600);
        assert!(!has_scanned_pages(&doc));
    }

    #[test]
    fn small_raster_does_not_look_scanned() {
        let content = b"q 300 0 0 300 0 0 cm /Im0 Do Q";
        let (doc, _) = doc_with_image(1000, content, 600);
        assert!(!has_scanned_pages(&doc));
    }
}
