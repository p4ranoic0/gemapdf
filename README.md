# GemaPDF

A pure-Rust, portable PDF compression engine. It ships as three crates:

- **`gema-core`** — the compression engine itself. Pure Rust, no C bindings,
  no system dependencies. Compiles to both WebAssembly and native code from
  the same source.
- **`gema-cli`** — a command-line tool (`gema`) built on `gema-core`.
- **`gema-wasm`** — WebAssembly bindings (via `wasm-bindgen`) so the same
  engine runs in a browser tab, client-side, with no server round-trip.

Project license: **MIT OR Apache-2.0** (see [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE)) — **no AGPL, anywhere in the dependency
tree.** That's a deliberate design constraint, not an accident: it's what lets
`gema-core` run client-side in a browser and be embedded in commercial or
closed-source products without copyleft obligations.

## Why this exists

Most PDF compression on the web is a thin wrapper around Ghostscript, either
shelled out to on a server or compiled to WebAssembly. Ghostscript is AGPL-3.0
— any service built on it that's reachable over a network has to offer its
own source under the AGPL too, and its `ghostscript-wasm` builds run **~14 MB**
of compiled Postscript interpreter in the browser.

GemaPDF takes a different approach: implement the actual PDF image-compression
pipeline (parse, decode, downsample, re-encode, rewrite) in pure Rust, with no
GPL/AGPL code anywhere in the tree. The payoff:

- **WASM bundle size:** ~1.1 MB (pre-gzip), versus ~14 MB for `ghostscript-wasm`
  (measured `wasm-pack build --target web` output of `crates/gema-wasm`
  — `pkg/gema_wasm_bg.wasm`, no Ghostscript in the dependency graph to compare
  against — the 14 MB figure is the published size of AGPL ghostscript-wasm
  builds).
- **License-clean:** permissive dependencies only (enforced with `cargo-deny`),
  safe to embed anywhere, including closed-source and commercial products,
  without triggering AGPL network-use clauses.
- **One codebase, two targets:** the same `gema-core` crate compiles to
  `wasm32-unknown-unknown` for the browser and to native for the CLI/server,
  with no `#[cfg]`-gated fork of the compression logic.

## What it actually does (honest, measured claims)

GemaPDF compresses the *images* inside a PDF — that's where nearly all the
recoverable size lives in real-world documents (scans, reports, forms). It
does **not** currently touch page-level content (fonts, glyph outlines,
non-image content streams beyond `save_modern`'s object/xref-stream packing).

Measured on a real corpus (see `docs/USAGE-ANALYSIS.md` for the full v1
report, `docs/USAGE-ANALYSIS-v2.md` for the v2.0 measurement, and
`crates/gema-core/examples/usage_report.rs`, the harness used to produce
these numbers):

- **v1** (recompress-only, no real DPI downsampling, DCT/PNG images only):
  ~3.7–8.1% size reduction on a 31-PDF / ~283 MB real-document corpus. The
  95% of images that stayed untouched were split between "already efficient"
  (`Kept`) and formats the decoder couldn't open yet — 16.8% were `Skipped`
  (FlateDecode-raw scans, CCITT/JBIG2, JPX).
- **v2.0** (this codebase today) adds: a real CTM-based effective-DPI
  downsampler (v1's DPI estimate was inert — 0 downsamples in 5,799 images;
  v2.0 actually fires), FlateDecode image decoding (raw zlib pixel data with
  PNG/TIFF predictor de-filtering, plus `ASCIIHex`/`ASCII85`/`RunLength`
  de-chaining), `Indexed`/`ICCBased`/`DeviceCMYK` colorspace support, and
  gray→luma JPEG output (grayscale scans no longer get inflated to 3-channel
  RGB before re-encoding). Measured on a 37-PDF real-document corpus (see
  [`docs/USAGE-ANALYSIS-v2.md`](docs/USAGE-ANALYSIS-v2.md) — a different,
  newer corpus than the v1 analysis, so the two numbers aren't a strict
  apples-to-apples before/after) this brings the *global* output/input ratio
  to **64.7%** (~35% reduction) — a large jump from v1's single-digit
  percentages, driven mostly by downsampling actually firing on high-DPI
  scanned content (1,842 images downsampled in that corpus).
- **Visible signature preservation:** `SignaturePolicy::Flatten` is the product
  default. It embeds visible signatures and seals into page content so they
  remain visible in the compressed PDF; cryptographic validity is lost because
  the document changes. `SignaturePolicy::Strict` is available explicitly when
  validity matters: it returns a signed PDF byte-for-byte unchanged and reports
  the requested transformation as blocked.
- **SMask (soft-mask) preservation:** images with an attached transparency
  mask (`/SMask`) are left untouched rather than recompressed, since
  recompressing the color data without also handling the mask would either
  discard the alpha channel or orphan the mask XObject.

What it doesn't do yet — CCITT/JBIG2/JPX decoding, a document-level JBIG2
symbol dictionary for repeated letterheads/stamps, image deduplication,
Form XObject DPI recursion — is tracked honestly in
[`TODO-v2.md`](TODO-v2.md). Perceptual (JND) quality selection is available as
an experimental, native-only CLI feature via `--quality-target`. The fuller
design rationale
(why DPI and codec choice are the two big levers, and what the v2.1
"high-compression" native-only mode looks like) is in
[`docs/V2-DESIGN.md`](docs/V2-DESIGN.md).

## The three crates

| Crate | What it is | Targets |
|---|---|---|
| [`gema-core`](crates/gema-core) | The compression engine: PDF parsing (via `lopdf`), image decode/downsample/recompress pipeline, progress reporting. | `wasm32-unknown-unknown` + native |
| [`gema-cli`](crates/gema-cli) | `gema` binary: compress/analyze a PDF from the command line. | native |
| [`gema-wasm`](crates/gema-wasm) | `wasm-bindgen` bindings exposing `compress`, `analyze`, `compress_with_report` to JS. | `wasm32-unknown-unknown` |

## Usage

### Rust (`gema-core`)

```rust
use gema_core::{compress, CompressOptions, Profile};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::fs::read("input.pdf")?;

    let opts = CompressOptions {
        profile: Profile::Ebook, // Screen | Ebook | Printer | Custom
        ..Default::default()
    };

    let result = compress(&input, &opts)?;
    std::fs::write("output.pdf", &result.output)?;

    println!(
        "{} -> {} bytes ({:.1}% of original, {} images touched)",
        input.len(),
        result.output.len(),
        result.report.ratio.unwrap_or(1.0) * 100.0,
        result.report.images.len(),
    );
    Ok(())
}
```

`CompressOptions` lets you override the per-profile defaults
(`image_dpi`, `jpeg_quality`), choose the signature policy
(`SignaturePolicy::Flatten` — the default —, `Strict`, or the advanced
`Ignore` policy), and toggle
`downsample`/`recompress_streams`/`remove_metadata`. The opt-in
`dedupe_images` flag collapses byte-identical image XObjects when their render
semantics match; signature/seal images and transparency masks are excluded.
See
`crates/gema-core/src/options.rs` for the full set and their profile
defaults (Screen 72dpi/q40, Ebook 150dpi/q65, Printer 300dpi/q80).

Native callers that process untrusted or very large PDFs can bound image work
with `max_memory_bytes`, `max_parallel_images`, and `max_image_bytes`. These are
opt-in in core/CLI to preserve native throughput. The WASM binding uses a
256 MiB scheduling budget by default because its image loop is serial.

For progress reporting during a long compression, use
`compress_with_progress(&input, &opts, &mut |phase| { .. })`, which calls
back with `Phase::{Analyzing, OptimizingImages { done, total }, Rewriting, Done}`.

### CLI (`gema-cli`)

```sh
cargo install --path crates/gema-cli
# or, from a release build:
gema compress in.pdf out.pdf --profile ebook

# Preserve cryptographic validity by leaving signed PDFs unchanged:
gema compress in.pdf out.pdf --profile ebook --signatures strict

# Bound concurrent image work and reject single images over 256 MiB:
gema compress in.pdf out.pdf --max-memory-mib 512 --max-parallel-images 4 \
  --max-image-mib 256

# Collapse repeated byte-identical letterheads/stamps when safe:
gema compress in.pdf out.pdf --dedupe-images

gema analyze in.pdf
```

Profiles: `screen` | `ebook` | `printer` | `custom` (default `ebook`; `custom`
takes Ebook's base values and expects `--image-dpi` / `--jpeg-quality` on top).
Signature policies: `flatten` (default, preserves the visible appearance) |
`strict` (returns signed PDFs unchanged) | `ignore`. Both sets are parsed by
`gema-core` itself (`Profile: FromStr`, `SignaturePolicy: FromStr`), so the CLI
and the WASM binding accept exactly the same names.

### Web (`gema-wasm`)

Build the WASM package:

```sh
cd crates/gema-wasm
wasm-pack build --target web
```

Then, in JS (reading the real exported API from
`crates/gema-wasm/src/lib.rs` — `compress_with_report` returns
`{ output: Uint8Array, report }` and takes an optional progress callback):

```js
import init, { compress_with_report } from "./pkg/gema_wasm.js";

await init(); // loads the .wasm module

const input = new Uint8Array(await file.arrayBuffer());

const { output, report } = compress_with_report(
  input,
  "ebook", // "screen" | "ebook" | "printer"
  { image_dpi: 150, jpeg_quality: 65, max_memory_bytes: 268435456,
    dedupe_images: true,
    signatures: "flatten" }, // optional, or null
  ({ phase, done, total }) => {
    // phase: "analyzing" | "optimizing" | "rewriting" | "done"
    // done/total only present during "optimizing"
    console.log(phase, done, total);
  },
);

console.log(report.ratio, report.images_recompressed, report.warnings);
// `output` is a Uint8Array of the compressed PDF — download or upload it directly.
```

Reports include `has_scanned_pages`, a conservative estimate that becomes true
when at least one large raster covers most of a page with little painted text.
It is detection metadata, not an OCR result. Skipped images include a typed
`skip_reason`, while `image_skip_summary` aggregates image counts and input
bytes by reason so unsupported-format opportunities can be measured directly.

There's also a plain `compress(input, profile)` (returns just the output
bytes, no report/options/progress — kept for a simpler v1-style call) and
`analyze(input)` (inspects a PDF without compressing it, e.g. to show page
count and signature status before the user commits to compressing).

### JSON report

`gema compress --json` and `gema analyze --json` print the report to stdout as
JSON. The WASM binding currently emits its own report shape and will be aligned
with this schema. Human output stays the default.

Add `--json-images` to `compress` for a per-image `images.detail` array. It is
opt-in because `object_id` refers to the *input* document.

```json
{
  "report_schema_version": 1,
  "input": {
    "bytes": 7917380,
    "pages": 113
  },
  "output": {
    "bytes": 7872133,
    "ratio": 0.9942851
  },
  "document": {
    "is_signed": false,
    "has_scanned_pages": true,
    "signature_policy": "flatten",
    "flattened_signatures": 0,
    "visual_appearance_preserved": true,
    "cryptographic_validity_preserved": true,
    "operation_blocked": false,
    "document_modified": false
  },
  "images": {
    "total": 162,
    "by_action": {
      "recompressed": 3,
      "downsampled": 0,
      "kept": 74,
      "skipped": 0,
      "preserved": 85
    },
    "deduplicated": 0,
    "deduplicated_bytes": 0,
    "skipped_by_reason": []
  },
  "warnings": [
    {
      "kind": "other",
      "message": "85 firma(s)/sello(s) preservados sin recomprimir"
    }
  ]
}
```

`analyze --json` emits the same shape, but without the `output` key or per-image
statistics, because it does not compress anything.

`report_schema_version` only increases when the JSON stops being backward
compatible — a key renamed, removed, or retyped. **Adding** a key or an enum
variant does not bump it, so consumers must ignore unknown keys. `warnings[].kind`
is the stable discriminant; `warnings[].message` is prose and may be reworded.

### API stability

The supported surface is what `gema-core` re-exports; implementation modules are
private.

- Enums the pipeline grows are `#[non_exhaustive]` — `ImageSkipReason`,
  `Warning`, `GemaError`, `Phase`, `ImageAction`. Match them with a `_` arm.
- `Profile` and `SignaturePolicy` are deliberately exhaustive: closed product
  concepts, and you want the compiler to tell you when they change.
- Structs are exhaustive because callers build `CompressOptions` and tests build
  report fixtures with struct literals. `#[non_exhaustive]` on a struct forbids
  the literal from another crate *even with* `..Default::default()`, so it is not
  used. Adding a field is a breaking change.

There is no error variant for "this document is signed": `SignaturePolicy::Strict`
does not fail, it returns the document untouched. Observe it through
`Report::is_signed` plus an output identical to the input.

## Regression benchmarking

To compare the current working tree with a Git revision over the private
`~/Downloads/doc-A` corpus:

```sh
scripts/compare-revisions.sh HEAD ebook 3
```

Then validate rendered output against the immutable originals:

```sh
scripts/compare-visuals.py ../gemapdf-internal-docs/benchmarks/<run-id> \
  --against original

# A strict visual-regression gate between the two compressed revisions:
scripts/compare-visuals.py ../gemapdf-internal-docs/benchmarks/<run-id> \
  --against baseline --max-changed-pct 0
```

The default `--pages auto` samples the document uniformly, adds the pages with
the largest raster footprint, and includes signature-widget pages discovered
with qpdf. Reports include pixel-change rate, MAE, RMS, PSNR, heatmaps, and
side-by-side sheets. Explicit page/range selection remains available.

The corpus is read-only. Derived PDFs, timings, validation logs, checksums and
the summary are written to a timestamped directory under the external sibling
`../gemapdf-internal-docs/benchmarks/`; nothing from the corpus is copied
into this repository. Run `scripts/compare-revisions.sh --help` to override the
baseline, profile, repetitions, corpus or results root.

## Roadmap

See [`docs/V2-DESIGN.md`](docs/V2-DESIGN.md) for the design rationale (why
DPI and per-content codec choice are the two highest-impact levers, and what
the native-only "high compression" mode looks like), and
[`TODO-v2.md`](TODO-v2.md) for the itemized, kept-honest list of what's
shipped versus what's still open.
