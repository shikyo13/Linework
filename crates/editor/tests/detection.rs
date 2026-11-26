use linework_editor::detect_language;
use std::path::Path;

#[test]
fn detection_prefers_explicit_content_and_safe_defaults() {
    assert_eq!(
        detect_language(
            Some(Path::new("config.txt")),
            "#!/usr/bin/env -S python3 -u\n"
        ),
        "Python"
    );
    assert_eq!(
        detect_language(Some(Path::new("tools")), "// vim: set ft=rust :\n"),
        "Rust"
    );
    assert_eq!(
        detect_language(Some(Path::new("README.MD")), ""),
        "Markdown"
    );
    assert_eq!(
        detect_language(Some(Path::new("CMakeLists.txt")), ""),
        "CMake"
    );
    assert_eq!(detect_language(None, "{\"hello\":42}"), "JSON");
    assert_eq!(
        detect_language(None, "this is ordinary writing"),
        "Plain Text"
    );
}
