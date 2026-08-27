use lopdf::{dictionary, Document, Object, Stream};
use std::path::PathBuf;
use std::process::Command;

fn minimal_pdf() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let content_id = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

fn temp_path(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = format!(
        "gemapdf-cli-test-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    std::env::temp_dir().join(unique).join(name)
}

#[test]
fn help_exposes_signature_policy() {
    let output = Command::new(env!("CARGO_BIN_EXE_gema"))
        .args(["compress", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--signatures"));
    assert!(stdout.contains("strict"));
    assert!(stdout.contains("flatten"));
    assert!(stdout.contains("--max-memory-mib"));
    assert!(stdout.contains("--max-parallel-images"));
    assert!(stdout.contains("--max-image-mib"));
}

#[test]
fn invalid_signature_policy_fails_before_processing() {
    let output = Command::new(env!("CARGO_BIN_EXE_gema"))
        .args([
            "compress",
            "missing.pdf",
            "out.pdf",
            "--signatures",
            "aggressive",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("invalid value 'aggressive'"));
}

#[test]
fn analyze_and_compress_work_end_to_end() {
    let input = temp_path("input.pdf");
    let dir = input.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&input, minimal_pdf()).unwrap();

    let analyze = Command::new(env!("CARGO_BIN_EXE_gema"))
        .arg("analyze")
        .arg(&input)
        .output()
        .unwrap();
    assert!(analyze.status.success());
    assert!(String::from_utf8(analyze.stdout)
        .unwrap()
        .contains("páginas: 1"));

    let compressed = dir.join("output.pdf");
    let compress = Command::new(env!("CARGO_BIN_EXE_gema"))
        .arg("compress")
        .arg(&input)
        .arg(&compressed)
        .output()
        .unwrap();
    assert!(compress.status.success());
    assert!(Document::load(&compressed).is_ok());

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn analyze_json_emits_pure_parseable_json() {
    let input = temp_path("input.pdf");
    let dir = input.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&input, minimal_pdf()).unwrap();

    // stdout con --json debe parsear entero: ni una línea de texto humano.
    let output = Command::new(env!("CARGO_BIN_EXE_gema"))
        .arg("analyze")
        .arg(&input)
        .arg("--json")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("stdout debe ser JSON puro");
    assert_eq!(v["report_schema_version"], 1);
    assert!(v.get("output").is_none(), "analyze no produce salida");

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn human_output_is_unchanged_without_json() {
    let input = temp_path("input.pdf");
    let dir = input.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&input, minimal_pdf()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_gema"))
        .arg("analyze")
        .arg(&input)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("páginas: "));
    assert!(serde_json::from_str::<serde_json::Value>(&stdout).is_err());

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn compress_json_images_adds_the_detail_array() {
    let input = temp_path("input.pdf");
    let dir = input.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&input, minimal_pdf()).unwrap();
    let compressed = dir.join("output.pdf");

    let output = Command::new(env!("CARGO_BIN_EXE_gema"))
        .arg("compress")
        .arg(&input)
        .arg(&compressed)
        .arg("--json-images")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(v["images"]["detail"].is_array());
    assert_eq!(v["document"]["signature_policy"], "flatten");

    std::fs::remove_dir_all(dir).unwrap();
}
