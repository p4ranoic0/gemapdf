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
  Bloqueado para promoción por costo CPU en el Beta wasm (ver §1). En NATIVO el
  bucle de imágenes ya va en paralelo (§1.4 hecho: 4.5–6.7× byte-idéntico).

---

## 1. Optimización CPU del modo perceptual — DESBLOQUEA la promoción

**Estado:** medido y fallido el gate (2.8×–23.8× vs presupuesto 2.5×; ~240 ms
por imagen de búsqueda, dominado por ~7 pasos de encode+SSIM2).

**Mitigaciones — estado tras medición (2026-07-17):**
1. ~~**Semilla de la imagen anterior**~~ · 2. ~~**Bracket estrecho ±10 + fallback**~~
   — **FALSIFICADOS por medición.** Implementado como búsqueda exponencial
   (galloping) sembrada con la q ganadora previa (subsume 1 y 2), con TDD
   completo y equivalencia probada *bajo monotonía*. En el corpus real NO rinde,
   por dos causas independientes:
   - **La premisa "la q apenas se mueve" es falsa.** La q* salta **5–15** entre
     imágenes elegibles (p. ej. doc-F: 53→51→48→44→39→38→28→27→20), no ≤2. El
     galloping sobretira cuando la semilla está lejos y cuesta MÁS que un binario
     fresco. Probes/imagen medidos (sembrado vs binario): doc-A 5.74 vs 6.13
     (−6%), doc-E 7.04 vs ~6.5, **doc-F 8.53 vs 6.47 (+32%)**. Neto:
     nulo-a-negativo.
   - **⚠️ Las curvas SSIM2(q) reales son NO monótonas.** El binario de v2.1 ya es
     un heurístico bajo esa realidad. **Cualquier** búsqueda que cambie el CAMINO
     de sondeo (semilla, bracket, galloping) aterriza en una q* distinta a la del
     binario en imágenes con curva no monótona → **cambia la salida, y en
     promedio la EMPEORA** (doc-A: galloping da +3991 bytes vs v2.1). Por eso el
     bracket (§1.2) muere por la misma causa: no se arregla cambiando el algoritmo
     de búsqueda. Reducir el *número* de probes es, en este corpus, un callejón.
   Rama revertida; `v2.1` intacto. Rehacer sólo si aparece un corpus con q*
   demostrablemente estable (±2) Y curvas monótonas — improbable.
3. **Proxy más chico para el scoring** (0.25 MPx vs 1 MPx). ✅ **HECHO**
   (2026-07-17). Medido a τ65/90dpi contra gema v2.0 (q fija 45) y producción
   Ghostscript:
   - **CPU (user)**: doc-A 87→47s, expediente 53→27s, doc-F 7.5→3.3s —
     ~**2× menos**, y es el único lever que también sirve al Beta wasm.
   - **Tamaño**: MENOR en todo el corpus (doc-A 13.45→10.55 MB −21.6%;
     expediente 7.08→5.73 −19%). vs producción Ghostscript (único par real,
     doc-A 18.3 MB): τ65@0.25MPx da **10.55 MB, −42%**.
   - **⚠️ Corre la escala de τ**: el proxy borroso puntúa más benévolo → al
     mismo τ pasa una q menor. τ65@0.25MPx ≈ vara más baja que τ65@1MPx.
     **τ RECALIBRADOS a la escala nueva (2026-07-17): screen 68 · ebook 84 ·
     printer 85** — mayor τ con tamaño ≤ q fija del perfil sobre 3 docs, con
     margen; gate visual pasado. Detalle en spec §3 y baseline de la skill.
   - **Gate visual**: PASADO — peor página de doc-A (p151, −27.6%: manuscrita,
     sello ministerio y firmas legibles) y sello tenue de expediente p98.
   - **Gate CPU 2.5× vs fija**: expediente 2.96×, prueba 2.93× (rozando);
     doc-A 14× (patológico: 218 imágenes elegibles). El proxy solo no
     desbloquea la promoción universal; sí deja el modo ~2× más barato y
     estrictamente mejor en tamaño.
4. **Paralelizar el bucle de imágenes** (rayon, solo nativo). ✅ **HECHO**
   (commit e920d47, 2026-07-17). `process_image` se partió en `prepare_image`
   (etapas 1-4, read-only) + `commit_prepared` (etapa 5, muta en serie); el
   cómputo pesado corre en paralelo sobre `&Document`. **Byte-idéntico** al
   serial (verificado en corpus). Medido en 12 cores: **doc-F 4.5×**
   (5.21→1.17s), **doc-A 6.7×** (61.82→9.26s) — más imágenes, más ganancia.
   rayon es dep `cfg(not wasm32)`: el Beta wasm (single-thread) se queda serial
   y no lo arrastra (wasm 1.30 MB, sin cambios). El Beta NO se beneficia — su
   CPU por-imagen sigue igual; para el Beta el lever pendiente es §1.3 (proxy).
   Corpus verificado byte-idéntico: 11 docs reales (15–88 MB). Revisión
   adversarial halló un único caso teórico (paleta Indexed que a la vez es
   `/Subtype /Image`, sólo input malformado) → cerrado con guard en
   `colorspace.rs` (commit 6db9062, inerte en docs reales). Garantía airtight.
5. **Cache de búsquedas por identidad de fuente** (commit eb941fd, 2026-07-18).
   ✅ **HECHO**. Copias byte-idénticas del mismo stream (ruta DCT) no repiten la
   búsqueda de q: clave = raw + `/Filter` + `/DecodeParms` + `/DP` + dims + τ,
   comparada completa → **output byte-idéntico** al sin-cache (verificado). Solo
   ruta DCT (ahí los píxeles salen solo de los bytes JPEG); Flate no se cachea.
   Tope de memoria 128 MB. Medido en `doc-B1` (88 MB merge, 25%
   imgs redundantes): 83 hits, **user 110.6→80.1s (−27%)**; doc-A (0 dups) sin
   regresión. Ataca el caso patológico de docs de merge, ortogonal a §1.3/§1.4.

**Cierre §1:** los levers baratos están agotados. El modo perceptual quedó ~2×
más barato (proxy), 4.5–6.7× en wall-clock (rayon nativo), con cache para docs
de merge, calibrado (τ 68/84/85) y estrictamente mejor en tamaño que la q fija.
El único bloqueador de la promoción universal es el costo serial en docs muy
image-heavy tipo doc-A (~14× vs fija en el Beta wasm single-thread) — eso solo
lo mueve un cambio de códec (§2 bake-off). MRC (§3) queda MATADO por medición
(no aplica a la resolución del corpus, ver §3).

**Lección de medición (NO repetir):** jamás medir calidad con SSIM2 sobre
renders de página — el resampleo desplaza la rejilla sub-píxel y páginas
visualmente idénticas puntúan −4. La garantía válida es in-pipeline (rejilla
alineada). Ídem: nunca comparar encoders a la misma q nominal ni por PSNR
(trellis sacrifica PSNR a propósito); solo iso-perceptual. **Y (nuevo):** la
curva SSIM2(q) NO es monótona en escaneos reales — no asumir que "menor q que
pasa" está bien definido ni que dos búsquedas distintas coinciden.

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

## 3. MRC — Mixed Raster Content · ❌ MATADO por medición (2026-07-20)

**Veredicto: NO aplica a este corpus.** Spike medido (`examples/mrc_spike.rs`,
spec `docs/superpowers/specs/2026-07-20-mrc-spike-design.md`): Sauvola + máscara
G4 (`fax`) + frente constante + fondo JPEG, recompuesto en PDF renderizable y
juzgado a ojo sobre páginas reales. Resultado:
- **La premisa de MRC no se cumple aquí.** El 3–15× de la literatura / Internet
  Archive asume escaneos de **300+ dpi**. Las páginas del corpus son **~120 dpi**
  (1007px para un A4 → texto de ~10px de alto). Binarizar a bilevel a esa
  resolución **destruye el anti-aliasing** que hacía legible el texto pequeño del
  JPEG → salida ~4× más chica pero con el texto **FRAGMENTADO**
  ("Dosihcac ion" en vez de "Dosificación"). MRC no crea resolución que no está.
- Tamaño: ~4× ✓ (cl_p90 formulario, cl_p151 manuscrito). Legibilidad ≥ actual:
  ✗ (bilevel más rugoso que el JPEG anti-aliased; crop lado a lado lo prueba).
  Una limpieza de speckle quita el moteado del fondo pero NO la fragmentación
  del texto (intrínseca a la resolución).
- La estructura MRC **sí renderiza** en poppler (el encoder G4 de `fax`
  funciona, ImageMask válida) — el bloqueo es de CALIDAD, no de plomería.
- **Revivir SOLO si** el corpus migra a escaneos 300+ dpi (archivo formal):
  re-correr `examples/mrc_spike.rs`. No antes: es un subsistema de semanas
  (segmentación robusta, frente de color, clasificador por-página, manejo CMYK)
  para un beneficio que en *este* corpus CUESTA legibilidad.

**Contexto histórico (por qué se creía que era el techo):** el texto nunca
pasaría por DCT → nítido a resolución completa Y archivo 3-15× menor; atacaba la
pérdida #1 medida (resolución: −23.2 puntos SSIM2 de −43.8; ver
`examples/edu_loss.rs`). El spike mostró que a 120 dpi esa "resolución completa"
ya está agotada — el JPEG anti-aliased la aprovecha mejor que un bilevel.

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
