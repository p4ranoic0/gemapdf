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
set -euo pipefail
CEILING="${1:-1384370}"
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
