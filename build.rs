fn main() {
    println!("cargo:rerun-if-changed=assets/linework.ico");
    println!("cargo:rerun-if-changed=assets/linework.manifest");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/linework.ico")
            .set_manifest_file("assets/linework.manifest")
            .set("ProductName", "Linework")
            .set("FileDescription", "Linework text editor");
        res.compile().expect("Windows desktop resources");
    }
}
