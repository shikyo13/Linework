//! lw reports Linework's version, read from the repository's root Cargo.toml
//! (the same line the installer reads), so a release bumps one place.
use std::{env, fs, path::Path};

fn main() {
    let root = Path::new(&env::var("CARGO_MANIFEST_DIR").expect("set by cargo")).join("../../Cargo.toml");
    println!("cargo:rerun-if-changed={}", root.display());
    let text = fs::read_to_string(&root).unwrap_or_else(|e| panic!("cannot read {}: {e}", root.display()));
    let mut in_package = false;
    let mut version = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package {
            if let Some(value) = line.strip_prefix("version").map(str::trim_start).and_then(|v| v.strip_prefix('=')) {
                version = Some(value.trim().trim_matches('"').to_owned());
                break;
            }
        }
    }
    let version = version.unwrap_or_else(|| panic!("no [package] version in {}", root.display()));
    println!("cargo:rustc-env=LINEWORK_VERSION={version}");
}
