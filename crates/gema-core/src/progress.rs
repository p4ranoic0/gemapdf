//! Fases de progreso del pipeline de compresión.
//!
//! Diseñado para ser WASM-compatible: sin hilos ni canales. El pipeline invoca
//! un `FnMut(Phase)` síncrono en puntos fijos; el consumidor (CLI, wasm, UI)
//! decide qué hacer con cada fase.

/// Fase actual del pipeline. Se emite en este orden:
/// `Analyzing` → `OptimizingImages` (0..=N veces) → `Rewriting` → `Done`.
///
/// Casos especiales:
/// - Documento firmado con `SignaturePolicy::Strict`: `Analyzing` → `Done`
///   (retorno temprano, no se toca nada).
/// - PDF sin imágenes: se emite `OptimizingImages { done: 0, total: 0 }` una
///   sola vez antes de pasar a `Rewriting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Parseo del PDF y análisis inicial (páginas, firma, tamaño).
    Analyzing,
    /// Bucle de optimización de imágenes. Se emite con `done = 0` antes de la
    /// primera imagen y de nuevo tras procesar cada una (`done = i + 1`).
    OptimizingImages { done: usize, total: usize },
    /// Limpieza estructural y serialización (strip de metadata, prune,
    /// recompresión de streams, save).
    Rewriting,
    /// Pipeline terminado; el resultado está listo.
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_is_copy_and_comparable() {
        let p = Phase::OptimizingImages { done: 2, total: 5 };
        let q = p; // Copy
        assert_eq!(p, q);
        assert_ne!(Phase::Analyzing, Phase::Done);
        assert_ne!(
            Phase::OptimizingImages { done: 1, total: 5 },
            Phase::OptimizingImages { done: 2, total: 5 }
        );
    }
}
