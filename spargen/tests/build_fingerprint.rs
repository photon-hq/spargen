#[path = "../build_fingerprint.rs"]
mod build_fingerprint;

use std::fs;
use std::path::Path;

fn write_fixture(root: &Path) {
    let manifest = root.join("spargen");
    fs::create_dir_all(manifest.join("src/nested")).unwrap();
    fs::write(root.join("Cargo.lock"), "workspace lock\n").unwrap();
    fs::write(manifest.join("Cargo.toml"), "manifest\n").unwrap();
    fs::write(manifest.join("build.rs"), "build script\n").unwrap();
    fs::write(
        manifest.join("build_fingerprint.rs"),
        "fingerprint helper\n",
    )
    .unwrap();
    fs::write(manifest.join("src/lib.rs"), "library\n").unwrap();
    fs::write(manifest.join("src/nested/module.rs"), "module\n").unwrap();
}

fn fingerprint_payload(manifest: &Path) -> (Vec<String>, Vec<u8>) {
    let inputs = build_fingerprint::inputs(manifest);
    let labels = inputs
        .iter()
        .map(|input| input.label.clone())
        .collect::<Vec<_>>();
    let mut payload = Vec::new();
    for input in inputs {
        payload.extend_from_slice(&(input.label.len() as u64).to_be_bytes());
        payload.extend_from_slice(input.label.as_bytes());
        let bytes = fs::read(input.path).unwrap();
        payload.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        payload.extend_from_slice(&bytes);
    }
    (labels, payload)
}

#[test]
fn build_fingerprint_is_independent_of_checkout_location() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_fixture(first.path());
    write_fixture(second.path());

    let (first_labels, first_payload) = fingerprint_payload(&first.path().join("spargen"));
    let (second_labels, second_payload) = fingerprint_payload(&second.path().join("spargen"));

    assert_eq!(first_labels, second_labels);
    assert_eq!(first_payload, second_payload);
    assert_eq!(
        first_labels,
        vec![
            "../Cargo.lock",
            "Cargo.toml",
            "build.rs",
            "build_fingerprint.rs",
            "src/lib.rs",
            "src/nested/module.rs",
        ]
    );
}
