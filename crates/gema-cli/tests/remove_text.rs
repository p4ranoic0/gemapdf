use lopdf::{dictionary, Document, Object, Stream};
use std::path::PathBuf;
use std::process::Command;

fn pdf(sig_flags: Option<i64>) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font = doc.add_object(
        dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
    );
    let content = doc.add_object(Stream::new(
        dictionary! {},
        b"BT /F1 12 Tf 10 10 Td (hola) Tj ET".to_vec(),
    ));
    let page = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()], "Contents" => content, "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } } });
    doc.objects.insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![Object::Reference(page)], "Count" => 1 }));
    let mut catalog = dictionary! { "Type" => "Catalog", "Pages" => pages_id };
    if let Some(flags) = sig_flags {
        catalog.set("AcroForm", dictionary! { "SigFlags" => flags });
    }
    let catalog_id = doc.add_object(catalog);
    doc.trailer.set("Root", catalog_id);
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

fn workdir(name: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("gema-cli-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    (dir.join("in.pdf"), dir.join("out.pdf"))
}
fn run(input: &PathBuf, output: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gema"))
        .arg("remove-text")
        .arg(input)
        .arg(output)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn zero_regions_is_a_usage_error_and_writes_nothing() {
    let (input, output) = workdir("zero");
    std::fs::write(&input, pdf(None)).unwrap();
    let out = run(&input, &output, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(!output.exists());
    assert!(String::from_utf8_lossy(&out.stderr).contains("at least one --region"));
}
#[test]
fn clean_removal_exits_0_with_json_on_stdout() {
    let (input, output) = workdir("clean");
    std::fs::write(&input, pdf(None)).unwrap();
    let out = run(&input, &output, &["--region", "1:0,0,612,792", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(output.exists());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["ok"], true);
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["regions"][0]["status"], "removed");
    assert_eq!(v["regions"][0]["removed_glyphs"], 4);
    assert!(out.stderr.is_empty());
}
#[test]
fn mixed_removed_and_nothing_found_is_still_0() {
    let (input, output) = workdir("mixed");
    std::fs::write(&input, pdf(None)).unwrap();
    let out = run(
        &input,
        &output,
        &["--region", "a@1:0,0,612,792", "--region", "b@1:500,700,1,1"],
    );
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("region a (page 1): removed, 4 glyphs"),
        "{text}"
    );
    assert!(text.contains("region b (page 1): nothing_found"), "{text}");
    assert!(out.stderr.is_empty());
    assert!(output.exists());
}
#[test]
fn signed_and_modified_exits_2_and_still_writes() {
    let (input, output) = workdir("signed");
    std::fs::write(&input, pdf(Some(1))).unwrap();
    let out = run(&input, &output, &["--region", "1:0,0,612,792", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        output.exists(),
        "el 2 significa escrito pero no garantizado"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["exit_code"], 2);
    assert_eq!(v["signature"]["sig_flags"], true);
    assert!(out.stderr.is_empty());
}
#[test]
fn signed_but_untouched_exits_0() {
    let (input, output) = workdir("signed-untouched");
    std::fs::write(&input, pdf(Some(1))).unwrap();
    let out = run(&input, &output, &["--region", "1:500,700,1,1"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(output.exists());
    assert!(out.stderr.is_empty());
}
#[test]
fn json_error_shape_on_unparseable_input() {
    let (input, output) = workdir("garbage");
    std::fs::write(&input, b"not a pdf").unwrap();
    let out = run(&input, &output, &["--region", "1:0,0,1,1", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!output.exists());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert!(v["error"]
        .as_str()
        .unwrap()
        .starts_with("could not parse the PDF"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("error:"));
}

#[test]
fn json_clap_error_is_structured() {
    let (input, output) = workdir("clap-json");
    std::fs::write(&input, pdf(None)).unwrap();
    let out = run(&input, &output, &["--json", "--bogus"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!output.exists());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["exit_code"], 1);
    assert!(out.stderr.starts_with(b"error:"));
}
#[test]
fn repeated_json_is_accepted() {
    let (input, output) = workdir("json-repeat");
    std::fs::write(&input, pdf(None)).unwrap();
    let out = run(
        &input,
        &output,
        &["--region", "1:0,0,612,792", "--json", "--json"],
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(serde_json::from_slice::<serde_json::Value>(&out.stdout).is_ok());
    assert!(out.stderr.is_empty());
}
#[test]
fn region_without_value_json_is_structured_error() {
    let (input, output) = workdir("region-missing");
    std::fs::write(&input, pdf(None)).unwrap();
    let out = run(&input, &output, &["--region", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!output.exists());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["ok"],
        false
    );
    assert!(out.stderr.starts_with(b"error:"));
}
#[test]
fn json_after_double_dash_does_not_activate_json_mode() {
    let (input, output) = workdir("double-dash");
    std::fs::write(&input, pdf(None)).unwrap();
    let out = run(
        &input,
        &output,
        &["--region", "1:0,0,612,792", "--", "--json"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(out.stderr.starts_with(b"error:"));
}
#[test]
fn unknown_flag_without_json_is_text_error() {
    let (input, output) = workdir("unknown");
    std::fs::write(&input, pdf(None)).unwrap();
    let out = run(&input, &output, &["--bogus"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(out.stderr.starts_with(b"error:"));
}
#[test]
fn signed_untouched_broken_annotation_is_not_guaranteed() {
    let (input, output) = workdir("signed-gap");
    let mut bytes = pdf(Some(1));
    let mut doc = Document::load_mem(&bytes).unwrap();
    let page = doc.get_pages()[&1];
    doc.get_dictionary_mut(page)
        .unwrap()
        .set("Annots", vec![Object::Reference((999, 0))]);
    bytes.clear();
    doc.save_to(&mut bytes).unwrap();
    std::fs::write(&input, bytes).unwrap();
    let out = run(&input, &output, &["--region", "1:500,700,1,1", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["not_guaranteed_because"],
        serde_json::json!(["inspection_incomplete"])
    );
    assert!(output.exists());
}
#[test]
fn signed_untouched_clean_is_zero_with_signature() {
    let (input, output) = workdir("signed-clean");
    std::fs::write(&input, pdf(Some(1))).unwrap();
    let out = run(&input, &output, &["--region", "1:500,700,1,1", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["signature"]["sig_flags"], true);
    assert!(output.exists());
}
#[test]
fn output_directory_missing_is_usage_error() {
    let input = std::env::temp_dir().join(format!("gema-cli-input-{}", std::process::id()));
    std::fs::write(&input, pdf(None)).unwrap();
    let output = input.with_file_name("missing-dir/out.pdf");
    let out = Command::new(env!("CARGO_BIN_EXE_gema"))
        .args(["remove-text"])
        .arg(&input)
        .arg(&output)
        .args(["--region", "1:0,0,612,792"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(out.stderr.starts_with(b"error:"));
}
#[test]
fn compress_usage_error_never_prints_the_edit_json() {
    let out = Command::new(env!("CARGO_BIN_EXE_gema"))
        .args(["compress", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(out.stderr.starts_with(b"error:"));
}
