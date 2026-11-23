use linework_storage::{Document, SearchOptions};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    thread,
    time::{Duration, Instant},
};
fn hash(path: &Path) -> io::Result<sha2::digest::Output<Sha256>> {
    let mut digest = Sha256::new();
    io::copy(&mut File::open(path)?, &mut digest)?;
    Ok(digest.finalize())
}
fn main() -> io::Result<()> {
    let gib = env::args()
        .nth(1)
        .map(|x| x.parse::<u64>().expect("GiB"))
        .unwrap_or(3);
    assert!((1..=32).contains(&gib));
    let dir = tempfile::tempdir_in(
        env::args_os()
            .nth(2)
            .unwrap_or_else(|| env::temp_dir().into()),
    )?;
    let p = dir.path().join("source.log");
    let dest = dir.path().join("saved.log");
    let size = gib * (1 << 30) + 128;
    let marker = b"LINEWORK FINAL VERIFIED";
    let edit = b"UPDATED through Rust";
    let marker_at = size - 128 + 32;
    let mut rec = [b' '; 128];
    rec[..22].copy_from_slice(b"data id=565 status=200");
    rec[126] = 13;
    rec[127] = 10;
    let block = rec.repeat(16384);
    {
        let mut writer = File::create(&p)?;
        let mut at = 0;
        while at < size {
            let n = (size - at).min(block.len() as u64) as usize;
            writer.write_all(&block[..n])?;
            at += n as u64;
        }
        writer.seek(SeekFrom::Start(marker_at))?;
        writer.write_all(marker)?;
        writer.sync_all()?;
    }
    let before = Instant::now();
    let mut d = Document::open(&p)?;
    let opened = before.elapsed();
    assert_eq!(d.read(marker_at, marker.len())?, marker);
    while !d.indexing_complete() {
        assert!(d.index_error().is_none());
        thread::sleep(Duration::from_millis(1));
    }
    let indexed = before.elapsed();
    assert_eq!(d.line_count(), size / 128 + 1);
    let seek = Instant::now();
    assert_eq!(d.line_start(size / 128 - 1), size - 128);
    assert_eq!(d.line_of(marker_at), size / 128 - 1);
    let sought = seek.elapsed();
    let search = Instant::now();
    assert_eq!(
        d.find(
            std::str::from_utf8(marker).unwrap(),
            0,
            SearchOptions {
                case_sensitive: true,
                ..Default::default()
            }
        )?
        .unwrap()
        .start,
        marker_at
    );
    let searched = search.elapsed();
    let change = Instant::now();
    d.replace(marker_at, marker.len() as u64, edit)?;
    assert!(d.undo());
    assert_eq!(d.read(marker_at, marker.len())?, marker);
    assert!(d.redo());
    let edited = change.elapsed();
    let save = Instant::now();
    d.save(&dest)?;
    let saved = save.elapsed();
    let mut source = File::open(&p)?;
    let mut expected = Sha256::new();
    io::copy(&mut (&mut source).take(marker_at), &mut expected)?;
    expected.update(edit);
    source.seek(SeekFrom::Start(marker_at + marker.len() as u64))?;
    io::copy(&mut source, &mut expected)?;
    let actual = hash(&dest)?;
    assert_eq!(actual, expected.finalize());
    let report=format!("{} bytes, {} records; open {:?}, fully indexed {:?}, seek final record {:?}, literal scan {:?}, edit+undo+redo {:?}, atomic save {:?}, SHA256 {:x}",
        size,size/128,opened,indexed,sought,searched,edited,saved,actual);
    println!("{report}");
    Ok(())
}
