# Changelog

All notable changes to GemaPDF are documented in this file. The project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Fixed

- README: the WASM bundle size quoted 1,519,364 bytes, which was an
  intermediate build without LTO. The `gema_wasm_bg.wasm` published as
  `wasm-v0.6.0` measures 1,430,909 bytes (569,305 with `gzip -9`).

### Medido y descartado

- **Exporting `max_pages` / `max_objects` / `max_total_work_bytes` /
  `max_stream_bytes` and cancellation through `gema-wasm`.** Cancellation in the
  browser already works by terminating the worker (a synchronous `.wasm` call
  cannot poll a flag without `SharedArrayBuffer`). For the limits, four
  synthetic adversarial PDFs were run through the deployed site build: 20,000
  pages (OK, 1.5 s), 500,000 objects (OK, 144 s), a 205 KB file whose content
  stream inflates to 200 MB (OK, 1.0 s) and 300 flat 4000×4000 images, about
  14 GB decoded (OK, 115 s, renderer under 1 GB thanks to the 256 MiB batches).
  None crashed or hung, and the real corpus already reaches 375 pages and
  31,483 objects, so a tight limit would reject real documents. No 0.7.0.

## 0.6.0 — 2026-09-17

### Changed (breaking)

- `gema-compress` is the compression engine formerly named `gema-core`; the
  `gema-core` 0.5.0 package is the last release under that name. The engine
  remains pure Rust and is shared by native and WebAssembly targets.
- `gema-edit` introduces the public editing API: `TextRegion`,
  `RemovalStatus`, `RemovalResult`, `remove_text_glyphs`, and
  `remove_text_glyphs_with(…, &EditOptions)`. `removed` reports what was
  rewritten; it does not claim that the region is clean.
- `gema-edit`: `EditError` uses English messages and gains
  `LimitExceeded(LimitKind)`. `RemovalResult` exposes `residual_risks`,
  `inspection_incomplete`, `inspection_gaps`, `signature`, `modified`, and
  `not_inspected`, with a versioned JSON report (`schema_version = 1`).
- `gema-wasm`: the editing export is `remove_text_glyphs` and returns the
  report v1; existing consumers must migrate before using this release.
- `gema-cli`: usage errors from `compress` and `analyze` exit with code 1;
  `remove-text` uses 0 for guaranteed output, 1 for an error, 2 for written
  but not guaranteed, and 3 when the report cannot be printed.

### Added

- `gema-edit`: bounded stream reading and conservative residual-risk
  inspection. `EditOptions` limits input bytes, regions, decompressed bytes,
  total decompressed bytes, content operations, and object traversal. Flate
  Decode has a bounded raw-deflate fallback; unsupported filters and gaps are
  reported instead of silently treated as safe.
- Text removal preserves the rest of a line with equivalent `TJ` displacement,
  never rewrites glyphs outside a requested region, and restores the original
  page if re-interpretation finds an unrelated glyph move or code change.
  Shared streams, Form XObjects, annotations, optional content, `ActualText`,
  and signature indicators are reported. Document metadata, XMP and the other
  surfaces listed in `not_inspected` are never inspected, so an empty
  `residual_risks` does not mean the document is clean; this is not a
  redaction primitive.
- `gema-cli`: `remove-text <in> <out> --region [<id>@]<page>:<x>,<y>,<w>,<h>
  [--json]`.
- The compression and editing engines are separate crates; the WebAssembly
  size gate and crate-boundary gate are part of CI.

### Measured

- WASM release package (`wasm-pack build --target web`): **1,413,929 → 1,430,909
  bytes (+17 KB)** against 0.5.0. Link-time optimization, never configured
  before, removes about 151 KB; text editing and residual-risk inspection add
  the rest (per-commit breakdown in `scripts/wasm-size-gate.sh`). A first
  version of the editor that decoded fonts through lopdf's
  `get_font_encoding` pulled in its glyph-name `match` (thousands of arms):
  +889 KB, and the debug module was rejected by V8 with "too many locals".
  Own static tables replace it.

### Verified

- Compression output is byte-identical to `d9d2c77` (before the editing work)
  on the real corpus: 0 bytes of difference, 12/12 identical outputs.
- The bounded stream reader matches lopdf on every page of that corpus: 2,139
  identical pages, 0 different, 0 errors.
- The editing work adds 46,539 raw bytes (+3.4 %) and 18,134 gzip-9 bytes to
  the `.wasm` against `d9d2c77`; the ceiling of 1,430,909 bytes was measured
  and ratified.

## 0.5.0 — 2026-08-29

### Added

- Optional whole-document limits for page count, object count, and estimated
  total image work, with typed `GemaError::LimitExceeded` failures and
  `LimitKind` identifiers. A rejected document never returns partial output.
- `compress_with_control` and the `CancelSignal` trait for cooperative
  cancellation between phases and image batches. Cancellation returns the typed
  `GemaError::Cancelled` error.
- Adversarial coverage for huge page trees, deeply nested objects, absurd image
  dimensions, and oversized inflated streams, plus a fuzz target combining all
  limits with pseudo-random cancellation.
- Structural telemetry behind the non-default `telemetry` cargo feature: counts
  inline images, images reachable only inside Form XObjects, ExtGState
  luminosity soft masks, and uninspectable resources. Consumed by the internal
  `usage_report` example.

  It is **not** stable surface: it lives outside `Report` and the JSON schema,
  is absent from the default build, and changes no output byte. A metric is
  promoted to the contract only if it is shown to drive a decision.

### Changed

- **Incompatible Rust API change:** exhaustive `CompressOptions` gains
  `max_pages`, `max_objects`, `max_stream_bytes`, and `max_total_work_bytes`.
  This requires the semver-0.x bump from 0.4.0 to 0.5.0.
- The existing 256 MiB stream-inflation defense is configurable through
  `max_stream_bytes`. Streams that exceed it remain intact and produce one
  summary warning; `None` preserves the prior 256 MiB default.
- With all new limits unset and no cancellation, output remains byte-identical
  on the 13-document regression corpus. Median measured overhead versus
  `b8a818f` was **+0.34%** across eight counterbalanced rounds (acceptance
  threshold: at most 2%).
- The lockfile updates `chacha20` from 0.10.1, which was yanked, to 0.10.2 via
  `lopdf 0.43` and `rand 0.10.2`. No manifest changed.

### Measured and rejected

- Slice D did not open Slice E. Form XObjects passed the written gate, but the
  previously completed production A/B measured **0 bytes of benefit**. Inline
  images represented **0.062%** of the corpus input. Of eight ExtGState
  luminosity soft masks, seven remained byte-identical and the only recompressed
  case showed no visual loss in the measured render (maximum delta: 1/255).

## 0.4.0 — 2026-08-29

### Added

- Esquema JSON versionado del reporte (`report_schema_version`), disponible en
  la CLI mediante `--json` y `--json-images`.
- `document.flattened_signatures` en el esquema JSON: firmas o sellos aplanados
  al contenido de página. Aditivo — `report_schema_version` sigue en 1.

### Removed

- `GemaError::SignedDocument`, que la librería nunca construía. La política
  `Strict` no falla: devuelve el documento intacto, y eso se observa con
  `Report::is_signed` más una salida idéntica a la entrada. Publicar un error
  imposible obligaría a los consumidores a manejarlo para siempre.

### Changed

- **Incompatible para consumidores del binding WASM:** el reporte pasa al
  esquema versionado compartido con la CLI y desaparece `JsReport`. Los campos
  planos se mueven así: `is_signed` → `document.is_signed`,
  `images_preserved` → `images.by_action.preserved` y `flattened_signatures` →
  `document.flattened_signatures`; los contadores por acción pasan a
  `images.by_action.*`, y `warnings` deja de ser un array de strings para pasar
  a objetos `{ kind, object_id?, message }`. `portfolio` necesita actualizar
  `src/components/pdf-tools/compressJob.js` y `src/pages/pdf/Aplanar.jsx` antes
  de tomar esta versión.
- `Profile` y `SignaturePolicy` implementan `Display` y `FromStr`. La CLI y el
  binding WASM delegan en core en vez de repetir el mapeo de nombres; `custom`
  pasa a ser un perfil seleccionable por nombre en ambos.
- Los enums públicos que el pipeline hace crecer — `ImageSkipReason`,
  `Warning`, `GemaError`, `Phase`, `ImageAction` — son `#[non_exhaustive]`:
  agregar variantes deja de ser un cambio incompatible. `Profile`,
  `SignaturePolicy` y las structs de opciones/reporte siguen siendo
  exhaustivas a propósito.
- Toda la superficie pública de `gema-core` está documentada y el crate
  activa `#![warn(missing_docs)]`, que el clippy de CI convierte en error.
- Los tests de `pipeline.rs` se reorganizaron por dominio bajo `pipeline/tests/`
  (`compression`, `masks`, `memory`, `progress`, `signatures` y los `fixtures`
  compartidos). Refactor mecánico: el código productivo quedó byte-idéntico —
  el diff de `pipeline.rs` es una sola línea — y la salida sobre el corpus sigue
  byte-idéntica 11/11.

### Medido y descartado

- **Form XObject recursion for effective DPI.** It was fully implemented and
  measured on the real corpus: **0 bytes of difference and 11/11 byte-identical
  outputs** across 537 MB of input, with no validation failures. Production code
  was reverted because that A/B measured zero compression benefit, leaving the
  conclusion and a characterization test for the gap. The default behavior did
  not change. Slice D later established that Forms do pass the written gate, so
  this was not an acceptance-threshold failure.

- `SignaturePolicy::Flatten` is the documented product default in core, CLI and
  WASM, preserving visible signatures and seals in the compressed document.
- The CLI exposes `--signatures strict|ignore|flatten` and reports the effect on
  signed documents accurately.
- The supported public surface of `gema-core` is limited to its re-exported API;
  implementation modules are private.
- Rust 1.97.1 and wasm-pack 0.15.0 are pinned for reproducible builds.
- Internal diagnostic examples are excluded from the published core package.

### Added

- End-to-end CLI tests and Node-based WASM tests.
- Memory-aware image batches with optional total-work, parallel-image and
  per-image limits. Core/CLI remain opt-in; WASM defaults to a 256 MiB
  scheduling budget.
- Automated original-vs-output and baseline-vs-output rendering checks with
  uniform, raster-heavy and signature-page selection, PSNR/pixel metrics,
  heatmaps and side-by-side review sheets.
- Conservative opt-in image-XObject deduplication in core, CLI and WASM, with
  signature/transparency exclusions and marginal removed-object/byte reporting.
- Conservative scanned-page detection in core, CLI and WASM, sharing the
  effective-DPI content-stream traversal during compression.
- Typed image-skip telemetry with per-document image/byte aggregates in core,
  CLI, WASM and the corpus usage harness.
- Automated license, dependency-source and wildcard checks through
  `cargo-deny`.
- Package validation in CI.
- Initial fuzz targets for `analyze` and `compress`.

### Fixed

- `/SMask /None`, split page-content arrays, adversarial image heights, mask
  warning distinctions and perceptual Q_MAX warning coupling.

- `gema-cli` and `gema-wasm` now specify a version for their local
  `gema-core` dependency, allowing Cargo to prepare them for publication once
  that core version exists in the registry.
- README and roadmap statements now match the implemented signature,
  perceptual-quality and Rayon behavior.
