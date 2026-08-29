use crate::error::{GemaError, LimitKind};
use crate::image_opt::process::{commit_prepared, prepare_image, ImageParams};
use crate::options::{CompressOptions, SignaturePolicy};
use crate::progress::Phase;
use crate::report::{ImageAction, ImageSkipSummary, Report, Warning};
use lopdf::Document;

/// PDF comprimido junto con el reporte de lo que se hizo.
pub struct CompressResult {
    /// Bytes del PDF de salida.
    pub output: Vec<u8>,
    /// Qué se hizo con el documento y sus imágenes.
    pub report: Report,
}

/// Parte una secuencia ordenada de estimaciones en lotes que respetan el
/// presupuesto y el máximo de elementos. Una imagen mayor que el presupuesto
/// corre sola: `max_image_bytes` es el mecanismo para rechazarla por completo.
fn image_batch_ranges(
    estimates: &[u64],
    max_memory_bytes: Option<u64>,
    max_parallel_images: Option<usize>,
) -> Vec<std::ops::Range<usize>> {
    if estimates.is_empty() {
        return Vec::new();
    }
    let budget = max_memory_bytes.unwrap_or(u64::MAX).max(1);
    let max_images = max_parallel_images.unwrap_or(usize::MAX).max(1);
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < estimates.len() {
        let mut end = start;
        let mut bytes = 0u64;
        while end < estimates.len() && end - start < max_images {
            let next = estimates[end].max(1);
            if end > start && bytes.saturating_add(next) > budget {
                break;
            }
            bytes = bytes.saturating_add(next);
            end += 1;
            if bytes >= budget {
                break;
            }
        }
        ranges.push(start..end);
        start = end;
    }
    ranges
}

/// Comprime un PDF sin reportar progreso. Envoltorio fino sobre
/// [`compress_with_progress`] con un callback no-op; misma API pública que v1.
pub fn compress(input: &[u8], opts: &CompressOptions) -> Result<CompressResult, GemaError> {
    compress_with_progress(input, opts, &mut |_| {})
}

/// Igual que [`compress`], pero invoca `on_phase` en cada transición de fase
/// del pipeline. Orden garantizado (ver [`Phase`]): `Analyzing` →
/// `OptimizingImages { done: 0..=N, total: N }` → `Rewriting` → `Done`; en el
/// retorno temprano firmado-Strict sólo `Analyzing` → `Done`. El callback es
/// síncrono y corre en el mismo hilo: WASM-compatible (sin threads ni canales).
pub fn compress_with_progress(
    input: &[u8],
    opts: &CompressOptions,
    on_phase: &mut dyn FnMut(Phase),
) -> Result<CompressResult, GemaError> {
    on_phase(Phase::Analyzing);

    // F4: parseamos el PDF una sola vez y derivamos el reporte base del mismo
    // doc (antes se hacía load_mem dentro de analyze() y otra vez aquí).
    let mut doc = Document::load_mem(input).map_err(|e| GemaError::Parse(e.to_string()))?;
    if doc.is_encrypted() {
        return Err(GemaError::Encrypted);
    }
    if let Some(limit) = opts.max_pages {
        // El reporte cuenta páginas con la misma expresión a propósito: ésta
        // es la comprobación temprana, antes del recorrido caro de geometría.
        let observed = doc.get_pages().len();
        if observed > limit {
            return Err(GemaError::LimitExceeded {
                limit: LimitKind::Pages,
                observed: observed as u64,
                allowed: limit as u64,
            });
        }
    }
    if let Some(limit) = opts.max_objects {
        let observed = doc.objects.len();
        if observed > limit {
            return Err(GemaError::LimitExceeded {
                limit: LimitKind::Objects,
                observed: observed as u64,
                allowed: limit as u64,
            });
        }
    }
    // DPI y evidencia de escaneo comparten el mismo recorrido de content
    // streams, evitando duplicar el costo durante la compresión.
    let (dpi_map, has_scanned_pages) = crate::geometry::document_geometry(&doc);
    let report0 =
        crate::analyze::report_from_doc_with_scan(&doc, input.len() as u64, has_scanned_pages);

    // política de firma
    if report0.is_signed && opts.signatures == SignaturePolicy::Strict {
        let result = CompressResult {
            output: input.to_vec(),
            report: Report {
                output_size: Some(input.len() as u64),
                warnings: vec![Warning::SignedDocument],
                ..report0
            }
            .with_ratio(),
        };
        on_phase(Phase::Done);
        return Ok(result);
    }

    let params = opts.resolved();

    // Pasada pre-flight de firmas/sellos: produce el set de imágenes XObject a
    // preservar byte-idénticas (apariencias de firma + sellos/logos pequeños).
    // Corre una vez, tras el early-return de `Strict` (un doc con firma cripto ya
    // se devolvió intacto arriba) y antes del bucle de imágenes. No muta el doc.
    let preserve = crate::signatures::collect_preserved_images(&doc);

    // Política Flatten: hornea las firmas visibles al contenido de página. Va
    // DESPUÉS de calcular `preserve` (que necesita los widgets en /Annots) y
    // ANTES del bucle de imágenes (las imágenes de firma siguen preservándose por
    // ObjectId). Sacrifica la validez cripto (ya rota por la compresión) a cambio
    // de que Acrobat renderice las firmas.
    let flattened = if opts.signatures == SignaturePolicy::Flatten {
        crate::flatten::flatten_signatures(&mut doc)
    } else {
        0
    };

    // P2: DPI efectivo real de cada imagen a partir del CTM del content stream.
    // Se calcula una vez, antes del bucle de imágenes. Las imágenes ausentes del
    // mapa (nunca pintadas / CTM degenerado) no se downsamplean (fallback v1).
    // recolectar ids de imágenes (XObject /Subtype /Image)
    let image_ids: Vec<lopdf::ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(id, obj)| {
            let s = obj.as_stream().ok()?;
            if s.dict.get(b"Subtype").and_then(|o| o.as_name()).ok()? == b"Image" {
                Some(*id)
            } else {
                None
            }
        })
        .collect();

    let total = image_ids.len();
    on_phase(Phase::OptimizingImages { done: 0, total });

    // Pre-pasada de máscaras (lever C): imágenes usadas como /SMask.
    let smask_ids = crate::image_opt::masks::collect_smask_ids(&doc);

    let mut stats = Vec::new();
    let mut img_warnings = Vec::new();

    // Parámetros por-imagen: lookups read-only en los mapas ya construidos.
    let mk_params = |id: lopdf::ObjectId| ImageParams {
        quality: params.jpeg_quality,
        quality_target: opts.quality_target,
        target_dpi: params.image_dpi,
        transcode_dpi: opts.transcode_dpi,
        transcode_quality: opts.transcode_quality,
        max_image_bytes: opts.max_image_bytes,
        downsample: opts.downsample,
        effective_dpi: dpi_map.get(&id).copied(),
        preserve: preserve.contains(&id),
        is_smask: smask_ids.contains(&id),
    };

    // Cache de búsquedas perceptuales por-documento (cierre §1): copias
    // byte-idénticas del mismo stream (mismo dict, mismas dims, misma τ) no
    // repiten la búsqueda. Compartido entre hilos (Mutex interno); una carrera
    // recomputa el mismo resultado determinista, así que el output es idéntico
    // con o sin hit.
    #[cfg(feature = "perceptual")]
    let perceptual_cache = {
        const DEFAULT_CACHE_BYTES: u64 = crate::image_opt::perceptual::CACHE_MAX_BYTES;
        let cache_bytes = opts
            .max_memory_bytes
            .map(|budget| (budget / 4).min(DEFAULT_CACHE_BYTES))
            .unwrap_or(DEFAULT_CACHE_BYTES);
        crate::image_opt::perceptual::SearchCache::with_max_bytes(cache_bytes)
    };

    let work_estimates: Vec<u64> = image_ids
        .iter()
        .map(|&id| {
            crate::image_opt::process::estimated_working_bytes(
                &doc,
                id,
                opts.quality_target.is_some(),
            )
        })
        .collect();
    if let Some(limit) = opts.max_total_work_bytes {
        let observed = work_estimates.iter().sum();
        if observed > limit {
            return Err(GemaError::LimitExceeded {
                limit: LimitKind::TotalWork,
                observed,
                allowed: limit,
            });
        }
    }
    let batches = image_batch_ranges(
        &work_estimates,
        opts.max_memory_bytes,
        opts.max_parallel_images,
    );

    // §1.4 — El cómputo pesado por-imagen (decode + búsqueda + encode) es
    // read-only sobre el doc: en NATIVO corre en paralelo con rayon; sólo la
    // reescritura (`commit_prepared`) muta el doc y va en SERIE, en orden de
    // `image_ids`. El resultado es byte-idéntico al serial — cada imagen se
    // procesa de forma independiente sobre el doc original y las escrituras van a
    // objetos disjuntos. El wasm/Beta es single-thread → nuestro bucle se queda
    // serial y la dependencia directa sólo existe bajo `cfg(not(wasm32))`.
    let mut done = 0;
    for range in batches {
        // Sólo este lote conserva raster/encoded bytes en RAM. Al terminar se
        // confirma en orden y se libera antes de preparar el siguiente.
        #[cfg(not(target_arch = "wasm32"))]
        let prepared: Vec<_> = {
            use rayon::prelude::*;
            image_ids[range.clone()]
                .par_iter()
                .map(|&id| {
                    prepare_image(
                        &doc,
                        id,
                        &mk_params(id),
                        #[cfg(feature = "perceptual")]
                        &perceptual_cache,
                    )
                })
                .collect()
        };
        #[cfg(target_arch = "wasm32")]
        let prepared: Vec<_> = image_ids[range]
            .iter()
            .map(|&id| {
                prepare_image(
                    &doc,
                    id,
                    &mk_params(id),
                    #[cfg(feature = "perceptual")]
                    &perceptual_cache,
                )
            })
            .collect();

        // Fase serial: aplicar las escrituras en el orden original y emitir los
        // mismos eventos de progreso que antes del presupuesto.
        for prep in prepared {
            if let Some(outcome) = commit_prepared(&mut doc, prep) {
                stats.push(outcome.stat);
                img_warnings.extend(outcome.warnings);
            }
            done += 1;
            on_phase(Phase::OptimizingImages { done, total });
        }
    }

    on_phase(Phase::Rewriting);

    if opts.remove_metadata {
        crate::rewrite::strip_metadata(&mut doc);
    }
    let dedupe_stats = if opts.dedupe_images {
        crate::rewrite::dedupe_images(&mut doc, &preserve, &smask_ids)
    } else {
        crate::rewrite::ImageDedupeStats::default()
    };
    crate::rewrite::cleanup_and_compress(&mut doc, opts.recompress_streams);
    // Tras comprimir (para que el XMP no se recomprima): estampa la marca gemaPDF.
    crate::rewrite::brand_metadata(&mut doc);
    let output = crate::rewrite::serialize(&mut doc)?;

    // F5: movemos las warnings del reporte base en vez de clonarlas; luego le
    // sumamos las de imágenes.
    let mut report = report0;
    report.warnings.extend(img_warnings);
    report.images = stats;
    let mut skip_totals = std::collections::BTreeMap::new();
    for stat in &report.images {
        if let Some(reason) = stat.skip_reason {
            let entry = skip_totals.entry(reason).or_insert((0usize, 0u64));
            entry.0 += 1;
            entry.1 = entry.1.saturating_add(stat.original_bytes);
        }
    }
    report.image_skip_summary = skip_totals
        .into_iter()
        .map(|(reason, (images, original_bytes))| ImageSkipSummary {
            reason,
            images,
            original_bytes,
        })
        .collect();

    // Firmas/sellos preservados: contamos los stats con acción Preserved y, si
    // hubo alguno, emitimos UN solo warning de resumen (no uno por imagen, para
    // no hacer ruido en el reporte).
    let preserved_count = report
        .images
        .iter()
        .filter(|s| s.action == ImageAction::Preserved)
        .count();
    report.preserved_images = preserved_count;
    report.flattened_signatures = flattened;
    report.deduplicated_images = dedupe_stats.images_removed;
    report.deduplicated_image_bytes = dedupe_stats.stream_bytes_removed;
    if preserved_count > 0 {
        report.warnings.push(Warning::Other(format!(
            "{preserved_count} firma(s)/sello(s) preservados sin recomprimir"
        )));
    }

    // F10: piso a nivel-documento. Si tras serializar el output recomprimido
    // resulta MÁS grande que el input (p. ej. la sobrecarga de reescritura
    // supera el ahorro en un PDF ya pequeño), descartamos el output y
    // devolvemos los bytes originales. El reporte refleja que no hubo mejora
    // (output_size = input.len(), ratio = 1.0). La ruta de SignaturePolicy::Strict
    // ya devuelve el original más arriba y no pasa por aquí.
    //
    // Excepción: cuando se aplanaron firmas (flattened > 0) se hicieron cambios
    // semánticos intencionales (widget eliminado, /AcroForm quitado, marca
    // gemaPDF estampada). Incluso si el output resulta ligeramente mayor que el
    // input por overhead de firma+branding, se devuelve el output procesado —no
    // el original sin aplanar— para que el documento sea universalmente visible
    // en Acrobat. Sin firmas (flattened == 0) la política es irrelevante y el
    // piso sigue aplicando.
    let result = if output.len() > input.len() && flattened == 0 {
        // La salida efectiva es el original, así que no reportamos objetos que
        // sólo se eliminaron en un candidato descartado.
        report.deduplicated_images = 0;
        report.deduplicated_image_bytes = 0;
        report.output_size = Some(input.len() as u64);
        report.warnings.push(Warning::Other(
            "sin mejora: se conservó el documento original".into(),
        ));
        CompressResult {
            output: input.to_vec(),
            report: report.with_ratio(),
        }
    } else {
        report.output_size = Some(output.len() as u64);
        CompressResult {
            output,
            report: report.with_ratio(),
        }
    };

    on_phase(Phase::Done);
    Ok(result)
}

#[cfg(test)]
mod tests;
