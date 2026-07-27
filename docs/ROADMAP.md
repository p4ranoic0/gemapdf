# GemaPDF — Roadmap de mejoras futuras

> Registro durable de los levers investigados que requieren inversión grande.
> Cada entrada lleva payoff MEDIDO (no estimado) donde existe, riesgo, y los
> bloques de construcción con sus licencias (constraint del proyecto: sin AGPL).
> Estado al 2026-07-27.

## Estado actual (para ubicarse)

**Se trabaja sobre `main`.** La rama `v2.1` se retiró el 2026-07-27: todo su
contenido está en `main` y mantenerla como "rama activa" ya sólo confundía.
Ramas vivas: `main` (código) y `npm` (distribución generada por `wasm-pack`,
sin fuente). `v2.0-levers`, `v2.0-portable` y `exp/save-modern` son anclas
históricas: no se les commitea.

Qué corre hoy en el Beta — **gema-wasm 0.3.0** (tag `wasm-v0.3.0`, rama `npm`):

- **Levers v2.0**: A (cadena Flate→DCT), B (encoder 4:2:0), C (/SMask),
  `reflate_streams`.
- **Clasificador de papel escaneado** (§2.b, commit 9064b60): el raster
  guardado sin pérdida ya no se queda en Flate. Contra las 9 referencias reales
  de Ghostscript, da vuelta el marcador de **perdía 6 de 9** a **gana 6 de 9**.
- **Perillas de transcodificado** (§2.b, commit 5f649e3): `transcode_dpi 110` /
  `transcode_quality 30`, **sólo en `ebook`** — en `printer` el dpi es inerte y
  en `screen` el hallazgo se invierte. Verificado en el navegador:
  `doc-B2` 50.91 → 7.23 MB (−85.8%).

Qué NO corre en el Beta:

- **Modo perceptual** `--quality-target`: COMPLETO pero EXPERIMENTAL (opt-in,
  CLI-only, feature `perceptual`, wasm blindado). Bloqueado por costo CPU en el
  Beta wasm (ver §1). En NATIVO el bucle de imágenes ya va en paralelo
  (§1.4: 4.5–6.7× byte-idéntico). **Ojo con el framing de §2:** ese −12/−28%
  está medido contra *perceptual sin bake-off*, NO contra el modo desplegado;
  medido contra la q fija real empata o pierde en la mitad del corpus
  (doc-A +7.7%, doc-D +8.0%) pagando 5–26× de CPU. Antes de
  retomarlo conviene decidir si justifica su complejidad.

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
   El experimento se revirtió y no llegó a integrarse (el "binario de v2.1" de
   arriba es el código que hoy vive en `main`). Rehacer sólo si aparece un
   corpus con q* demostrablemente estable (±2) Y curvas monótonas — improbable.
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

## 2.b. ✅ RESUELTO: imágenes raw/Flate iban a lossless (2026-07-24 → 07-26)

**Medido contra 9 referencias reales de Ghostscript** (generadas con
`portfolio/scripts/gs-reference.mjs`, mismos args que el worker desplegado).
Corrigió el supuesto viejo de que "gema le gana a producción en todo doc con par
de referencia" — eso se midió cuando SÓLO `doc-A` tenía referencia.
**Ghostscript ganaba en 6 de 9**, con correlación casi perfecta con el tipo de
contenido; tras el fix (commit 9064b60) **gema gana 6 de 9**:

| doc | peso en raw/Flate | antes | después | GS |
|---|--:|--:|--:|--:|
| doc-B2 | ~100% | 37.79 (**4.8×** peor) | **6.61** ✅ | 7.85 |
| doc-D | 88.4% | 11.06 | **7.94** ✅ | 9.10 |
| doc-B3 | 67.1% | 25.01 (**3.2×** peor) | **7.45** ✅ | 7.93 |
| doc-E | 21.6% | 10.50 | 10.50 🔴 | 9.03 |
| doc-A | 0.7% | **13.18** ✅ | 13.18 ✅ | 17.44 |
| doc-C | 0.3% | **6.53** ✅ | 6.53 ✅ | 7.55 |

**Causa raíz** (`diag_buckets` + `pdfimages -list` sobre `doc-B2`):
el raster raw/Flate lo mandaba `classify()` a `LineArt` → `Codec::FlateLossless`,
así que **se re-comprimía sin pérdida y nunca pasaba a JPEG**: 49.2 → 36.3 MB
(−26%) contra 7.0 MB de Ghostscript. Una página escaneada es tonalmente pobre
(62 colores cuantizados) y por eso caía del lado line-art, pero NO es línea
sintética.

**Gate visual: NO hay diferencia perceptible.** Crops 2× de la misma página
(memo con texto tecleado) desde ambas salidas son indistinguibles. La política
"line-art siempre lossless" estaba comprando 4.8× de peso a cambio de una
calidad que en este contenido **no se ve**.

### Qué se hizo

**Fix (commit 9064b60):** `classify()` suma una señal de GRANO — fracción de
vecinos horizontales con `2 <= |Δluma| < 32`, la huella del ruido de sensor que
un escaneo tiene en toda su superficie y el arte sintético no (éste alterna
regiones exactamente planas con bordes duros, y ninguna cuenta). Umbrales
medidos, no elegidos a ojo: **grano ≥ 0.20** (8 páginas escaneadas completas dan
0.336-0.422; el mayor sintético del corpus, firma institucional 1436×340, da
0.091) y **lado menor ≥ 400 px** (el grano NO separa emblemas vectoriales
chicos: el escudo del Perú a 110×112 da 0.356, dentro del rango de los
escaneos). Los 7 docs restantes del corpus quedan byte-idénticos: el path DCT no
se toca.

**Diverge del plan original (decidirlo por imagen con el modo perceptual):** esa
maquinaria es CLI-only (feature `perceptual` fuera del árbol wasm), así que no
puede llegar al Beta, que es donde estaba la pérdida. La señal de grano corre en
el path por defecto.

### Calibración: dos regímenes, no uno (commit 5f649e3)

La q global no era la perilla correcta. Lo que cambia entre un escaneo que
llega SIN pérdida y uno que ya venía en JPEG es **cómo conviene repartir los
bytes**:

- **Primera generación** (raster Flate → JPEG): la fuente está intacta, así que
  rinde más gastar en RESOLUCIÓN que en cuantización.
- **Segunda generación** (ya en DCT): carga artefactos de anillo que la
  cuantización extra compone, y subir el dpi sólo preserva esos artefactos.

Medido sobre `doc-B2` (97% del peso en Flate), contra 7.85 MB de
Ghostscript: 90/q45 = 6.61 MB (firma por debajo de producción) · 90/q65 =
7.72 MB (≈ producción) · **110/q30 = 7.23 MB (≈ producción y MÁS nítida que
90/q65 → domina)**. Ghostscript resultó estar en 110 dpi: su imagen de la
pág. 11 mide 787×1210 sobre 515.231 pt; el Beta le daba 90.

`transcode_dpi` / `transcode_quality` (opt-in, `None` = inerte) aplican sólo
cuando la fuente llegó sin pérdida Y sale como JPEG. Con 110/30: ad2 6.61→7.23,
ad3 7.45→**7.23** (más chico Y mejor), doc-D 7.94→8.05; doc-A,
doc-C, doc-E y doc-B1 **byte-idénticos**.

**Sólo aplica a `ebook`.** Los otros dos perfiles se midieron el 2026-07-26 y
quedan SIN perillas de transcodificado, por razones distintas:

- **`printer` (175/70): el dpi es INERTE en este corpus.** Los escaneos están a
  150 dpi efectivos, por debajo del objetivo 175 → no hay downsampling que
  canjear. Medido: 175, 200 y 250 dpi dan 14.66/14.67/14.67 MB y el ancho máx
  se queda en 1363 px (el original). No existe el trade que calibramos.
- **`screen` (50/25): el hallazgo se INVIERTE.** A iso-tamaño (50/q25 = 3.00 MB
  vs 60/q12 = 2.96 MB), la firma manuscrita sale MEJOR con la perilla actual:
  a q12 el bloqueo se come los trazos y la fecha se vuelve manchas. Hay un piso
  de q y screen ya está cerca. El bloque de firma digital es ilegible en TODAS
  las variantes (a 50-60 dpi el texto de 6 pt no sobrevive), así que ahí el
  criterio de legibilidad ni siquiera puede arbitrar.

**Lección:** la asignación óptima depende del punto de operación. No extrapolar
la proporción de ebook (dpi ×1.22, q ×0.67) a otros perfiles — medida en los
tres, sólo se sostiene en uno.

### Falsos leads, cerrados por medición (2026-07-26) — NO reabrir

1. ~~"El path line-art no downsamplea; investigar por qué `effective_dpi` sale
   desconocido"~~ — **la premisa era falsa.** El content stream pinta la imagen
   directo (`q 515.231 0 0 792 48.38452 0 cm /Im15 Do Q`) y con 1076×1654 px da
   **150.4 dpi**, perfectamente derivable. A 90 dpi, `diag_buckets` da **67 de
   97** XObjects downsampleados, que concentran **49.11 de los 49.12 MB** de
   imagen: el resample se aplica sobre todo el peso. El "sólo 67 de 194"
   comparaba los 97 stats de gema contra las 198 filas de `pdfimages` — unidades
   distintas. El `ppi=0` de `pdfimages` es artefacto de esa herramienta.
2. ~~"El ancho máx de la salida sigue en 1363 px"~~ — es **una `/SMask`, por
   diseño**. Objeto compartido `obj 164` (1363×60) reusado en páginas 17-24; su
   base sí se downsampleó, la máscara no, porque `process.rs` desactiva el
   resample en máscaras para no mover valores de transparencia. Pesa 0.01 MB.
3. ~~"~100 objetos no generan stat"~~ — es diferencia de unidades: `pdfimages`
   emite una fila por *colocación en página* (136 image + 62 smask = 198); gema
   un stat por *XObject* (82 bases + 15 máscaras = **97**). Cuadra exacto: no
   hay objetos perdidos ni inline images sin contabilizar.

Reproducir: `npm run gs:ref -- ~/Downloads/doc-A ebook` y después
`gemapdf/scripts/compare-engines.sh ~/Downloads/doc-A ebook 84` (desde el
commit 57e30ac el arnés recompila siempre: antes podía medir un binario viejo y
emitir una tabla creíble pero falsa).

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
