# Tracking items / deferred to v2

Items intentionally out of scope for v1, captured here so they are not lost.
Each lists a one-line rationale and the file(s) it concerns.

1. **ColorSpace fidelity (DeviceGray → luma JPEG).**
   Grayscale (DeviceGray) images are currently re-encoded as RGB JPEG (3x
   channels), inflating scanned documents. v2 should detect DeviceGray and emit
   an L8/luma JPEG plus `ColorSpace DeviceGray`.
   _Files: `crates/gema-core/src/pipeline.rs`, `crates/gema-core/src/image_opt/jpeg.rs` — thread the colorspace through `Encoded`._

2. **Effective-DPI downsampling.**
   `process_image` estimates display size as pixels@72dpi, so `downsample`
   rarely fires on real high-DPI content. v2 should read the real CTM from the
   page content stream to compute the true render size.
   _File: `crates/gema-core/src/pipeline.rs`._

3. **`dedupe_images` is a no-op.**
   The option exists (default `true`) but does nothing. v2 should implement
   content-hash dedup of identical image XObjects.
   _Files: `crates/gema-core/src/pipeline.rs`, `crates/gema-core/src/options.rs`._

4. **`has_scanned_pages` never computed.**
   The `Report` field is always `false`; it will be set by the v2 OCR module.
   _Files: `crates/gema-core/src/analyze.rs`, `crates/gema-core/src/report.rs`._

5. **Non-DCT image recompression.**
   FlateDecode/PNG/CCITT images are currently preserved untouched (Skipped)
   because their raw stream bytes are not pixel data. v2 should decompress per
   `ColorSpace`/`BitsPerComponent` and recompress.
   _File: `crates/gema-core/src/pipeline.rs`._

6. **`GemaError::UnsupportedImage` is unused.**
   Skips currently go via `Warning::ImageSkipped`. Decide whether to use this
   error variant or remove it.
   _File: `crates/gema-core/src/error.rs`._

7. **Recurse into Form XObjects for DPI (v2.1).**
   `effective_dpi_map` only walks the page content streams and their direct
   image XObjects. Images drawn *inside* a Form XObject (`/Subtype /Form` with
   its own content + `/Resources`) are never reached, so their effective DPI is
   unknown and they are not downsampled (conservative fallback). v2.1 should
   recurse into Form XObjects, composing the form's `/Matrix` and the `cm` from
   the outer `Do`.
   _File: `crates/gema-core/src/geometry.rs`._

8. **Inline images (`BI`/`ID`/`EI`) are ignored.**
   Images embedded inline in a content stream are not XObjects and never enter
   the image pipeline, so they are neither DPI-analyzed nor recompressed.
   Uncommon in real corpora and acceptable to skip for now.
   _Files: `crates/gema-core/src/geometry.rs`, `crates/gema-core/src/pipeline.rs`._

9. **Regression test for `/Contents` as an array of streams.**
   A page's `/Contents` may be an array of stream references (a single logical
   content stream split across objects). `get_and_decode_page_content` handles
   this, but there is no test pinning that DPI accumulation works across a split
   content array. Add one.
   _File: `crates/gema-core/src/geometry.rs` (tests)._
