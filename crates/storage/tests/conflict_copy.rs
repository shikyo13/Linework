use linework_storage::Document;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

fn digest(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut sha = Sha256::new();
    let mut block = vec![0; 1024 * 1024];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        sha.update(&block[..n]);
    }
    Ok(format!("{:x}", sha.finalize()))
}

fn write_paged_text(f: &mut File) -> io::Result<()> {
    let block = vec![b'x'; 1024 * 1024];
    for _ in 0..26 {
        f.write_all(&block)?;
    }
    f.sync_all()
}

#[test]
fn external_overwrite_preserves_full_draft_on_save_as() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let original = dir.path().join("original.log");
    let copy = dir.path().join("local.log");
    let page = b"unchanged line\r\n".repeat(4000);
    fs::write(&original, &page)?;
    let mut doc = Document::open(&original)?;
    doc.replace(0, 0, b"local\n")?;
    fs::write(&original, b"external revision\n")?;
    assert!(doc.save(&original).is_err());
    assert!(doc.dirty());
    doc.save(&copy)?;
    assert_eq!(fs::read(&copy)?, [b"local\n".as_slice(), &page].concat());
    assert_eq!(fs::read(&original)?, b"external revision\n");
    assert!(!doc.dirty());
    assert!(doc.undo());
    doc.save(&copy)?;
    assert_eq!(fs::read(&copy)?, page);
    Ok(())
}

#[test]
fn same_length_conflict_preserving_modified_time_cannot_overwrite_either_revision() -> io::Result<()>
{
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("source.txt");
    let copy = dir.path().join("draft.txt");
    fs::write(&file, b"original\n")?;
    let modified = fs::metadata(&file)?.modified()?;
    let mut d = Document::open(&file)?;
    d.replace(0, 0, b"local ")?;
    let mut writer = OpenOptions::new().write(true).open(&file)?;
    writer.write_all(b"external\n")?;
    writer.set_modified(modified)?;
    drop(writer);
    assert_eq!(fs::metadata(&file)?.modified()?, modified);
    assert!(
        d.save(&file).is_err(),
        "Same-path save overwrote an external revision with unchanged metadata"
    );
    d.save(&copy)?;
    assert_eq!(fs::read(file)?, b"external\n");
    assert_eq!(fs::read(copy)?, b"local original\n");
    Ok(())
}

#[test]
fn saved_checkpoint_verifies_written_encoding_and_survives_worker_handoff() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    for (encoding, bom, text) in [
        ("UTF-8 BOM", b"\xef\xbb\xbf".as_slice(), b"a\r\nb\r\n".as_slice()),
        ("UTF-16 LE", b"\xff\xfe".as_slice(), b"a\0\r\0\n\0b\0\r\0\n\0".as_slice()),
    ] {
        let path = dir.path().join(encoding);
        let mut doc = Document::empty();
        doc.set_encoding(encoding)?;
        doc.replace(0, 0, b"a\r\nb\r\n")?;
        let mut worker = doc.clone();
        worker.save(&path)?;
        doc.adopt_save(worker);
        let disk = [bom, text].concat();
        assert_eq!(fs::read(&path)?, disk);
        doc.replace(0, 1, b"x")?;
        doc.save(&path)?;
        let stamp = fs::metadata(&path)?.modified()?;
        let mut other = fs::read(&path)?;
        other[bom.len()] = b'z';
        let mut f = OpenOptions::new().write(true).open(&path)?;
        f.write_all(&other)?;
        f.set_modified(stamp)?;
        drop(f);
        assert!(doc.undo());
        assert_eq!(doc.save(&path).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path)?, other);
        let copy = path.with_extension("copy");
        doc.save(&copy)?;
        assert_eq!(fs::read(copy)?, disk);
    }
    Ok(())
}

#[test]
fn refresh_after_saved_checkpoint_tracks_the_appended_disk_revision() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("append.txt");
    let mut doc = Document::empty();
    doc.replace(0, 0, b"first\r\n")?;
    doc.save(&path)?;
    OpenOptions::new().append(true).open(&path)?.write_all(b"next\r\n")?;
    assert!(doc.refresh_from_disk()?);
    doc.replace(0, 0, b"local\r\n")?;
    doc.save(&path)?;
    assert_eq!(fs::read(path)?, b"local\r\nfirst\r\nnext\r\n");
    Ok(())
}

#[test]
fn encoded_same_timestamp_conflict_uses_original_disk_bytes() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    for (label, encoding, base, changed, local) in [
        (
            "utf16",
            "UTF-16 LE",
            b"\xff\xfeA\0Z\0".as_slice(),
            b"\xff\xfeA\0Y\0".as_slice(),
            b"\xff\xfe!\0A\0Z\0".as_slice(),
        ),
        (
            "legacy",
            "windows-1252",
            b"p\xe9".as_slice(),
            b"n\xe9".as_slice(),
            b"!p\xe9".as_slice(),
        ),
    ] {
        let file = dir.path().join(label);
        let copy = dir.path().join(format!("{label}.copy"));
        fs::write(&file, base)?;
        let modified = fs::metadata(&file)?.modified()?;
        let mut d = Document::open_as(&file, encoding)?;
        d.replace(0, 0, b"!")?;
        let mut writer = OpenOptions::new().write(true).open(&file)?;
        writer.write_all(changed)?;
        writer.set_modified(modified)?;
        drop(writer);
        assert!(d.save(&file).is_err());
        d.save(&copy)?;
        assert_eq!(fs::read(&file)?, changed);
        assert_eq!(fs::read(&copy)?, local);
        assert!(d.undo());
        d.save(&copy)?;
        assert_eq!(fs::read(&copy)?, base);
    }
    Ok(())
}

// Set LINEWORK_CONFLICT_MIB=3072 to run the full scorecard fixture.
#[test]
fn overwrite_fixture_child() -> io::Result<()> {
    let Ok(path) = std::env::var("LINEWORK_CONFLICT_WRITER_PATH") else {
        return Ok(());
    };
    let offset = std::env::var("LINEWORK_CONFLICT_WRITER_OFFSET")
        .unwrap()
        .parse()
        .unwrap();
    let mut f = OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(b"OTHER")?;
    f.sync_all()
}

#[test]
fn existing_writer_can_edit_after_fallback_snapshot() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let p = dir.path().join("shared");
    let copy = dir.path().join("local");
    let mut writer = File::create(&p)?;
    write_paged_text(&mut writer)?;
    let mut doc = Document::open(&p)?;
    let t = Instant::now();
    doc.replace(0, 0, b"DRAFT")?;
    let first_edit_seconds = t.elapsed().as_secs_f64();
    writer.seek(SeekFrom::Start(25 * 1024 * 1024))?;
    writer.write_all(b"R")?;
    writer.sync_all()?;
    drop(writer);
    assert!(doc.save(&p).is_err());
    doc.save(&copy)?;
    assert_eq!(Document::open(&copy)?.read(0, 5)?, b"DRAFT");
    assert_eq!(Document::open(&copy)?.read(25 * 1024 * 1024 + 5, 1)?, b"x");
    assert_eq!(Document::open(&p)?.read(25 * 1024 * 1024, 1)?, b"R");
    println!("Existing writable handle, 26 MiB, first edit {first_edit_seconds:.6} seconds");
    Ok(())
}

#[cfg(windows)]
#[test]
fn snapshot_failure_keeps_draft_and_fails_conflicting_writer_promptly() -> io::Result<()> {
    if let Ok(dir) = std::env::var("LINEWORK_SNAPSHOT_FAILURE_DIR") {
        let p = Path::new(&dir).join("shared");
        let q = Path::new(&dir).join("local");
        let mut doc = Document::open(&p)?;
        doc.replace(0, 0, b"draft")?;
        let t = Instant::now();
        assert_eq!(
            OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap_err()
                .raw_os_error(),
            Some(32)
        );
        assert!(t.elapsed().as_secs() < 5);
        doc.save(&q)?;
        assert_eq!(Document::open(&q)?.read(0, 5)?, b"draft");
        assert_eq!(fs::metadata(&q)?.len(), 26 * 1024 * 1024 + 5);
        assert_eq!(Document::open(&p)?.read(0, 5)?, b"xxxxx");
        drop(doc);
        assert!(OpenOptions::new().write(true).open(&p).is_ok());
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    write_paged_text(&mut File::create(dir.path().join("shared"))?)?;
    let unavailable = dir.path().join("no-temporary-folder");
    let child = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "snapshot_failure_keeps_draft_and_fails_conflicting_writer_promptly",
            "--nocapture",
        ])
        .env("TMP", &unavailable)
        .env("TEMP", &unavailable)
        .env("LINEWORK_SNAPSHOT_FAILURE_DIR", dir.path())
        .output()?;
    assert!(
        child.status.success(),
        "{} {}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    Ok(())
}

#[test]
fn paged_conflict_keeps_an_off_page_revision() -> io::Result<()> {
    let mib: u64 = std::env::var("LINEWORK_CONFLICT_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(27);
    let len = mib * 1024 * 1024;
    let dir = tempfile::tempdir()?;
    let original = dir.path().join("paged.txt");
    let copy = dir.path().join("local.txt");
    let mut f = File::create(&original)?;
    let mut block = vec![b'x'; 1024 * 1024];
    for i in (127..block.len()).step_by(128) {
        block[i] = b'\n';
    }
    let mut local = Sha256::new();
    let mut remote = Sha256::new();
    let mut base = Sha256::new();
    for i in 0..mib {
        f.write_all(&block)?;
        base.update(&block);
        if i == 0 {
            local.update(b"LOCAL");
            local.update(&block[5..]);
        } else {
            local.update(&block);
        }
        if i + 1 == mib {
            remote.update(&block[..1024]);
            remote.update(b"OTHER");
            remote.update(&block[1029..]);
        } else {
            remote.update(&block);
        }
    }
    f.sync_all()?;
    drop(f);
    let start = Instant::now();
    let mut doc = Document::open(&original)?;
    let open_secs = start.elapsed().as_secs_f64();
    let first = Instant::now();
    doc.replace(0, 5, b"LOCAL")?;
    let edit_secs = first.elapsed().as_secs_f64();
    while !doc.indexing_complete() {
        assert!(doc.index_error().is_none());
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(doc.line_start(doc.line_count() - 1), len);
    let indexed_eof_secs = start.elapsed().as_secs_f64();
    let writer = Instant::now();
    let writer_status = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "overwrite_fixture_child"])
        .env("LINEWORK_CONFLICT_WRITER_PATH", &original)
        .env(
            "LINEWORK_CONFLICT_WRITER_OFFSET",
            (len - 1024 * 1024 + 1024).to_string(),
        )
        .output()?;
    assert!(
        writer_status.status.success(),
        "{}",
        String::from_utf8_lossy(&writer_status.stderr)
    );
    let writer_secs = writer.elapsed().as_secs_f64();
    assert!(doc.save(&original).is_err());
    let save = Instant::now();
    doc.save(&copy)?;
    let save_secs = save.elapsed().as_secs_f64();
    let local_hash = format!("{:x}", local.finalize());
    let remote_hash = format!("{:x}", remote.finalize());
    let base_hash = format!("{:x}", base.finalize());
    assert_eq!(digest(&original)?, remote_hash);
    assert_eq!(digest(&copy)?, local_hash);
    assert_eq!(Document::open(&copy)?.read(0, 5)?, b"LOCAL");
    assert_eq!(
        Document::open(&original)?.read(len - 1024 * 1024 + 1024, 5)?,
        b"OTHER"
    );
    assert!(doc.undo());
    doc.save(&copy)?;
    assert_eq!(digest(&copy)?, base_hash);
    assert_eq!(digest(&original)?, remote_hash);
    println!(
        "{}",
        serde_json::json!({"mib":mib,"off_page_position":len-1024*1024+1024,"source_sha256":remote_hash,"local_sha256":local_hash,"undo_sha256":base_hash,"open_secs":open_secs,"first_edit_secs":edit_secs,"indexed_eof_secs":indexed_eof_secs,"external_writer_secs":writer_secs,"save_as_secs":save_secs,"same_path_refused":true,"undo_saved":true,"both_reopened":true})
    );
    Ok(())
}
