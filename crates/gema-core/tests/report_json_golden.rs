//! Fixtures dorados del esquema JSON. Si un test de acá falla, el contrato
//! cambió: hay que decidir si es aditivo (actualizar el .json) o incompatible
//! (subir además `REPORT_SCHEMA_VERSION`).
#![cfg(feature = "serde")]

use gema_core::{
    ImageAction, ImageSkipReason, ImageSkipSummary, ImageStat, Report, ReportJson, SignaturePolicy,
    Warning,
};

fn assert_golden(name: &str, view: &ReportJson) {
    let actual = serde_json::to_string_pretty(view).unwrap() + "\n";
    let path = format!("{}/tests/golden/{name}.json", env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("falta el fixture {path}; generalo con UPDATE_GOLDEN=1"));
    assert_eq!(actual, expected, "el esquema JSON cambió en `{name}`");
}

fn empty_report() -> Report {
    Report {
        pages: 3,
        original_size: 2048,
        ..Default::default()
    }
}

#[test]
fn golden_analyze() {
    assert_golden("analyze", &ReportJson::from_report(&empty_report(), None));
}

#[test]
fn golden_compress_simple() {
    let mut r = empty_report();
    r.output_size = Some(1024);
    r.ratio = Some(0.5);
    r.images = vec![ImageStat {
        object_id: 4,
        original_bytes: 900,
        output_bytes: 300,
        action: ImageAction::Downsampled,
        skip_reason: None,
    }];
    assert_golden(
        "compress_simple",
        &ReportJson::from_report(&r, Some(SignaturePolicy::Flatten)),
    );
}

#[test]
fn golden_with_skips() {
    let mut r = empty_report();
    r.output_size = Some(2000);
    r.ratio = Some(0.976);
    r.images = vec![ImageStat {
        object_id: 11,
        original_bytes: 500,
        output_bytes: 500,
        action: ImageAction::Skipped,
        skip_reason: Some(ImageSkipReason::Jpx),
    }];
    r.image_skip_summary = vec![ImageSkipSummary {
        reason: ImageSkipReason::Jpx,
        images: 1,
        original_bytes: 500,
    }];
    r.warnings = vec![Warning::ImageSkipped(11)];
    assert_golden(
        "with_skips",
        &ReportJson::from_report_with_images(&r, Some(SignaturePolicy::Flatten)),
    );
}

fn signed_report() -> Report {
    let mut r = empty_report();
    r.output_size = Some(2048);
    r.ratio = Some(1.0);
    r.is_signed = true;
    r.warnings = vec![Warning::SignedDocument];
    r
}

#[test]
fn golden_signed_flatten() {
    let mut r = signed_report();
    r.flattened_signatures = 2;
    assert_golden(
        "signed_flatten",
        &ReportJson::from_report(&r, Some(SignaturePolicy::Flatten)),
    );
}

#[test]
fn golden_signed_strict() {
    assert_golden(
        "signed_strict",
        &ReportJson::from_report(&signed_report(), Some(SignaturePolicy::Strict)),
    );
}

#[test]
fn golden_signed_ignore() {
    assert_golden(
        "signed_ignore",
        &ReportJson::from_report(&signed_report(), Some(SignaturePolicy::Ignore)),
    );
}
