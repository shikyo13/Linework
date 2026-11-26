use linework_storage::{Document, SearchOptions};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    thread,
    time::{Duration, Instant},
};
fn indexed(d: &Document) {
    let began = Instant::now();
    while !d.indexing_complete() {
        assert!(d.index_error().is_none());
        assert!(began.elapsed() < Duration::from_secs(15));
        thread::sleep(Duration::from_millis(1));
    }
}
fn lines(bytes: &[u8]) -> Vec<u64> {
    let mut result = vec![0];
    for i in 0..bytes.len() {
        if bytes[i] == 10 || (bytes[i] == 13 && bytes.get(i + 1) != Some(&10)) {
            result.push((i + 1) as u64);
        }
    }
    result
}
#[test]
fn random_edits_match_logical_lines_and_history() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("seams.txt");
    let mut text = vec![b'x'; 131080];
    for i in (0..text.len()).step_by(31) {
        text[i] = 13;
    }
    text[65535] = 13;
    text[65536] = 10;
    fs::write(&path, &text)?;
    let mut d = Document::open(&path)?;
    indexed(&d);
    let mut rand = 17u64;
    let mut next = || {
        rand = rand.wrapping_mul(6364136223846793005).wrapping_add(1);
        rand >> 23
    };
    for step in 0..650 {
        let old = text.clone();
        let pos = (next() % (text.len() + 1) as u64) as usize;
        let del = ((next() % 33) as usize).min(text.len() - pos);
        let choices = [b"\n".as_slice(), b"\r\n", b"\r", b"123", b"\n\r\n", b""];
        let insert = choices[(next() % 6) as usize];
        d.replace(pos as u64, del as u64, insert)?;
        text.splice(pos..pos + del, insert.iter().copied());
        assert_eq!(d.read(0, text.len() + 1)?, text);
        let expected = lines(&text);
        assert_eq!(d.line_count(), expected.len() as u64, "step {step}");
        for _ in 0..7 {
            let n = (next() % expected.len() as u64) as usize;
            assert_eq!(d.line_start(n as u64), expected[n], "step {step}, line {n}");
            let off = next() % (d.len() + 1);
            assert_eq!(
                d.line_of(off),
                expected.partition_point(|&i| i <= off) as u64 - 1
            );
        }
        if step % 29 == 0 && (del > 0 || !insert.is_empty()) {
            assert!(d.undo());
            assert_eq!(d.read(0, old.len() + 1)?, old);
            assert!(d.redo());
            assert_eq!(d.read(0, text.len() + 1)?, text);
        }
    }
    Ok(())
}

#[test]
fn codecs_preserve_round_trip_and_failed_save() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let p = dir.path().join("x");
    for be in [false, true] {
        let text = "ab".repeat(65534) + "😀\r\n";
        let mut b = if be {
            vec![0xfe, 0xff]
        } else {
            vec![0xff, 0xfe]
        };
        for c in text.encode_utf16() {
            b.extend(if be { c.to_be_bytes() } else { c.to_le_bytes() });
        }
        fs::write(&p, &b)?;
        let mut d = Document::open(&p)?;
        assert_eq!(d.read(0, d.len() as usize)?, text.as_bytes());
        d.save(&p)?;
        assert_eq!(fs::read(&p)?, b);
    }
    let bytes = b"caf\xe9 \x80\r\n";
    fs::write(&p, bytes)?;
    let mut d = Document::open(&p)?;
    assert_eq!(d.encoding_name().to_ascii_lowercase(), "windows-1252");
    assert_eq!(d.read(0, 90)?, "café €\r\n".as_bytes());
    d.save(&p)?;
    assert_eq!(fs::read(&p)?, bytes);
    d.replace(0, 0, "😀".as_bytes())?;
    assert!(d.save(&p).is_err());
    assert_eq!(fs::read(&p)?, bytes);
    d.set_encoding("UTF-8 BOM")?;
    d.save(&p)?;
    assert!(fs::read(&p)?.starts_with(b"\xef\xbb\xbf"));
    Ok(())
}
#[test]
fn search_wrap_substitutions_and_boundaries() -> io::Result<()> {
    let mut d = Document::empty();
    let input = ".".repeat(1024 * 1024 - 2) + "🙂 foo café CAFÉ foo";
    d.replace(0, 0, input.as_bytes())?;
    let opts = SearchOptions::default();
    let first = input.find("foo").unwrap() as u64;
    let second = input.rfind("foo").unwrap() as u64;
    assert_eq!(
        d.find("🙂 foo", 0, opts)?.unwrap().start,
        input.find("🙂").unwrap() as u64
    );
    assert_eq!(d.find("FOO", first + 1, opts)?, Some(second..second + 3));
    assert_eq!(
        d.find(
            "FOO",
            first,
            SearchOptions {
                backwards: true,
                ..opts
            }
        )?,
        Some(second..second + 3)
    );
    assert_eq!(
        d.replace_all(
            "caf(é)",
            "x$1",
            SearchOptions {
                regex: true,
                ..opts
            }
        )?,
        2
    );
    assert!(String::from_utf8(d.read(0, d.len() as usize)?)
        .unwrap()
        .ends_with("🙂 foo xé xÉ foo"));
    assert!(d.undo());
    let dir = tempfile::tempdir()?;
    let p = dir.path().join("large");
    let mut file = fs::File::create(&p)?;
    let chunk = b"012345678901234567890123456789\n".repeat(16384);
    for _ in 0..51 {
        file.write_all(&chunk)?;
    }
    file.write_all(b"fn-last(491)\r\n")?;
    drop(file);
    let mut large = Document::open(&p)?;
    let regex = SearchOptions {
        regex: true,
        ..opts
    };
    let result = large.find(r"^fn-last\((\d+)\)$", 0, regex)?.unwrap();
    assert_eq!(
        large.read(result.start, (result.end - result.start) as usize)?,
        b"fn-last(491)"
    );
    assert_eq!(
        large.find(
            r"^fn-last\((\d+)\)$",
            0,
            SearchOptions {
                backwards: true,
                ..regex
            }
        )?,
        Some(result.clone())
    );
    assert_eq!(
        large.replace_all(r"^fn-last\((\d+)\)$", "fn-new($1)", regex)?,
        1
    );
    assert_eq!(large.read(result.start, 40)?, b"fn-new(491)\r\n");
    Ok(())
}
#[test]
fn save_recover_follow_and_external_conflict() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log.txt");
    let snap = dir.path().join("recovery");
    fs::write(&path, b"a\r")?;
    let mut d = Document::open(&path)?;
    {
        let mut f = OpenOptions::new().append(true).open(&path)?;
        f.write_all(b"\nb\n")?;
    }
    assert!(d.refresh_from_disk()?);
    assert_eq!(d.line_count(), 3);
    assert_eq!(d.line_start(1), 3);
    d.replace(0, 1, b"edited")?;
    d.save(&path)?;
    let original = d.read(0, 200)?;
    d.replace(1, 2, &vec![b'q'; 100000])?;
    d.save_recovery(&snap)?;
    let mut recovered = Document::recover(&snap)?;
    assert_eq!(recovered.read(0, 200000)?, d.read(0, 200000)?);
    assert!(recovered.dirty());
    recovered.save(&path)?;
    assert_ne!(fs::read(&path)?, original);
    assert!(d.save(&path).is_err());
    let detached = Document::recover(&snap)?;
    assert!(detached.path().is_none());
    assert_eq!(detached.read(0, 200000)?, d.read(0, 200000)?);
    Ok(())
}

#[test]
fn save_as_preserves_an_open_file_after_external_replacement() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let open_path = dir.path().join("notes.txt");
    let old_path = dir.path().join("notes.old");
    let copy = dir.path().join("edited.txt");
    fs::write(&open_path, b"old text")?;
    let mut doc = Document::open(&open_path)?;
    doc.begin_undo_group();
    doc.replace(0, 0, b"my ")?;
    doc.begin_undo_group();
    doc.replace(doc.len(), 0, b"!")?;
    doc.end_undo_group();
    doc.end_undo_group();
    assert!(doc.undo());
    assert_eq!(doc.read(0, 100)?, b"old text");
    assert!(doc.redo());
    fs::rename(&open_path, &old_path)?;
    fs::write(&open_path, b"external replacement")?;
    assert!(doc.save(&open_path).is_err());
    doc.save(&copy)?;
    assert_eq!(fs::read(copy)?, b"my old text!");
    assert_eq!(fs::read(open_path)?, b"external replacement");
    fs::write(&old_path, b"overwritten")?;
    let next_copy = dir.path().join("second-copy");
    doc.save(&next_copy)?;
    assert_eq!(fs::read(next_copy)?, b"my old text!");
    Ok(())
}

#[test]
fn huge_regex_crosses_lines_pages_and_edits() -> io::Result<()> {
    const EDGE: usize = 256 * 1024;
    let mut bytes = vec![b'x'; 27 * 1024 * 1024];
    let start = EDGE - 1;
    let value = "🙂start\r\nport=1739\r\nend";
    bytes[start..start + value.len()].copy_from_slice(value.as_bytes());
    let dir = tempfile::tempdir()?;
    let p = dir.path().join("long-log.txt");
    fs::write(&p, bytes)?;
    let mut d = Document::open(&p)?;
    let port = start + value.find("1739").unwrap();
    d.replace(port as u64, 4, b"4621")?;
    let mut snapshot = d.clone();
    let opt = SearchOptions {
        regex: true,
        case_sensitive: true,
        ..SearchOptions::default()
    };
    let pattern = r"🙂start\r?\nport=(?<port>\d+)\r?\nend";
    let hit = snapshot.find(pattern, 0, opt)?.unwrap();
    assert_eq!(hit, start as u64..(start + value.len()) as u64);
    assert_eq!(
        snapshot.replacement_for(pattern, hit, opt, "[$port]", &Default::default())?,
        b"[4621]"
    );
    assert_eq!(snapshot.replace_all(pattern, "[$port]", opt)?, 1);
    d.apply_snapshot(snapshot)?;
    assert_eq!(d.read(start as u64, 6)?, b"[4621]");
    assert!(d.undo());
    assert_eq!(d.read(port as u64, 4)?, b"4621");
    assert!(d.undo());
    assert_eq!(d.read(port as u64, 4)?, b"1739");
    assert_eq!(d.find(r"\z", 0, opt)?, Some(d.len()..d.len()));
    assert_eq!(d.find(r"\A", d.len(), opt)?, Some(0..0));
    Ok(())
}

#[test]
fn selected_search_respects_bounds_context_captures_and_wrap() -> io::Result<()> {
    let mut d = Document::empty();
    let value = "cat 😺cat cat catalog cat";
    d.replace(0, 0, value.as_bytes())?;
    let lo = value.find("😺").unwrap() as u64;
    let hi = value.find("alog").unwrap() as u64;
    let scope = lo..hi;
    let opts = SearchOptions {
        regex: true,
        whole_word: true,
        ..SearchOptions::default()
    };
    let cancel = Default::default();
    let first = lo + 4;
    assert_eq!(
        d.find_in("cat", lo, opts, scope.clone(), &cancel)?,
        Some(first..first + 3)
    );
    assert_eq!(
        d.find_in("cat", hi, opts, scope.clone(), &cancel)?,
        Some(first..first + 3)
    );
    assert_eq!(
        d.find_in(
            "cat",
            lo,
            SearchOptions {
                backwards: true,
                ..opts
            },
            scope.clone(),
            &cancel
        )?,
        Some(first + 4..first + 7)
    );
    assert_eq!(
        d.find_in(
            r"\A.",
            lo,
            SearchOptions {
                whole_word: false,
                ..opts
            },
            scope.clone(),
            &cancel
        )?,
        None
    );
    assert_eq!(
        d.find_in(
            "catalog",
            lo,
            SearchOptions {
                regex: false,
                whole_word: false,
                case_sensitive: true,
                backwards: true,
                ..opts
            },
            scope.clone(),
            &cancel
        )?,
        None
    );
    assert_eq!(
        d.replace_all_in(r"(c)(at)", "$1@$2", opts, scope, &cancel)?,
        2
    );
    assert_eq!(
        String::from_utf8(d.read(0, 100)?).unwrap(),
        "cat 😺c@at c@at catalog cat"
    );
    Ok(())
}

#[test]
fn legacy_byte_after_ascii_header_keeps_file_offsets_and_data() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("legacy.txt");
    let mut bytes = vec![b'a'; 70000];
    bytes.extend(b" \x80 end");
    fs::write(&path, &bytes)?;
    let mut d = Document::open(&path)?;
    assert_eq!(d.encoding_name(), "windows-1252");
    assert_eq!(d.read(70000, 30)?, " € end".as_bytes());
    d.replace(0, 1, b"b")?;
    d.save(&path)?;
    bytes[0] = b'b';
    assert_eq!(fs::read(path)?, bytes);
    Ok(())
}

#[test]
fn dense_replace_is_atomic_and_preserves_encoding_history_and_source() -> io::Result<()> {
    let rows = 120_003;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("csv");
    let row = "café,猫,007\r\n";
    let original = ["\u{feff}".as_bytes(), &row.as_bytes().repeat(rows)].concat();
    fs::write(&path, &original)?;
    let mut d = Document::open(&path)?;
    let mut worker = d.clone();
    let options = SearchOptions {
        regex: true,
        case_sensitive: true,
        ..Default::default()
    };
    let trailer = "z".repeat(600);
    let replacement = format!("$2,$label,$3,{trailer},$$");
    let expected = format!("猫,café,007,{trailer},$\r\n");
    assert_eq!(
        worker.replace_all(r"(?<label>café),(猫),(\d+)", &replacement, options)?,
        rows
    );
    let size = (expected.len() * rows) as u64;
    assert!(size > 64 * 1024 * 1024);
    assert_eq!(worker.len(), size);
    for offset in [0, size / 3, size / 2, size - expected.len() as u64] {
        let pos = offset / expected.len() as u64 * expected.len() as u64;
        assert_eq!(worker.read(pos, expected.len())?, expected.as_bytes());
    }
    let mut changed = d.clone();
    changed.replace(0, 0, b"New edit")?;
    let mut stale = d.clone();
    stale.replace(0, 0, b"Worker edit")?;
    assert!(changed.apply_snapshot(stale).is_err());
    d.apply_snapshot(worker)?;
    assert_eq!(d.encoding_name(), "UTF-8 BOM");
    assert!(d.undo());
    assert!(!d.dirty());
    assert_eq!(d.len(), original.len() as u64 - 3);
    assert!(d.redo());
    assert_eq!(d.len(), size);
    let copy = dir.path().join("edited.csv");
    d.save(&copy)?;
    assert_eq!(fs::metadata(&copy)?.len(), size + 3);
    let reopened = Document::open(&copy)?;
    assert_eq!(reopened.encoding_name(), "UTF-8 BOM");
    assert_eq!(
        reopened.read(size - expected.len() as u64, expected.len())?,
        expected.as_bytes()
    );
    assert_eq!(fs::read(path)?, original);
    Ok(())
}

#[test]
fn one_giant_capture_is_replaced_without_materializing_it() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("long.txt");
    let mut f = fs::File::create(&path)?;
    let block = vec![b'x'; 1024 * 1024];
    for _ in 0..33 {
        f.write_all(&block)?;
    }
    drop(f);
    let mut doc = Document::open(&path)?;
    let size = doc.len();
    let opts = SearchOptions {
        regex: true,
        case_sensitive: true,
        ..Default::default()
    };
    assert_eq!(doc.replace_all(r"\A(x+)\z", ">$1|$1<", opts)?, 1);
    assert_eq!(doc.len(), 2 * size + 3);
    assert_eq!(doc.read(0, 2)?, b">x");
    assert_eq!(doc.read(size, 3)?, b"x|x");
    assert_eq!(doc.read(doc.len() - 2, 2)?, b"x<");
    assert!(doc.undo());
    assert_eq!(doc.len(), size);
    assert_eq!(fs::metadata(path)?.len(), size);
    Ok(())
}

#[test]
fn recovery_uses_preserved_bytes_after_backdated_rewrite_or_deletion() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("draft.txt");
    fs::write(&file, b"left right")?;
    let mtime = fs::metadata(&file)?.modified()?;
    let mut d = Document::open(&file)?;
    d.replace(5, 0, b"my ")?;
    let a = dir.path().join("first.lwr");
    let b = dir.path().join("second.lwr");
    d.save_recovery(&a)?;
    d.replace(d.len(), 0, b"!")?;
    d.save_recovery(&b)?;
    assert_eq!(
        Document::recovery_archive(&a)?,
        Document::recovery_archive(&b)?
    );
    fs::write(&file, b"LEFT RIGHT")?;
    OpenOptions::new()
        .write(true)
        .open(&file)?
        .set_modified(mtime)?;
    let recovered = Document::recover(&b)?;
    assert!(recovered.path().is_none());
    assert_eq!(recovered.read(0, 99)?, b"left my right!");
    fs::remove_file(&file)?;
    assert_eq!(Document::recover(&a)?.read(0, 99)?, b"left my right");
    fs::write(Document::recovery_archive(&a)?.unwrap(), b"damaged")?;
    assert!(Document::recover(&a).is_err());
    Ok(())
}
#[test]
fn follow_regrowth_and_explicit_overwrite_preserve_independent_snapshots() -> io::Result<()> {
    use linework_storage::OverwriteTarget;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("log");
    fs::write(&path, b"first\n")?;
    let mut d = Document::open(&path)?;
    fs::write(&path, b"replacement\nlonger\n")?;
    assert!(d.refresh_from_disk()?);
    assert_eq!(d.read(0, 99)?, b"replacement\nlonger\n");
    d.replace(0, 11, b"local")?;
    fs::write(&path, b"external\n")?;
    assert!(d.save(&path).is_err());
    d.overwrite(OverwriteTarget::inspect(&path)?)?;
    assert_eq!(fs::read(&path)?, b"local\nlonger\n");
    assert!(d.undo());
    fs::remove_file(&path)?;
    let vacant = OverwriteTarget::inspect(&path)?;
    fs::write(&path, b"racing writer")?;
    assert!(d.overwrite(vacant).is_err());
    assert_eq!(fs::read(&path)?, b"racing writer");
    Ok(())
}

/// A line's place is estimated only when the lines are one length, and is exact then.
#[test]
fn line_start_is_estimated_only_for_lines_of_one_length() -> io::Result<()> {
    let doc = |text: &[u8]| -> io::Result<Document> {
        let mut d = Document::empty();
        d.replace(0, 0, text)?;
        Ok(d)
    };
    let mut even = Vec::new();
    for n in 0..100_000u32 {
        even.extend_from_slice(format!("line {n:09} of the log\r\n").as_bytes());
    }
    let length = 27u64;
    let d = doc(&even)?;
    for line in [1u64, 5_000, 99_999] {
        assert_eq!(d.estimate_line_start(line), Some(line * length), "line {line}");
    }
    assert_eq!(d.estimate_line_start(100_000), None, "past the end");
    // A last line without its line feed is still found.
    let mut cut = even.clone();
    cut.truncate(cut.len() - 2);
    assert_eq!(doc(&cut)?.estimate_line_start(99_999), Some(99_999 * length));
    // Different lines at the estimate: no estimate.
    let mut later = even.clone();
    later.splice(50_000 * length as usize..50_000 * length as usize, b"a longer line\r\n".iter().copied());
    assert_eq!(doc(&later)?.estimate_line_start(80_000), None);
    // Uneven lines: no estimate.
    let mut uneven = Vec::new();
    for n in 0..100_000u32 {
        uneven.extend_from_slice(format!("{n}\n").as_bytes());
    }
    assert_eq!(doc(&uneven)?.estimate_line_start(50_000), None);
    Ok(())
}
