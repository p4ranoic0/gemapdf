# Changelog

All notable changes to GemaPDF are documented in this file. The project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Added

- Structural telemetry behind the non-default `telemetry` cargo feature: counts
  inline images, images reachable only inside Form XObjects, ExtGState
  luminosity soft masks, and uninspectable resources. Consumed by the internal
  `usage_report` example.

  It is **not** stable surface: it lives outside `Report` and the JSON schema,
  is absent from the default build, and changes no output byte. A metric is
  promoted to the contract only if it is shown to drive a decision.

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

- **Recursión en Form XObjects para DPI efectivo.** Se implementó completa y se
  midió sobre el corpus real: **0 bytes de diferencia y 11/11 salidas
  byte-idénticas** sobre 537 MB de entrada, sin fallos de validación. No alcanza
  ningún umbral de aceptación, así que el código productivo se revirtió y quedó
  la conclusión más un test que caracteriza el gap. El comportamiento por
  defecto no cambia.

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
