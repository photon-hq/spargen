mod build_fingerprint;
#[path = "src/source/sha256.rs"]
mod sha256;

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());

    let mut input = b"spargen-build-fingerprint-v2\0".to_vec();
    for source in build_fingerprint::inputs(&manifest_dir) {
        println!("cargo:rerun-if-changed={}", source.path.display());
        append(&mut input, source.label.as_bytes());
        append(&mut input, &std::fs::read(&source.path).unwrap());
    }
    println!(
        "cargo:rustc-env=SPARGEN_BUILD_FINGERPRINT={}",
        sha256::sha256_hex(&input)
    );
}

fn append(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}
