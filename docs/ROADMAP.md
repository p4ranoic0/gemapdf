# GemaPDF — Roadmap de mejoras futuras

> Registro durable de los levers investigados que requieren inversión grande.
> Cada entrada lleva payoff MEDIDO (no estimado) donde existe, riesgo, y los
> bloques de construcción con sus licencias (constraint del proyecto: sin AGPL).
> Estado al 2026-07-12. Contexto: specs en `docs/superpowers/specs/`, mediciones
> en el baseline de la skill `gemapdf-optimize`.

## Estado actual (para ubicarse)

- **v2.0-levers** (== main == producción): levers A (cadena Flate→DCT),
  B (encoder 4:2:0), C (/SMask), reflate_streams. Corpus: gana a producción
  Ghostscript en todos los docs con par de referencia.
- **v2.1** (rama activa): modo perceptual `--quality-target` COMPLETO pero
  EXPERIMENTAL (opt-in, CLI-only, feature `perceptual`, wasm blindado).
  Bloqueado para promoción por costo CPU (ver §1).

---

## 1. Optimización CPU del modo perceptual — DESBLOQUEA la promoción

**Estado:** medido y fallido el gate (2.8×–23.8× vs presupuesto 2.5×; ~240 ms
por imagen de búsqueda, dominado por ~7 pasos de encode+SSIM2).
**Mitigaciones identificadas, sin implementar, en orden de payoff esperado:**
1. **Semilla de la imagen anterior:** los escaneos de un doc son homogéneos; la
   q ganadora de la imagen N-1 como punto de partida deja la búsqueda en 1-3
   pasos (esperable 3-4× menos costo).
2. Bracket inicial estrecho alrededor de la semilla (±10) con fallback al rango
   completo.
3. Proxy más chico para el scoring (0.25 MPx en vez de 1 MPx — validar sesgo).
4. Paralelizar el bucle de imágenes (rayon, solo nativo).

**Lección de medición (NO repetir):** jamás medir calidad con SSIM2 sobre
renders de página — el resampleo desplaza la rejilla sub-píxel y páginas
visualmente idénticas puntúan −4. La garantía válida es in-pipeline (rejilla
alineada). Ídem: nunca comparar encoders a la misma q nominal ni por PSNR
(trellis sacrifica PSNR a propósito); solo iso-perceptual.

## 2. Bake-off de encoders por imagen (fase 2 del perceptual)

**Estado:** spike medido (commit d8df044, `examples/enc_mozjpeg.rs`).
[`mozjpeg-rs`](https://github.com/imazen/mozjpeg-rs) (Rust puro, BSD-3,
`forbid(unsafe_code)`, encoder-only) con `Preset::BaselineBalanced` emite
baseline C0 que **zune-jpeg decodifica bien CON Huffman optimizado** — el bug
que obligó a apagar huffman-opt era de `jpeg-encoder`, no del concepto.
**Payoff iso-SSIM2: dependiente de contenido** — −13.6% en escaneos grandes,
+15.4% (peor) en docs tipo doc-A → NO adoptar a ciegas; adoptarlo POR IMAGEN
dentro del modo perceptual (a la q encontrada, gana el más chico al target).
Duplica el costo CPU del modo → depende de §1. Verificar build wasm32 si algún
día va al Beta.

## 3. MRC — Mixed Raster Content (el techo: 3–15×, LA deuda grande)

**Qué es:** segmentar cada página escaneada en máscara de texto bilevel +
frente de color + fondo, cada capa a su códec óptimo, recompuestas con 3
XObjects estándar (`ImageMask true`, PDF 1.4 — lo abre cualquier visor). El
texto nunca pasa por DCT → nítido a resolución completa Y archivo 3-15× menor.
Ataca la pérdida #1 medida (resolución: −23.2 puntos SSIM2 de −43.8 totales;
ver `examples/edu_loss.rs` y el experimento "Anatomía de la pérdida").

**Matemática núcleo:** binarización adaptativa de Sauvola
`T(x,y) = μ(x,y)·(1 + k·(σ(x,y)/R − 1))` para la máscara; k-means para los
colores de frente/fondo; downsample fuerte de ambos (el fondo tolera mucho).

**Bloques en Rust (licencias verificadas 2026-07-11):**
- Máscara → [`fax`](https://lib.rs/crates/fax) (CCITT G4, encoder+decoder) —
  camino simple; o [`jbig2enc`](https://crates.io/crates/jbig2enc) (Apache-2,
  crate joven) — mejor ratio.
- Referencia de arquitectura: [`archive-pdf-tools`](https://github.com/internetarchive/archive-pdf-tools)
  del Internet Archive (la única implementación open-source probada, millones
  de páginas/día) — **AGPL: solo leer papers/arquitectura, clean-room, jamás
  portar código**.

**⚠️ Advertencia grabada a fuego:** JBIG2 en modo símbolos con pérdida causó el
escándalo Xerox (números intercambiados en escaneos). Para expedientes legales:
G4 o JBIG2 genérico, JAMÁS symbol-matching lossy.

**Riesgos:** segmentación que se come trazos finos (gate visual obligatorio con
el corpus), CPU por página, es un subsistema entero (semanas). Reutiliza el
arnés perceptual (§1) para controlar la calidad de sus capas JPEG.

## 4. JPX / JPEG2000 (mejor códec dentro del estándar PDF)

Wavelets, sin artefactos de bloque, ~20-30% mejor que JPEG a la misma calidad,
`/JPXDecode` es PDF 1.5 estándar. **Bloqueador:** ecosistema de encoders en
Rust puro es pobre (los maduros son bindings a OpenJPEG, C). Revisar el
ecosistema antes de invertir; va DETRÁS de MRC en prioridad.

## 5. Subsetting de fuentes

Medido 2026-07-06: payoff ~1.5 MB en docs merge extremos (OS2736), factible con
el crate `subsetter`, pero riesgo VISUAL (glifo perdido → blanco). **Bloqueado
por:** arnés render-compare que aún no existe. Opt-in "máxima" cuando exista.

## 6. Coberturas menores (horas, no semanas)

- `/SMask /None` (Name): hoy preserva la base innecesariamente — soportarlo.
- ExtGState luminosity softmasks (`/SMask <</G form>>`): fuera del alcance del
  lever C; las imágenes dentro del grupo /G hoy se recomprimen lossy.
- Test de dims oversized cubre solo width (falta height/cero).
- Warning "mejor esfuerzo a q=90" hardcodea el valor de Q_MAX (acople latente).
- Warning `matte_or_unknown` conflata /Matte y máscara no-inspeccionable.

## 7. Publicación del repo (contexto para todo lo anterior)

Plan declarado: publicar la librería en un repo aparte. Checklist:
- Licencias del árbol: todas permisivas (MIT/Apache/BSD) — verificado; el modo
  perceptual añade ssimulacra2 (BSD-2) solo bajo feature.
- Al crear el remote: **empujar también las ramas ancla** (`v2.0-levers`,
  `v2.0-portable`) — hoy solo existen localmente.
- CI ya listo (`.github/workflows/ci.yml`: fmt, tests y clippy en ambas
  variantes de features, build wasm + guard anti-ssimulacra2).
- Gate local activo: hook `hooks/pre-commit` (`core.hooksPath=hooks`).
