use super::*;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

#[test]
fn previews_capture_groups_selectively_and_preserves_buffers_and_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    let c = dir.path().join("c.txt");
    fs::write(&a, "old on disk\n").unwrap();
    fs::write(
        &b,
        [
            &[0xff, 0xfe][..],
            &"😀 red1\r\nred2\rblue"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect::<Vec<u8>>(),
        ]
        .concat(),
    )
    .unwrap();
    fs::write(&c, "red3\n").unwrap();
    let mut q = SearchRequest::new(dir.path().into(), r"red(\d)".into());
    q.regex = true;
    q.match_case = true;
    q.include = "*.txt".into();
    q.replacement = Some("blue$1".into());
    let stream = OpenStream {
        path: a.clone(),
        buffer_id: None,
        revision: 22,
        reader: Box::new(std::io::BufReader::with_capacity(
            3,
            std::io::Cursor::new(b"red9\nred8\n"),
        )),
    };
    let r = search_with_streams(&q, vec![stream], &AtomicBool::new(false)).unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.hits.len(), 5);
    let indices = r
        .hits
        .iter()
        .enumerate()
        .filter(|(_, h)| h.matched != "red2")
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    fs::write(&c, "red3 changed\n").unwrap();
    let done = r.apply_selected(&indices, &[a.clone()]);
    assert_eq!(done.applied, 1, "{:?}", done.errors);
    assert_eq!(done.errors.len(), 1);
    assert_eq!(fs::read_to_string(&a).unwrap(), "old on disk\n");
    let mut text = "red9\nred8\n".to_owned();
    assert!(done.open[0].apply_to_text(&mut text, 23).is_err());
    done.open[0].apply_to_text(&mut text, 22).unwrap();
    assert_eq!(text, "blue9\nblue8\n");
    let raw = fs::read(b).unwrap();
    assert_eq!(&raw[..2], &[255, 254]);
    let u: Vec<_> = raw[2..]
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    assert_eq!(String::from_utf16(&u).unwrap(), "😀 blue1\r\nred2\rblue");
}

#[test]
fn open_tabs_with_matching_names_retain_distinct_buffer_identity() {
    let mut q = SearchRequest::new("folder-that-does-not-exist".into(), "name".into());
    q.buffers_only = true;
    q.replacement = Some("renamed".into());
    let tabs = [41, 82].map(|id| OpenStream {
        path: "Untitled".into(),
        buffer_id: Some(id),
        revision: 4,
        reader: Box::new(std::io::Cursor::new("name")),
    });
    let r = search_with_streams(&q, Vec::from(tabs), &AtomicBool::new(false)).unwrap();
    assert_eq!(
        r.hits.iter().map(|h| h.buffer_id).collect::<Vec<_>>(),
        [Some(41), Some(82)]
    );
    let pending = r.apply_selected(&[1], &[]);
    assert_eq!(pending.open.len(), 1);
    assert_eq!(pending.open[0].buffer_id, Some(82));
    assert!(pending.open[0].validate("name", 4));
    assert!(!pending.open[0].validate("name", 5));
}
#[test]
fn filters_and_positions_across_crlf_and_empty_lines() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("lib")).unwrap();
    fs::write(dir.path().join("lib/a.rs"), "α\r\nβ match\rnext match\n").unwrap();
    fs::write(dir.path().join("lib/test.rs"), "match").unwrap();
    let mut q = SearchRequest::new(dir.path().into(), "MATCH".into());
    q.include = "*.rs".into();
    q.exclude = "test.rs".into();
    let r = search(&q, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        r.hits
            .iter()
            .map(|h| (h.line, h.column, h.offset))
            .collect::<Vec<_>>(),
        [(2, 3, 7), (3, 6, 18)]
    );
}
#[test]
fn native_symbols_and_references_skip_comments() {
    for (name, src, expected) in [
        (
            "lib.rs",
            "fn square(x:i32)->i32{x*x} fn a(){square(4);} // square",
            "square",
        ),
        (
            "lib.py",
            "def square(x):\n    return x*x\nsquare(4)",
            "square",
        ),
        (
            "lib.ts",
            "export function square(x:number){return x*x;}",
            "square",
        ),
        (
            "lib.cs",
            "class Demo { int Square(int x){return x*x;} }",
            "Square",
        ),
        ("file.md", "# First heading\n\nContent", "First heading"),
    ] {
        let symbols = symbols_for_file(Path::new(name), src);
        assert!(
            symbols.iter().any(|s| s.name == expected),
            "{name}: {symbols:?}"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("lib.rs"),
        "fn square(){} fn a(){square();} // square\n",
    )
    .unwrap();
    let index = index(dir.path(), &AtomicBool::new(false));
    assert!(index.issues.is_empty(), "{:?}", index.issues);
    assert_eq!(index.references("square").len(), 1);
    assert_eq!(index.complete("squ", 5), vec!["square".to_string()]);
}
#[test]
fn captures_build_and_real_filter_output() {
    let dir = tempfile::tempdir().unwrap();
    let report = build(
        dir.path(),
        "echo src/main.rs:2:4: error: unexpected identifier",
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(report.success, "{}", report.output);
    assert_eq!(report.diagnostics.len(), 1);
    assert_eq!(report.diagnostics[0].column, 4);
    let p = dir.path().join("tools.json");
    ToolConfig::ensure_example(&p).unwrap();
    let tool = ToolConfig::load(&p).unwrap().tools.remove(0);
    let report = run_tool(
        &tool,
        dir.path(),
        None,
        "{\"x\":2}",
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(report.success, "{}", report.output);
    assert!(report.output.contains("  \"x\": 2"));
}

#[test]
fn diagnostics_do_not_treat_elapsed_time_as_a_path() {
    let out = diagnostics(
        std::path::Path::new("."),
        "Time Elapsed 00:00:05\nsource.rs:4:2: real error",
    );
    assert_eq!(out.len(), 1);
    assert_eq!((out[0].line, out[0].column), (4, 2));
}
#[cfg(windows)]
#[test]
fn windows_path_identity_and_component_boundaries() {
    use std::path::Path;
    assert_eq!(
        path_key(Path::new(r"\\?\UNC\Host\Share\a")),
        path_key(Path::new(r"\\host\share\A"))
    );
    assert_eq!(
        relative_path(
            Path::new(r"\\?\c:\PROJECT\Src\A.rs"),
            Path::new(r"C:\Project")
        ),
        Some(Path::new(r"Src\A.rs"))
    );
    assert!(relative_path(Path::new(r"C:\Projects\A"), Path::new(r"c:\Project")).is_none());
}

#[test]
fn workspace_search_retries_late_1252_and_edits_bomless_utf16_without_duplicate_hits() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("legacy.txt");
    let utf = dir.path().join("wide.txt");
    let mut bytes = b"match\n".to_vec();
    bytes.extend(b"long ascii line\n".repeat(800));
    bytes.extend_from_slice(b"caf\xe9 match\n");
    fs::write(&legacy, &bytes).unwrap();
    let wide = |s: &str| {
        s.encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>()
    };
    fs::write(&utf, wide("wide match\r\n")).unwrap();
    let mut q = SearchRequest::new(dir.path().into(), "match".into());
    q.replacement = Some("done".into());
    let r = search(&q, &AtomicBool::new(false)).unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.hits.len(), 3);
    let done = r.apply_selected(&[0, 1, 2], &[]);
    assert!(done.errors.is_empty(), "{:?}", done.errors);
    assert_eq!(fs::read(utf).unwrap(), wide("wide done\r\n"));
    let out = fs::read(legacy).unwrap();
    assert!(out.starts_with(b"done\n") && out.ends_with(b"caf\xe9 done\n"));
}
