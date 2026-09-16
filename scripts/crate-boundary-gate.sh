#!/usr/bin/env bash
# Gate de frontera entre crates: el crate de edición no puede arrastrar al de
# compresión ni al revés. Si se cruzan, el aislamiento de peso es ficticio.
#
# El crate de compresión cambia de nombre a mitad del refactor (gema-core pasa
# a gema-compress), así que el nombre se DESCUBRE en vez de asumirse: entre
# medio, asumirlo hacía que `cargo tree` fallara, el grep recibiera salida
# vacía y el gate diera verde sin haber verificado nada.
set -euo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)"

META="$(cargo metadata --no-deps --format-version 1)"

if ! grep -q '"name":"gema-edit"' <<<"$META"; then
  echo "gema-edit todavía no existe; nada que verificar"
  exit 0
fi

if grep -q '"name":"gema-compress"' <<<"$META"; then
  COMPRESS="gema-compress"
elif grep -q '"name":"gema-core"' <<<"$META"; then
  COMPRESS="gema-core"
else
  echo "::error::no encontré el crate de compresión (ni gema-compress ni gema-core)"
  exit 1
fi

fail=0

# Un fallo del propio comando es un fallo del gate, no un pase libre.
if ! edit_tree="$(cargo tree -p gema-edit -e normal 2>&1)"; then
  echo "::error::cargo tree -p gema-edit falló:"; echo "$edit_tree"; exit 1
fi
if ! compress_tree="$(cargo tree -p "$COMPRESS" -e normal 2>&1)"; then
  echo "::error::cargo tree -p $COMPRESS falló:"; echo "$compress_tree"; exit 1
fi

if grep -q "$COMPRESS" <<<"$edit_tree"; then
  echo "::error::gema-edit depende de $COMPRESS"
  fail=1
fi
if grep -q 'gema-edit' <<<"$compress_tree"; then
  echo "::error::$COMPRESS depende de gema-edit"
  fail=1
fi

if [ "$fail" -eq 0 ]; then
  echo "✓ frontera intacta (edición: gema-edit · compresión: $COMPRESS)"
fi
exit "$fail"
