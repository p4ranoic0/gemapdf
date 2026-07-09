//! Filtros de decodificación de streams: la cadena `/Filter` (`filter_chain`),
//! la resolución de `/DecodeParms` por etapa (`decode_parms_for`) y los
//! decodificadores texto-a-binario (Rust puro, WASM-safe) que des-encadenan un
//! stream hasta los bytes de píxel. La etapa Flate delega el des-filtrado por
//! predictor en [`super::predictor`].

use lopdf::Object;

use super::predictor::apply_predictor;
use super::MAX_DECODE_BYTES;

/// Un filtro reconocido por el des-encadenador. Cualquier filtro fuera de esta
/// lista (LZW, DCT/JPX/CCITT/JBIG2 intermedios, …) hace que la imagen se salte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::image_opt) enum Filter {
    AsciiHex,
    Ascii85,
    Flate,
    RunLength,
}

impl Filter {
    /// Mapea un `/Filter` (nombre completo o abreviatura PDF) a un `Filter`
    /// soportado, o `None` si no lo soportamos (→ SKIP de la imagen).
    fn from_name(name: &[u8]) -> Option<Filter> {
        match name {
            b"ASCIIHexDecode" | b"AHx" => Some(Filter::AsciiHex),
            b"ASCII85Decode" | b"A85" => Some(Filter::Ascii85),
            b"FlateDecode" | b"Fl" => Some(Filter::Flate),
            b"RunLengthDecode" | b"RL" => Some(Filter::RunLength),
            _ => None,
        }
    }

    /// ¿Este filtro es la etapa a la que le corresponde el predictor?
    /// El predictor de los `/DecodeParms` aplica sólo a la etapa Flate (o LZW,
    /// que no soportamos). Los decodificadores texto-a-binario no lo llevan.
    fn takes_predictor(self) -> bool {
        matches!(self, Filter::Flate)
    }
}

/// Lee la lista de filtros del dict como una secuencia de `Filter` soportados.
/// `/Filter` puede ser un Name o un Array de Names. Devuelve `None` si algún
/// filtro no está soportado (→ SKIP) o si no hay filtro.
pub(in crate::image_opt) fn filter_chain(dict: &lopdf::Dictionary) -> Option<Vec<Filter>> {
    match dict.get(b"Filter").ok()? {
        Object::Name(n) => Some(vec![Filter::from_name(n)?]),
        Object::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for obj in arr {
                let n = obj.as_name().ok()?;
                out.push(Filter::from_name(n)?);
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

/// Devuelve el `/DecodeParms` (o `/DP`) que corresponde a la etapa en el índice
/// `idx` de la cadena de filtros. `/DecodeParms` puede ser:
///  - ausente / null → sin parámetros,
///  - un diccionario → aplica a la (única) etapa Flate,
///  - un array paralelo a `/Filter` → la entrada `idx` (puede ser null).
///
/// Devuelve `None` si no hay parámetros para esa etapa.
pub(in crate::image_opt) fn decode_parms_for(
    dict: &lopdf::Dictionary,
    idx: usize,
) -> Option<&lopdf::Dictionary> {
    let parms = dict.get(b"DecodeParms").or_else(|_| dict.get(b"DP")).ok()?;
    match parms {
        Object::Dictionary(d) => Some(d),
        Object::Array(arr) => match arr.get(idx) {
            Some(Object::Dictionary(d)) => Some(d),
            _ => None,
        },
        _ => None,
    }
}

// --------------------------------------------------------------------------
// Decodificadores texto-a-binario (Rust puro, WASM-safe).
// --------------------------------------------------------------------------

/// `ASCIIHexDecode`: pares de dígitos hex → bytes. Ignora blancos. `>` marca el
/// fin (EOD). Un dígito impar final se completa con `0`. Devuelve `None` ante un
/// carácter inválido (no hex, no blanco, no EOD).
pub(super) fn decode_ascii_hex(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 2);
    let mut hi: Option<u8> = None;
    for &b in input {
        if b == b'>' {
            break; // EOD
        }
        if b.is_ascii_whitespace() {
            continue;
        }
        let nyb = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        match hi.take() {
            None => hi = Some(nyb),
            Some(h) => out.push((h << 4) | nyb),
        }
    }
    if let Some(h) = hi {
        // dígito impar → el último byte se completa con un 0 bajo.
        out.push(h << 4);
    }
    Some(out)
}

/// `ASCII85Decode`: grupos de 5 caracteres base-85 → 4 bytes. `z` = 4 ceros.
/// `~>` marca el EOD. Ignora blancos. Devuelve `None` ante entrada inválida.
pub(super) fn decode_ascii85(input: &[u8]) -> Option<Vec<u8>> {
    // Recorta el EOD `~>` si está presente.
    let input = match input.iter().position(|&b| b == b'~') {
        Some(pos) => &input[..pos],
        None => input,
    };
    let mut out = Vec::with_capacity(input.len() * 4 / 5 + 4);
    let mut group = [0u8; 5];
    let mut count = 0usize;
    for &b in input {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'z' {
            if count != 0 {
                return None; // `z` no puede aparecer en mitad de un grupo
            }
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        if !(b'!'..=b'u').contains(&b) {
            return None;
        }
        group[count] = b - b'!';
        count += 1;
        if count == 5 {
            let mut val: u32 = 0;
            for &g in &group {
                val = val.checked_mul(85)?.checked_add(g as u32)?;
            }
            out.extend_from_slice(&val.to_be_bytes());
            count = 0;
        }
    }
    if count > 0 {
        // Grupo final parcial: rellena con la 'u' máxima (84) y emite count-1 bytes.
        if count == 1 {
            return None; // un solo carácter no forma ningún byte → malformado
        }
        for g in group.iter_mut().skip(count) {
            *g = 84;
        }
        let mut val: u32 = 0;
        for &g in &group {
            val = val.checked_mul(85)?.checked_add(g as u32)?;
        }
        let bytes = val.to_be_bytes();
        out.extend_from_slice(&bytes[..count - 1]);
    }
    Some(out)
}

/// `RunLengthDecode`: longitud-de-carrera de PackBits.
///
/// - `0..=127`: copia literal los siguientes `len+1` bytes,
/// - `129..=255`: repite el byte siguiente `257-len` veces,
/// - `128`: EOD.
///
/// Devuelve `None` si el stream se trunca a mitad de una carrera.
pub(super) fn decode_run_length(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 2);
    let mut i = 0usize;
    while i < input.len() {
        let len = input[i];
        i += 1;
        match len {
            128 => break, // EOD
            0..=127 => {
                let n = len as usize + 1;
                let end = i.checked_add(n)?;
                if end > input.len() {
                    return None; // truncado
                }
                out.extend_from_slice(&input[i..end]);
                i = end;
            }
            _ => {
                // 129..=255
                let n = 257 - len as usize;
                if i >= input.len() {
                    return None; // falta el byte a repetir
                }
                let b = input[i];
                i += 1;
                out.extend(std::iter::repeat_n(b, n));
            }
        }
        // Techo de seguridad: RunLength puede expandir; cortamos ante hostilidad.
        if out.len() > MAX_DECODE_BYTES {
            return None;
        }
    }
    Some(out)
}

/// Infla un stream zlib acotando el consumo de memoria a `MAX_DECODE_BYTES + 1`.
/// Devuelve `None` ante zlib corrupto o salida excesiva.
fn inflate_zlib(input: &[u8]) -> Option<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    let mut out = Vec::new();
    let decoder = ZlibDecoder::new(input);
    // Acota la lectura para no reventar memoria ante un "zip bomb".
    let mut limited = decoder.take(MAX_DECODE_BYTES as u64 + 1);
    if limited.read_to_end(&mut out).is_err() {
        return None;
    }
    if out.len() > MAX_DECODE_BYTES {
        return None;
    }
    Some(out)
}

/// Aplica un único filtro de decodificación a `input`. `parms` son los
/// `/DecodeParms` de esta etapa (sólo relevantes para Flate → predictor).
pub(in crate::image_opt) fn apply_filter(
    filter: Filter,
    input: &[u8],
    parms: Option<&lopdf::Dictionary>,
) -> Option<Vec<u8>> {
    let raw = match filter {
        Filter::AsciiHex => decode_ascii_hex(input)?,
        Filter::Ascii85 => decode_ascii85(input)?,
        Filter::RunLength => decode_run_length(input)?,
        Filter::Flate => inflate_zlib(input)?,
    };
    if filter.takes_predictor() {
        apply_predictor(raw, parms)
    } else {
        Some(raw)
    }
}
