//! Lectura acotada de streams.
//!
//! `lopdf::Stream::decompressed_content` descomprime sin tope y
//! `Document::get_page_content` devuelve bytes crudos en silencio cuando la
//! descompresión falla. Acá se lee el stream crudo, se acepta **sólo**
//! `FlateDecode` sin predictor (o sin filtro) y se infla por bloques,
//! rechazando antes de reservar un byte por encima del límite. Cualquier otro
//! filtro es `UnsupportedFilter`: no se reimplementa, se informa.
//!
//! Compatibilidad con `lopdf::decompress_zlib`: si zlib falla, se reintenta
//! deflate crudo sobre `raw[2..]` (checksum Adler32 roto, cabecera rota), con
//! el mismo techo. A diferencia de `lopdf`, **ninguna salida parcial se
//! acepta**: sólo cuenta un stream que llega a su bloque final.

use flate2::{Decompress, FlushDecompress, Status};
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::options::{BudgetMeter, EditOptions};
use crate::LimitKind;

/// Por qué no se pudo leer un stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamReadError {
    /// El `ObjectId` no resuelve.
    Missing,
    /// Resuelve, pero no es un stream.
    NotAStream,
    /// Filtro (o cadena de filtros, separada por coma) que no se soporta.
    UnsupportedFilter(String),
    /// Ni zlib ni deflate crudo llegan al final del stream, o el diccionario
    /// está malformado.
    Corrupt,
    /// La salida de **este** stream superaría el `max` recibido.
    TooLarge,
    /// Se agotó un límite acumulado: presupuesto de streams, bytes de la
    /// página (`DecompressedBytes`) o bytes de la llamada
    /// (`TotalDecompressedBytes`).
    Budget(LimitKind),
}

const CHUNK: usize = 16 * 1024;

/// Infla con `Decompress` de bajo nivel para saber si se llegó a
/// `StreamEnd`: `flate2::read::ZlibDecoder` no distingue un stream truncado
/// de uno completo. La salida nunca crece por encima de `max`: la suma se
/// hace con `checked_add` y se compara antes de `extend`.
fn inflate_with(raw: &[u8], zlib_header: bool, max: usize) -> Result<Vec<u8>, StreamReadError> {
    let mut d = Decompress::new(zlib_header);
    let mut out = Vec::new();
    let mut chunk = [0u8; CHUNK];
    loop {
        let consumed = usize::try_from(d.total_in()).map_err(|_| StreamReadError::Corrupt)?;
        let before_out = d.total_out();
        let status = d
            // `None`, NO `Finish`: con `Finish`, miniz_oxide devuelve error en
            // la segunda vuelta cuando la salida no cupo en un bloque (medido
            // el 2026-09-16: todo stream de más de 16 KB salía `Corrupt`).
            .decompress(&raw[consumed..], &mut chunk, FlushDecompress::None)
            .map_err(|_| StreamReadError::Corrupt)?;
        let produced =
            usize::try_from(d.total_out() - before_out).map_err(|_| StreamReadError::TooLarge)?;
        let len = out
            .len()
            .checked_add(produced)
            .ok_or(StreamReadError::TooLarge)?;
        if len > max {
            return Err(StreamReadError::TooLarge);
        }
        out.extend_from_slice(&chunk[..produced]);
        match status {
            Status::StreamEnd => return Ok(out),
            // Sin avance en ninguno de los dos lados: la entrada se acabó
            // antes del bloque final. Truncado, no completo.
            _ if produced == 0 && usize::try_from(d.total_in()).ok() == Some(consumed) => {
                return Err(StreamReadError::Corrupt);
            }
            _ => {}
        }
    }
}

/// Infla `raw` sin que la salida supere `max`. zlib primero; si falla por
/// `Corrupt` y hay más de dos bytes, deflate crudo sobre `raw[2..]` con el
/// mismo techo. `TooLarge` no reintenta. Entrada vacía → salida vacía, como
/// `lopdf`.
pub(crate) fn inflate_bounded(raw: &[u8], max: usize) -> Result<Vec<u8>, StreamReadError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    match inflate_with(raw, true, max) {
        Err(StreamReadError::Corrupt) if raw.len() > 2 => inflate_with(&raw[2..], false, max),
        other => other,
    }
}

fn filter_names(
    doc: &Document,
    dict: &Dictionary,
    meter: &mut BudgetMeter,
) -> Result<Vec<String>, StreamReadError> {
    let name = |o: &Object| match o {
        Object::Name(n) => Ok(String::from_utf8_lossy(n).into_owned()),
        _ => Err(StreamReadError::Corrupt),
    };
    let resolved_name = |o: &Object| -> Result<String, StreamReadError> {
        match o {
            Object::Reference(id) => {
                meter.touch_object().map_err(StreamReadError::Budget)?;
                name(doc.get_object(*id).map_err(|_| StreamReadError::Corrupt)?)
            }
            other => name(other),
        }
    };
    match dict.get(b"Filter") {
        Err(_) => Ok(Vec::new()),
        Ok(Object::Array(items)) => items.iter().map(resolved_name).collect(),
        Ok(Object::Reference(id)) => {
            meter.touch_object().map_err(StreamReadError::Budget)?;
            let resolved = doc.get_object(*id).map_err(|_| StreamReadError::Corrupt)?;
            Ok(vec![name(resolved)?])
        }
        Ok(other) => Ok(vec![name(other)?]),
    }
}

/// `true` si `/DecodeParms` declara un predictor (o no se puede saber).
fn has_predictor(
    doc: &Document,
    dict: &Dictionary,
    meter: &mut BudgetMeter,
) -> Result<bool, StreamReadError> {
    let parms = match dict.get(b"DecodeParms") {
        Err(_) => return Ok(false),
        Ok(Object::Dictionary(d)) => d.clone(),
        Ok(Object::Array(items)) => match items.first() {
            Some(Object::Dictionary(d)) => d.clone(),
            Some(Object::Null) | None => return Ok(false),
            Some(Object::Reference(id)) => {
                meter.touch_object().map_err(StreamReadError::Budget)?;
                match doc.get_object(*id).map_err(|_| StreamReadError::Corrupt)? {
                    Object::Dictionary(d) => d.clone(),
                    _ => return Ok(true),
                }
            }
            _ => return Ok(true),
        },
        Ok(Object::Reference(id)) => {
            meter.touch_object().map_err(StreamReadError::Budget)?;
            match doc.get_object(*id).map_err(|_| StreamReadError::Corrupt)? {
                Object::Dictionary(d) => d.clone(),
                _ => return Ok(true),
            }
        }
        Ok(Object::Null) => return Ok(false),
        Ok(_) => return Ok(true),
    };
    match parms.get(b"Predictor") {
        Ok(Object::Integer(p)) => Ok(*p > 1),
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

/// Lee el stream `id` descomprimido, sin superar `max` bytes de salida.
pub(crate) fn read_stream_bounded(
    doc: &Document,
    id: ObjectId,
    max: usize,
    meter: &mut BudgetMeter,
) -> Result<Vec<u8>, StreamReadError> {
    meter.open_stream().map_err(StreamReadError::Budget)?;
    let stream = match doc.get_object(id) {
        Ok(Object::Stream(s)) => s,
        Ok(_) => return Err(StreamReadError::NotAStream),
        Err(_) => return Err(StreamReadError::Missing),
    };
    let filters = filter_names(doc, &stream.dict, meter)?;
    match filters.as_slice() {
        [] => {
            if stream.content.len() > max {
                Err(StreamReadError::TooLarge)
            } else {
                Ok(stream.content.clone())
            }
        }
        [only] if only == "FlateDecode" => {
            if has_predictor(doc, &stream.dict, meter)? {
                return Err(StreamReadError::UnsupportedFilter(
                    "FlateDecode+Predictor".into(),
                ));
            }
            inflate_bounded(&stream.content, max)
        }
        other => Err(StreamReadError::UnsupportedFilter(other.join(","))),
    }
}

/// Concatena los content streams de la página, separados por `\n` como manda
/// la especificación. El techo es **acumulado**: la página entera no supera
/// `max_decompressed_bytes`, y la llamada entera no supera
/// `ObjectBudget::max_total_decompressed_bytes`. Cada stream se infla con el
/// menor de los dos márgenes restantes, así que nunca se reserva por encima.
pub(crate) fn read_page_content_bounded(
    doc: &Document,
    page_id: ObjectId,
    opts: &EditOptions,
    meter: &mut BudgetMeter,
) -> Result<Vec<u8>, StreamReadError> {
    let mut out = Vec::new();
    for id in doc.get_page_contents(page_id) {
        if !out.is_empty() {
            out.push(b'\n');
        }
        let page_left = opts.max_decompressed_bytes.saturating_sub(out.len());
        let call_left = meter.remaining_bytes().saturating_sub(out.len());
        let (cap, kind) = if call_left < page_left {
            (call_left, LimitKind::TotalDecompressedBytes)
        } else {
            (page_left, LimitKind::DecompressedBytes)
        };
        let part = match read_stream_bounded(doc, id, cap, meter) {
            Err(StreamReadError::TooLarge) => return Err(StreamReadError::Budget(kind)),
            other => other?,
        };
        out.extend_from_slice(&part);
        if out.len() > opts.max_decompressed_bytes {
            return Err(StreamReadError::Budget(LimitKind::DecompressedBytes));
        }
    }
    meter
        .charge_bytes(out.len())
        .map_err(StreamReadError::Budget)?;
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{BudgetMeter, ObjectBudget};
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use lopdf::{dictionary, Document, Object, Stream};
    use std::io::Write;

    fn deflate(raw: &[u8]) -> Vec<u8> {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(raw).unwrap();
        enc.finish().unwrap()
    }

    fn doc_with_stream(dict: lopdf::Dictionary, content: Vec<u8>) -> (Document, lopdf::ObjectId) {
        let mut doc = Document::with_version("1.5");
        let id = doc.add_object(Stream::new(dict, content));
        (doc, id)
    }

    fn meter() -> BudgetMeter {
        BudgetMeter::new(&ObjectBudget::default())
    }

    #[test]
    fn inflate_stops_before_allocating_past_the_limit() {
        let raw = vec![0u8; 1024 * 1024];
        let packed = deflate(&raw);
        assert!(packed.len() < 4096, "el fixture debe ser chico comprimido");
        assert_eq!(
            inflate_bounded(&packed, 4096).unwrap_err(),
            StreamReadError::TooLarge
        );
        assert_eq!(
            inflate_bounded(&packed, raw.len()).unwrap().len(),
            raw.len()
        );
        // El borde exacto: raw.len() cabe, raw.len() - 1 no.
        assert_eq!(
            inflate_bounded(&packed, raw.len() - 1).unwrap_err(),
            StreamReadError::TooLarge
        );
    }

    #[test]
    fn corrupt_zlib_is_reported_not_passed_through() {
        // lopdf devolvería los bytes crudos en silencio; nosotros no.
        assert_eq!(
            inflate_bounded(b"esto no es zlib", 1 << 20).unwrap_err(),
            StreamReadError::Corrupt
        );
    }

    #[test]
    fn broken_adler32_still_reads_through_the_raw_deflate_fallback() {
        // Compatibilidad con lopdf::decompress_zlib: datos completos con el
        // checksum roto (típico de PDFs cifrados mal escritos) se editan hoy.
        let text = b"BT /F1 12 Tf 10 10 Td (hola) Tj ET".repeat(50);
        let mut packed = deflate(&text);
        let last = packed.len() - 1;
        packed[last] ^= 0xFF;
        // El fixture tiene que ejercitar el fallback de verdad: zlib solo falla.
        assert_eq!(
            inflate_with(&packed, true, 1 << 20).unwrap_err(),
            StreamReadError::Corrupt
        );
        assert_eq!(inflate_bounded(&packed, 1 << 20).unwrap(), text);
        // El fallback respeta el mismo techo.
        assert_eq!(
            inflate_bounded(&packed, 10).unwrap_err(),
            StreamReadError::TooLarge
        );
    }

    #[test]
    fn large_streams_are_read_across_many_chunks() {
        // Regresión medida: con FlushDecompress::Finish esto daba Corrupt.
        let big: Vec<u8> = (0..3_000_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        let packed = deflate(&big);
        assert_eq!(inflate_bounded(&packed, big.len()).unwrap(), big);
        assert_eq!(
            inflate_bounded(&packed, big.len() - 1).unwrap_err(),
            StreamReadError::TooLarge
        );
        let mut bad_adler = packed.clone();
        let last = bad_adler.len() - 1;
        bad_adler[last] ^= 1;
        assert_eq!(inflate_bounded(&bad_adler, big.len()).unwrap(), big);
        // Sin los 4 bytes de Adler32 pero con el deflate completo: también se lee.
        assert_eq!(
            inflate_bounded(&packed[..packed.len() - 4], big.len()).unwrap(),
            big
        );
    }

    #[test]
    fn broken_zlib_header_still_reads_through_the_raw_deflate_fallback() {
        let mut packed = deflate(b"BT ET");
        packed[0] = 0x00;
        packed[1] = 0x00;
        assert_eq!(inflate_bounded(&packed, 1 << 20).unwrap(), b"BT ET");
    }

    #[test]
    fn truncated_stream_is_corrupt_not_a_silent_prefix() {
        // lopdf entrega el prefijo; reescribirlo borraría el resto de la página.
        let text: Vec<u8> = (0..20_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let packed = deflate(&text);
        let cut = &packed[..packed.len() / 2];
        assert_eq!(
            inflate_bounded(cut, 1 << 20).unwrap_err(),
            StreamReadError::Corrupt
        );
    }

    #[test]
    fn empty_flate_stream_reads_as_empty_like_lopdf() {
        // lopdf::decompress_zlib devuelve vacío sin error para entrada vacía;
        // hay páginas en blanco con un /FlateDecode de longitud 0.
        assert_eq!(inflate_bounded(b"", 1 << 20).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn flate_stream_is_read() {
        let (doc, id) =
            doc_with_stream(dictionary! { "Filter" => "FlateDecode" }, deflate(b"BT ET"));
        assert_eq!(
            read_stream_bounded(&doc, id, 1 << 20, &mut meter()).unwrap(),
            b"BT ET"
        );
    }

    #[test]
    fn raw_stream_is_read_and_bounded() {
        let (doc, id) = doc_with_stream(dictionary! {}, b"BT ET".to_vec());
        assert_eq!(
            read_stream_bounded(&doc, id, 1 << 20, &mut meter()).unwrap(),
            b"BT ET"
        );
        assert_eq!(
            read_stream_bounded(&doc, id, 2, &mut meter()).unwrap_err(),
            StreamReadError::TooLarge
        );
    }

    #[test]
    fn any_other_filter_is_unsupported_not_reimplemented() {
        let (doc, id) = doc_with_stream(dictionary! { "Filter" => "LZWDecode" }, vec![0, 1, 2]);
        assert_eq!(
            read_stream_bounded(&doc, id, 1 << 20, &mut meter()).unwrap_err(),
            StreamReadError::UnsupportedFilter("LZWDecode".into())
        );
        let chained = dictionary! {
            "Filter" => vec![Object::Name(b"ASCII85Decode".to_vec()), Object::Name(b"FlateDecode".to_vec())]
        };
        let (doc, id) = doc_with_stream(chained, vec![0, 1, 2]);
        assert_eq!(
            read_stream_bounded(&doc, id, 1 << 20, &mut meter()).unwrap_err(),
            StreamReadError::UnsupportedFilter("ASCII85Decode,FlateDecode".into())
        );
    }

    #[test]
    fn flate_with_predictor_is_unsupported() {
        let dict = dictionary! {
            "Filter" => "FlateDecode",
            "DecodeParms" => dictionary! { "Predictor" => 12, "Columns" => 4 }
        };
        let (doc, id) = doc_with_stream(dict, deflate(b"xxxx"));
        assert_eq!(
            read_stream_bounded(&doc, id, 1 << 20, &mut meter()).unwrap_err(),
            StreamReadError::UnsupportedFilter("FlateDecode+Predictor".into())
        );
    }

    #[test]
    fn indirect_flate_filter_is_resolved() {
        let mut doc = Document::with_version("1.5");
        let filter = doc.add_object(Object::Name(b"FlateDecode".to_vec()));
        let id = doc.add_object(Stream::new(
            dictionary! { "Filter" => Object::Reference(filter) },
            deflate(b"BT ET"),
        ));
        assert_eq!(
            read_stream_bounded(&doc, id, 1 << 20, &mut meter()).unwrap(),
            b"BT ET"
        );
    }

    #[test]
    fn indirect_decode_params_without_predictor_are_accepted() {
        let mut doc = Document::with_version("1.5");
        let parms = doc.add_object(dictionary! {});
        let id = doc.add_object(Stream::new(
            dictionary! {
                "Filter" => "FlateDecode", "DecodeParms" => Object::Reference(parms)
            },
            deflate(b"BT ET"),
        ));
        assert_eq!(
            read_stream_bounded(&doc, id, 1 << 20, &mut meter()).unwrap(),
            b"BT ET"
        );
    }

    #[test]
    fn indirect_decode_params_with_predictor_are_unsupported() {
        let mut doc = Document::with_version("1.5");
        let parms = doc.add_object(dictionary! { "Predictor" => 12 });
        let id = doc.add_object(Stream::new(
            dictionary! {
                "Filter" => "FlateDecode", "DecodeParms" => Object::Reference(parms)
            },
            deflate(b"BT ET"),
        ));
        assert!(matches!(
            read_stream_bounded(&doc, id, 1 << 20, &mut meter()),
            Err(StreamReadError::UnsupportedFilter(_))
        ));
    }

    #[test]
    fn broken_indirect_filter_is_corrupt() {
        let (doc, id) = doc_with_stream(
            dictionary! { "Filter" => Object::Reference((999, 0)) },
            vec![0, 1, 2],
        );
        assert_eq!(
            read_stream_bounded(&doc, id, 1 << 20, &mut meter()).unwrap_err(),
            StreamReadError::Corrupt
        );
    }

    #[test]
    fn missing_or_non_stream_objects_are_distinguished() {
        let mut doc = Document::with_version("1.5");
        let dict_id = doc.add_object(dictionary! { "A" => 1 });
        assert_eq!(
            read_stream_bounded(&doc, dict_id, 1 << 20, &mut meter()).unwrap_err(),
            StreamReadError::NotAStream
        );
        assert_eq!(
            read_stream_bounded(&doc, (999, 0), 1 << 20, &mut meter()).unwrap_err(),
            StreamReadError::Missing
        );
    }

    #[test]
    fn stream_budget_is_charged_per_open() {
        let (doc, id) = doc_with_stream(dictionary! {}, b"x".to_vec());
        let budget = ObjectBudget {
            max_streams: 1,
            ..ObjectBudget::default()
        };
        let mut m = BudgetMeter::new(&budget);
        assert!(read_stream_bounded(&doc, id, 1 << 20, &mut m).is_ok());
        assert_eq!(
            read_stream_bounded(&doc, id, 1 << 20, &mut m).unwrap_err(),
            StreamReadError::Budget(crate::LimitKind::Streams)
        );
    }

    /// Página con `/Contents [a b c]`, cada stream de `each` bytes crudos.
    fn page_with_streams(n: usize, each: usize) -> (Document, lopdf::ObjectId) {
        let mut doc = Document::with_version("1.5");
        let ids: Vec<Object> = (0..n)
            .map(|_| {
                Object::Reference(doc.add_object(Stream::new(dictionary! {}, vec![b' '; each])))
            })
            .collect();
        let page = doc.add_object(dictionary! { "Type" => "Page", "Contents" => ids });
        (doc, page)
    }

    #[test]
    fn page_limit_is_cumulative_not_per_stream() {
        // Tres streams de 40 bytes: cada uno cabe en 100, la página (122 con
        // los dos separadores) no.
        let (doc, page) = page_with_streams(3, 40);
        let opts = EditOptions {
            max_decompressed_bytes: 100,
            ..EditOptions::default()
        };
        assert_eq!(
            read_page_content_bounded(&doc, page, &opts, &mut meter()).unwrap_err(),
            StreamReadError::Budget(crate::LimitKind::DecompressedBytes)
        );
        let roomy = EditOptions {
            max_decompressed_bytes: 122,
            ..EditOptions::default()
        };
        assert_eq!(
            read_page_content_bounded(&doc, page, &roomy, &mut meter())
                .unwrap()
                .len(),
            122
        );
    }

    #[test]
    fn call_limit_accumulates_across_pages() {
        let (doc, page) = page_with_streams(1, 60);
        let budget = ObjectBudget {
            max_total_decompressed_bytes: 100,
            ..ObjectBudget::default()
        };
        let mut m = BudgetMeter::new(&budget);
        let opts = EditOptions::default();
        assert!(read_page_content_bounded(&doc, page, &opts, &mut m).is_ok());
        assert_eq!(
            read_page_content_bounded(&doc, page, &opts, &mut m).unwrap_err(),
            StreamReadError::Budget(crate::LimitKind::TotalDecompressedBytes)
        );
    }
}
