# GemaPDF v1 — Análisis de uso real

**Fecha:** 2026-06-29
**Método:** se corrió `compress()` (vía `examples/usage_report.rs`) sobre un corpus de
**31 PDFs reales** (~283 MB) de documentos cotidianos: órdenes de servicio escaneadas,
informes, formularios, certificados, exportaciones de office. Solo se recogieron métricas
agregadas (tamaños, conteos de acción por imagen, firmas) — **sin contenido**. Los nombres
de archivo se omiten por privacidad.

## Resultado global

| Perfil | Bytes salida / original | Reducción |
|---|---|---|
| `ebook` (q65) | 96.3% | **3.7%** |
| `screen` (q40) | 91.9% | **8.1%** |

**v1 comprime poco los PDFs reales.** Incluso en el perfil más agresivo (`screen`) la
reducción global es ~8%. La calidad JPEG **no** es la palanca: bajar de q65 a q40 solo movió
3.7% → 8.1%.

## Acciones por imagen (5 799 imágenes en el corpus)

| Acción | ebook | screen | Qué significa |
|---|---|---|---|
| Recomprimida | 266 (4.6%) | 880 (15.2%) | re-encodeada más chica |
| **Downsampled** | **0** | **0** | reducción de resolución por DPI |
| Kept | 4 561 | 3 947 | re-encode no daba ganancia → original |
| Skipped | 972 (16.8%) | 972 | no se pudo decodificar |

### Hallazgo 1 — el downsampling por DPI es inerte (0 de 5 799)
Confirmado empíricamente: **ninguna** imagen se redujo de resolución en todo el corpus.
Causa: `process_image` estima el tamaño de display como píxeles@72dpi, así que el DPI
efectivo calculado siempre queda ≤ objetivo y nunca dispara. Es el mayor lever desperdiciado,
sobre todo en escaneos de alta resolución.

### Hallazgo 2 — el 95% de las imágenes quedan intactas (ebook)
Kept + Skipped = 5 533 / 5 799 (95.4%). Solo el 4.6% se recomprime. Dos causas:
- **Kept**: muchas imágenes ya son JPEG razonablemente comprimidos; re-encodear a q65 no
  gana → se conserva el original (correcto, pero no comprime).
- **Skipped (16.8%)**: imágenes en formatos que el decodificador no abre (FlateDecode raw,
  CCITT/JBIG2 de escáner, JPXDecode). Dominan en los documentos escaneados — justo el caso
  de uso estrella. Son la mayor masa de bytes sin tocar.

### Hallazgo 3 — qué SÍ comprime
Los ganadores son PDFs con **JPEG embebidos de fotos** a calidad alta: una familia de
documentos bajó a 62.6% (ebook) / 47.6% (screen). Ahí v1 funciona como se espera.

## Robustez (el motor es sólido)

| Métrica | Resultado |
|---|---|
| PDFs parseados sin crash | **31 / 31** |
| Errores de parseo / pánico | 0 |
| PDFs cifrados | 0 (manejados como error, no crash) |
| **Firmados detectados → original preservado** | **5 / 5** (política Strict, 1 warning c/u) |
| Salidas que crecieron | **0** (el piso documento devolvió el original) |

El motor es **seguro y correcto**: no corrompe, no crece, respeta firmas, no se cae. El
problema de v1 no es la robustez — es la **cobertura de compresión**.

## Conclusión: prioridades de v2 reordenadas por impacto medido

La calidad JPEG no es la palanca (8% techo). Lo que desbloquea compresión real en documentos
reales —y en escaneos en particular— es, en orden de impacto empírico:

1. **Decodificar imágenes no-DCT** (Flate/CCITT/JBIG2 → 972 saltadas, 16.8%). Es la mayor
   masa de bytes intocada en escaneos. *(TODO-v2 item 5)*
2. **Downsampling por DPI real** leyendo el CTM del content stream (0 disparos hoy).
   Enorme en escaneos de alta resolución. *(TODO-v2 item 2)*
3. **Fidelidad grayscale** (DeviceGray sin inflar a RGB) — relevante en escaneos. *(item 1)*

Estas tres juntas son lo que convierte a GemaPDF en un compresor útil para PDFs escaneados.
Ajustar perfiles/calidad NO lo logra.

> Tests de caracterización que fijan estos comportamientos (skip en no-DCT, 0 downsampling):
> `crates/gema-core/tests/usage_characterization.rs`. Cuando v2 los corrija, esos tests se
> actualizan — son el ancla de regresión.
