use lopdf::{dictionary, Document, Object, ObjectId, StringFormat};

use super::*;
use crate::test_support::Fixture;

fn marker(n: u32) -> Vec<u8> {
    format!("BT /F1 12 Tf 10 10 Td (PAGINA {n}) Tj ET").into_bytes()
}

fn doc_con_paginas(n: u32) -> Fixture {
    let mut fx = Fixture::new();
    for k in 1..=n {
        fx.text_page(&marker(k));
    }
    fx
}

fn plan(items: &[(u32, Rotation)]) -> Vec<PageSpec> {
    items
        .iter()
        .map(|&(source, rotate)| PageSpec { source, rotate })
        .collect()
}

fn order(sources: &[u32]) -> Vec<PageSpec> {
    plan(
        &sources
            .iter()
            .map(|&s| (s, Rotation::R0))
            .collect::<Vec<_>>(),
    )
}

fn run(fx: &mut Fixture, p: &[PageSpec]) -> ArrangeResult {
    arrange_pages(&fx.bytes(), p, &EditOptions::default()).unwrap()
}

fn page_number(doc: &Document, page: ObjectId) -> u32 {
    let content = doc.get_page_content(page).unwrap();
    let text = String::from_utf8_lossy(&content);
    let start = text.find("(PAGINA ").unwrap() + 8;
    let end = start + text[start..].find(')').unwrap();
    text[start..end].parse().unwrap()
}

fn pages(bytes: &[u8]) -> Vec<(u32, i64)> {
    let doc = Document::load_mem(bytes).unwrap();
    doc.get_pages()
        .values()
        .map(|&id| {
            let rotate = doc
                .get_dictionary(id)
                .unwrap()
                .get(b"Rotate")
                .and_then(Object::as_i64)
                .unwrap_or(0);
            (page_number(&doc, id), rotate)
        })
        .collect()
}

fn catalog(doc: &Document) -> &lopdf::Dictionary {
    doc.catalog().unwrap()
}

// ---------- Task 1: plan y errores ----------

#[test]
fn rechaza_plan_vacio() {
    let mut fx = doc_con_paginas(3);
    let err = arrange_pages(&fx.bytes(), &[], &EditOptions::default()).unwrap_err();
    assert!(matches!(err, EditError::PagePlan(PagePlanError::Empty)));
}

#[test]
fn rechaza_indice_fuera_de_rango() {
    let mut fx = doc_con_paginas(3);
    let err = arrange_pages(&fx.bytes(), &order(&[0, 3]), &EditOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        EditError::PagePlan(PagePlanError::OutOfRange { index: 3, count: 3 })
    ));
}

#[test]
fn rechaza_indice_repetido() {
    let mut fx = doc_con_paginas(3);
    let err = arrange_pages(&fx.bytes(), &order(&[1, 0, 1]), &EditOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        EditError::PagePlan(PagePlanError::Duplicate { index: 1 })
    ));
}

#[test]
fn rechaza_cifrado() {
    let mut fx = doc_con_paginas(2);
    let encrypt = fx.doc.add_object(dictionary! {
        "Filter" => "Standard", "V" => 1, "R" => 2, "Length" => 40, "P" => -4,
        "O" => Object::String(vec![0; 32], StringFormat::Hexadecimal),
        "U" => Object::String(vec![0; 32], StringFormat::Hexadecimal),
    });
    fx.doc.trailer.set("Encrypt", encrypt);
    fx.doc.trailer.set(
        "ID",
        vec![
            Object::String(vec![1; 16], StringFormat::Hexadecimal),
            Object::String(vec![1; 16], StringFormat::Hexadecimal),
        ],
    );
    let err = arrange_pages(&fx.bytes(), &order(&[1, 0]), &EditOptions::default()).unwrap_err();
    // lopdf puede rechazarlo al parsear o abrirlo como cifrado; nunca debe salir sin cifrar.
    assert!(
        matches!(err, EditError::Encrypted | EditError::Parse(_)),
        "{err:?}"
    );
}

#[test]
fn respeta_max_input_bytes() {
    let mut fx = doc_con_paginas(2);
    let opts = EditOptions {
        max_input_bytes: 10,
        ..Default::default()
    };
    let err = arrange_pages(&fx.bytes(), &order(&[0]), &opts).unwrap_err();
    assert!(matches!(
        err,
        EditError::LimitExceeded(LimitKind::InputBytes)
    ));
}

// ---------- Task 2: árbol, orden y rotación ----------

#[test]
fn invierte_orden() {
    let mut fx = doc_con_paginas(3);
    let out = run(&mut fx, &order(&[2, 1, 0]));
    assert_eq!(pages(&out.output), vec![(3, 0), (2, 0), (1, 0)]);
    assert_eq!(out.report.pages_out, 3);
    assert!(out.report.dropped.is_empty());
}

#[test]
fn suma_rotacion() {
    let mut fx = Fixture::new();
    let c = fx.content_stream(dictionary! {}, &marker(1));
    fx.add_page(
        c,
        Some(Fixture::default_resources()),
        vec![("Rotate", 90.into())],
    );
    let out = run(&mut fx, &plan(&[(0, Rotation::R180)]));
    assert_eq!(pages(&out.output), vec![(1, 270)]);
}

#[test]
fn normaliza_rotacion_negativa() {
    let mut fx = Fixture::new();
    let c = fx.content_stream(dictionary! {}, &marker(1));
    fx.add_page(
        c,
        Some(Fixture::default_resources()),
        vec![("Rotate", (-90).into())],
    );
    let out = run(&mut fx, &plan(&[(0, Rotation::R90)]));
    assert_eq!(pages(&out.output), vec![(1, 0)]);
}

/// 6 páginas en dos `/Pages` intermedios que llevan Resources, MediaBox y
/// (el segundo) Rotate 90; las hojas no tienen ninguno de los tres.
fn arbol_anidado() -> Fixture {
    let mut fx = doc_con_paginas(6);
    let root = fx.pages_id;
    let mut groups = Vec::new();
    for (g, chunk) in fx.page_ids.clone().chunks(3).enumerate() {
        let mut node = dictionary! {
            "Type" => "Pages", "Parent" => root, "Count" => 3,
            "Kids" => chunk.iter().map(|id| Object::Reference(*id)).collect::<Vec<_>>(),
            "Resources" => Fixture::default_resources(),
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 400.into()],
        };
        if g == 1 {
            node.set("Rotate", 90);
        }
        let node_id = fx.doc.add_object(node);
        for id in chunk {
            let page = fx.doc.get_dictionary_mut(*id).unwrap();
            page.set("Parent", node_id);
            page.remove(b"Resources");
            page.remove(b"MediaBox");
        }
        groups.push(Object::Reference(node_id));
    }
    fx.set_pages("Kids", groups);
    fx
}

#[test]
fn hereda_atributos() {
    let mut fx = arbol_anidado();
    let out = run(&mut fx, &order(&[5, 4, 3, 2, 1, 0]));
    assert_eq!(
        pages(&out.output),
        vec![(6, 90), (5, 90), (4, 90), (3, 0), (2, 0), (1, 0)]
    );
    let doc = Document::load_mem(&out.output).unwrap();
    let root = catalog(&doc).get(b"Pages").unwrap().as_reference().unwrap();
    for id in doc.get_pages().values() {
        let page = doc.get_dictionary(*id).unwrap();
        assert!(page.has(b"Resources"));
        let media: Vec<i64> = page
            .get(b"MediaBox")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o.as_i64().unwrap())
            .collect();
        assert_eq!(media, vec![0, 0, 300, 400]);
        assert_eq!(page.get(b"Parent").unwrap().as_reference().unwrap(), root);
    }
    let root = doc.get_dictionary(root).unwrap();
    assert!(!root.has(b"Rotate") && !root.has(b"Resources"));
}

#[test]
fn identidad_no_toca_nada() {
    let mut fx = doc_con_paginas(3);
    fx.set_catalog(
        "PageLabels",
        dictionary! { "Nums" => vec![0.into(), dictionary! { "S" => "D" }.into()] },
    );
    let out = run(&mut fx, &order(&[0, 1, 2]));
    assert_eq!(pages(&out.output), vec![(1, 0), (2, 0), (3, 0)]);
    let r = &out.report;
    assert_eq!(r.page_labels, CatalogEntry::Kept);
    assert_eq!(
        (
            r.outline_items_cleared,
            r.named_dests_removed,
            r.links_cleared,
            r.form_fields_removed,
            r.other_refs_cleared
        ),
        (0, 0, 0, 0, 0)
    );
}

#[test]
fn arbol_con_ciclo_falla_sin_panico() {
    let mut fx = doc_con_paginas(2);
    let root = fx.pages_id;
    let mut kids = fx
        .page_ids
        .iter()
        .map(|id| Object::Reference(*id))
        .collect::<Vec<_>>();
    kids.push(Object::Reference(root));
    fx.set_pages("Kids", kids);
    let err = arrange_pages(&fx.bytes(), &order(&[0]), &EditOptions::default()).unwrap_err();
    assert!(matches!(err, EditError::Parse(_)), "{err:?}");
}

// ---------- Task 3: limpieza contada ----------

struct Completo {
    fx: Fixture,
}

fn dest(page: ObjectId) -> Object {
    vec![Object::Reference(page), "Fit".into()].into()
}

fn s(text: &str) -> Object {
    Object::String(text.as_bytes().to_vec(), StringFormat::Literal)
}

/// 12 páginas con todo lo que cuelga del catálogo. Índices 0-based: las
/// páginas «2 y 5» del spec son 1 y 4.
fn catalogo_completo() -> Completo {
    let mut fx = doc_con_paginas(12);
    let p = fx.page_ids.clone();
    let info = fx.doc.add_object(dictionary! { "Title" => s("Fixture") });
    fx.doc.trailer.set("Info", info);
    let xmp = fx.doc.add_object(lopdf::Stream::new(
        dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
        b"<x:xmpmeta/>".to_vec(),
    ));
    fx.set_catalog("Metadata", xmp);
    // Marcadores: Cap 1 → p0 (con hijo → p1), Cap 2 → p4 por acción GoTo, Cap 3 → p9 por nombre d5.
    let outlines = fx.doc.new_object_id();
    let cap1 = fx.doc.new_object_id();
    let sec = fx
        .doc
        .add_object(dictionary! { "Title" => s("Sec"), "Parent" => cap1, "Dest" => dest(p[1]) });
    let cap2 = fx.doc.new_object_id();
    let cap3 = fx.doc.add_object(dictionary! { "Title" => s("Cap 3"), "Parent" => outlines, "Prev" => cap2, "Dest" => s("d5") });
    fx.doc.objects.insert(cap1, dictionary! { "Title" => s("Cap 1"), "Parent" => outlines, "Dest" => dest(p[0]), "First" => sec, "Last" => sec, "Count" => 1, "Next" => cap2 }.into());
    fx.doc.objects.insert(cap2, dictionary! { "Title" => s("Cap 2"), "Parent" => outlines, "Prev" => cap1, "Next" => cap3, "A" => dictionary! { "S" => "GoTo", "D" => dest(p[4]) } }.into());
    fx.doc.objects.insert(
        outlines,
        dictionary! { "Type" => "Outlines", "First" => cap1, "Last" => cap3, "Count" => 4 }.into(),
    );
    fx.set_catalog("Outlines", outlines);
    // Destinos con nombre: árbol (d2 → p1, d5 → p4, d9 → p8) y /Dests del catálogo (viejo → p1, otro → p10).
    let tree = fx.doc.add_object(dictionary! { "Names" => vec![s("d2"), dest(p[1]), s("d5"), dest(p[4]), s("d9"), dest(p[8])] });
    fx.set_catalog("Names", dictionary! { "Dests" => tree });
    fx.set_catalog(
        "Dests",
        dictionary! { "viejo" => dest(p[1]), "otro" => dictionary! { "D" => dest(p[10]) } },
    );
    // Enlaces: p0 → p4 explícito, p2 → d2 por nombre, p6 → p8.
    for (from, d) in [(0usize, dest(p[4])), (2, s("d2")), (6, dest(p[8]))] {
        let link = fx.doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Link", "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()], "Dest" => d });
        fx.doc
            .get_dictionary_mut(p[from])
            .unwrap()
            .set("Annots", vec![Object::Reference(link)]);
    }
    fx.set_catalog(
        "PageLabels",
        dictionary! { "Nums" => vec![0.into(), dictionary! { "S" => "r" }.into()] },
    );
    let ocg = fx
        .doc
        .add_object(dictionary! { "Type" => "OCG", "Name" => s("Capa") });
    fx.set_catalog("OCProperties", dictionary! { "OCGs" => vec![Object::Reference(ocg)], "D" => dictionary! { "ON" => vec![Object::Reference(ocg)] } });
    let root = fx.doc.new_object_id();
    let elems: Vec<Object> = p
        .iter()
        .map(|&page| Object::Reference(fx.doc.add_object(dictionary! { "Type" => "StructElem", "S" => "P", "P" => root, "Pg" => page, "K" => 0 })))
        .collect();
    fx.doc.objects.insert(
        root,
        dictionary! { "Type" => "StructTreeRoot", "K" => elems }.into(),
    );
    fx.set_catalog("StructTreeRoot", root);
    fx.set_catalog("MarkInfo", dictionary! { "Marked" => true });
    // Formulario: campo «a» con widget en p1; campo «b» con widgets en p4 y p6.
    let widget = |fx: &mut Fixture, page: ObjectId, parent: Option<ObjectId>| {
        let mut w = dictionary! { "Type" => "Annot", "Subtype" => "Widget", "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()], "P" => page };
        if let Some(parent) = parent {
            w.set("Parent", parent);
        } else {
            w.set("T", s("a"));
            w.set("FT", "Tx");
        }
        let id = fx.doc.add_object(w);
        let page = fx.doc.get_dictionary_mut(page).unwrap();
        let mut annots = page
            .get(b"Annots")
            .and_then(Object::as_array)
            .cloned()
            .unwrap_or_default();
        annots.push(Object::Reference(id));
        page.set("Annots", annots);
        id
    };
    let a = widget(&mut fx, p[1], None);
    let b = fx.doc.new_object_id();
    let b4 = widget(&mut fx, p[4], Some(b));
    let b6 = widget(&mut fx, p[6], Some(b));
    fx.doc.objects.insert(b, dictionary! { "T" => s("b"), "FT" => "Tx", "Kids" => vec![Object::Reference(b4), Object::Reference(b6)] }.into());
    fx.set_catalog(
        "AcroForm",
        dictionary! { "Fields" => vec![Object::Reference(a), Object::Reference(b)] },
    );
    // Nota en p6 cuyo /P apunta (mal) a p4: sólo el barrido la encuentra.
    let note = fx.doc.add_object(dictionary! { "Type" => "Annot", "Subtype" => "Text", "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()], "P" => p[4] });
    let page6 = fx.doc.get_dictionary_mut(p[6]).unwrap();
    let mut annots = page6
        .get(b"Annots")
        .and_then(Object::as_array)
        .cloned()
        .unwrap();
    annots.push(Object::Reference(note));
    page6.set("Annots", annots);
    Completo { fx }
}

fn sin_2_y_5() -> Vec<PageSpec> {
    order(&(0..12).filter(|i| *i != 1 && *i != 4).collect::<Vec<_>>())
}

fn quitadas() -> (ArrangeReport, Document) {
    let mut c = catalogo_completo();
    let out = run(&mut c.fx, &sin_2_y_5());
    let doc = Document::load_mem(&out.output).unwrap();
    (out.report, doc)
}

fn deref_dict<'a>(doc: &'a Document, o: &'a Object) -> &'a lopdf::Dictionary {
    deref(doc, o).as_dict().unwrap()
}

#[test]
fn quitar_paginas_conserva_orden_y_las_informa() {
    let mut c = catalogo_completo();
    let out = run(&mut c.fx, &sin_2_y_5());
    assert_eq!(
        pages(&out.output).iter().map(|p| p.0).collect::<Vec<_>>(),
        vec![1, 3, 4, 6, 7, 8, 9, 10, 11, 12]
    );
    assert_eq!(out.report.dropped, vec![1, 4]);
}

#[test]
fn marcadores_sin_destino_se_cuentan() {
    let (report, doc) = quitadas();
    // Sec (→p1), Cap 2 (GoTo →p4) y Cap 3 (→ d5, quitado) pierden destino; Cap 1 lo conserva.
    assert_eq!(report.outline_items_cleared, 3);
    let outlines = deref_dict(&doc, catalog(&doc).get(b"Outlines").unwrap());
    let cap1 = deref_dict(&doc, outlines.get(b"First").unwrap());
    assert!(cap1.has(b"Dest"));
    let cap2 = deref_dict(&doc, cap1.get(b"Next").unwrap());
    assert!(!cap2.has(b"A") && cap2.has(b"Title"));
}

#[test]
fn destinos_con_nombre_quitados() {
    let (report, doc) = quitadas();
    // d2, d5 del árbol y «viejo» de /Dests.
    assert_eq!(report.named_dests_removed, 3);
    let names = deref_dict(&doc, catalog(&doc).get(b"Names").unwrap());
    let tree = deref_dict(&doc, names.get(b"Dests").unwrap());
    assert_eq!(tree.get(b"Names").unwrap().as_array().unwrap().len(), 2);
    let dests = deref_dict(&doc, catalog(&doc).get(b"Dests").unwrap());
    assert!(dests.has(b"otro") && !dests.has(b"viejo"));
}

#[test]
fn enlaces_a_quitadas_se_cuentan() {
    let (report, _) = quitadas();
    // p0 → p4 y p2 → d2; p6 → p8 sigue.
    assert_eq!(report.links_cleared, 2);
}

#[test]
fn campo_sin_widgets_se_va_y_el_compartido_queda() {
    let (report, doc) = quitadas();
    assert_eq!(report.form_fields_removed, 1);
    let acro = deref_dict(&doc, catalog(&doc).get(b"AcroForm").unwrap());
    let fields = acro.get(b"Fields").unwrap().as_array().unwrap();
    assert_eq!(fields.len(), 1);
    let b = deref_dict(&doc, &fields[0]);
    assert_eq!(b.get(b"Kids").unwrap().as_array().unwrap().len(), 1);
}

#[test]
fn etiquetas_y_estructura_se_descartan() {
    let (report, doc) = quitadas();
    assert_eq!(report.page_labels, CatalogEntry::Dropped);
    assert_eq!(report.struct_tree, CatalogEntry::Dropped);
    let cat = catalog(&doc);
    assert!(!cat.has(b"PageLabels") && !cat.has(b"StructTreeRoot") && !cat.has(b"MarkInfo"));
    assert!(cat.has(b"OCProperties") && cat.has(b"Metadata"));
    assert!(doc.trailer.has(b"Info"));
}

#[test]
fn barrido_cuenta_otras_referencias() {
    let (report, _) = quitadas();
    // El /P de la nota de p6 hacia p4.
    assert_eq!(report.other_refs_cleared, 1);
}

#[test]
fn pagina_quitada_no_queda_en_el_archivo() {
    let (_, doc) = quitadas();
    for object in doc.objects.values() {
        if let Object::Stream(stream) = object {
            let body = stream
                .decompressed_content()
                .unwrap_or_else(|_| stream.content.clone());
            let text = String::from_utf8_lossy(&body);
            assert!(
                !text.contains("(PAGINA 2)") && !text.contains("(PAGINA 5)"),
                "{text}"
            );
        }
    }
}

#[test]
fn con_solo_reordenar_conserva_todo() {
    let mut c = catalogo_completo();
    let out = run(&mut c.fx, &order(&(0..12).rev().collect::<Vec<_>>()));
    let r = &out.report;
    assert_eq!(
        (
            r.outline_items_cleared,
            r.named_dests_removed,
            r.links_cleared,
            r.form_fields_removed,
            r.other_refs_cleared
        ),
        (0, 0, 0, 0, 0)
    );
    assert_eq!(r.page_labels, CatalogEntry::Dropped);
    assert_eq!(r.struct_tree, CatalogEntry::Kept);
    let doc = Document::load_mem(&out.output).unwrap();
    let cat = catalog(&doc);
    for key in [
        &b"Outlines"[..],
        b"Names",
        b"Dests",
        b"OCProperties",
        b"Metadata",
        b"StructTreeRoot",
        b"MarkInfo",
        b"AcroForm",
    ] {
        assert!(cat.has(key), "{}", String::from_utf8_lossy(key));
    }
    let acro = deref_dict(&doc, cat.get(b"AcroForm").unwrap());
    assert_eq!(acro.get(b"Fields").unwrap().as_array().unwrap().len(), 2);
}

#[test]
fn firma_se_informa() {
    let mut fx = doc_con_paginas(2);
    fx.set_catalog(
        "AcroForm",
        dictionary! { "SigFlags" => 3, "Fields" => Vec::<Object>::new() },
    );
    let out = run(&mut fx, &order(&[1, 0]));
    assert!(out.report.signatures.sig_flags);
}

#[cfg(feature = "serde")]
#[test]
fn plan_desde_json() {
    let plan: Vec<PageSpec> =
        serde_json::from_str(r#"[{"source":1,"rotate":"r90"},{"source":0}]"#).unwrap();
    assert_eq!(plan, plan_ref());
    assert!(serde_json::from_str::<Vec<PageSpec>>(r#"[{"source":0,"rotate":"r45"}]"#).is_err());
}

#[cfg(feature = "serde")]
fn plan_ref() -> Vec<PageSpec> {
    plan(&[(1, Rotation::R90), (0, Rotation::R0)])
}
