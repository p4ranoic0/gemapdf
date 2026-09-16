#!/usr/bin/env bash
# Gate de frontera entre crates. gema-edit no puede arrastrar a gema-compress
# ni al revés: si se cruzan, el aislamiento de peso es ficticio.
# Antes de que exista gema-edit, este script no falla: informa y sale 0.
set -euo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)"
fail=0
if cargo metadata --no-deps --format-version 1 | grep -q '"name":"gema-edit"'; then
  if cargo tree -p gema-edit -e normal | grep -q 'gema-compress'; then
    echo "::error::gema-edit depende de gema-compress"; fail=1
  fi
  if cargo tree -p gema-compress -e normal | grep -q 'gema-edit'; then
    echo "::error::gema-compress depende de gema-edit"; fail=1
  fi
  [ "$fail" -eq 0 ] && echo "✓ frontera intacta"
else
  echo "gema-edit todavía no existe; nada que verificar"
fi
exit "$fail"
