//! Passes the product's name, `name` under `[workspace.metadata]` in the workspace's `Cargo.toml`,
//! to the code as `PRODUCT_NAME`.

use std::path::Path;

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    println!("cargo::rerun-if-changed={}", manifest.display());
    let text = std::fs::read_to_string(&manifest).expect("the workspace's Cargo.toml can be read");
    let mut section = "";
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
        } else if section == "[workspace.metadata]"
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "name"
        {
            let name = value.trim().trim_matches('"');
            assert!(!name.is_empty(), "the product's name is empty");
            println!("cargo::rustc-env=PRODUCT_NAME={name}");
            return;
        }
    }
    panic!("the workspace's Cargo.toml has no `name` under [workspace.metadata]");
}
