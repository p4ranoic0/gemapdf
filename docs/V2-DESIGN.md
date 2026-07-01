# GemaPDF v2 — Diseño / Roadmap de compresión de imágenes

**Fecha:** 2026-07-01
**Base empírica:** `docs/USAGE-ANALYSIS.md` (v1 comprime ~4–8% global; los escaneos —caso
de uso principal— casi nada, porque sus imágenes son no-DCT y de alta resolución).

## Modelo mental (dos palancas)

Comprimir un PDF es, sobre todo, comprimir sus imágenes. Dos palancas de impacto distinto:

1. **Resolución (píxeles) — CUADRÁTICA.** 300→150 DPI = 4× menos píxeles. La palanca grande.
2. **Codec por contenido — lineal.** El mejor codec depende del contenido:
   foto→JPEG/WebP/AVIF · texto/línea→PNG/Flate · escaneo B/N→CCITT/JBIG2 · grises→JPEG luma.

Un escaneo de texto es **texto encima de un fondo**, no "una imagen": tratarlo como un solo
raster JPEG es el error (la DCT es pésima con bordes → pesado y borroso).

## Niveles (orden lógico de impacto)

| Nivel | Qué | Gana en | Portabilidad |
|---|---|---|---|
| 0 (v1, hecho) | recomprime cada imagen decodificable | poco (escaneos casi nada) | WASM + nativo |
| **1 (v2.0)** | **DPI real (CTM) + decodificar Flate** | TODO lo rasterizable, JPEG grandes | **WASM + nativo, sin AGPL** |
| 2 (v2.x) | codec por contenido + calidad perceptual (JND) | grises 3×, línea sin halos | mayormente portable |
| 3 (v2.1) | **MRC (segmentación por capas) + JBIG2 doc-dict** | **el salto grande en escaneos (10–20×)** | solo-nativo (power-up) |

## Milestone v2.0 — Nivel 1 (portable, siguiente)

Objetivo: que el downsampling **funcione** (hoy inerte, 0 disparos) y abrir las imágenes Flate.

### P2 — Downsampling por DPI efectivo (leer el CTM)
Una imagen se dibuja sobre el cuadrado unitario transformado por la matriz actual (`… cm /Im0 Do`).
Tamaño en página (pt) = escala del CTM; `dpi = px / (pt/72)`.

Algoritmo (mini-intérprete del content stream por página):
- mantener pila de CTM con `q`/`Q`/`cm`;
- en cada `Do` de un XObject imagen, derivar `w_pt=hypot(a,b)`, `h_pt=hypot(c,d)`, `dpi=px/(pt/72)`;
- mapear el nombre de recurso (`/Im0`) → ObjectId vía `/Resources /XObject` de la página;
- por imagen guardar el **DPI máximo** entre todos sus usos;
- en `process_image`, usar el tamaño de display real en vez de px@72dpi → downsample si `dpi > objetivo`.

Impacto: cuadrático y aplica también a los JPEG ya decodificables que hoy quedan *Kept*.
Invierte el test ancla `large_image_is_not_downsampled_in_v1`.

### P1-Flate — decodificar imágenes FlateDecode
Descomprimir el stream (zlib) e interpretar los píxeles crudos según `ColorSpace` +
`BitsPerComponent` → recomprimir. Abre parte del 16.8% saltado, con crate puro-Rust (`flate2`,
ya presente). CCITT/JBIG2 quedan para v2.1 (nativo).

**Licencia/portabilidad v2.0:** 100% puro-Rust, WASM-safe, sin AGPL. Se mantiene el objetivo de v1.

## Milestone v2.1 — Nivel 3: MRC nativo (modo "alta compresión")

Segmentación por capas (estilo DjVu / "alta compresión" de Adobe/iLovePDF):
- **Máscara** bilevel (dónde hay texto/líneas) → **JBIG2** (símbolos repetidos) → KB.
- **Frente** (color del texto) → baja resolución.
- **Fondo** (foto/papel) → JPEG baja resolución.
- Recomposición: fondo + frente recortado por la máscara (tres XObjects apilados con imagemask;
  PDF estándar, lo lee cualquier visor).

Multiplicadores para documentos administrativos repetitivos (los "OS …" del corpus):
- **Diccionario de símbolos JBIG2 a nivel documento** (no por página): glifo/logo/sello se guardan
  una vez para todo el PDF; N páginas los referencian.
- **Dedup de imágenes idénticas** (implementar el `dedupe_images` hoy no-op): membrete repetido → 1 objeto.

**Portabilidad/licencia:** pesado (segmentación + rasterización + JBIG2) → **solo-nativo**
(desktop/servidor). La web se queda en Niveles 1–2. Verificar licencia del encoder JBIG2 elegido
antes de integrarlo.

## Métrica de éxito
Re-correr `examples/usage_report.rs` sobre el corpus real tras cada nivel. Meta v2.0: que los
PDFs con imágenes de alta resolución bajen sensiblemente (hoy ~100%). Meta v2.1: escaneos a
color/B-N en el rango 10–20× del original.
