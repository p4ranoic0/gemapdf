# Tracking items / roadmap

Items intentionally out of scope for v1 (or for the current v2.0 slice),
captured here so they are not lost. Each lists a one-line rationale and the
file(s) it concerns. Delivered items are kept for history but marked done —
do not delete them, they document what shipped and when.

## Delivered in v2.0

1. **~~ColorSpace fidelity (DeviceGray → luma JPEG).~~ DONE.**
   `JpegRecompressor` now encodes `Luma8` images as L8 JPEG
   (`image::ExtendedColorType::L8`) instead of always converting to RGB8.
   `Encoded` carries a `color_space` field (`DeviceGray`/`DeviceRGB`) that the
   pipeline writes to the output dict instead of hardcoding `DeviceRGB`.
   _Files: `crates/gema-core/src/image_opt/jpeg.rs`, `crates/gema-core/src/image_opt/mod.rs`, `crates/gema-core/src/pipeline.rs`._
   _Caveat: the FlateDecode+`classify()` path can never route pure grayscale
   into `Photo`/JPEG (grayscale caps at 256 unique quantized tones, well under
   the 4096-color `Photo` threshold — see `classify::tests::grayscale_gradient_stays_line_art`).
   The real-world trigger is a grayscale image already in DCT/PNG that
   `image::load_from_memory` decodes straight to `Luma8`._

2. **~~Effective-DPI downsampling.~~ DONE.**
   `effective_dpi_map` (in `geometry.rs`) walks each page's content stream with
   a mini CTM interpreter (`q`/`Q`/`cm` stack) and derives the real on-page DPI
   per image `Do`, so `downsample` now actually fires on real high-DPI content
   instead of the old pixels@72dpi estimate.
   _File: `crates/gema-core/src/geometry.rs`, wired into `crates/gema-core/src/pipeline.rs`._

3. **~~Non-DCT image recompression (FlateDecode).~~ DONE for Flate.**
   FlateDecode images are decoded (zlib inflate + PNG/TIFF predictor
   de-filtering + `ASCIIHex`/`ASCII85`/`RunLength` de-chaining) and
   recompressed: content-aware codec choice (`classify()` picks JPEG for
   photos, lossless Flate re-encode for line-art/text, never DCT on line art).
   CCITT/JBIG2/JPX images still fall outside this decoder and stay `Skipped`
   (native-only work, see below).
   _Files: `crates/gema-core/src/image_opt/decode.rs`, `crates/gema-core/src/image_opt/colorspace.rs`, `crates/gema-core/src/image_opt/classify.rs`, `crates/gema-core/src/image_opt/flate.rs`._

4. **~~Indexed / ICCBased / DeviceCMYK colorspace support.~~ DONE (P1c).**
   `interpret_color_space` handles `Indexed` (8-bpc index, RGB/Gray/ICC(N=1|3)
   base, palette bounds-checked), `ICCBased` (interpreted as its `/N` device
   space; profile itself ignored), and `DeviceCMYK` (standard conversion
   formula, no Adobe DCT inversion). Sub-byte Indexed (1/2/4 bpc) and CMYK-base
   Indexed remain out of scope (SKIP, safe).
   _File: `crates/gema-core/src/image_opt/colorspace.rs`._

5. **~~`save_modern` (object streams + xref streams).~~ DONE.**
   Output serialization uses `lopdf::Document::save_modern`, producing more
   compact PDF 1.5+ output (object streams, cross-reference streams) instead
   of classic xref tables.
   _File: `crates/gema-core/src/rewrite.rs`._

6. **~~Progress callback per phase.~~ DONE.**
   `compress_with_progress` emits `Phase::{Analyzing, OptimizingImages{done,total}, Rewriting, Done}`
   to a caller-supplied callback; the WASM binding (`compress_with_report`)
   forwards it to an optional JS function.
   _Files: `crates/gema-core/src/progress.rs`, `crates/gema-core/src/pipeline.rs`, `crates/gema-wasm/src/lib.rs`._

7. **~~JND-based perceptual quality selection.~~ DONE (experimental).**
   The optional native-only `perceptual` feature implements per-image quality
   search with SSIMULACRA2 and an encoder bake-off. The CLI exposes it through
   `--quality-target`; it remains intentionally outside the WASM dependency
   graph because of CPU and bundle-cost constraints.
   _Files: `crates/gema-core/src/image_opt/perceptual.rs`, `crates/gema-core/src/image_opt/process.rs`, `crates/gema-cli/src/main.rs`._

8. **~~Rayon-based parallelism for image processing.~~ DONE.**
   Native builds prepare images in parallel and commit mutations serially in a
   deterministic order. GemaPDF's direct Rayon dependency and parallel code
   are native-only; WASM keeps the image loop serial (some upstream crates may
   still carry Rayon transitively).
   _Files: `crates/gema-core/src/pipeline.rs`, `crates/gema-core/src/image_opt/process.rs`._

9. **~~Memory-aware image scheduling.~~ DONE (configurable).**
   Image preparation runs in ordered batches sized from a conservative working-
   set estimate. Core/CLI expose opt-in total, concurrency and per-image limits;
   WASM uses a 256 MiB scheduling budget by default. The per-image limit skips
   oversized rasters conservatively and reports the decision.
   _Files: `crates/gema-core/src/options.rs`, `crates/gema-core/src/pipeline.rs`, `crates/gema-core/src/image_opt/process.rs`._

10. **~~Automated visual validation.~~ DONE (initial gate).**
    `compare-visuals.py` compares the current output against either the original
    PDF or a baseline revision. Auto mode selects uniform, raster-heavy and
    signature-widget pages and emits pixel metrics, PSNR, heatmaps and review
    sheets outside the repository.
    _Files: `scripts/compare-visuals.py`, `scripts/tests/test_compare_visuals.py`._

11. **~~Implement `dedupe_images`.~~ DONE (conservative, opt-in).**
    Byte-identical image XObjects now collapse when every rendering key is
    equivalent. Non-render bookkeeping may differ; preserved signature/seal
    images and transparency masks are excluded. Core, CLI and WASM expose the
    opt-in flag and report marginal objects/bytes beyond the exact generic
    stream deduplication that already runs by default.
    _Files: `crates/gema-core/src/rewrite.rs`, `crates/gema-core/src/pipeline.rs`._

12. **~~Compute `has_scanned_pages`.~~ DONE (conservative heuristic).**
    A page is classified only when a raster of at least 400×400 pixels covers
    80% of its inherited CropBox/MediaBox and the page paints at most 32 bytes
    of text. DPI and scan evidence share one content-stream pass in compression.
    The result is exposed by core, CLI and WASM as an estimate.
    _Files: `crates/gema-core/src/geometry.rs`, `crates/gema-core/src/analyze.rs`._

13. **~~Compatibility and skip telemetry sprint.~~ DONE.**
    Reports now expose stable per-image skip reasons and document-level counts
    plus input bytes. `/SMask /None`, split `/Contents`, negative/zero/oversized
    dimensions, `/Matte` vs uninspectable masks and perceptual Q_MAX warnings
    have dedicated behavior/tests. Corpus output remained byte-identical 11/11.
    _Files: `crates/gema-core/src/report.rs`, `crates/gema-core/src/image_opt/process.rs`, `crates/gema-core/src/geometry.rs`._

## Remaining

14. **CCITT / JBIG2 / MRC native-only image pipeline (v2.1 "high compression" mode).**
   The big lever for administrative scans (10-20x): layered segmentation
   (mask/foreground/background, DjVu/MRC-style), JBIG2 bilevel mask with a
   document-level symbol dictionary (repeated glyphs/logos/stamps stored once),
   low-res JPEG background. Requires rasterization + a JBIG2 encoder — heavy,
   native-only (desktop/server), not WASM-portable. Verify the chosen JBIG2
   encoder's license before integrating (project constraint: no AGPL).
   _New module(s) under `crates/gema-core/src/image_opt/`, gated behind a
   `native` feature per the reserved features list in `gema-core/Cargo.toml`._

15. **~~Recurse into Form XObjects for DPI.~~ DONE (2026-08-10).**
    The content-stream walk now enters `/Subtype /Form` XObjects: a `Do` on a
    form composes the form's `/Matrix` with the caller's CTM and continues with
    the form's own `/Resources`, falling back to the painting context's when the
    form declares none (PDF 32000 §8.10.1). Cycles are cut by an active-form
    set, nesting is capped at 8 levels and each page has a 200k-operator budget.
    Both consumers of the walk benefit: images nested in forms now get an
    effective DPI (so they can be downsampled) and `has_scanned_pages` sees
    rasters painted through a form.
    **Behavior note:** documents that nest images in forms now produce different
    output than before. The 2026-07-27 measurement still stands — this is a
    correctness fix, not a compression lever: in the current corpus
    `doc-B2` paints all 97 images straight from page content, so
    no corpus-wide size change is expected from it.
    _File: `crates/gema-core/src/geometry.rs`._

16. **Inline images (`BI`/`ID`/`EI`) are ignored.**
    Images embedded inline in a content stream are not XObjects and never enter
    the image pipeline, so they are neither DPI-analyzed nor recompressed.
    Uncommon in real corpora and acceptable to skip for now.
    _Files: `crates/gema-core/src/geometry.rs`, `crates/gema-core/src/pipeline.rs`._

17. **Sub-byte Indexed colorspace (1/2/4 bpc) — measured LOW priority.**
    `interpret_color_space` only supports 8-bpc Indexed. Sub-byte packed
    indices (common in small palette images, e.g. 1-bpc bilevel or 4-bpc
    16-color) would need bit-unpacking before palette lookup. Currently SKIP
    (safe, no corruption, just no recompression gain for these images).
    The 2026-08-09 telemetry run found only 1 such image / 1,664 encoded bytes
    in the 11-document corpus, so implementation is deferred.
    _File: `crates/gema-core/src/image_opt/colorspace.rs`._
