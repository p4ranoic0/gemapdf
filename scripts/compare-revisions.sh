#!/bin/zsh
# Compara una revisión Git de GemaPDF contra el working tree actual usando un
# corpus real. Los originales se abren sólo para lectura y todas las salidas se
# guardan fuera del repositorio, bajo gemapdf-internal-docs.
#
# Uso:
#   scripts/compare-revisions.sh [baseline_ref] [profile] [repetitions] \
#     [corpus_dir] [results_root]
#
# Defaults:
#   baseline_ref = HEAD
#   profile      = ebook
#   repetitions  = 3
#   corpus_dir   = ~/Downloads/doc-A
#   results_root = ../gemapdf-internal-docs/benchmarks
#
# Ejemplos:
#   scripts/compare-revisions.sh
#   scripts/compare-revisions.sh wasm-v0.3.0 ebook 5
#   GEMAPDF_INTERNAL_DOCS=/ruta/privada scripts/compare-revisions.sh HEAD screen 3

set -euo pipefail
setopt null_glob

ROOT="${0:A:h:h}"

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
fi

BASELINE_REF="${1:-HEAD}"
PROFILE="${2:-ebook}"
REPETITIONS="${3:-3}"
CORPUS="${4:-$HOME/Downloads/doc-A}"
INTERNAL_DOCS="${GEMAPDF_INTERNAL_DOCS:-$ROOT:h/gemapdf-internal-docs}"
RESULTS_ROOT="${5:-$INTERNAL_DOCS/benchmarks}"
RUST_TOOLCHAIN="${GEMA_RUST_TOOLCHAIN:-1.97.1}"

for required in git cargo tar stat shasum awk sort seq; do
  if ! command -v "$required" >/dev/null 2>&1; then
    echo "ERROR: falta el comando requerido: $required" >&2
    exit 2
  fi
done
if [[ ! -x /usr/bin/time ]]; then
  echo "ERROR: falta /usr/bin/time" >&2
  exit 2
fi

case "$PROFILE" in
  screen|ebook|printer) ;;
  *)
    echo "ERROR: perfil desconocido: $PROFILE (screen|ebook|printer)" >&2
    exit 2
    ;;
esac

if [[ ! "$REPETITIONS" =~ '^[1-9][0-9]*$' ]]; then
  echo "ERROR: repetitions debe ser un entero positivo" >&2
  exit 2
fi

if [[ ! -d "$CORPUS" ]]; then
  echo "ERROR: no existe el corpus: $CORPUS" >&2
  exit 2
fi

CORPUS="${CORPUS:A}"
RESULTS_ROOT="${RESULTS_ROOT:A}"

# Guardas de integridad: resultados y originales no pueden vivir dentro del
# repositorio ni solaparse entre sí.
if [[ "$RESULTS_ROOT" == "$ROOT" || "$RESULTS_ROOT" == "$ROOT"/* ]]; then
  echo "ERROR: results_root debe estar fuera del repositorio: $ROOT" >&2
  exit 2
fi
if [[ "$RESULTS_ROOT" == "$CORPUS" || "$RESULTS_ROOT" == "$CORPUS"/* || \
      "$CORPUS" == "$RESULTS_ROOT"/* ]]; then
  echo "ERROR: results_root y corpus no pueden solaparse" >&2
  exit 2
fi

git -C "$ROOT" rev-parse --verify "${BASELINE_REF}^{commit}" >/dev/null
BASELINE_COMMIT=$(git -C "$ROOT" rev-parse "${BASELINE_REF}^{commit}")
CANDIDATE_COMMIT=$(git -C "$ROOT" rev-parse HEAD)
SHORT_CANDIDATE="${CANDIDATE_COMMIT[1,8]}"

PDFS=("$CORPUS"/*.pdf(N))
INPUTS=()
for pdf in "${PDFS[@]}"; do
  stem="${pdf:t:r}"
  # Salidas previas de producción son referencias, no entradas del benchmark.
  [[ "$stem" == *_comprimido* || "$stem" == *_compressed* ]] && continue
  INPUTS+=("$pdf")
done

if (( ${#INPUTS[@]} == 0 )); then
  echo "ERROR: no hay PDFs originales en $CORPUS" >&2
  exit 2
fi

RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-${SHORT_CANDIDATE}"
RUN_DIR="$RESULTS_ROOT/$RUN_ID"
if [[ -e "$RUN_DIR" ]]; then
  RUN_DIR="${RUN_DIR}-$$"
fi

mkdir -p \
  "$RUN_DIR/baseline" \
  "$RUN_DIR/current" \
  "$RUN_DIR/logs" \
  "$RUN_DIR/metadata"

TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/gemapdf-revisions.XXXXXX")
cleanup() {
  [[ -n "${TMP_ROOT:-}" && -d "$TMP_ROOT" ]] && rm -rf -- "$TMP_ROOT"
}
trap cleanup EXIT INT TERM

BASELINE_SRC="$TMP_ROOT/baseline-src"
BASELINE_TARGET="$TMP_ROOT/baseline-target"
CURRENT_TARGET="$TMP_ROOT/current-target"
mkdir -p "$BASELINE_SRC"

echo "→ extrayendo baseline ${BASELINE_REF} (${BASELINE_COMMIT[1,12]})"
git -C "$ROOT" archive "$BASELINE_COMMIT" | tar -x -C "$BASELINE_SRC"

echo "→ compilando baseline con Rust $RUST_TOOLCHAIN"
CARGO_TARGET_DIR="$BASELINE_TARGET" \
  cargo "+$RUST_TOOLCHAIN" build \
    --release --locked --manifest-path "$BASELINE_SRC/Cargo.toml" -p gema-cli \
    >"$RUN_DIR/logs/build-baseline.log" 2>&1

echo "→ compilando working tree actual con Rust $RUST_TOOLCHAIN"
CARGO_TARGET_DIR="$CURRENT_TARGET" \
  cargo "+$RUST_TOOLCHAIN" build \
    --release --locked --manifest-path "$ROOT/Cargo.toml" -p gema-cli \
    >"$RUN_DIR/logs/build-current.log" 2>&1

BASELINE_BIN="$BASELINE_TARGET/release/gema"
CURRENT_BIN="$CURRENT_TARGET/release/gema"
if [[ ! -x "$BASELINE_BIN" || ! -x "$CURRENT_BIN" ]]; then
  echo "ERROR: no se generaron ambos binarios" >&2
  exit 1
fi

# Evidencia suficiente para reproducir la ejecución sin copiar los originales.
{
  echo "run_id=$RUN_ID"
  echo "created_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "repository=$ROOT"
  echo "corpus=$CORPUS"
  echo "results=$RUN_DIR"
  echo "baseline_ref=$BASELINE_REF"
  echo "baseline_commit=$BASELINE_COMMIT"
  echo "candidate_commit=$CANDIDATE_COMMIT"
  echo "candidate_dirty=$([[ -n "$(git -C "$ROOT" status --porcelain)" ]] && echo true || echo false)"
  echo "profile=$PROFILE"
  echo "signature_policy=default (flatten)"
  echo "repetitions=$REPETITIONS"
  echo "rust_toolchain=$RUST_TOOLCHAIN"
  cargo "+$RUST_TOOLCHAIN" --version
  rustc "+$RUST_TOOLCHAIN" --version
} >"$RUN_DIR/metadata/run.txt"

git -C "$ROOT" status --short >"$RUN_DIR/metadata/candidate-status.txt"
git -C "$ROOT" diff --binary >"$RUN_DIR/metadata/candidate.patch"

: >"$RUN_DIR/metadata/inputs.sha256"
for pdf in "${INPUTS[@]}"; do
  input_hash=$(shasum -a 256 "$pdf" | awk '{print $1}')
  printf '%s  %s\n' "$input_hash" "${pdf:t}" >>"$RUN_DIR/metadata/inputs.sha256"
done

median_column() {
  local file="$1"
  local column="$2"
  awk -v column="$column" '{ print $column }' "$file" | sort -n | awk '
    { values[NR] = $1 }
    END {
      if (NR % 2 == 1) printf "%.6f", values[(NR + 1) / 2]
      else printf "%.6f", (values[NR / 2] + values[NR / 2 + 1]) / 2
    }
  '
}

measure_once() {
  local label="$1"
  local binary="$2"
  local input="$3"
  local output="$4"
  local iteration="$5"
  local timings="$6"
  local log="$RUN_DIR/logs/${input:t:r}.${label}.${iteration}.log"
  local time_log="$TMP_ROOT/time-${label}-${iteration}.txt"

  if ! /usr/bin/time -lp "$binary" compress "$input" "$output" \
      --profile "$PROFILE" >"$log" 2>"$time_log"; then
    {
      echo
      echo "--- time/stderr ---"
      sed -n '1,240p' "$time_log"
    } >>"$log"
    echo "ERROR: falló $label para ${input:t}; ver $log" >&2
    exit 1
  fi

  local real_seconds
  local peak_rss
  real_seconds=$(awk '$1 == "real" { print $2; exit }' "$time_log")
  peak_rss=$(awk '/maximum resident set size/ { print $1; exit }' "$time_log")
  [[ -n "$real_seconds" ]] || real_seconds=0
  [[ -n "$peak_rss" ]] || peak_rss=0
  printf '%s\t%s\n' "$real_seconds" "$peak_rss" >>"$timings"
}

validate_pdf() {
  local pdf="$1"
  local log="$2"
  if command -v qpdf >/dev/null 2>&1; then
    local qpdf_exit
    if qpdf --check "$pdf" >"$log" 2>&1; then
      echo ok
    else
      qpdf_exit=$?
      # qpdf usa 3 cuando la operación termina con advertencias y 2 para
      # errores. Una advertencia debe quedar registrada, pero no invalidar
      # todo el benchmark.
      [[ "$qpdf_exit" -eq 3 ]] && echo warning || echo fail
    fi
  elif command -v pdfinfo >/dev/null 2>&1; then
    pdfinfo "$pdf" >"$log" 2>&1 && echo ok || echo fail
  else
    echo "no se encontró qpdf ni pdfinfo" >"$log"
    echo not-run
  fi
}

csv_escape() {
  local value="${1//\"/\"\"}"
  printf '"%s"' "$value"
}

md_escape() {
  printf '%s' "${1//|/\\|}"
}

CSV="$RUN_DIR/results.csv"
SUMMARY="$RUN_DIR/SUMMARY.md"
printf '%s\n' \
  'file,input_bytes,baseline_bytes,current_bytes,size_delta_bytes,size_delta_pct,hash_equal,baseline_median_s,current_median_s,time_delta_pct,baseline_peak_rss,current_peak_rss,baseline_valid,current_valid' \
  >"$CSV"

cat >"$SUMMARY" <<EOF
# Comparación de revisiones GemaPDF

- Baseline: \`$BASELINE_REF\` (\`${BASELINE_COMMIT[1,12]}\`)
- Candidate: working tree sobre \`${CANDIDATE_COMMIT[1,12]}\`
- Perfil: \`$PROFILE\`
- Política de firmas: default \`flatten\`
- Repeticiones por versión y archivo: $REPETITIONS
- PDFs originales: ${#INPUTS[@]}

| Archivo | Original | Baseline | Actual | Δ tamaño | Hash igual | t baseline | t actual | Válidos |
|---|---:|---:|---:|---:|:---:|---:|---:|:---:|
EOF

TOTAL_INPUT=0
TOTAL_BASELINE=0
TOTAL_CURRENT=0
IDENTICAL=0
VALID_FAILURES=0
VALID_WARNINGS=0

echo "→ comparando ${#INPUTS[@]} PDFs ($REPETITIONS repeticiones por versión)"
for input in "${INPUTS[@]}"; do
  name="${input:t}"
  stem="${name:r}"
  baseline_output="$RUN_DIR/baseline/$name"
  current_output="$RUN_DIR/current/$name"
  baseline_timings="$TMP_ROOT/${stem}.baseline.tsv"
  current_timings="$TMP_ROOT/${stem}.current.tsv"
  : >"$baseline_timings"
  : >"$current_timings"

  echo "  • $name"
  for iteration in $(seq 1 "$REPETITIONS"); do
    # Alternar el orden reduce el sesgo por caché y temperatura de CPU.
    if (( iteration % 2 == 1 )); then
      measure_once baseline "$BASELINE_BIN" "$input" "$baseline_output" \
        "$iteration" "$baseline_timings"
      measure_once current "$CURRENT_BIN" "$input" "$current_output" \
        "$iteration" "$current_timings"
    else
      measure_once current "$CURRENT_BIN" "$input" "$current_output" \
        "$iteration" "$current_timings"
      measure_once baseline "$BASELINE_BIN" "$input" "$baseline_output" \
        "$iteration" "$baseline_timings"
    fi
  done

  input_bytes=$(stat -f%z "$input")
  baseline_bytes=$(stat -f%z "$baseline_output")
  current_bytes=$(stat -f%z "$current_output")
  size_delta=$(( current_bytes - baseline_bytes ))
  size_delta_pct=$(awk -v current="$current_bytes" -v base="$baseline_bytes" \
    'BEGIN { if (base == 0) print "0.000"; else printf "%.3f", (current-base)*100/base }')

  baseline_hash=$(shasum -a 256 "$baseline_output" | awk '{print $1}')
  current_hash=$(shasum -a 256 "$current_output" | awk '{print $1}')
  if [[ "$baseline_hash" == "$current_hash" ]]; then
    hash_equal=yes
    (( IDENTICAL += 1 ))
  else
    hash_equal=no
  fi

  baseline_time=$(median_column "$baseline_timings" 1)
  current_time=$(median_column "$current_timings" 1)
  baseline_rss=$(median_column "$baseline_timings" 2)
  current_rss=$(median_column "$current_timings" 2)
  time_delta_pct=$(awk -v current="$current_time" -v base="$baseline_time" \
    'BEGIN { if (base == 0) print "0.000"; else printf "%.3f", (current-base)*100/base }')

  baseline_valid=$(validate_pdf "$baseline_output" "$RUN_DIR/logs/${stem}.baseline.validate.log")
  current_valid=$(validate_pdf "$current_output" "$RUN_DIR/logs/${stem}.current.validate.log")
  [[ "$baseline_valid" == fail || "$current_valid" == fail ]] && (( VALID_FAILURES += 1 ))
  [[ "$baseline_valid" == warning || "$current_valid" == warning ]] && (( VALID_WARNINGS += 1 ))

  {
    csv_escape "$name"
    printf ',%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
      "$input_bytes" "$baseline_bytes" "$current_bytes" "$size_delta" \
      "$size_delta_pct" "$hash_equal" "$baseline_time" "$current_time" \
      "$time_delta_pct" "$baseline_rss" "$current_rss" \
      "$baseline_valid" "$current_valid"
  } >>"$CSV"

  printf '| %s | %s | %s | %s | %s%% | %s | %ss | %ss | %s/%s |\n' \
    "$(md_escape "$name")" "$input_bytes" "$baseline_bytes" "$current_bytes" \
    "$size_delta_pct" "$hash_equal" "$baseline_time" "$current_time" \
    "$baseline_valid" "$current_valid" >>"$SUMMARY"

  (( TOTAL_INPUT += input_bytes ))
  (( TOTAL_BASELINE += baseline_bytes ))
  (( TOTAL_CURRENT += current_bytes ))
done

TOTAL_DELTA=$(( TOTAL_CURRENT - TOTAL_BASELINE ))
TOTAL_DELTA_PCT=$(awk -v current="$TOTAL_CURRENT" -v base="$TOTAL_BASELINE" \
  'BEGIN { if (base == 0) print "0.000"; else printf "%.3f", (current-base)*100/base }')

cat >>"$SUMMARY" <<EOF

## Totales

- Entrada: $TOTAL_INPUT bytes
- Baseline: $TOTAL_BASELINE bytes
- Actual: $TOTAL_CURRENT bytes
- Diferencia: $TOTAL_DELTA bytes ($TOTAL_DELTA_PCT%)
- Outputs byte-idénticos: $IDENTICAL/${#INPUTS[@]}
- Fallos de validación: $VALID_FAILURES
- Archivos con advertencias de validación: $VALID_WARNINGS

Los originales no fueron copiados ni modificados. Sus hashes están en
\`metadata/inputs.sha256\`. Los PDFs derivados, logs, parámetros y el patch del
working tree están contenidos exclusivamente en esta carpeta de ejecución.
EOF

echo
echo "✓ comparación terminada"
echo "  resumen: $SUMMARY"
echo "  datos:   $CSV"
echo "  salidas: $RUN_DIR"

if (( VALID_FAILURES > 0 )); then
  echo "⚠️  hubo $VALID_FAILURES archivo(s) con validación fallida" >&2
  exit 1
fi
