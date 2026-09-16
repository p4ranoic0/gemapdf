#!/usr/bin/env bash
# Gate de tamaño del paquete WASM. El techo se pasa por argumento o sale del
# valor por defecto, que es el tamaño medido en la 0.6.0 antes de separar los
# crates. Mover código entre crates no puede mover un byte de este número.
set -euo pipefail
CEILING="${1:-1519364}"
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
