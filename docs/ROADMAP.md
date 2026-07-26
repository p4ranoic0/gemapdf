# GemaPDF — Roadmap de mejoras futuras

> Registro durable de los levers investigados que requieren inversión grande.
> Cada entrada lleva payoff MEDIDO (no estimado) donde existe, riesgo, y los
> bloques de construcción con sus licencias (constraint del proyecto: sin AGPL).
> Estado al 2026-07-12.

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

## 2. Bake-off de encoder por imagen · ✅ HECHO (2026-07-20, commit 9ca7f12)

Cada imagen del modo perceptual busca la menor q@τ con jpeg-encoder Y
[`mozjpeg-rs`](https://github.com/imazen/mozjpeg-rs) (Rust puro, BSD-3,
`BaselineBalanced` → C0 baseline) y se queda con el output MÁS CHICO que cumple
τ. Seguro por construcción: el re-decode zune de `score()` solo deja elegir
mozjpeg si zune lo abre a ≥τ → jamás corrupto ni más grande (selección ≤
jpeg-encoder).

**Correcciones al spike original (medido sobre TODAS las imágenes, no top-6):**
- El "+15.4% peor en doc-A" era de las top-6 (no representativo). En el doc
  completo mozjpeg gana en **202/232** imágenes.
- "usar mozjpeg a ciegas" es PELIGROSO: en `doc-D` mozjpeg-siempre
  da +18.6% (agranda), pero la selección por-imagen lo protege → −0.4%. Por eso
  el bake-off (min por-imagen) es lo correcto, no cambiar el encoder.

**Medido end-to-end a ebook τ84 vs perceptual sin bake-off:** doc-C
9.78→7.06 MB (**−27.9%**), doc-A 19.58→15.53 (**−20.7%**), doc-F 3.61→3.17
(**−12.1%**). Gate poppler pasado (certificado color CMYK y páginas de fotos
renderizan fiel). CPU ~2× de la búsqueda (§1 la abarató para pagarlo).

mozjpeg-rs es dep OPCIONAL bajo el feature `perceptual` → fuera del árbol wasm
(el Beta no lo paga). El cache §1 sirve igual (resultado = función pura de los
inputs). Detalle de medición en `examples/enc_mozjpeg.rs` (reporta el net de
selección) y baseline de la skill gemapdf-optimize.

## 2.b. 🔴 El lever GRANDE que faltaba: imágenes raw/Flate (2026-07-24)

**Medido contra 9 referencias reales de Ghostscript** (generadas con
`portfolio/scripts/gs-reference.mjs`, mismos args que el worker desplegado).
Corrige el supuesto viejo de que "gema le gana a producción en todo doc con par
de referencia" — eso se midió cuando SÓLO `doc-A` tenía referencia.
**Ghostscript gana en 6 de 9**, y la correlación con el tipo de contenido es
prácticamente perfecta:

| doc | peso en raw/Flate | resultado |
|---|--:|---|
| doc-B2 | ~100% | GS 7.85 vs gema 37.79 MB (**4.8×**) |
| doc-D | 88.4% | GS 9.10 vs gema 11.06 MB |
| doc-B3 | 67.1% | GS 7.93 vs gema 25.01 MB (**3.2×**) |
| doc-E | 21.6% | GS 9.03 vs gema 10.50 MB |
| doc-A | 0.7% | **gema 13.18** vs GS 17.44 MB |
| doc-C | 0.3% | **gema 6.53** vs GS 7.55 MB |

**gema gana donde el contenido es JPEG y pierde donde es raw/Flate.**

**Causa raíz** (`diag_buckets` + `pdfimages -list` sobre `doc-B2`):
las 194 imágenes son raster raw/Flate; `classify()` las manda a `LineArt` →
`Codec::FlateLossless`, así que **se re-comprimen sin pérdida y NO se convierten
a JPEG**: 49.2 → 36.3 MB (−26%). Ghostscript las pasa a JPEG q45 y las
downsamplea (ancho máx 1363 → 857 px): 7.0 MB. Además el ancho máx de la salida
de gema **sigue en 1363 px**: buena parte no se downsampleó tampoco (sólo 67 de
194 quedaron en el bucket `downsampled`).

**Gate visual: NO hay diferencia perceptible.** Crops 2× de la misma página
(memo con texto tecleado) desde ambas salidas son indistinguibles — mismo texto
nítido y legible. Es decir: la política "line-art siempre lossless" está
comprando 4.8× de peso a cambio de una calidad que en este contenido **no se
ve**.

**Qué hacer (sin implementar aún, en orden):**
1. Que el path line-art también downsamplee por DPI efectivo (hoy buena parte
   se salta el resample: investigar por qué `effective_dpi` sale desconocido en
   estos docs).
2. Permitir JPEG en line-art cuando el ahorro es grande, decidiéndolo **por
   imagen con el modo perceptual** (§1/§2 ya dan el arnés: buscar la q que
   cumple τ y comparar contra el Flate lossless; quedarse con el más chico).
   Esto reusa maquinaria existente en vez de inventar heurística nueva.
3. Revisar la discrepancia de conteo: `pdfimages` ve 198 imágenes y el reporte
   de gema emite 97 stats — hay ~100 objetos que no generan stat (¿SMask,
   inline images, XObjects no-Image?).

Reproducir: `npm run gs:ref -- ~/Downloads/doc-A ebook` y después
`gemapdf/scripts/compare-engines.sh ~/Downloads/doc-A ebook 84`.

## 3. MRC — Mixed Raster Content · ❌ MATADO por medición (2026-07-20)

**Veredicto: NO aplica a este corpus.** Spike medido (`examples/mrc_spike.rs`):
Sauvola + máscara G4 (`fax`) + frente constante + fondo JPEG, recompuesto en PDF
renderizable y juzgado a ojo sobre páginas reales. Resultado:
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
