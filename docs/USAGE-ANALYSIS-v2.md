# GemaPDF v2.0 — Medición de uso real

**Fecha:** 2026-07-05
**Método:** se corrió `compress()` con perfil `ebook` (vía `examples/usage_report.rs`,
el mismo harness de la medición v1) sobre un corpus de **37 PDFs reales** de un corpus
personal: órdenes de servicio escaneadas, informes, formularios, declaraciones. Solo se
recogieron métricas agregadas (tamaños, conteos de acción por imagen, firmas) — **sin
contenido**. Los nombres de archivo se omiten por privacidad.

> El corpus evolucionó desde la medición v1 (31 PDFs / ~283 MB, ver
> [`docs/USAGE-ANALYSIS.md`](USAGE-ANALYSIS.md)) — no es el mismo conjunto de archivos,
> así que estos números **no son directamente comparables** contra los de v1. Sirven
> para caracterizar v2.0 en su propio corpus, no para calcular un "antes/después" preciso
> archivo por archivo.

## Resumen (salida verbatim de `usage_report`)

```
archivos:        37
comprimidos ok:  37
firmados:        8  (Strict → se conserva original)
cifrados:        0
parse_error:     0
crecieron(piso): 0  (se devolvió el original)
bytes orig:      359736592
bytes out:       232879814
ratio global:    64.7% del original
--- acciones de imagen (totales) ---
recomprimidas:   341
downsampled:     1842   <-- clave para validar el caveat de DPI
kept (sin gano): 3184
skipped:         701
```

37/37 PDFs se comprimieron sin error; 8 estaban firmados y se conservaron intactos
(política `Strict`, sin romper la firma); 0 crecieron por encima del original (el piso
de "devolver el original si no hay ganancia" funcionó en todos los casos).

## Qué cambió vs v1

- **Downsampling por DPI real ahora dispara.** v1 medía 0 downsamples en 5 799 imágenes
  (el estimado de DPI era inerte). v2.0 lee el CTM del content stream para el DPI
  efectivo real, y en este corpus disparó en **1 842 imágenes** — la mayor palanca de
  la mejora de ratio.
- **Decodificación de imágenes Flate/predictor/colorspace.** Además de DCT/PNG, v2.0
  decodifica zlib crudo con de-filtrado de predictor PNG/TIFF, cadenas
  `ASCIIHex`/`ASCII85`/`RunLength`, y colorspaces `Indexed`/`ICCBased`/`DeviceCMYK` —
  antes esas imágenes caían en `Skipped`.
- **Object streams vía `lopdf` 0.43**, para PDFs que empaquetan objetos en
  `/ObjStm` (más cobertura de parseo en PDFs modernos generados por herramientas de
  oficina).
- **Gray→luma en salida JPEG.** Escaneos en `DeviceGray` ya no se infla a RGB de 3
  canales antes de recodificar — se codifica directamente como JPEG en escala de
  grises, evitando bytes desperdiciados en canales redundantes.

Con estos cuatro cambios el ratio global pasa de un solo dígito (v1: 3.7–8.1%) a
**64.7% del original** (~35% de reducción) en este corpus — consistente con el análisis
de v1 de que DPI real y cobertura de decodificación eran los levers desperdiciados, no
la calidad JPEG.
