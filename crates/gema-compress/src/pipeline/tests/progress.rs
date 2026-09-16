//! Secuencia de `Phase` que ve el consumidor del callback de progreso,
//! incluido el retorno temprano de un documento firmado bajo política Strict.

use super::super::*;
use super::fixtures::*;
use crate::options::CompressOptions;

#[test]
fn progress_phases_for_multi_image_pdf() {
    use crate::progress::Phase;

    const N: usize = 3;
    let input = pdf_with_jpegs(N);
    let opts = CompressOptions {
        profile: crate::options::Profile::Screen,
        ..Default::default()
    };

    let mut phases: Vec<Phase> = Vec::new();
    let res = compress_with_progress(&input, &opts, &mut |p| phases.push(p)).unwrap();
    assert!(!res.output.is_empty());

    // empieza con Analyzing y termina con Done
    assert_eq!(phases.first(), Some(&Phase::Analyzing), "phases={phases:?}");
    assert_eq!(phases.last(), Some(&Phase::Done), "phases={phases:?}");

    // eventos de imágenes: arranca en done=0 y acaba en done==total==N,
    // con `done` monótono y `total` constante
    let img_events: Vec<(usize, usize)> = phases
        .iter()
        .filter_map(|p| match p {
            Phase::OptimizingImages { done, total } => Some((*done, *total)),
            _ => None,
        })
        .collect();
    assert_eq!(img_events.first(), Some(&(0, N)), "phases={phases:?}");
    assert_eq!(img_events.last(), Some(&(N, N)), "phases={phases:?}");
    for w in img_events.windows(2) {
        assert!(w[1].0 >= w[0].0, "done debe ser monótono: {img_events:?}");
        assert_eq!(w[1].1, N, "total debe ser constante: {img_events:?}");
    }

    // Rewriting va después de todos los eventos de imágenes
    let rewriting_idx = phases
        .iter()
        .position(|p| *p == Phase::Rewriting)
        .expect("debe emitirse Rewriting");
    let last_img_idx = phases
        .iter()
        .rposition(|p| matches!(p, Phase::OptimizingImages { .. }))
        .expect("debe haber eventos de imágenes");
    assert!(last_img_idx < rewriting_idx, "phases={phases:?}");
}

#[test]
fn progress_signed_strict_emits_analyzing_then_done_only() {
    use crate::progress::Phase;

    let input = signed_pdf();
    // Strict es opt-in: ejercita el retorno temprano byte-idéntico sin
    // cambiar el default de producto, que preserva la apariencia visual.
    let opts = CompressOptions {
        signatures: SignaturePolicy::Strict,
        ..Default::default()
    };

    let mut phases: Vec<Phase> = Vec::new();
    let res = compress_with_progress(&input, &opts, &mut |p| phases.push(p)).unwrap();

    assert_eq!(
        phases,
        vec![Phase::Analyzing, Phase::Done],
        "retorno temprano firmado-Strict"
    );
    // el retorno temprano devuelve el original intacto
    assert_eq!(res.output, input);
    assert!(res.report.is_signed);
}
