//! Búsqueda de q por calidad perceptual (feature `perceptual`, spec 2026-07-11).
//!
//! Para una imagen decodificada, encuentra la MENOR q∈[Q_MIN, Q_MAX] cuyo
//! re-decode (con `image`/zune — el decoder real del pipeline) puntúa
//! SSIMULACRA2 ≥ target contra los píxeles fuente. El score se calcula sobre
//! un proxy de ≤0.25 MPx (mismo reduce CatmullRom para fuente y candidata: el
//! sesgo es consistente y la búsqueda solo necesita orden, no valor absoluto).
//! Si ni Q_MAX alcanza el target devuelve el encode a Q_MAX con `false`
//! (mejor esfuerzo — el orquestador lo reporta como warning).

use crate::image_opt::jpeg::JpegRecompressor;
use crate::image_opt::{Encoded, RawImage, Recompressor};
use ssimulacra2::{compute_frame_ssimulacra2, ColorPrimaries, Rgb, TransferCharacteristic};

const Q_MIN: u8 = 20;
const Q_MAX: u8 = 90;
/// §1.3: 0.25 MPx (antes 1 MPx). SSIM2 escala con los píxeles del proxy, así
/// que esto abarata ~4× el scoring de cada probe — el único lever de CPU que
/// también sirve al Beta wasm (single-thread). Validado sobre el corpus contra
/// gema v2.0 (q fija) y producción Ghostscript: ver baseline de la skill
/// gemapdf-optimize y ROADMAP §1.3.
const PROXY_MAX_PX: u64 = 250_000;

/// Reduce a ≤[`PROXY_MAX_PX`] si hace falta (CatmullRom ≈ interpolación de viewer).
fn proxy(img: &image::RgbImage) -> image::RgbImage {
    let (w, h) = img.dimensions();
    let px = w as u64 * h as u64;
    if px <= PROXY_MAX_PX {
        return img.clone();
    }
    let s = (PROXY_MAX_PX as f64 / px as f64).sqrt();
    image::imageops::resize(
        img,
        ((w as f64 * s) as u32).max(1),
        ((h as f64 * s) as u32).max(1),
        image::imageops::FilterType::CatmullRom,
    )
}

/// Convierte a la representación del métrico. `None` si la conversión falla
/// (dims inválidas): el llamador degrada a q fija, nunca panic.
fn to_metric(img: &image::RgbImage) -> Option<Rgb> {
    let data: Vec<[f32; 3]> = img
        .pixels()
        .map(|p| {
            [
                p.0[0] as f32 / 255.0,
                p.0[1] as f32 / 255.0,
                p.0[2] as f32 / 255.0,
            ]
        })
        .collect();
    Rgb::new(
        data,
        img.width() as usize,
        img.height() as usize,
        TransferCharacteristic::SRGB,
        ColorPrimaries::BT709,
    )
    .ok()
}

/// SSIM2 de `bytes` re-decodificados contra el proxy fuente. `None` si zune no
/// abre los bytes o el métrico falla.
fn score(src_proxy: &image::RgbImage, src_metric: &Rgb, bytes: &[u8]) -> Option<f64> {
    let dec = image::load_from_memory(bytes).ok()?.to_rgb8();
    // misma reducción que la fuente; dims explícitas para garantizar igualdad
    let dec_proxy = if dec.dimensions() == src_proxy.dimensions() {
        dec
    } else {
        image::imageops::resize(
            &dec,
            src_proxy.width(),
            src_proxy.height(),
            image::imageops::FilterType::CatmullRom,
        )
    };
    let dm = to_metric(&dec_proxy)?;
    compute_frame_ssimulacra2(src_metric.clone(), dm).ok()
}

/// Ver doc del módulo. `None` solo si el ENCODER falla (dims > u16, etc.) —
/// el orquestador lo traduce a Skipped igual que hoy.
pub(crate) fn encode_jpeg_at_target(raw: &RawImage, target: f32) -> Option<(Encoded, bool)> {
    let src = raw.image.to_rgb8();
    let src_proxy = proxy(&src);
    let Some(src_metric) = to_metric(&src_proxy) else {
        // métrico no construible → degradar a mejor esfuerzo a Q_MAX
        return JpegRecompressor.recompress(raw, Q_MAX).map(|e| (e, false));
    };

    let (mut lo, mut hi) = (Q_MIN, Q_MAX);
    let mut best: Option<Encoded> = None;
    while lo <= hi {
        let mid = ((lo as u16 + hi as u16) / 2) as u8;
        let enc = JpegRecompressor.recompress(raw, mid)?;
        match score(&src_proxy, &src_metric, &enc.bytes) {
            Some(s) if s >= target as f64 => {
                best = Some(enc);
                if mid == Q_MIN {
                    break;
                }
                hi = mid - 1;
            }
            _ => lo = mid + 1,
        }
    }
    match best {
        Some(e) => Some((e, true)),
        None => JpegRecompressor.recompress(raw, Q_MAX).map(|e| (e, false)),
    }
}

/// Cache de búsquedas dentro de UN documento (cierre §1), SOLO para imágenes
/// decodificadas por la ruta DCT. En esa ruta los píxeles son función
/// exclusivamente de los bytes JPEG (tras des-encadenar el prefijo /Filter):
/// el dict `/ColorSpace` se IGNORA al decodificar y el encoder pone el suyo en
/// la salida — por eso la clave correcta es `raw + /Filter + dims + τ`, sin
/// dict. Medido en `doc-B1` (merge): 127/301 búsquedas eran
/// copias byte-idénticas del mismo stream cuyo dict solo variaba en claves
/// irrelevantes (p. ej. `/ColorSpace` duplicado con otro id, `/Name`).
///
/// La ruta Flate NO se cachea (ahí `/ColorSpace`/`/BitsPerComponent`/
/// `/DecodeParms` sí determinan píxeles y resolver sus indirecciones de forma
/// comparable no paga): conservador y correcto.
///
/// La clave se compara COMPLETA en el hit — bytes crudos, `/Filter`,
/// `/DecodeParms`, `/DP`, dims y τ: identidad de las ENTRADAS de la función de
/// píxeles, sin hashes truncados → el output es EXACTAMENTE el mismo que sin
/// cache (byte-idéntico, incluso con input adversarial). Compartido entre los
/// hilos de rayon tras un Mutex que solo se toma para consultar/insertar, nunca
/// durante la búsqueda: una carrera entre dos copias idénticas computa dos veces
/// el mismo resultado determinista (correcto, solo algo de trabajo extra).
///
/// Tope de memoria: guarda una copia del `raw` de cada entrada única, así que el
/// peso crece con las imágenes ÚNICAS. Se capea a [`CACHE_MAX_BYTES`]; pasado el
/// tope deja de insertar (los hits sobre lo ya guardado siguen). No afecta
/// corrección: una entrada no guardada solo significa que una copia idéntica
/// futura re-busca — siempre correcto. En docs con muchos duplicados (el caso
/// que gana) el peso único es chico y el tope no molesta; en docs todo-únicos
/// (hits≈0) capar no pierde nada.
pub(crate) struct SearchCache {
    entries: std::sync::Mutex<Vec<CacheEntry>>,
    /// Bytes de `raw`+`enc` acumulados; guardado bajo el mismo lock que `entries`.
    stored_bytes: std::sync::atomic::AtomicU64,
    /// Tope de memoria (= [`CACHE_MAX_BYTES`] en prod; los tests lo bajan).
    max_bytes: u64,
    hits: std::sync::atomic::AtomicU32,
}

/// Tope de memoria del cache (§1): pasado esto no se insertan entradas nuevas.
/// Generoso — cubre el corpus real (el doc más pesado, 88 MB, guardó ~90 MB de
/// únicos y ganó); solo frena docs patológicos todo-únicos de cientos de MB.
const CACHE_MAX_BYTES: u64 = 128 * 1024 * 1024;

/// Identidad de la FUENTE de una imagen elegible para el cache: los bytes
/// crudos del stream, su `/Filter` (la cadena determina cómo se des-encadena
/// hasta el JPEG interno) y sus `/DecodeParms`//`/DP` (los predictores del
/// prefijo Flate participan en el des-encadenado — `unwrap_to_dct` los aplica,
/// así que dos raws idénticos con parms distintos pueden dar bytes JPEG
/// distintos). Solo la ruta DCT construye una — la ruta Flate pasa `None` al
/// wrapper y va directo a la búsqueda.
pub(crate) struct CacheKeySrc<'a> {
    pub(crate) raw_bytes: &'a [u8],
    pub(crate) filter: Option<lopdf::Object>,
    pub(crate) decode_parms: Option<lopdf::Object>,
    pub(crate) dp: Option<lopdf::Object>,
}

struct CacheEntry {
    raw: Vec<u8>,
    filter: Option<lopdf::Object>,
    decode_parms: Option<lopdf::Object>,
    dp: Option<lopdf::Object>,
    w: u32,
    h: u32,
    target_bits: u32,
    enc: Encoded,
    reached: bool,
}

impl SearchCache {
    pub(crate) fn new() -> Self {
        Self::with_max_bytes(CACHE_MAX_BYTES)
    }

    fn with_max_bytes(max_bytes: u64) -> Self {
        Self {
            entries: std::sync::Mutex::new(Vec::new()),
            stored_bytes: std::sync::atomic::AtomicU64::new(0),
            max_bytes,
            hits: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Hits acumulados (telemetría de tests/diagnóstico).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn hits(&self) -> u32 {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// [`encode_jpeg_at_target`] con memoización por identidad de fuente (ver
/// [`SearchCache`]). `src: None` (ruta Flate) busca directo, sin cachear; las
/// dims post-transform van implícitas en `raw.image`.
pub(crate) fn encode_jpeg_at_target_cached(
    cache: &SearchCache,
    src: Option<CacheKeySrc<'_>>,
    raw: &RawImage,
    target: f32,
) -> Option<(Encoded, bool)> {
    let Some(src) = src else {
        return encode_jpeg_at_target(raw, target);
    };
    let (w, h) = (raw.image.width(), raw.image.height());
    let target_bits = target.to_bits();
    {
        let entries = cache.entries.lock().expect("cache lock");
        if let Some(e) = entries.iter().find(|e| {
            e.target_bits == target_bits
                && e.w == w
                && e.h == h
                && e.raw == src.raw_bytes
                && e.filter == src.filter
                && e.decode_parms == src.decode_parms
                && e.dp == src.dp
        }) {
            cache
                .hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Some((e.enc.clone(), e.reached));
        }
        // miss → soltar el lock ANTES de la búsqueda (no serializar rayon)
    }
    let (enc, reached) = encode_jpeg_at_target(raw, target)?;
    // Insertar bajo el lock, respetando el tope de memoria. Pasado el tope se
    // devuelve el resultado igual pero NO se guarda (una copia futura re-busca —
    // siempre correcto). El contador vive bajo el mismo lock, así que no hace
    // falta atomicidad fina: `Relaxed` basta.
    use std::sync::atomic::Ordering::Relaxed;
    let entry_bytes = (src.raw_bytes.len() + enc.bytes.len()) as u64;
    let mut entries = cache.entries.lock().expect("cache lock");
    if cache.stored_bytes.load(Relaxed) + entry_bytes <= cache.max_bytes {
        cache.stored_bytes.fetch_add(entry_bytes, Relaxed);
        entries.push(CacheEntry {
            raw: src.raw_bytes.to_vec(),
            filter: src.filter,
            decode_parms: src.decode_parms,
            dp: src.dp,
            w,
            h,
            target_bits,
            enc: enc.clone(),
            reached,
        });
    }
    Some((enc, reached))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    /// Foto sintética con contenido real (gradientes + textura): 400×400.
    fn photo() -> RawImage {
        let img = image::RgbImage::from_fn(400, 400, |x, y| {
            let fx = x as f32 / 31.0;
            let fy = y as f32 / 23.0;
            image::Rgb([
                (128.0 + 90.0 * (fx.sin() * fy.cos())) as u8,
                (128.0 + 70.0 * ((fx * 0.7 + 1.0).cos())) as u8,
                (128.0 + 80.0 * ((fy * 1.3).sin())) as u8,
            ])
        });
        RawImage {
            image: image::DynamicImage::ImageRgb8(img),
        }
    }

    #[test]
    fn reaches_reasonable_target_and_score_holds() {
        let raw = photo();
        let (enc, reached) = encode_jpeg_at_target(&raw, 60.0).expect("debe encodear");
        assert!(reached, "τ=60 debe ser alcanzable en una foto sintética");
        // el score real del resultado (misma vía que la búsqueda) cumple el target
        let src = raw.image.to_rgb8();
        let sp = proxy(&src);
        let sm = to_metric(&sp).unwrap();
        let s = score(&sp, &sm, &enc.bytes).expect("re-decode debe abrir");
        assert!(s >= 60.0, "score {s:.1} < target");
        assert_eq!(enc.filter, "DCTDecode");
    }

    #[test]
    fn unreachable_target_falls_back_to_qmax() {
        let raw = photo();
        let (enc, reached) = encode_jpeg_at_target(&raw, 99.9).expect("debe encodear");
        assert!(!reached, "τ=99.9 no debe alcanzarse con JPEG q90");
        let at_qmax = JpegRecompressor.recompress(&raw, Q_MAX).unwrap();
        assert_eq!(
            enc.bytes, at_qmax.bytes,
            "el mejor esfuerzo debe ser exactamente q=Q_MAX"
        );
    }

    #[test]
    fn higher_target_costs_more_bytes() {
        let raw = photo();
        let (lo, _) = encode_jpeg_at_target(&raw, 45.0).unwrap();
        let (hi, _) = encode_jpeg_at_target(&raw, 75.0).unwrap();
        assert!(
            hi.bytes.len() >= lo.bytes.len(),
            "τ mayor no puede costar menos bytes ({} vs {})",
            hi.bytes.len(),
            lo.bytes.len()
        );
    }

    /// Foto sintética >1 MPx (1200×1200) con el mismo patrón: ejerce la rama de
    /// reduce a proxy (`proxy()` cuando `px > PROXY_MAX_PX`), sin cobertura
    /// previa.
    fn large_photo() -> RawImage {
        let img = image::RgbImage::from_fn(1200, 1200, |x, y| {
            let fx = x as f32 / 31.0;
            let fy = y as f32 / 23.0;
            image::Rgb([
                (128.0 + 90.0 * (fx.sin() * fy.cos())) as u8,
                (128.0 + 70.0 * ((fx * 0.7 + 1.0).cos())) as u8,
                (128.0 + 80.0 * ((fy * 1.3).sin())) as u8,
            ])
        });
        RawImage {
            image: image::DynamicImage::ImageRgb8(img),
        }
    }

    #[test]
    fn proxy_branch_covers_large_images() {
        let raw = large_photo();
        let (_, reached) = encode_jpeg_at_target(&raw, 50.0).expect("debe encodear");
        assert!(
            reached,
            "τ=50 debe ser alcanzable en una foto sintética >1MPx (rama proxy)"
        );
    }

    /// §1.3: el proxy de scoring debe capar a 0.25 MPx (no 1 MPx) — es lo que
    /// abarata cada probe de la búsqueda (SSIM2 escala con los píxeles del
    /// proxy). Gate de tamaño/calidad validado aparte sobre el corpus contra
    /// gema v2.0 y producción Ghostscript.
    #[test]
    fn proxy_caps_at_quarter_megapixel() {
        let img = image::RgbImage::from_pixel(1200, 1200, image::Rgb([100, 100, 100]));
        let p = proxy(&img);
        let px = p.width() as u64 * p.height() as u64;
        assert!(
            px <= 250_000,
            "el proxy debe capar a 0.25 MPx, quedó en {px} px ({}×{})",
            p.width(),
            p.height()
        );
        // una imagen ya ≤0.25 MPx no se toca (mismo objeto, sin resample)
        let small = image::RgbImage::from_pixel(400, 400, image::Rgb([50, 50, 50]));
        let sp = proxy(&small);
        assert_eq!(sp.dimensions(), (400, 400), "≤0.25 MPx no se remuestrea");
    }

    #[test]
    fn grayscale_stays_l8_devicegray() {
        let gray = image::GrayImage::from_fn(400, 400, |x, y| {
            image::Luma([((x * 7 + y * 13) % 256) as u8])
        });
        let raw = RawImage {
            image: image::DynamicImage::ImageLuma8(gray),
        };
        let (enc, _) = encode_jpeg_at_target(&raw, 55.0).expect("gris debe encodear");
        assert_eq!(enc.color_space, "DeviceGray");
    }

    /// Cierre §1 — cache por identidad de fuente DCT: misma (raw, filtro,
    /// dims, τ) → hit con resultado idéntico a la búsqueda directa; cambiar
    /// CUALQUIER componente → miss; ruta Flate (src None) → nunca cachea.
    #[test]
    fn search_cache_hits_only_identical_dct_sources() {
        let cache = SearchCache::new();
        let raw = photo();
        let filt = || Some(lopdf::Object::Name(b"DCTDecode".to_vec()));
        let src = |bytes: &'static [u8]| {
            Some(CacheKeySrc {
                raw_bytes: bytes,
                filter: filt(),
                decode_parms: None,
                dp: None,
            })
        };
        let (e1, r1) =
            encode_jpeg_at_target_cached(&cache, src(b"STREAMBYTES"), &raw, 60.0).unwrap();
        assert_eq!(cache.hits(), 0, "primera vez: miss");
        let (e2, r2) =
            encode_jpeg_at_target_cached(&cache, src(b"STREAMBYTES"), &raw, 60.0).unwrap();
        assert_eq!(cache.hits(), 1, "misma clave: hit");
        assert_eq!(e1.bytes, e2.bytes, "el hit devuelve bytes idénticos");
        assert_eq!(r1, r2);
        // el resultado cacheado = el de la búsqueda directa (equivalencia)
        let (direct, dr) = encode_jpeg_at_target(&raw, 60.0).unwrap();
        assert_eq!(e2.bytes, direct.bytes, "hit ≡ búsqueda directa");
        assert_eq!(r2, dr);
        // τ distinta → miss
        encode_jpeg_at_target_cached(&cache, src(b"STREAMBYTES"), &raw, 55.0).unwrap();
        assert_eq!(cache.hits(), 1, "τ distinta no puede hacer hit");
        // raw distinto → miss
        encode_jpeg_at_target_cached(&cache, src(b"OTROSBYTES"), &raw, 60.0).unwrap();
        assert_eq!(cache.hits(), 1, "stream distinto no puede hacer hit");
        // cadena /Filter distinta → miss (des-encadena a bytes distintos)
        let src_flate = Some(CacheKeySrc {
            raw_bytes: b"STREAMBYTES",
            filter: Some(lopdf::Object::Array(vec![
                lopdf::Object::Name(b"FlateDecode".to_vec()),
                lopdf::Object::Name(b"DCTDecode".to_vec()),
            ])),
            decode_parms: None,
            dp: None,
        });
        encode_jpeg_at_target_cached(&cache, src_flate, &raw, 60.0).unwrap();
        assert_eq!(
            cache.hits(),
            1,
            "cadena /Filter distinta no puede hacer hit"
        );
        // mismo raw+filter pero /DecodeParms distinto → miss: el prefijo Flate
        // aplica predictores de DecodeParms al des-encadenar (unwrap_to_dct),
        // así que los bytes JPEG internos pueden diferir.
        let parms = lopdf::Object::Dictionary(dictionary! {
            "Predictor" => 12,
            "Colors" => 3,
            "Columns" => 400,
        });
        let src_parms = Some(CacheKeySrc {
            raw_bytes: b"STREAMBYTES",
            filter: filt(),
            decode_parms: Some(parms),
            dp: None,
        });
        encode_jpeg_at_target_cached(&cache, src_parms, &raw, 60.0).unwrap();
        assert_eq!(cache.hits(), 1, "/DecodeParms distinto no puede hacer hit");
        // ruta Flate (src None): busca directo, ni hace hit ni inserta
        let before = cache.hits();
        encode_jpeg_at_target_cached(&cache, None, &raw, 60.0).unwrap();
        encode_jpeg_at_target_cached(&cache, None, &raw, 60.0).unwrap();
        assert_eq!(cache.hits(), before, "la ruta sin identidad nunca cachea");
    }

    /// Cierre §1 — las dims post-transform participan en la clave: el mismo
    /// stream pintado a otro tamaño produce otra búsqueda (píxeles distintos).
    #[test]
    fn search_cache_discriminates_dims() {
        let cache = SearchCache::new();
        let src = || {
            Some(CacheKeySrc {
                raw_bytes: b"STREAMBYTES",
                filter: Some(lopdf::Object::Name(b"DCTDecode".to_vec())),
                decode_parms: None,
                dp: None,
            })
        };
        encode_jpeg_at_target_cached(&cache, src(), &photo(), 60.0).unwrap();
        // misma fuente, dims distintas (large_photo 1200×1200 vs 400×400)
        encode_jpeg_at_target_cached(&cache, src(), &large_photo(), 60.0).unwrap();
        assert_eq!(cache.hits(), 0, "dims distintas no pueden hacer hit");
    }

    /// Cierre §1 — el tope de memoria frena la inserción SIN romper corrección:
    /// con tope 0 no cachea (dos idénticas siguen siendo miss) pero devuelve el
    /// resultado correcto; con tope amplio, la segunda idéntica hace hit.
    #[test]
    fn search_cache_respects_byte_cap() {
        let raw = photo();
        let mk = |bytes: &'static [u8]| {
            Some(CacheKeySrc {
                raw_bytes: bytes,
                filter: Some(lopdf::Object::Name(b"DCTDecode".to_vec())),
                decode_parms: None,
                dp: None,
            })
        };
        // tope 0 → nunca guarda → dos idénticas siguen siendo miss...
        let capped = SearchCache::with_max_bytes(0);
        encode_jpeg_at_target_cached(&capped, mk(b"AAAA"), &raw, 60.0).unwrap();
        let (capres, _) = encode_jpeg_at_target_cached(&capped, mk(b"AAAA"), &raw, 60.0).unwrap();
        assert_eq!(capped.hits(), 0, "con tope 0 no debe cachear");
        // ...pero el resultado sigue siendo el de la búsqueda directa
        let (direct, _) = encode_jpeg_at_target(&raw, 60.0).unwrap();
        assert_eq!(
            capres.bytes, direct.bytes,
            "capado sigue devolviendo lo correcto"
        );
        // sin tope, la segunda idéntica SÍ hace hit (contraste)
        let big = SearchCache::with_max_bytes(u64::MAX);
        encode_jpeg_at_target_cached(&big, mk(b"AAAA"), &raw, 60.0).unwrap();
        encode_jpeg_at_target_cached(&big, mk(b"AAAA"), &raw, 60.0).unwrap();
        assert_eq!(big.hits(), 1, "sin tope debe cachear");
    }
}
