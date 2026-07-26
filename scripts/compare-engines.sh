#!/bin/zsh
# Compara los modos de compresión de GemaPDF sobre un corpus real y, cuando
# existe, contra la salida real de producción (Ghostscript).
#
#   MODO ACTUAL  = q fija con las perillas del Beta desplegado (lo que hoy
#                  recibe el usuario en la web con el toggle Beta activado).
#                  v2.1 con q fija es BYTE-IDÉNTICO a v2.0/main: el modo
#                  perceptual es opt-in e inerte sin --quality-target.
#   MODO NUEVO   = perceptual + bake-off de encoder por imagen (§1+§2, v2.1).
#                  Sólo CLI: el feature `perceptual` NO se compila en wasm.
#   PRODUCCIÓN   = archivos *_comprimido*.pdf / *_compressed*.pdf del corpus,
#                  que son salidas reales de Ghostscript para ese mismo original.
#
# Uso:
#   scripts/compare-engines.sh [corpus_dir] [perfil] [tau]
#   scripts/compare-engines.sh ~/Downloads/doc-A ebook 84
#
# El TAMAÑO es sólo la mitad del veredicto: para el gate de calidad renderizá
# páginas de las dos salidas y miralas (ver el bloque final que imprime el
# comando pdftoppm listo para copiar).

set -u
CORPUS="${1:-$HOME/Downloads/doc-A}"
PROFILE="${2:-ebook}"
TAU="${3:-84}"

# Perillas del Beta (src/workers/gemapdf-worker.js → MATCHED_KNOBS)
case "$PROFILE" in
  screen)  DPI=50;  Q=25 ;;
  ebook)   DPI=90;  Q=45 ;;
  printer) DPI=175; Q=70 ;;
  *) echo "perfil desconocido: $PROFILE (screen|ebook|printer)"; exit 2 ;;
esac

ROOT="${0:A:h}/.."
GEMA="$ROOT/target/release/gema"
OUT="${TMPDIR:-/tmp}/gema-compare"
mkdir -p "$OUT"

# SIEMPRE recompilar antes de medir. Cargo es incremental (~3s cuando no hay
# cambios), así que el costo es ruido; saltarse este paso porque el binario ya
# existe es lo que hace que un cambio en el fuente se mida con la versión
# VIEJA — y la tabla sale perfectamente creíble siendo mentira.
echo "→ compilando el CLI (con feature perceptual)…"
(cd "$ROOT" && cargo build --release -p gema-cli) || exit 1

# El checkout puede no tener el modo perceptual (p. ej. main): ahí
# --quality-target no existe y el "modo nuevo" fallaría en silencio.
"$GEMA" compress --help 2>&1 | grep -q -- '--quality-target' || {
  echo "ERROR: este checkout no expone --quality-target (¿estás en main?). Cambiá a la rama con el modo perceptual." >&2
  exit 1
}

mb()   { echo "scale=2; $1/1048576" | bc }
pct()  { [[ "$2" -eq 0 ]] && echo "-" || echo "scale=1; $1*100/$2" | bc }
secs() { /usr/bin/time -p "$@" 2>&1 >/dev/null | awk '/^real/{print $2}' }

printf "corpus: %s   perfil: %s (q fija %sdpi/q%s  ·  perceptual τ%s)\n\n" \
  "$CORPUS" "$PROFILE" "$DPI" "$Q" "$TAU"
printf "%-34s %9s %9s %9s %8s %9s %7s %7s\n" \
  ARCHIVO ORIGINAL ACTUAL NUEVO "Δ%" PRODUCC. t_act t_nue
printf '%.0s─' {1..100}; echo

for f in "$CORPUS"/*.pdf; do
  base="${f:t:r}"
  # los *_comprimido*/_compressed* son REFERENCIAS de producción, no entradas
  [[ "$base" == *_comprimido* || "$base" == *_compressed* ]] && continue

  orig=$(stat -f%z "$f")
  a="$OUT/${base}_actual.pdf"; n="$OUT/${base}_nuevo.pdf"
  rm -f "$a" "$n"
  ta=$(secs "$GEMA" compress "$f" "$a" --profile "$PROFILE" --image-dpi "$DPI" --jpeg-quality "$Q")
  # MISMA dpi que el modo actual: la única variable que cambia es el mecanismo
  # de calidad (q fija vs τ perceptual). Si se dejara la dpi por defecto del
  # perfil (150) estaríamos comparando dos cosas a la vez.
  tn=$(secs "$GEMA" compress "$f" "$n" --profile "$PROFILE" --image-dpi "$DPI" --quality-target "$TAU")
  # sin salida = la compresión falló: abortar en vez de reportar números falsos
  if [[ ! -f "$a" || ! -f "$n" ]]; then
    echo "ERROR: falló la compresión de '$base' (actual=$([[ -f $a ]] && echo ok || echo FALLO), nuevo=$([[ -f $n ]] && echo ok || echo FALLO))" >&2
    "$GEMA" compress "$f" "$n" --profile "$PROFILE" --quality-target "$TAU" >/dev/null || true
    exit 1
  fi
  sa=$(stat -f%z "$a"); sn=$(stat -f%z "$n")

  # Referencia de producción (Ghostscript). Se prefiere la canónica
  # `<base>_comprimido.pdf` que genera portfolio/scripts/gs-reference.mjs con
  # los args exactos del worker desplegado; si no está, se acepta cualquier
  # otra salida de producción que haya en el corpus (_comprimido_VIEJO, etc.).
  prod="-"
  for cand in "$CORPUS/${base}_comprimido.pdf"(N) "$CORPUS/${base}_comprimido"*.pdf(N) "$CORPUS/${base}"*_compressed*.pdf(N); do
    [[ -f "$cand" ]] && { prod=$(mb $(stat -f%z "$cand")); break }
  done

  delta=$(echo "scale=1; ($sn-$sa)*100/$sa" | bc)
  printf "%-34.34s %8sM %8sM %8sM %7s%% %8s %6ss %6ss\n" \
    "$base" "$(mb $orig)" "$(mb $sa)" "$(mb $sn)" "$delta" "$prod" "$ta" "$tn"
done

cat <<EOF

Salidas en: $OUT
Δ% = tamaño del modo NUEVO respecto del ACTUAL (negativo = el nuevo comprime más).

GATE DE CALIDAD (obligatorio antes de concluir nada):
  pdftoppm -png -r 150 -f <pág> -l <pág> "$OUT/<archivo>_actual.pdf" /tmp/q_actual
  pdftoppm -png -r 150 -f <pág> -l <pág> "$OUT/<archivo>_nuevo.pdf"  /tmp/q_nuevo
y comparar a ojo (texto chico, sellos, firmas). Un archivo más chico con el
texto roto es una derrota, no una mejora.
EOF
