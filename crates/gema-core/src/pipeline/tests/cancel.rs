//! Cancelación cooperativa y compatibilidad del wrapper de progreso.

use super::super::*;
use super::fixtures::*;
use crate::progress::{CancelSignal, Phase};
use std::cell::Cell;

struct NeverCancelled;

impl CancelSignal for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

struct AlwaysCancelled;

impl CancelSignal for AlwaysCancelled {
    fn is_cancelled(&self) -> bool {
        true
    }
}

struct CellSignal<'a>(&'a Cell<bool>);

impl CancelSignal for CellSignal<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.get()
    }
}

#[test]
fn never_cancelled_is_byte_identical_to_compress() {
    let input = pdf_with_jpeg();
    let opts = CompressOptions {
        profile: crate::options::Profile::Screen,
        ..Default::default()
    };

    let expected = compress(&input, &opts).unwrap();
    let actual = compress_with_control(&input, &opts, &mut |_| {}, &NeverCancelled).unwrap();

    assert_eq!(actual.output, expected.output);
}

#[test]
fn cancelled_from_start_returns_error_without_done() {
    let input = pdf_with_jpeg();
    let mut phases = Vec::new();

    let result = compress_with_control(
        &input,
        &CompressOptions::default(),
        &mut |phase| phases.push(phase),
        &AlwaysCancelled,
    );

    assert!(matches!(result, Err(GemaError::Cancelled)));
    assert!(!phases.contains(&Phase::Done), "phases={phases:?}");
}

#[test]
fn cancellation_after_first_phase_returns_error() {
    let input = pdf_with_jpeg();
    let cancelled = Cell::new(false);
    let signal = CellSignal(&cancelled);
    let mut phases = Vec::new();

    let result = compress_with_control(
        &input,
        &CompressOptions::default(),
        &mut |phase| {
            phases.push(phase);
            if phase == Phase::Analyzing {
                cancelled.set(true);
            }
        },
        &signal,
    );

    assert!(matches!(result, Err(GemaError::Cancelled)));
    assert_eq!(phases, vec![Phase::Analyzing]);
}

#[test]
fn progress_wrapper_keeps_documented_phase_sequence() {
    const N: usize = 2;
    let input = pdf_with_jpegs(N);
    let opts = CompressOptions {
        profile: crate::options::Profile::Screen,
        ..Default::default()
    };
    let mut phases = Vec::new();

    compress_with_progress(&input, &opts, &mut |phase| phases.push(phase)).unwrap();

    assert_eq!(
        phases,
        vec![
            Phase::Analyzing,
            Phase::OptimizingImages { done: 0, total: N },
            Phase::OptimizingImages { done: 1, total: N },
            Phase::OptimizingImages { done: 2, total: N },
            Phase::Rewriting,
            Phase::Done,
        ]
    );
}
