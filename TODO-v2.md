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

## Remaining

7. **`dedupe_images` is a no-op.**
   The option exists (default now `false`, see Milestone MB hygiene pass — it
   was dishonest to default `true` for an unimplemented feature) but does
   nothing. v2.x should implement content-hash dedup of identical image
   XObjects (the repeated-letterhead/stamp case in administrative-document
   corpora).
   _Files: `crates/gema-core/src/pipeline.rs`, `crates/gema-core/src/options.rs`._

8. **`has_scanned_pages` never computed.**
   The `Report` field is always `false`; it will be set by the v2 OCR module.
   _Files: `crates/gema-core/src/analyze.rs`, `crates/gema-core/src/report.rs`._

9. **CCITT / JBIG2 / MRC native-only image pipeline (v2.1 "high compression" mode).**
   The big lever for administrative scans (10-20x): layered segmentation
   (mask/foreground/background, DjVu/MRC-style), JBIG2 bilevel mask with a
   document-level symbol dictionary (repeated glyphs/logos/stamps stored once),
   low-res JPEG background. Requires rasterization + a JBIG2 encoder — heavy,
   native-only (desktop/server), not WASM-portable. Verify the chosen JBIG2
   encoder's license before integrating (project constraint: no AGPL).
   _New module(s) under `crates/gema-core/src/image_opt/`, gated behind a
   `native` feature per the reserved features list in `gema-core/Cargo.toml`._

10. **JND-based perceptual quality (just-noticeable-difference) instead of a flat JPEG quality knob.**
    Pick quality per-image (or per-region) based on a perceptual model instead
    of one fixed `jpeg_quality` for the whole document.
    _File: `crates/gema-core/src/image_opt/jpeg.rs` (or a new perceptual module)._

11. **Rayon-based parallelism for image processing.**
    `process_image` is currently sequential per XObject; a multi-image PDF
    could recompress images in parallel on native builds. WASM has no threads
    by default, so this should be native-only (behind a feature), same
    constraint as item 9.
    _File: `crates/gema-core/src/pipeline.rs`._

12. **Recurse into Form XObjects for DPI (v2.1).**
    `effective_dpi_map` only walks the page content streams and their direct
    image XObjects. Images drawn *inside* a Form XObject (`/Subtype /Form` with
    its own content + `/Resources`) are never reached, so their effective DPI is
    unknown and they are not downsampled (conservative fallback). v2.1 should
    recurse into Form XObjects, composing the form's `/Matrix` and the `cm` from
    the outer `Do`.
    _File: `crates/gema-core/src/geometry.rs`._

13. **Inline images (`BI`/`ID`/`EI`) are ignored.**
    Images embedded inline in a content stream are not XObjects and never enter
    the image pipeline, so they are neither DPI-analyzed nor recompressed.
    Uncommon in real corpora and acceptable to skip for now.
    _Files: `crates/gema-core/src/geometry.rs`, `crates/gema-core/src/pipeline.rs`._

14. **Sub-byte Indexed colorspace (1/2/4 bpc).**
    `interpret_color_space` only supports 8-bpc Indexed. Sub-byte packed
    indices (common in small palette images, e.g. 1-bpc bilevel or 4-bpc
    16-color) would need bit-unpacking before palette lookup. Currently SKIP
    (safe, no corruption, just no recompression gain for these images).
    _File: `crates/gema-core/src/image_opt/colorspace.rs`._

15. **Regression test for `/Contents` as an array of streams.**
    A page's `/Contents` may be an array of stream references (a single logical
    content stream split across objects). `get_and_decode_page_content` handles
    this, but there is no test pinning that DPI accumulation works across a split
    content array. Add one.
    _File: `crates/gema-core/src/geometry.rs` (tests)._
