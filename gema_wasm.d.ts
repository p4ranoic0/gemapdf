/* tslint:disable */
/* eslint-disable */

/**
 * Analiza un PDF sin comprimirlo. Devuelve el reporte como objeto JS con la
 * misma forma que `report` en `compress_with_report`; `output_size` y `ratio`
 * vienen undefined porque no hubo compresión, y los contadores de imágenes
 * vienen a 0 (el análisis no recorre imágenes en v1).
 */
export function analyze(input: Uint8Array): any;

/**
 * Comprime un PDF. `profile`: "screen" | "ebook" | "printer".
 * Devuelve los bytes del PDF optimizado. (API v1: se mantiene sin cambios;
 * para reporte/opciones/progreso usa `compress_with_report`.)
 */
export function compress(input: Uint8Array, profile: string): Uint8Array;

/**
 * Comprime un PDF devolviendo `{ output: Uint8Array, report: {...} }`.
 *
 * - `profile`: "screen" | "ebook" | "printer".
 * - `options`: objeto `{ image_dpi?, jpeg_quality?, transcode_dpi?,
 *   transcode_quality?, max_memory_bytes?, max_parallel_images?,
 *   max_image_bytes?, dedupe_images?,
 *   signatures?: "strict"|"ignore"|"flatten" }`.
 *   Las `transcode_*` sólo afectan a escaneos que llegan sin pérdida y salen
 *   como JPEG (ver ROADMAP §2.b).
 *   o undefined/null para usar los defaults del perfil. Claves desconocidas se
 *   ignoran; valores inválidos son un error.
 * - `on_phase`: función opcional que recibe `{ phase, done?, total? }` con
 *   `phase` ∈ "analyzing" | "optimizing" | "rewriting" | "done" (`done`/`total`
 *   sólo en "optimizing"). Si el callback lanza, el error se ignora: un fallo
 *   de UI nunca aborta la compresión. Los eventos "optimizing" se limitan a
 *   ~100 llamadas JS (siempre el primero, el último, y cada ~1% del total)
 *   para no saturar el borde wasm en PDFs con miles de imágenes; el resto de
 *   fases (analyzing/rewriting/done) siempre se reenvían.
 */
export function compress_with_report(input: Uint8Array, profile: string, options: any, on_phase?: Function | null): any;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly analyze: (a: number, b: number) => [number, number, number];
    readonly compress: (a: number, b: number, c: number, d: number) => [number, number, number, number];
    readonly compress_with_report: (a: number, b: number, c: number, d: number, e: any, f: number) => [number, number, number];
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
