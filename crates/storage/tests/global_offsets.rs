use linework_storage::{Document, FoldMap, SelectionSet};
use std::{io, sync::atomic::AtomicBool};

#[test]
fn folded_projection_retains_original_offsets_and_skips_quoted_delimiters() -> io::Result<()> {
    let cancel = AtomicBool::new(false);
    for (language, input, expected) in [
        (
            "C++",
            "void f() {\n const char *s=\"}\"; // }\n nested {\n }\n}\nafter\n",
            "void f() {\n}\nafter\n",
        ),
        (
            "C++",
            "void f() {\n /* }\n */ return;\n}\nafter\n",
            "void f() {\n}\nafter\n",
        ),
        (
            "Python",
            "def f():\r\n    pass\r\nafter()\r\n",
            "def f():\r\nafter()\r\n",
        ),
        (
            "Markdown",
            "# One\nbody\n## Sub\nparagraph\n# Two\n",
            "# One\n# Two\n",
        ),
        ("XML", "<root>\n <child/>\n</root>\n", "<root>\n</root>\n"),
    ] {
        let mut doc = Document::empty();
        doc.replace(0, 0, input.as_bytes())?;
        let folds = doc.structural_folds(language, Some(0), &cancel)?;
        assert_eq!(folds.len(), 1, "{language}");
        let f = folds[0];
        let map = FoldMap::new(folds);
        let view = map.project(&doc)?;
        assert_eq!(
            view.read(0, view.len() as usize)?,
            expected.as_bytes(),
            "{language}"
        );
        assert_eq!(map.physical(map.visible(f.start)), f.end);
        assert_eq!(map.physical_end(map.visible(f.end)), f.start);
    }
    Ok(())
}

#[test]
fn spilled_file_wide_selections_edit_as_one_undo_step() -> io::Result<()> {
    let cancel = AtomicBool::new(false);
    let mut doc = Document::empty();
    let original = "cat dog\r\n".repeat(20000);
    doc.replace(0, 0, original.as_bytes())?;
    let set = doc.occurrences("cat", &cancel)?;
    assert_eq!(set.len(), 20000);
    assert_eq!(set.at(19999)?, 179991..179994);
    let (set, at) = doc.edit_carets(&set, 19999, &cancel, |_, r, _| Ok((r, b"L".to_vec())))?;
    assert_eq!(at, 139994);
    assert_eq!(set.at(19999)?, at..at);
    assert_eq!(doc.read(at - 1, 7)?, b"L dog\r\n");
    assert!(doc.undo());
    assert_eq!(doc.read(0, original.len())?, original.as_bytes());
    let lines = SelectionSet::split_lines(&doc, 0..doc.len(), &cancel)?;
    assert_eq!(lines.len(), 20000);
    Ok(())
}
