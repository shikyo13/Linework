use linework_storage::Document;
use std::io::{Seek, Write};

#[test]
fn a_range_replaced_from_a_file_reads_the_file_and_undoes_in_one_step() -> std::io::Result<()> {
    let mut d = Document::empty();
    d.replace(0, 0, b"head\nc\nb\na\ntail")?;
    let mut f = tempfile::tempfile()?;
    f.write_all(b"a\nb\nc\n")?;
    f.rewind()?;
    d.replace_from_file(5, 6, f)?;
    assert_eq!(d.read(0, d.len() as usize)?, b"head\na\nb\nc\ntail");
    assert!(d.undo());
    assert_eq!(d.read(0, d.len() as usize)?, b"head\nc\nb\na\ntail");
    assert!(d.redo());
    assert_eq!(d.read(0, d.len() as usize)?, b"head\na\nb\nc\ntail");
    assert!(d.replace_from_file(100, 1, tempfile::tempfile()?).is_err());
    Ok(())
}
