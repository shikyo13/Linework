use linework_storage::{ChangeMap, Document};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
};
use tempfile::tempdir;

#[test]
fn unicode_points_cross_index_seams_and_edits_retain_one_undo() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("unicode.txt");
    let text = format!(
        "{}🌍\r\nélève e\u{301} ∑ 끝\n{}",
        "a".repeat(256 * 1024 - 2),
        "z".repeat(32768)
    );
    fs::write(&path, &text).unwrap();
    let mut doc = Document::open(&path).unwrap();
    let pos = text.find('🌍').unwrap();
    assert_eq!(doc.char_len().unwrap(), text.chars().count() as u64);
    for (byte, _) in text.char_indices().skip(pos - 3).take(20) {
        let point = text[..byte].chars().count() as u64;
        assert_eq!(doc.byte_to_char(byte as u64).unwrap(), point);
        assert_eq!(doc.char_to_byte(point).unwrap(), byte as u64);
    }
    assert!(doc.byte_to_char(pos as u64 + 2).is_err());
    assert!(doc.char_to_byte(doc.char_len().unwrap() + 1).is_err());
    let mut edit = doc.clone();
    edit.begin_undo_group();
    edit.replace(pos as u64, 4, "🐝🐝".as_bytes()).unwrap();
    edit.replace(0, 0, "π".as_bytes()).unwrap();
    edit.spill_inserted().unwrap();
    let expected = format!("π{}", text.replacen("🌍", "🐝🐝", 1));
    let ch = expected.find('끝').unwrap();
    assert_eq!(
        edit.char_to_byte(expected[..ch].chars().count() as u64)
            .unwrap(),
        ch as u64
    );
    edit.end_undo_group();
    doc.apply_snapshot(edit).unwrap();
    assert_eq!(doc.read(0, expected.len()).unwrap(), expected.as_bytes());
    // Only contiguous disk-backed slices are eligible for direct mapping.
    let (mut file, off) = doc.file_range(0..2).unwrap().unwrap();
    file.seek(SeekFrom::Start(off)).unwrap();
    let mut buf = [0; 2];
    file.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, "π".as_bytes());
    assert!(doc.file_range(0..5).unwrap().is_none());
    assert!(doc.undo());
    assert_eq!(doc.read(0, text.len()).unwrap(), text.as_bytes());
    assert!(!doc.can_undo());
    assert!(doc.redo());
}

#[test]
fn successive_overlapping_edits_keep_original_coordinates() {
    // Give each original slot its own identity; inserted slots have no source.
    let mut model: Vec<Option<u64>> = (0..96).map(Some).collect();
    let mut map = ChangeMap::new(model.len() as u64);
    let mut seed = 7u64;
    let mut draw = |n: usize| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (seed >> 33) as usize % n
    };
    for _ in 0..1000 {
        let start = draw(model.len() + 1);
        let del = draw((model.len() - start).min(8) + 1);
        let add = draw(9);
        model.splice(start..start + del, vec![None; add]);
        map.replace(start as u64, del as u64, add as u64).unwrap();
        let mut rebuilt: Vec<_> = (0..96u64).map(Some).collect();
        for (p, d, i) in map.changes().into_iter().rev() {
            rebuilt.splice(p as usize..(p + d) as usize, vec![None; i as usize]);
        }
        assert_eq!(model, rebuilt);
    }
    let old = map.changes();
    assert!(map.replace(u64::MAX, 0, 1).is_err());
    assert_eq!(old, map.changes());
}
