//! Decodificación de imágenes FlateDecode a píxeles crudos (P1-Flate + P1b, v2.0).
//!
//! Muchas imágenes de escaneos y de generadores PDF no vienen en DCTDecode
//! (JPEG) sino en FlateDecode: el stream es zlib sobre los bytes de píxel
//! crudos. `image::load_from_memory` no abre ese blob, así que hasta v1 estas
//! imágenes se saltaban. Este módulo las descomprime y las interpreta según su
//! `ColorSpace`/`BitsPerComponent`, produciendo un `DynamicImage` que el resto
//! del pipeline puede downsamplear y recomprimir.
//!
//! **P1b amplía la cobertura a dos casos antes saltados, sin perder exactitud:**
//!
//! - **Cadenas de filtros** (`/Filter` como array, p. ej.
//!   `[ASCII85Decode FlateDecode]`): des-encadenamos aplicando cada filtro de
//!   izquierda a derecha. Soportamos `ASCIIHexDecode`, `ASCII85Decode`,
//!   `FlateDecode` y `RunLengthDecode` (todos Rust puro, WASM-safe). Cualquier
//!   otro filtro en la cadena (LZW, DCT/JPX/CCITT/JBIG2 intermedios) → SALTAMOS.
//! - **Predictores PNG/TIFF** (`/Predictor` en los `/DecodeParms` de la etapa
//!   Flate): tras inflar aplicamos el des-filtrado exacto — TIFF (predictor 2,
//!   sólo 8-bpc) y PNG (10–15: None/Sub/Up/Average/Paeth). Un des-filtrado
//!   equivocado garabatearía píxeles, así que validamos longitudes y saltamos
//!   ante cualquier caso fuera de alcance en vez de adivinar.
//!
//! **Alcance deliberadamente acotado — corrección antes que cobertura.** Sólo
//! decodificamos los casos que podemos reconstruir *exactamente*; cualquier
//! otra cosa devuelve `None` para que el pipeline la marque `Skipped` (seguro,
//! sin corromper):
//!
//! - **ColorSpace/BitsPerComponent soportados:** sólo `DeviceRGB`/8 →
//!   `RgbImage` y `DeviceGray`/8 → `GrayImage`. Indexed, ICCBased, CMYK,
//!   1/2/4-bpc y colorspaces por array/indirectos → SALTAMOS.
//! - **Validación de longitud:** tras des-filtrar, la longitud debe ser
//!   exactamente `W*H*canales`. Si no coincide (stream malformado o
//!   interpretación errónea) SALTAMOS en vez de adivinar.
//!
//! Nunca hace panic ante entrada hostil.

use lopdf::{Object, Stream};

/// Colorspaces soportados por el decodificador Flate de v2.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlateColor {
    /// `DeviceRGB`, 3 canales.
    Rgb,
    /// `DeviceGray`, 1 canal.
    Gray,
}

impl FlateColor {
    fn channels(self) -> usize {
        match self {
            FlateColor::Rgb => 3,
            FlateColor::Gray => 1,
        }
    }
}

/// Techo de bytes descomprimidos permitidos (~805 MB para RGB 16 384×16 384).
/// Evita que un PDF malicioso con dimensiones enormes (p. ej. 100 000×100 000)
/// provoque una pre-reserva de ~30 GB antes de que la validación de longitud
/// pueda descartarlo.
const MAX_DECODE_BYTES: usize = 16_384 * 16_384 * 3; // ≈ 805 MB

/// Un filtro reconocido por el des-encadenador. Cualquier filtro fuera de esta
/// lista (LZW, DCT/JPX/CCITT/JBIG2 intermedios, …) hace que la imagen se salte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
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
fn filter_chain(dict: &lopdf::Dictionary) -> Option<Vec<Filter>> {
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
fn decode_parms_for(dict: &lopdf::Dictionary, idx: usize) -> Option<&lopdf::Dictionary> {
    let parms = dict
        .get(b"DecodeParms")
        .or_else(|_| dict.get(b"DP"))
        .ok()?;
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
fn decode_ascii_hex(input: &[u8]) -> Option<Vec<u8>> {
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
fn decode_ascii85(input: &[u8]) -> Option<Vec<u8>> {
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
fn decode_run_length(input: &[u8]) -> Option<Vec<u8>> {
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
fn apply_filter(filter: Filter, input: &[u8], parms: Option<&lopdf::Dictionary>) -> Option<Vec<u8>> {
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

// --------------------------------------------------------------------------
// Des-filtrado por predictor (PNG 10–15, TIFF 2). Se aplica tras inflar.
// --------------------------------------------------------------------------

/// Parámetros de predictor leídos de `/DecodeParms`.
struct PredictorParams {
    predictor: i64,
    colors: usize,
    bpc: usize,
    columns: usize,
}

fn read_predictor_params(parms: Option<&lopdf::Dictionary>) -> PredictorParams {
    let get = |key: &[u8], default: i64| -> i64 {
        parms
            .and_then(|p| p.get(key).ok())
            .and_then(|o| o.as_i64().ok())
            .unwrap_or(default)
    };
    PredictorParams {
        predictor: get(b"Predictor", 1),
        colors: get(b"Colors", 1).max(1) as usize,
        bpc: get(b"BitsPerComponent", 8).max(1) as usize,
        columns: get(b"Columns", 1).max(1) as usize,
    }
}

/// Aplica el des-filtrado de predictor a los bytes ya inflados, o los devuelve
/// tal cual si `Predictor` == 1 (o ausente). Devuelve `None` si el caso está
/// fuera de alcance (p. ej. TIFF con bpc != 8) o si la longitud no cuadra
/// (nunca adivina: mejor SKIP que píxeles garabateados).
fn apply_predictor(data: Vec<u8>, parms: Option<&lopdf::Dictionary>) -> Option<Vec<u8>> {
    let p = read_predictor_params(parms);
    match p.predictor {
        1 => Some(data), // sin predicción
        2 => tiff_predictor2(data, &p),
        10..=15 => png_predictor(data, &p),
        _ => None, // predictor desconocido → SKIP
    }
}

/// Bytes por píxel: `ceil(colors * bpc / 8)`, mínimo 1.
fn bytes_per_pixel(colors: usize, bpc: usize) -> usize {
    (colors * bpc).div_ceil(8).max(1)
}

/// Longitud de fila en bytes: `ceil(colors * bpc * columns / 8)`.
fn row_len(colors: usize, bpc: usize, columns: usize) -> usize {
    (colors * bpc * columns).div_ceil(8)
}

/// Predictor 2 de TIFF: diferenciación horizontal. Sólo soportamos 8 bpc
/// (cada muestra es un byte); otros bpc → SKIP. Cada muestra se reconstruye
/// sumándole la muestra `bpp` posiciones a la izquierda (misma componente).
fn tiff_predictor2(mut data: Vec<u8>, p: &PredictorParams) -> Option<Vec<u8>> {
    if p.bpc != 8 {
        return None; // sólo TIFF 8-bpc en alcance
    }
    let rl = row_len(p.colors, p.bpc, p.columns);
    if rl == 0 || !data.len().is_multiple_of(rl) {
        return None; // longitud no cuadra
    }
    let bpp = bytes_per_pixel(p.colors, p.bpc); // = colors para 8 bpc
    let rows = data.len() / rl;
    for r in 0..rows {
        let base = r * rl;
        for i in bpp..rl {
            data[base + i] = data[base + i].wrapping_add(data[base + i - bpp]);
        }
    }
    Some(data)
}

/// Predictor de PNG (10–15): los datos son filas de `1 + row_len` bytes, con un
/// byte inicial por fila que indica el tipo de filtro (0 None, 1 Sub, 2 Up,
/// 3 Average, 4 Paeth). Reconstruye cada fila con el algoritmo estándar de PNG
/// (la fila previa arranca en ceros). Devuelve `None` si la longitud no cuadra
/// o si aparece un tipo de filtro inválido.
fn png_predictor(data: Vec<u8>, p: &PredictorParams) -> Option<Vec<u8>> {
    let rl = row_len(p.colors, p.bpc, p.columns);
    if rl == 0 {
        return None;
    }
    let stride = rl + 1; // byte de tipo + fila
    if !data.len().is_multiple_of(stride) {
        return None; // longitud no cuadra
    }
    let bpp = bytes_per_pixel(p.colors, p.bpc);
    let rows = data.len() / stride;
    let mut out = Vec::with_capacity(rows * rl);
    let mut prev = vec![0u8; rl];
    let mut cur = vec![0u8; rl];
    for r in 0..rows {
        let base = r * stride;
        let ftype = data[base];
        cur.copy_from_slice(&data[base + 1..base + 1 + rl]);
        unfilter_png_row(ftype, bpp, &prev, &mut cur)?;
        out.extend_from_slice(&cur);
        std::mem::swap(&mut prev, &mut cur);
    }
    Some(out)
}

/// Predictor de Paeth (a=izquierda, b=arriba, c=arriba-izquierda). El clásico
/// del estándar PNG: elige el vecino más cercano a `a + b - c`.
#[inline]
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i32 + b as i32 - c as i32;
    let pa = (p - a as i32).abs();
    let pb = (p - b as i32).abs();
    let pc = (p - c as i32).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Reconstruye una fila PNG in-place a partir de su tipo de filtro y la fila
/// previa (ya reconstruida). `bpp` = bytes por píxel. Devuelve `None` si el
/// tipo de filtro es inválido.
fn unfilter_png_row(ftype: u8, bpp: usize, prev: &[u8], cur: &mut [u8]) -> Option<()> {
    let len = cur.len();
    match ftype {
        0 => {} // None
        1 => {
            // Sub: cur[i] += cur[i-bpp]
            for i in bpp..len {
                cur[i] = cur[i].wrapping_add(cur[i - bpp]);
            }
        }
        2 => {
            // Up: cur[i] += prev[i]
            for i in 0..len {
                cur[i] = cur[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            // Average: cur[i] += floor((left + up) / 2)
            for i in 0..len {
                let left = if i >= bpp { cur[i - bpp] as u16 } else { 0 };
                let up = prev[i] as u16;
                cur[i] = cur[i].wrapping_add(((left + up) / 2) as u8);
            }
        }
        4 => {
            // Paeth
            for i in 0..len {
                let a = if i >= bpp { cur[i - bpp] } else { 0 };
                let b = prev[i];
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                cur[i] = cur[i].wrapping_add(paeth(a, b, c));
            }
        }
        _ => return None, // tipo de filtro inválido
    }
    Some(())
}

// --------------------------------------------------------------------------
// Interpretación de colorspace.
// --------------------------------------------------------------------------

/// Lee el colorspace soportado del dict, o `None` si no es uno de los dos que
/// manejamos (RGB/8, Gray/8). Un colorspace por array o referencia indirecta
/// (Indexed, ICCBased, …) devuelve `None` → SKIP.
fn supported_color(dict: &lopdf::Dictionary) -> Option<FlateColor> {
    // BitsPerComponent debe ser exactamente 8.
    let bpc = dict.get(b"BitsPerComponent").and_then(|o| o.as_i64()).ok()?;
    if bpc != 8 {
        return None;
    }
    // ColorSpace debe ser un Name simple; array/ref (Indexed, ICCBased, …) no.
    let cs = dict.get(b"ColorSpace").ok()?;
    match cs {
        Object::Name(n) if n == b"DeviceRGB" || n == b"RGB" => Some(FlateColor::Rgb),
        Object::Name(n) if n == b"DeviceGray" || n == b"G" => Some(FlateColor::Gray),
        _ => None,
    }
}

/// Descomprime un stream de imagen a un `DynamicImage`, o devuelve `None` si no
/// es un caso soportado (filtro no soportado en la cadena, colorspace/bpc no
/// soportado, predictor fuera de alcance, o longitud que no cuadra). Nunca hace
/// panic.
///
/// `width`/`height` son las dimensiones ya validadas (>0, sanas) que el
/// pipeline leyó del dict.
pub(crate) fn decode_flate_image(
    stream: &Stream,
    width: u32,
    height: u32,
) -> Option<image::DynamicImage> {
    let dict = &stream.dict;

    // La cadena de filtros debe existir y ser toda soportada (si no → SKIP).
    let chain = filter_chain(dict)?;
    // Debe contener exactamente una etapa Flate (nuestra fuente de píxeles). Sin
    // Flate no sabemos interpretar los bytes; con más de una no tiene sentido.
    if chain.iter().filter(|f| **f == Filter::Flate).count() != 1 {
        return None;
    }

    let color = supported_color(dict)?;

    // longitud esperada de los píxeles crudos.
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(color.channels())?;

    // Techo de seguridad: rechaza imágenes cuyo tamaño supera MAX_DECODE_BYTES.
    if expected > MAX_DECODE_BYTES {
        return None;
    }

    // Des-encadenar: aplicar cada filtro de izquierda a derecha. El predictor
    // (si lo hay) se aplica dentro de `apply_filter` en la etapa Flate.
    let mut data = stream.content.clone();
    for (idx, filter) in chain.iter().enumerate() {
        let parms = decode_parms_for(dict, idx);
        data = apply_filter(*filter, &data, parms)?;
    }

    if data.len() != expected {
        // longitud no coincide (malformado o mal interpretado) → SKIP, no adivinar.
        return None;
    }

    match color {
        FlateColor::Rgb => {
            image::RgbImage::from_raw(width, height, data).map(image::DynamicImage::ImageRgb8)
        }
        FlateColor::Gray => {
            image::GrayImage::from_raw(width, height, data).map(image::DynamicImage::ImageLuma8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use lopdf::{dictionary, Stream};
    use std::io::Write;

    fn zlib(raw: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(raw).unwrap();
        e.finish().unwrap()
    }

    /// Codifica ASCII85 (con EOD `~>`), como haría un productor PDF.
    fn ascii85_encode(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in input.chunks(4) {
            let mut val: u32 = 0;
            for i in 0..4 {
                val <<= 8;
                if i < chunk.len() {
                    val |= chunk[i] as u32;
                }
            }
            let mut group = [0u8; 5];
            let mut v = val;
            for i in (0..5).rev() {
                group[i] = (v % 85) as u8 + b'!';
                v /= 85;
            }
            out.extend_from_slice(&group[..chunk.len() + 1]);
        }
        out.extend_from_slice(b"~>");
        out
    }

    fn rgb_flate_stream(w: u32, h: u32) -> Stream {
        let raw = vec![137u8; (w * h * 3) as usize];
        Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        )
    }

    // ---- decodificación básica (sin predictor, sin cadena) ----

    #[test]
    fn decodes_devicergb8_to_right_dimensions() {
        let s = rgb_flate_stream(10, 12);
        let img = decode_flate_image(&s, 10, 12).expect("debe decodificar RGB/8");
        assert_eq!(img.width(), 10);
        assert_eq!(img.height(), 12);
        assert!(matches!(img, image::DynamicImage::ImageRgb8(_)));
    }

    #[test]
    fn decodes_devicegray8() {
        let raw = vec![42u8; 8 * 8];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 8, "Height" => 8,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceGray",
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        let img = decode_flate_image(&s, 8, 8).expect("debe decodificar Gray/8");
        assert!(matches!(img, image::DynamicImage::ImageLuma8(_)));
        assert_eq!((img.width(), img.height()), (8, 8));
    }

    // ---- predictores: round-trips exactos ----

    /// Genera una imagen RGB8 con un patrón determinista (no plano) para que los
    /// predictores tengan algo que diferenciar.
    fn sample_rgb(w: usize, h: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                v.push(((x * 7 + y * 3) % 256) as u8);
                v.push(((x * 13 + y * 5 + 11) % 256) as u8);
                v.push(((x * 3 + y * 17 + 200) % 256) as u8);
            }
        }
        v
    }

    /// Codifica una imagen RGB8 aplicando el filtro PNG `ftype` fila a fila
    /// (con byte de tipo por fila) y luego zlib. Espejo exacto del decoder.
    fn png_encode_rgb(pixels: &[u8], w: usize, h: usize, ftype: u8) -> Vec<u8> {
        let bpp = 3usize;
        let rl = w * 3;
        let mut filtered = Vec::with_capacity(h * (rl + 1));
        let zero = vec![0u8; rl];
        for y in 0..h {
            let cur = &pixels[y * rl..(y + 1) * rl];
            let prev: &[u8] = if y == 0 { &zero } else { &pixels[(y - 1) * rl..y * rl] };
            filtered.push(ftype);
            let mut row = vec![0u8; rl];
            for i in 0..rl {
                let a = if i >= bpp { cur[i - bpp] } else { 0 };
                let b = prev[i];
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                row[i] = match ftype {
                    0 => cur[i],
                    1 => cur[i].wrapping_sub(a),
                    2 => cur[i].wrapping_sub(b),
                    3 => cur[i].wrapping_sub(((a as u16 + b as u16) / 2) as u8),
                    4 => cur[i].wrapping_sub(paeth(a, b, c)),
                    _ => unreachable!(),
                };
            }
            filtered.extend_from_slice(&row);
        }
        zlib(&filtered)
    }

    fn png_predicted_stream(pixels: &[u8], w: u32, h: u32, ftype: u8) -> Stream {
        Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                "DecodeParms" => dictionary! {
                    "Predictor" => 15, "Colors" => 3, "BitsPerComponent" => 8, "Columns" => w as i64
                },
            },
            png_encode_rgb(pixels, w as usize, h as usize, ftype),
        )
    }

    fn assert_png_roundtrip(ftype: u8) {
        let (w, h) = (9usize, 7usize);
        let pixels = sample_rgb(w, h);
        let s = png_predicted_stream(&pixels, w as u32, h as u32, ftype);
        let img = decode_flate_image(&s, w as u32, h as u32)
            .unwrap_or_else(|| panic!("predictor PNG tipo {ftype} debe decodificar"));
        let back = img.to_rgb8();
        assert_eq!(back.as_raw(), &pixels, "PNG filtro {ftype}: píxeles no coinciden");
    }

    #[test]
    fn png_predictor_none_roundtrip() {
        assert_png_roundtrip(0);
    }
    #[test]
    fn png_predictor_sub_roundtrip() {
        assert_png_roundtrip(1);
    }
    #[test]
    fn png_predictor_up_roundtrip() {
        assert_png_roundtrip(2);
    }
    #[test]
    fn png_predictor_average_roundtrip() {
        assert_png_roundtrip(3);
    }
    #[test]
    fn png_predictor_paeth_roundtrip() {
        assert_png_roundtrip(4);
    }

    /// Predictor 15 con filas de tipo mixto (cada fila un filtro distinto): así
    /// se ve el mundo real, donde el codificador "optimum" elige por fila.
    #[test]
    fn png_predictor_mixed_rows_roundtrip() {
        let (w, h) = (6usize, 5usize);
        let pixels = sample_rgb(w, h);
        let bpp = 3usize;
        let rl = w * 3;
        let mut filtered = Vec::new();
        let zero = vec![0u8; rl];
        for y in 0..h {
            let ftype = (y % 5) as u8; // 0,1,2,3,4,...
            let cur = &pixels[y * rl..(y + 1) * rl];
            let prev: &[u8] = if y == 0 { &zero } else { &pixels[(y - 1) * rl..y * rl] };
            filtered.push(ftype);
            for i in 0..rl {
                let a = if i >= bpp { cur[i - bpp] } else { 0 };
                let b = prev[i];
                let c = if i >= bpp { prev[i - bpp] } else { 0 };
                let enc = match ftype {
                    0 => cur[i],
                    1 => cur[i].wrapping_sub(a),
                    2 => cur[i].wrapping_sub(b),
                    3 => cur[i].wrapping_sub(((a as u16 + b as u16) / 2) as u8),
                    4 => cur[i].wrapping_sub(paeth(a, b, c)),
                    _ => unreachable!(),
                };
                filtered.push(enc);
            }
        }
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                "DecodeParms" => dictionary! { "Predictor" => 15, "Colors" => 3, "Columns" => w as i64 },
            },
            zlib(&filtered),
        );
        let img = decode_flate_image(&s, w as u32, h as u32).expect("mixto debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &pixels);
    }

    /// Predictor 2 de TIFF (diferenciación horizontal), RGB8.
    #[test]
    fn tiff_predictor2_roundtrip() {
        let (w, h) = (8usize, 4usize);
        let pixels = sample_rgb(w, h);
        let bpp = 3usize;
        let rl = w * 3;
        // Codificar: cada muestra = actual - vecino izquierdo (misma componente).
        let mut enc = pixels.clone();
        for y in 0..h {
            let base = y * rl;
            for i in (bpp..rl).rev() {
                enc[base + i] = pixels[base + i].wrapping_sub(pixels[base + i - bpp]);
            }
        }
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                "DecodeParms" => dictionary! { "Predictor" => 2, "Colors" => 3, "Columns" => w as i64 },
            },
            zlib(&enc),
        );
        let img = decode_flate_image(&s, w as u32, h as u32).expect("TIFF 2 debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &pixels, "TIFF predictor 2: píxeles no coinciden");
    }

    /// TIFF predictor 2 con bpc != 8 → fuera de alcance → SKIP (sin garabatear).
    #[test]
    fn tiff_predictor2_non8bpc_is_skipped() {
        let raw = vec![10u8; 4 * 4 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                // BitsPerComponent 16 en los parms del predictor → no soportado.
                "DecodeParms" => dictionary! { "Predictor" => 2, "Colors" => 3, "BitsPerComponent" => 16, "Columns" => 4 },
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 4, 4).is_none(), "TIFF 16-bpc debe saltarse");
    }

    // ---- de-chain ----

    /// `/Filter [ASCII85Decode FlateDecode]`: zlib primero, luego ASCII85 encima.
    #[test]
    fn dechain_ascii85_then_flate_roundtrip() {
        let (w, h) = (5u32, 4u32);
        let pixels = sample_rgb(w as usize, h as usize);
        // Codificación: raw -> zlib -> ascii85 (orden inverso al decode).
        let zipped = zlib(&pixels);
        let encoded = ascii85_encode(&zipped);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"ASCII85Decode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
            },
            encoded,
        );
        let img = decode_flate_image(&s, w, h).expect("cadena A85+Flate debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &pixels);
    }

    /// Cadena A85+Flate CON predictor PNG en un `/DecodeParms` array paralelo
    /// (null para A85, dict para Flate) — el caso real más completo.
    #[test]
    fn dechain_ascii85_flate_with_png_predictor_roundtrip() {
        let (w, h) = (6u32, 5u32);
        let pixels = sample_rgb(w as usize, h as usize);
        let png_zlib = png_encode_rgb(&pixels, w as usize, h as usize, 4); // Paeth
        // el contenido zlib ya está; ahora lo pasamos por ascii85.
        // png_encode_rgb ya devuelve zlib; decodificar necesita: A85 -> Flate(+pred).
        let encoded = ascii85_encode(&png_zlib);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => w as i64, "Height" => h as i64,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"ASCII85Decode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
                "DecodeParms" => vec![
                    Object::Null,
                    Object::Dictionary(dictionary! { "Predictor" => 15, "Colors" => 3, "Columns" => w as i64 }),
                ],
            },
            encoded,
        );
        let img = decode_flate_image(&s, w, h).expect("A85+Flate+pred debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &pixels);
    }

    /// `/Filter [FlateDecode]` (array de un solo elemento) debe funcionar igual
    /// que el nombre suelto.
    #[test]
    fn dechain_single_element_flate_array() {
        let raw = sample_rgb(4, 4);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![Object::Name(b"FlateDecode".to_vec())],
            },
            zlib(&raw),
        );
        let img = decode_flate_image(&s, 4, 4).expect("[FlateDecode] debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &raw);
    }

    /// `/Filter [ASCIIHexDecode FlateDecode]`: cadena con hex.
    #[test]
    fn dechain_asciihex_then_flate_roundtrip() {
        let raw = sample_rgb(4, 3);
        let zipped = zlib(&raw);
        let mut hex = String::new();
        for b in &zipped {
            hex.push_str(&format!("{b:02x}"));
        }
        hex.push('>');
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 3,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"ASCIIHexDecode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
            },
            hex.into_bytes(),
        );
        let img = decode_flate_image(&s, 4, 3).expect("AHx+Flate debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &raw);
    }

    /// `/Filter [RunLengthDecode FlateDecode]`: RunLength encima de zlib.
    #[test]
    fn dechain_runlength_then_flate_roundtrip() {
        let raw = sample_rgb(4, 3);
        let zipped = zlib(&raw);
        // Codificar zipped con RunLength en modo literal (bloques de <=128).
        let mut rl = Vec::new();
        for chunk in zipped.chunks(128) {
            rl.push((chunk.len() - 1) as u8);
            rl.extend_from_slice(chunk);
        }
        rl.push(128); // EOD
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 3,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"RunLengthDecode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
            },
            rl,
        );
        let img = decode_flate_image(&s, 4, 3).expect("RL+Flate debe decodificar");
        assert_eq!(img.to_rgb8().as_raw(), &raw);
    }

    // ---- casos que siguen saltándose ----

    /// Un filtro no soportado en la cadena (LZWDecode) → SKIP (sin corromper).
    #[test]
    fn unsupported_filter_in_chain_is_skipped() {
        let raw = sample_rgb(4, 4);
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => vec![
                    Object::Name(b"LZWDecode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 4, 4).is_none(), "LZW en la cadena → SKIP");
    }

    /// DCTDecode como filtro único no es asunto de este decoder (lo maneja el
    /// path `image` upstream): aquí devuelve None.
    #[test]
    fn refuses_non_flate_filter() {
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "DCTDecode",
            },
            vec![0u8; 16],
        );
        assert!(decode_flate_image(&s, 4, 4).is_none());
    }

    #[test]
    fn refuses_length_mismatch() {
        let raw = vec![7u8; 10 * 10 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 12, "Height" => 12,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 12, 12).is_none(), "longitud no coincide → skip");
    }

    #[test]
    fn refuses_unsupported_colorspace_indexed() {
        let raw = vec![0u8; 6 * 6];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 6, "Height" => 6,
                "BitsPerComponent" => 8,
                "ColorSpace" => vec![Object::Name(b"Indexed".to_vec())],
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 6, 6).is_none());
    }

    #[test]
    fn refuses_non_8_bpc() {
        let raw = vec![0u8; 8 * 8 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 8, "Height" => 8,
                "BitsPerComponent" => 4, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 8, 8).is_none());
    }

    #[test]
    fn does_not_panic_on_garbage_zlib() {
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
            },
            vec![0xFF, 0x00, 0x13, 0x37, 0xAB],
        );
        assert!(decode_flate_image(&s, 4, 4).is_none());
    }

    /// Predictor desconocido (p. ej. 99) → SKIP en vez de interpretar mal.
    #[test]
    fn refuses_unknown_predictor() {
        let raw = vec![1u8; 4 * 4 * 3];
        let s = Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image",
                "Width" => 4, "Height" => 4,
                "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
                "Filter" => "FlateDecode",
                "DecodeParms" => dictionary! { "Predictor" => 99, "Colors" => 3, "Columns" => 4 },
            },
            zlib(&raw),
        );
        assert!(decode_flate_image(&s, 4, 4).is_none());
    }

    /// Dimensiones enormes se rechazan antes de cualquier asignación gigante.
    #[test]
    fn refuses_oversized_dimensions() {
        let s = Stream::new(
            dictionary! {
                "Type"             => "XObject",
                "Subtype"          => "Image",
                "Width"            => 100_000_i64,
                "Height"           => 100_000_i64,
                "BitsPerComponent" => 8,
                "ColorSpace"       => "DeviceRGB",
                "Filter"           => "FlateDecode",
            },
            vec![],
        );
        assert!(
            decode_flate_image(&s, 100_000, 100_000).is_none(),
            "debe rechazar dimensiones que superan MAX_DECODE_BYTES"
        );
    }

    // ---- pruebas unitarias de los decodificadores texto-a-binario ----

    #[test]
    fn ascii85_unit_roundtrip() {
        let data = b"Hello, ASCII85 world! 12345";
        let enc = ascii85_encode(data);
        assert_eq!(decode_ascii85(&enc).unwrap(), data);
    }

    #[test]
    fn ascii85_z_shortcut() {
        // 'z' representa 4 ceros.
        let enc = b"z~>";
        assert_eq!(decode_ascii85(enc).unwrap(), vec![0, 0, 0, 0]);
    }

    #[test]
    fn asciihex_unit_roundtrip_and_odd() {
        assert_eq!(decode_ascii_hex(b"48656c6c6f>").unwrap(), b"Hello");
        // dígito impar → se completa con 0 bajo: "4" -> 0x40
        assert_eq!(decode_ascii_hex(b"4>").unwrap(), vec![0x40]);
        // blancos ignorados
        assert_eq!(decode_ascii_hex(b"48 65\n6c>").unwrap(), b"Hel");
        // inválido
        assert!(decode_ascii_hex(b"4G>").is_none());
    }

    #[test]
    fn runlength_unit_literal_and_run() {
        // literal: len=2 → copia 3 bytes
        assert_eq!(decode_run_length(&[2, 1, 2, 3, 128]).unwrap(), vec![1, 2, 3]);
        // run: len=254 → repite 257-254=3 veces
        assert_eq!(decode_run_length(&[254, 9, 128]).unwrap(), vec![9, 9, 9]);
        // truncado → None
        assert!(decode_run_length(&[5, 1, 2]).is_none());
    }
}
