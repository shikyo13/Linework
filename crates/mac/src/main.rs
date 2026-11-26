#[cfg(target_os = "macos")]
fn main() {
    linework_mac::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {}
