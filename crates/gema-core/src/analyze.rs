use crate::error::GemaError;
use crate::report::Report;
use lopdf::Document;

/// Detecta firma criptográfica, con sesgo deliberado hacia la seguridad.
///
/// Devuelve `true` si CUALQUIERA de estas condiciones se cumple:
///   - algún diccionario tiene `/ByteRange` (presente en toda firma real); O
///   - algún diccionario tiene `/Type` con nombre `Sig` o `DocTimeStamp`
///     (firmas normales y sellos de tiempo); O
///   - el `/AcroForm` del catálogo tiene `/SigFlags` con un entero != 0
///     (bit 1 = SignaturesExist).
///
/// El sesgo es intencional: un falso positivo (negarse a comprimir) es
/// aceptable; un falso negativo (corromper una firma) no lo es. Por eso ya
/// NO exigimos `/ByteRange` ni `/Type == Sig` juntos: basta cualquier señal.
/// Nota: una clave `/Sig` suelta —sin `/ByteRange`, sin `/Type` de firma y sin
/// `/SigFlags`— sigue sin marcar el documento como firmado.
fn detect_signed(doc: &Document) -> bool {
    let by_type_or_byterange = doc.objects.values().any(|obj| {
        obj.as_dict()
            .map(|d| {
                d.has(b"ByteRange")
                    || d.get(b"Type")
                        .and_then(|o| o.as_name())
                        .is_ok_and(|n| n == b"Sig" || n == b"DocTimeStamp")
            })
            .unwrap_or(false)
    });
    by_type_or_byterange || acroform_has_sigflags(doc)
}

/// Comprueba si el `/AcroForm` del catálogo declara `/SigFlags` con un valor
/// entero distinto de cero. `/AcroForm` puede ser una referencia indirecta, así
/// que la resolvemos vía `dereference`.
fn acroform_has_sigflags(doc: &Document) -> bool {
    let Ok(catalog) = doc.catalog() else {
        return false;
    };
    let Some(acroform_obj) = catalog.get(b"AcroForm").ok() else {
        return false;
    };
    let Ok((_, resolved)) = doc.dereference(acroform_obj) else {
        return false;
    };
    let Ok(acroform) = resolved.as_dict() else {
        return false;
    };
    acroform
        .get(b"SigFlags")
        .and_then(|o| o.as_i64())
        .is_ok_and(|v| v != 0)
}

/// Inspecciona un PDF sin modificarlo: páginas, tamaño, firma criptográfica y
/// la estimación conservadora de [`Report::has_scanned_pages`].
///
/// El reporte no trae `output_size` ni `ratio` (no hay salida) ni estadísticas
/// por imagen: para eso está [`compress`](crate::compress).
pub fn analyze(input: &[u8]) -> Result<Report, GemaError> {
    let doc = Document::load_mem(input).map_err(|e| GemaError::Parse(e.to_string()))?;
    if doc.is_encrypted() {
        return Err(GemaError::Encrypted);
    }
    Ok(report_from_doc(&doc, input.len() as u64))
}

/// Construye un `Report` a partir de un documento ya parseado (conteo de
/// páginas, detección de firma, tamaño original). Permite reutilizar un único
/// parseo entre `analyze()` y `compress()` (evita un doble `load_mem`).
pub(crate) fn report_from_doc(doc: &Document, original_size: u64) -> Report {
    let has_scanned_pages = crate::geometry::has_scanned_pages(doc);
    report_from_doc_with_scan(doc, original_size, has_scanned_pages)
}

/// Variante usada por el pipeline cuando ya obtuvo la evidencia de escaneo al
/// calcular DPI, para no decodificar los content streams una segunda vez.
pub(crate) fn report_from_doc_with_scan(
    doc: &Document,
    original_size: u64,
    has_scanned_pages: bool,
) -> Report {
    let pages = doc.get_pages().len();
    let is_signed = detect_signed(doc);

    let mut report = Report {
        pages,
        original_size,
        is_signed,
        has_scanned_pages,
        ..Default::default()
    };
    if is_signed {
        report.warnings.push(crate::report::Warning::SignedDocument);
    }
    report
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
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
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
        assert!(!report.has_scanned_pages);
    }

    #[test]
    fn reports_conservative_scanned_page_evidence() {
        use lopdf::{dictionary, Document, Object, Stream};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 1000, "Height" => 1000,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
            },
            vec![0; 16],
        ));
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            b"q 600 0 0 600 0 0 cm /Scan Do Q".to_vec(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
            "Resources" => dictionary! { "XObject" => dictionary! { "Scan" => image_id } },
            "MediaBox" => vec![0.into(), 0.into(), 600.into(), 600.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        assert!(analyze(&bytes).unwrap().has_scanned_pages);
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
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
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
        assert!(
            !report.is_signed,
            "una clave /Sig suelta no debe marcar firma"
        );
    }

    #[test]
    fn byterange_is_detected_as_signed() {
        use lopdf::dictionary;
        let bytes = pdf_with_page_extras(
            dictionary! { "ByteRange" => vec![0.into(), 100.into(), 200.into(), 50.into()] },
        );
        let report = analyze(&bytes).unwrap();
        assert!(
            report.is_signed,
            "/ByteRange debe marcar el documento como firmado"
        );
    }

    /// PDF mínimo con un objeto suelto extra y/o claves añadidas al catálogo.
    fn pdf_with_loose_object_and_catalog_extras(
        loose: Option<lopdf::Dictionary>,
        catalog_extras: lopdf::Dictionary,
    ) -> Vec<u8> {
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
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let mut catalog = dictionary! { "Type" => "Catalog", "Pages" => pages_id };
        if let Some(d) = loose {
            let loose_id = doc.add_object(d);
            // referencia desde el catálogo para que el objeto no se considere huérfano
            catalog.set("__GemaTestRef", loose_id);
        }
        for (k, v) in catalog_extras.iter() {
            catalog.set(k.clone(), v.clone());
        }
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn doctimestamp_type_is_detected_as_signed() {
        use lopdf::dictionary;
        // un objeto con /Type /DocTimeStamp (sello de tiempo) debe marcar firma
        let bytes = pdf_with_loose_object_and_catalog_extras(
            Some(dictionary! { "Type" => "DocTimeStamp" }),
            dictionary! {},
        );
        let report = analyze(&bytes).unwrap();
        assert!(report.is_signed, "/Type /DocTimeStamp debe marcar firma");
    }

    #[test]
    fn acroform_sigflags_is_detected_as_signed() {
        use lopdf::dictionary;
        // AcroForm inline con /SigFlags 3 (SignaturesExist | AppendOnly)
        let bytes = pdf_with_loose_object_and_catalog_extras(
            None,
            dictionary! { "AcroForm" => dictionary! { "SigFlags" => 3 } },
        );
        let report = analyze(&bytes).unwrap();
        assert!(
            report.is_signed,
            "AcroForm /SigFlags != 0 debe marcar firma"
        );
    }
}
