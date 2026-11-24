//! A document large enough to be searched on several threads must give the
//! same matches, and the same text after Replace All, as the same bytes held
//! in memory.
use linework_storage::{Document, SearchOptions};
use memchr::memmem;
use std::{fs, io, ops::Range, sync::atomic::AtomicBool};

const MIB: usize = 1024 * 1024;

/// Log-like text with needles scattered through it and runs of one letter
/// laid across every megabyte boundary, where a search chunk ends.
fn sample(len: usize) -> Vec<u8> {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut data = Vec::with_capacity(len + 256);
    while data.len() < len {
        let r = next();
        let line = match r % 23 {
            0 => format!("{:08x} Status=503 retry\r\n", r >> 32),
            1 => format!("{:08x} status=503 gave up\n", r >> 32),
            2 => format!("{:08x} STATUS=503\r", r >> 32),
            _ => format!("{:016x} status=200 ok {}\n", r, "x".repeat((r >> 40) as usize % 60)),
        };
        data.extend_from_slice(line.as_bytes());
    }
    data.truncate(len);
    for boundary in (MIB..len).step_by(MIB) {
        let run = 5 + (boundary / MIB) % 7;
        let start = boundary - 1 - (boundary / MIB) % 4;
        data[start..start + run].fill(b'a');
        data[start - 1] = b'\n';
    }
    let tail = b"end of log";
    let at = len - tail.len();
    data[at..].copy_from_slice(tail);
    data
}

fn options(case: bool, backwards: bool) -> SearchOptions {
    SearchOptions {
        case_sensitive: case,
        backwards,
        wrap: false,
        ..Default::default()
    }
}

fn all(doc: &Document, query: &str, case: bool) -> io::Result<Vec<Range<u64>>> {
    doc.find_all_bounded(query, options(case, false), usize::MAX, &AtomicBool::new(false))
}

fn expected(data: &[u8], needle: &[u8]) -> Vec<Range<u64>> {
    memmem::find_iter(data, needle)
        .map(|i| i as u64..(i + needle.len()) as u64)
        .collect()
}

fn check(doc: &Document, data: &[u8]) -> io::Result<()> {
    assert_eq!(doc.len(), data.len() as u64);
    let folded = data.to_ascii_lowercase();
    for needle in ["status=503", "aa", "aaa", "end of log", "\r\n", "not present anywhere"] {
        let exact = expected(data, needle.as_bytes());
        assert_eq!(all(doc, needle, true)?, exact, "all matches of {needle:?}");
        let any_case = expected(&folded, needle.as_bytes());
        assert_eq!(all(doc, needle, false)?, any_case, "all matches of {needle:?} in any case");

        // Forward from several starting points, including just before a chunk edge.
        for start in [0, 1, 3 * MIB - 2, 5 * MIB + 1, data.len() - 3] {
            let want = memmem::find(&data[start..], needle.as_bytes())
                .map(|i| (start + i) as u64..(start + i + needle.len()) as u64);
            assert_eq!(doc.find(needle, start as u64, options(true, false))?, want, "{needle:?} from {start}");
        }
        // Backwards: the last match that starts before the starting point.
        for start in [data.len(), data.len() - 4, 20 * MIB + 3, 6 * MIB, 5] {
            let reach = (start + needle.len() - 1).min(data.len());
            let want = memmem::rfind(&data[..reach], needle.as_bytes())
                .map(|i| i as u64..(i + needle.len()) as u64);
            assert_eq!(doc.find(needle, start as u64, options(true, true))?, want, "{needle:?} back from {start}");
            let want = memmem::rfind(&folded[..reach], needle.as_bytes())
                .map(|i| i as u64..(i + needle.len()) as u64);
            assert_eq!(doc.find(needle, start as u64, options(false, true))?, want, "{needle:?} back from {start} in any case");
        }
    }
    Ok(())
}

#[test]
fn threads_find_what_one_pass_over_memory_finds() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("search.log");
    let mut data = sample(40 * MIB + 12_345);
    fs::write(&path, &data)?;
    let mut doc = Document::open(&path)?;
    check(&doc, &data)?;

    // The same after edits have split the document into many pieces, some of
    // them in memory and some cut inside a chunk.
    let mut edits = Vec::new();
    for (i, at) in (7..data.len() - 100).step_by(3 * MIB + 77_777).enumerate() {
        let text = format!("<{i} status=503 aaaa>");
        edits.push((at as u64, (i % 5) as u64, text.into_bytes()));
    }
    for (at, removed, text) in edits.iter().rev() {
        data.splice(*at as usize..(*at + *removed) as usize, text.iter().copied());
    }
    doc.replace_many(&edits)?;
    check(&doc, &data)
}

#[test]
fn a_cancelled_search_on_threads_reports_the_interruption() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("cancel.log");
    fs::write(&path, sample(30 * MIB))?;
    let doc = Document::open(&path)?;
    let cancel = AtomicBool::new(true);
    let error = doc
        .find_cancel("not present anywhere", 0, options(true, false), &cancel)
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    Ok(())
}

fn replaced(data: &[u8], needle: &str, with: &str, case: bool, scope: Range<usize>) -> (Vec<u8>, usize) {
    let folded;
    let haystack = if case {
        data
    } else {
        folded = data.to_ascii_lowercase();
        &folded
    };
    let mut out = Vec::with_capacity(data.len());
    let (mut at, mut count) = (0, 0);
    for i in memmem::find_iter(&haystack[scope.start..scope.end], needle.as_bytes()) {
        let i = scope.start + i;
        out.extend_from_slice(&data[at..i]);
        out.extend_from_slice(with.as_bytes());
        at = i + needle.len();
        count += 1;
    }
    out.extend_from_slice(&data[at..]);
    (out, count)
}

fn line_starts(data: &[u8]) -> Vec<u64> {
    let mut starts = vec![0];
    for (i, &b) in data.iter().enumerate() {
        if b == b'\n' || (b == b'\r' && data.get(i + 1) != Some(&b'\n')) {
            starts.push(i as u64 + 1);
        }
    }
    starts
}

fn settle(doc: &Document) {
    while !doc.indexing_complete() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(doc.index_error(), None);
}

/// The document holds exactly `data`, and its line index agrees with a count
/// of the line breaks in `data`.
fn same_text(doc: &Document, data: &[u8], what: &str) -> io::Result<()> {
    assert_eq!(doc.len(), data.len() as u64, "{what}: length");
    assert!(doc.read(0, data.len())? == data, "{what}: text");
    settle(doc);
    let starts = line_starts(data);
    assert_eq!(doc.line_count(), starts.len() as u64, "{what}: line count");
    let step = (starts.len() / 2000).max(1);
    let sampled = (0..starts.len()).step_by(step).chain(starts.len().saturating_sub(40)..starts.len());
    for line in sampled {
        assert_eq!(doc.line_start(line as u64), starts[line], "{what}: start of line {line}");
        assert_eq!(doc.line_of(starts[line]), line as u64, "{what}: line at {}", starts[line]);
        if starts[line] > 0 {
            // The last byte of a line break still belongs to the line it ends.
            let before = starts[line] - 1;
            assert_eq!(doc.line_of(before), line as u64 - 1, "{what}: line at {before}");
        }
    }
    Ok(())
}

#[test]
fn replace_all_matches_an_in_memory_replace_and_keeps_the_line_index() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("replace.log");
    let original = sample(40 * MIB + 4_321);
    fs::write(&path, &original)?;
    let opened = Document::open(&path)?;
    // The line index of a file this size is itself counted by several threads.
    same_text(&opened, &original, "as opened")?;
    let cancel = AtomicBool::new(false);
    let cases = [
        // Longer, shorter, removed, and a replacement that is found again.
        ("status=503", "status=ERROR", true),
        ("status=503", "s=5", false),
        ("status=200 ok ", "", true),
        ("aa", "aaa", true),
        // Matches next to line breaks: text is cut between a CR and its LF.
        ("\n", "\r\n", true),
        ("\r", "", true),
        ("retry\r", "retry", true),
        ("x\n", "\n\n", true),
        // More matches than are kept as pieces: streamed to a temporary file.
        (" ", "  ", true),
        ("absent from the text", "anything", true),
    ];
    for (needle, with, case) in cases {
        let what = format!("{needle:?} -> {with:?}");
        let mut doc = opened.clone();
        let (want, count) = replaced(&original, needle, with, case, 0..original.len());
        assert_eq!(doc.replace_all_cancel(needle, with, options(case, false), &cancel)?, count, "{what}: count");
        same_text(&doc, &want, &what)?;
        if count == 0 {
            assert!(!doc.dirty(), "{what}: nothing changed");
            continue;
        }
        check_finds(&doc, &want)?;
        assert!(doc.undo());
        assert!(doc.read(0, original.len())? == original, "{what}: undo");
        assert!(doc.redo());

        // Again on the result, whose pieces no longer line up with anything,
        // limited to a selection that starts and ends inside pieces.
        let scope = want.len() / 5 + 3..want.len() / 5 * 4 + 1;
        let (second, count) = replaced(&want, "status=200", "S", true, scope.clone());
        let n = doc.replace_all_in("status=200", "S", options(true, false), scope.start as u64..scope.end as u64, &cancel)?;
        assert_eq!(n, count, "{what}: second count");
        same_text(&doc, &second, &format!("{what}, then a selection"))?;
        if count != 0 {
            assert!(doc.undo());
        }
        assert!(doc.read(0, want.len())? == want, "{what}: undo of the second");
    }
    // A selection with more matches than are kept as pieces.
    let scope = original.len() / 7 + 1..original.len() - 9;
    let (want, count) = replaced(&original, " ", "", true, scope.clone());
    let mut doc = opened.clone();
    let n = doc.replace_all_in(" ", "", options(true, false), scope.start as u64..scope.end as u64, &cancel)?;
    assert_eq!(n, count, "dense selection: count");
    same_text(&doc, &want, "dense selection")?;

    // The unsaved result of a Replace All is a small draft that recovers exactly.
    let (want, _) = replaced(&original, "status=503", "status=ERROR", true, 0..original.len());
    let mut doc = opened.clone();
    doc.replace_all_cancel("status=503", "status=ERROR", options(true, false), &cancel)?;
    let draft = dir.path().join("draft.lwr");
    doc.save_recovery(&draft)?;
    assert!(fs::metadata(&draft)?.len() < 16 * MIB as u64, "the draft holds changes, not the text");
    let recovered = Document::recover(&draft)?;
    same_text(&recovered, &want, "recovered draft")?;
    drop(recovered);

    assert!(opened.read(0, original.len())? == original, "the opened document is untouched");
    assert!(fs::read(&path)? == original, "the file is untouched");
    Ok(())
}

fn check_finds(doc: &Document, data: &[u8]) -> io::Result<()> {
    for needle in ["status=503", "ERROR", "end of log"] {
        assert_eq!(all(doc, needle, true)?, expected(data, needle.as_bytes()), "matches of {needle:?} after replacing");
    }
    Ok(())
}

/// Searching in any letter case takes a shortcut for text such as
/// "status=503". Its matches must be those of the general case-blind pattern,
/// including the two Unicode characters that match an ASCII letter: the long s
/// and the Kelvin sign.
#[test]
fn the_any_case_shortcut_agrees_with_the_general_pattern() -> io::Result<()> {
    let pieces = [
        "status=503", "STATUS=503", "Status=503", "\u{17f}tatus=503", "statu\u{17f}=503", "status=5033",
        "xstatus=503", "sstatus=503", "tas\u{212a}=5", "TASK=5", "task=5", "ta\u{17f}\u{212a}=5", "=503", "=5",
        "status=50", "\u{e9}tatus=503", "s\u{74}atus=\u{35}03", " ", "\r\n", "--", "a--b", "A--B", "k9k9k", "K9K9K9K",
        "\u{212a}9\u{212a}9k", "k9", "9k",
    ];
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut text = String::new();
    while text.len() < 3 * MIB {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        text.push_str(pieces[(seed % pieces.len() as u64) as usize]);
    }
    let mut doc = Document::empty();
    doc.replace(0, 0, text.as_bytes())?;
    let cancel = AtomicBool::new(false);
    for needle in ["status=503", "task=5", "a--b", "k9k9k", "=503", "9k", "s=5"] {
        let general = SearchOptions {
            regex: true,
            wrap: false,
            ..Default::default()
        };
        // The needle as a pattern, with everything but letters and digits escaped.
        let pattern: String = needle
            .chars()
            .flat_map(|c| (!c.is_ascii_alphanumeric()).then_some('\\').into_iter().chain([c]))
            .collect();
        let want = doc.find_all_bounded(&pattern, general, usize::MAX, &cancel)?;
        assert!(want.len() > 100, "{needle:?} occurs");
        assert_eq!(all(&doc, needle, false)?, want, "matches of {needle:?} in any case");
        let mut edited = doc.clone();
        assert_eq!(edited.replace_all_cancel(needle, "<>", options(false, false), &cancel)?, want.len());
        assert_eq!(edited.len(), doc.len() - want.iter().map(|r| r.end - r.start).sum::<u64>() + 2 * want.len() as u64);
    }
    Ok(())
}
