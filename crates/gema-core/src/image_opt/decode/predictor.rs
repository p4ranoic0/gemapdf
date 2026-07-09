//! Des-filtrado por predictor (PNG 10–15, TIFF 2). Se aplica tras inflar la
//! etapa Flate: los `/DecodeParms` describen el predictor y su geometría
//! (Colors/BitsPerComponent/Columns). Un des-filtrado equivocado garabatearía
//! píxeles, así que validamos longitudes y saltamos (`None`) ante cualquier caso
//! fuera de alcance en vez de adivinar. Nunca hace panic ante entrada hostil.

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
pub(super) fn apply_predictor(data: Vec<u8>, parms: Option<&lopdf::Dictionary>) -> Option<Vec<u8>> {
    let p = read_predictor_params(parms);
    match p.predictor {
        1 => Some(data), // sin predicción
        2 => tiff_predictor2(data, &p),
        10..=15 => png_predictor(data, &p),
        _ => None, // predictor desconocido → SKIP
    }
}

/// Bytes por píxel: `ceil(colors * bpc / 8)`, mínimo 1.
/// Devuelve `None` si la multiplicación desborda `usize`.
fn bytes_per_pixel(colors: usize, bpc: usize) -> Option<usize> {
    Some(colors.checked_mul(bpc)?.div_ceil(8).max(1))
}

/// Longitud de fila en bytes: `ceil(colors * bpc * columns / 8)`.
/// Devuelve `None` si cualquier operación desborda `usize`.
fn row_len(colors: usize, bpc: usize, columns: usize) -> Option<usize> {
    let bits = colors.checked_mul(bpc)?.checked_mul(columns)?;
    Some(bits.div_ceil(8))
}

/// Predictor 2 de TIFF: diferenciación horizontal. Sólo soportamos 8 bpc
/// (cada muestra es un byte); otros bpc → SKIP. Cada muestra se reconstruye
/// sumándole la muestra `bpp` posiciones a la izquierda (misma componente).
fn tiff_predictor2(mut data: Vec<u8>, p: &PredictorParams) -> Option<Vec<u8>> {
    if p.bpc != 8 {
        return None; // sólo TIFF 8-bpc en alcance
    }
    let rl = row_len(p.colors, p.bpc, p.columns)?;
    if rl == 0 || !data.len().is_multiple_of(rl) {
        return None; // longitud no cuadra
    }
    let bpp = bytes_per_pixel(p.colors, p.bpc)?; // = colors para 8 bpc
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
    let rl = row_len(p.colors, p.bpc, p.columns)?;
    if rl == 0 {
        return None;
    }
    let stride = rl.checked_add(1)?; // byte de tipo + fila
    if !data.len().is_multiple_of(stride) {
        return None; // longitud no cuadra
    }
    let bpp = bytes_per_pixel(p.colors, p.bpc)?;
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
pub(super) fn paeth(a: u8, b: u8, c: u8) -> u8 {
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
