//! On macOS Scintilla and Lexilla are linked into the program as static libraries,
//! built by tools/mac-libs.sh into the folder LINEWORK_MAC_LIBS names. On Windows
//! they stay the two DLLs in runtime/ and nothing is done here.
fn main() {
    println!("cargo:rerun-if-env-changed=LINEWORK_MAC_LIBS");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let Some(folder) = std::env::var_os("LINEWORK_MAC_LIBS").map(std::path::PathBuf::from) else {
        panic!("Set LINEWORK_MAC_LIBS to the folder tools/mac-libs.sh built libscintilla.a and liblexilla.a into.");
    };
    for library in ["scintilla", "lexilla"] {
        let file = folder.join(format!("lib{library}.a"));
        assert!(file.is_file(), "{} is missing; run tools/mac-libs.sh", file.display());
        println!("cargo:rerun-if-changed={}", file.display());
        println!("cargo:rustc-link-lib=static={library}");
    }
    println!("cargo:rustc-link-search=native={}", folder.display());
    println!("cargo:rustc-link-lib=framework=Cocoa");
    println!("cargo:rustc-link-lib=framework=QuartzCore");
    println!("cargo:rustc-link-lib=dylib=c++");
}
