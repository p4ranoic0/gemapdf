#!/usr/bin/env bash
# Gate de tamaño del paquete WASM. El techo se pasa por argumento o sale del
# valor por defecto.
#
# Medido el 2026-09-16, con `lto = "fat"` y `codegen-units = 1`:
#
#   antes de separar, sin LTO ... 1519364   (el techo viejo)
#   después de separar, sin LTO . 1537665   (+18301)
#   antes de separar, con LTO ... 1368389
#   después de separar, con LTO . 1384354   (+15965)
#   tras renombrar el crate ..... 1384370   (+16)     <- el techo de hoy
#
# Esos 16 bytes finales no son código: `gema-compress` tiene ocho caracteres
# más que `gema-core`, y el nombre del crate viaja dentro del .wasm en los
# símbolos y la metadata de wasm-bindgen.
#
# Dos conclusiones que conviene no perder. Primera: separar un crate en dos
# SÍ cuesta bytes —unos 16 KB de genéricas de lopdf instanciadas de los dos
# lados— y ni el LTO más agresivo las deduplica; la idea de que mover código
# entre crates es gratis quedó refutada por medición. Segunda: activar LTO
# saca 151 KB, así que el bundle queda más chico que antes de que existiera
# el borrado de texto.
#
# Medido el 2026-09-17, contrato de edición (plan 2026-09-16-contrato-y-semantica-edicion);
# ratificado por HG el 2026-09-17:
#
#   commit    tarea                                      wasm crudo   Δ     gzip-9   Δ gzip
#   d9d2c77   baseline                                   1384370      —     551171   —
#   a805f25   4 lector acotado + flate2                  1391670   +7300     554237  +3066
#   8ecc4c4   5 tipos de inspección (código muerto)      1391670      0     554237      0
#   bf561bf   7 inspección cableada                      1425868  +34198     567704 +13467
#   434a7ac   8 firmas + modified                       1427678   +1810     568083   +379
#   f7be4f9   9 informe serializable en WASM             1430906   +3228     569280  +1197
#   96453c9   10 CLI (no entra al wasm)                  1430906      0     569280      0
#   4b0e917   11 separador por índice                    1430909      3     569305    +25
#   uncommitted reemplazo por reuso de códigos (Fase 1)  1489459  +58550     595009 +25704
#   uncommitted caché y filtro del barrido                 1490035    +576     595092    +83
#   selección TJ multioperando + riesgo semántico          1492639   +2604     596304  +1212
#
# El grueso (+34198) entra cuando la inspección se conecta al borrado en la Task 7;
# en la Task 5 el compilador la descartaba por código muerto.
set -euo pipefail
CEILING="${1:-1492639}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/crates/gema-wasm"
wasm-pack build --target web >/dev/null 2>&1
WASM="pkg/gema_wasm_bg.wasm"
[ -f "$WASM" ] || { echo "::error::no se construyó $WASM"; exit 1; }
RAW=$(wc -c < "$WASM" | tr -d ' ')
GZ=$(gzip -9 -c "$WASM" | wc -c | tr -d ' ')
if command -v brotli >/dev/null 2>&1; then
  BR=$(brotli -c "$WASM" | wc -c | tr -d ' ')
else
  BR="n/d (brotli no instalado)"
fi
echo "wasm crudo : $RAW bytes (techo $CEILING)"
echo "wasm gzip-9: $GZ bytes"
echo "wasm brotli: $BR bytes"
if [ "$RAW" -gt "$CEILING" ]; then
  echo "::error::el .wasm creció $((RAW - CEILING)) bytes por encima del techo"
  exit 1
fi
echo "✓ tamaño dentro del techo"
