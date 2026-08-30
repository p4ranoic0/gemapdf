//! Telemetría estructural experimental para los arneses internos.
//!
//! Este módulo no es superficie estable. Una métrica se promueve al contrato
//! público sólo si Slice E confirma que guía decisiones.

use std::collections::HashSet;

use lopdf::{Document, Object};

use crate::GemaError;

/// Contadores de imágenes inline halladas en content streams de páginas.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InlineImageTelemetry {
    /// Cantidad de secuencias completas `BI`/`ID`/`EI`.
    pub count: u64,
    /// Bytes aproximados entre `ID` y `EI`.
    pub data_bytes: u64,
    /// Cantidad de páginas distintas que contienen al menos una imagen inline.
    pub pages: u64,
}

/// Contadores reservados para imágenes alcanzables únicamente dentro de Forms.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FormImageTelemetry {
    /// Cantidad de imágenes alcanzables únicamente dentro de Forms.
    pub count: u64,
    /// Bytes codificados de esas imágenes.
    pub encoded_bytes: u64,
    /// Profundidad máxima de anidado de Forms observada.
    pub max_depth: u64,
}

/// Contadores reservados para soft masks de luminosidad en ExtGState.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SoftMaskTelemetry {
    /// Cantidad de soft masks de luminosidad.
    pub count: u64,
    /// Cantidad de páginas distintas donde aparecen.
    pub pages: u64,
}

/// Conteos estructurales experimentales de un PDF.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StructuralTelemetry {
    /// Imágenes inline presentes en content streams de páginas.
    pub inline_images: InlineImageTelemetry,
    /// Imágenes alcanzables únicamente dentro de Forms; D1 lo deja en cero.
    pub images_only_in_forms: FormImageTelemetry,
    /// Soft masks de luminosidad en ExtGState; D1 lo deja en cero.
    pub extgstate_soft_masks: SoftMaskTelemetry,
    /// Recursos o streams que el recorrido no pudo resolver o decodificar.
    pub uninspectable_resources: u64,
}

/// Cuenta estructuras que todavía quedan fuera del pipeline de imágenes.
pub fn structural_telemetry(input: &[u8]) -> Result<StructuralTelemetry, GemaError> {
    let doc = Document::load_mem(input).map_err(|error| GemaError::Parse(error.to_string()))?;
    if doc.is_encrypted() {
        return Err(GemaError::Encrypted);
    }

    let mut telemetry = StructuralTelemetry::default();
    for page_id in doc.get_pages().into_values() {
        inspect_page_resources(&doc, page_id, &mut telemetry.uninspectable_resources);

        let mut page_has_inline_image = false;
        for content_id in doc.get_page_contents(page_id) {
            let Ok(stream) = doc.get_object(content_id).and_then(Object::as_stream) else {
                saturating_increment(&mut telemetry.uninspectable_resources);
                continue;
            };
            let Ok(content) = stream.decompressed_content() else {
                saturating_increment(&mut telemetry.uninspectable_resources);
                continue;
            };

            let inline = scan_inline_images(&content);
            telemetry.inline_images.count =
                telemetry.inline_images.count.saturating_add(inline.count);
            telemetry.inline_images.data_bytes = telemetry
                .inline_images
                .data_bytes
                .saturating_add(inline.data_bytes);
            page_has_inline_image |= inline.count != 0;
        }
        if page_has_inline_image {
            saturating_increment(&mut telemetry.inline_images.pages);
        }
    }

    Ok(telemetry)
}

fn inspect_page_resources(doc: &Document, page_id: lopdf::ObjectId, uninspectable: &mut u64) {
    let Ok((inline_resources, resource_ids)) = doc.get_page_resources(page_id) else {
        saturating_increment(uninspectable);
        return;
    };

    if inline_resources.is_none() && resource_ids.is_empty() {
        saturating_increment(uninspectable);
        return;
    }

    let mut visited = HashSet::new();
    if let Some(resources) = inline_resources {
        inspect_resource_object(
            doc,
            &Object::Dictionary(resources.clone()),
            &mut visited,
            uninspectable,
        );
    }
    for resource_id in resource_ids {
        inspect_resource_object(
            doc,
            &Object::Reference(resource_id),
            &mut visited,
            uninspectable,
        );
    }
}

fn inspect_resource_object(
    doc: &Document,
    object: &Object,
    visited: &mut HashSet<lopdf::ObjectId>,
    uninspectable: &mut u64,
) {
    match object {
        Object::Reference(id) => {
            if !visited.insert(*id) {
                return;
            }
            match doc.get_object(*id) {
                Ok(resolved) => inspect_resource_object(doc, resolved, visited, uninspectable),
                Err(_) => saturating_increment(uninspectable),
            }
        }
        Object::Dictionary(dictionary) => {
            for (_, value) in dictionary.iter() {
                inspect_resource_object(doc, value, visited, uninspectable);
            }
        }
        Object::Array(array) => {
            for value in array {
                inspect_resource_object(doc, value, visited, uninspectable);
            }
        }
        Object::Stream(stream) => {
            inspect_resource_object(
                doc,
                &Object::Dictionary(stream.dict.clone()),
                visited,
                uninspectable,
            );
        }
        _ => {}
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct InlineScan {
    count: u64,
    data_bytes: u64,
}

fn scan_inline_images(content: &[u8]) -> InlineScan {
    let mut scan = InlineScan::default();
    let mut cursor = 0;

    while let Some((token_start, token_end)) = next_token(content, cursor) {
        cursor = token_end;
        if &content[token_start..token_end] != b"BI" {
            continue;
        }

        let Some((data_start, after_id)) = find_inline_data_start(content, cursor) else {
            break;
        };
        let Some((data_end, after_ei)) = find_inline_data_end(content, data_start) else {
            break;
        };

        saturating_increment(&mut scan.count);
        scan.data_bytes = scan
            .data_bytes
            .saturating_add(usize_to_u64(data_end.saturating_sub(data_start)));
        cursor = after_ei.max(after_id);
    }

    scan
}

fn find_inline_data_start(content: &[u8], mut cursor: usize) -> Option<(usize, usize)> {
    while let Some((token_start, token_end)) = next_token(content, cursor) {
        cursor = token_end;
        if &content[token_start..token_end] == b"ID" {
            let data_start = consume_inline_separator(content, token_end)?;
            return Some((data_start, token_end));
        }
        if &content[token_start..token_end] == b"BI" {
            return None;
        }
    }
    None
}

fn find_inline_data_end(content: &[u8], data_start: usize) -> Option<(usize, usize)> {
    let mut cursor = data_start;
    while cursor < content.len() {
        if is_pdf_whitespace(content[cursor]) {
            let operator_start = cursor.saturating_add(1);
            let operator_end = operator_start.saturating_add(2);
            if content.get(operator_start..operator_end) == Some(b"EI")
                && content
                    .get(operator_end)
                    .is_none_or(|byte| is_pdf_whitespace(*byte) || is_pdf_delimiter(*byte))
            {
                return Some((cursor, operator_end));
            }
        }
        cursor = cursor.saturating_add(1);
    }
    None
}

fn next_token(content: &[u8], mut cursor: usize) -> Option<(usize, usize)> {
    loop {
        while content
            .get(cursor)
            .is_some_and(|byte| is_pdf_whitespace(*byte))
        {
            cursor = cursor.saturating_add(1);
        }
        if content.get(cursor) == Some(&b'%') {
            cursor = skip_comment(content, cursor);
            continue;
        }
        break;
    }

    let first = *content.get(cursor)?;
    let start = cursor;
    let end = match first {
        b'(' => skip_literal_string(content, cursor),
        b'<' if content.get(cursor.saturating_add(1)) != Some(&b'<') => {
            skip_hex_string(content, cursor)
        }
        b'/' => skip_regular_token(content, cursor.saturating_add(1)),
        byte if is_pdf_delimiter(byte) => cursor.saturating_add(
            if matches!(byte, b'<' | b'>') && content.get(cursor.saturating_add(1)) == Some(&byte) {
                2
            } else {
                1
            },
        ),
        _ => skip_regular_token(content, cursor),
    };
    Some((start, end.min(content.len())))
}

fn skip_comment(content: &[u8], mut cursor: usize) -> usize {
    while content
        .get(cursor)
        .is_some_and(|byte| !matches!(*byte, b'\r' | b'\n'))
    {
        cursor = cursor.saturating_add(1);
    }
    cursor
}

fn skip_literal_string(content: &[u8], mut cursor: usize) -> usize {
    let mut depth = 0_u64;
    while let Some(byte) = content.get(cursor) {
        cursor = cursor.saturating_add(1);
        match byte {
            b'\\' => cursor = cursor.saturating_add(1).min(content.len()),
            b'(' => depth = depth.saturating_add(1),
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    cursor
}

fn skip_hex_string(content: &[u8], mut cursor: usize) -> usize {
    cursor = cursor.saturating_add(1);
    while let Some(byte) = content.get(cursor) {
        cursor = cursor.saturating_add(1);
        if *byte == b'>' {
            break;
        }
    }
    cursor
}

fn skip_regular_token(content: &[u8], mut cursor: usize) -> usize {
    while content
        .get(cursor)
        .is_some_and(|byte| !is_pdf_whitespace(*byte) && !is_pdf_delimiter(*byte))
    {
        cursor = cursor.saturating_add(1);
    }
    cursor
}

fn consume_inline_separator(content: &[u8], cursor: usize) -> Option<usize> {
    match content.get(cursor) {
        Some(b'\r') if content.get(cursor.saturating_add(1)) == Some(&b'\n') => {
            Some(cursor.saturating_add(2))
        }
        Some(byte) if is_pdf_whitespace(*byte) => Some(cursor.saturating_add(1)),
        _ => None,
    }
}

fn is_pdf_whitespace(byte: u8) -> bool {
    matches!(byte, 0x00 | b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

fn is_pdf_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn saturating_increment(value: &mut u64) {
    *value = value.saturating_add(1);
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use lopdf::{dictionary, Document, Object, ObjectId, Stream};

    use super::*;

    fn pdf_with_pages(contents: &[&[u8]], resources: Object) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut page_ids = Vec::new();

        for content in contents {
            let content_id = doc.add_object(Stream::new(dictionary! {}, content.to_vec()));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => resources.clone(),
                "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            });
            page_ids.push(page_id.into());
        }

        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids,
                "Count" => usize_to_i64(contents.len()),
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    fn usize_to_i64(value: usize) -> i64 {
        i64::try_from(value).unwrap_or(i64::MAX)
    }

    #[test]
    fn counts_inline_images_bytes_and_distinct_pages() {
        let page_one =
            b"q BI /W 1 /H 1 /BPC 8 /CS /RGB ID abc EI Q\nBI /W 1 /H 1 /BPC 8 /CS /G ID z EI";
        let page_two = b"BI /W 1 /H 1 /BPC 8 /CS /RGB ID 12345 EI";
        let pdf = pdf_with_pages(&[page_one, page_two], Object::Dictionary(dictionary! {}));

        let telemetry = structural_telemetry(&pdf).unwrap();

        assert_eq!(telemetry.inline_images.count, 3);
        assert_eq!(telemetry.inline_images.data_bytes, 9);
        assert_eq!(telemetry.inline_images.pages, 2);
        assert_eq!(telemetry.uninspectable_resources, 0);
    }

    #[test]
    fn counts_an_unresolvable_resource() {
        let missing_id: ObjectId = (999, 0);
        let resources = Object::Dictionary(dictionary! {
            "XObject" => dictionary! { "Missing" => missing_id },
        });
        let pdf = pdf_with_pages(&[b"q Q"], resources);

        let telemetry = structural_telemetry(&pdf).unwrap();

        assert_eq!(telemetry.uninspectable_resources, 1);
    }

    #[test]
    fn reports_zero_for_an_inspectable_pdf_without_structural_gaps() {
        let pdf = pdf_with_pages(&[b"BT ET"], Object::Dictionary(dictionary! {}));

        assert_eq!(
            structural_telemetry(&pdf).unwrap(),
            StructuralTelemetry::default()
        );
    }

    #[test]
    fn truncated_inline_image_does_not_panic() {
        let pdf = pdf_with_pages(
            &[b"q BI /W 100000000000000000000 /H 2 /BPC 8 /CS /RGB ID abc"],
            Object::Dictionary(dictionary! {}),
        );

        let telemetry = structural_telemetry(&pdf).unwrap();

        assert_eq!(telemetry.inline_images.count, 0);
        assert_eq!(telemetry.inline_images.pages, 0);
    }
}
