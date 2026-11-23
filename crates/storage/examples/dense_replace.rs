use linework_storage::{Document, SearchOptions};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::File,
    io::{self, Write},
    time::Instant,
};

fn repeat_rows(writer: &mut impl Write, row: &[u8], count: usize) -> io::Result<()> {
    let block = row.repeat(2048);
    for _ in 0..count / 2048 {
        writer.write_all(&block)?;
    }
    writer.write_all(&block[..(count % 2048) * row.len()])
}
fn document_hash(doc: &Document) -> io::Result<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut buf = vec![0; 256 * 1024];
    let mut at = 0;
    while at < doc.len() {
        let n = (doc.len() - at).min(buf.len() as u64) as usize;
        doc.read_into(at, &mut buf[..n])?;
        hash.update(&buf[..n]);
        at += n as u64;
    }
    Ok(hash.finalize().into())
}
fn main() -> io::Result<()> {
    let rows = env::args()
        .nth(1)
        .map(|s| s.parse().expect("row count"))
        .unwrap_or(2_000_000usize);
    let dir = tempfile::tempdir_in(
        env::args_os()
            .nth(2)
            .unwrap_or_else(|| env::temp_dir().into()),
    )?;
    let path = dir.path().join("dense.csv");
    let before = "résumé,港,8732\r\n";
    let note = "csv-export/".repeat(5);
    let after = format!("港,résumé,8732,{note}\r\n");
    let mut source = File::create(&path)?;
    repeat_rows(&mut source, before.as_bytes(), rows)?;
    source.sync_all()?;
    drop(source);
    let mut doc = Document::open(&path)?;
    let mut original = Sha256::new();
    repeat_rows(&mut original, before.as_bytes(), rows)?;
    let original: [u8; 32] = original.finalize().into();
    let mut expected = Sha256::new();
    repeat_rows(&mut expected, after.as_bytes(), rows)?;
    let expected: [u8; 32] = expected.finalize().into();
    let start = Instant::now();
    let n = doc.replace_all(
        r"(?<label>résumé),(港),(\d+)",
        &format!("$2,$label,$3,{note}"),
        SearchOptions {
            regex: true,
            case_sensitive: true,
            ..Default::default()
        },
    )?;
    let time = start.elapsed();
    assert_eq!(n, rows);
    assert_eq!(document_hash(&doc)?, expected);
    let output = doc.len();
    assert!(doc.undo());
    assert_eq!(document_hash(&doc)?, original);
    assert!(!doc.dirty());
    assert!(doc.redo());
    assert_eq!(document_hash(&doc)?, expected);
    let mut unchanged = Sha256::new();
    io::copy(&mut File::open(&path)?, &mut unchanged)?;
    assert_eq!(<[u8; 32]>::from(unchanged.finalize()), original);
    println!("{rows} regex replacements in {time:?}; {} source bytes; {output} output bytes; complete result, undo, redo, and unchanged source hashes verified",before.len()*rows);
    Ok(())
}
