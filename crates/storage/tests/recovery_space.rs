use linework_storage::Document;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

const MIB: usize = 1 << 20;
fn file_hash(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    io::copy(&mut file, &mut hash)?;
    Ok(hash.finalize().into())
}
fn doc_hash(doc: &Document) -> io::Result<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut buf = vec![0; MIB];
    for at in (0..doc.len()).step_by(MIB) {
        let n = (doc.len() - at).min(MIB as u64) as usize;
        doc.read_into(at, &mut buf[..n])?;
        hash.update(&buf[..n]);
    }
    Ok(hash.finalize().into())
}
fn header(path: &Path) -> io::Result<serde_json::Value> {
    let mut f = File::open(path)?;
    let mut h = [0; 12];
    f.read_exact(&mut h)?;
    let n = u64::from_le_bytes(h[4..].try_into().unwrap()) as usize;
    let mut bytes = vec![0; n];
    f.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
#[cfg(windows)]
fn free_bytes(dir: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<_> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0;
    if unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(free)
}
#[test]
fn small_saved_edits_and_detached_sessions_share_verified_recovery_bytes() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("original.txt");
    let first = dir.path().join("first.lwr");
    let next = dir.path().join("next.lwr");
    let second_session = dir.path().join("session-2");
    fs::create_dir(&second_session)?;
    let migrated = second_session.join("detached.lwr");
    let mut block = vec![0u8; MIB];
    let require_refs = std::env::var_os("LINEWORK_TEST_REQUIRE_REFS_CLONE").is_some();
    let mut random = 136505867u32;
    let mut f = File::create(&path)?;
    for i in 0..32 {
        if i == 0 || require_refs {
            for byte in &mut block {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                *byte = b' ' + (random % 95) as u8;
            }
        }
        f.write_all(&block)?;
    }
    f.write_all(b"\nlast line\n")?;
    f.sync_all()?;
    drop(f);
    let mut d = Document::open(&path)?;
    let end = d.len();
    d.replace(end - 2, 1, b"Z")?;
    #[cfg(windows)]
    let free_before = require_refs.then(|| free_bytes(dir.path())).transpose()?;
    d.save_recovery(&first)?;
    let base = Document::recovery_archive(&first)?.unwrap();
    let format = header(&first)?["base"]["format"].clone();
    #[cfg(windows)]
    if let Some(before) = free_before {
        let used = before.saturating_sub(free_bytes(dir.path())?);
        println!("Exclusive ReFS volume free-space drop after durable checkpoint: {used} bytes");
        assert!(
            used < (16 * MIB) as u64,
            "The physical recovery allocation approached the 32 MiB source"
        );
    }
    if require_refs {
        assert!(
            format.get("Clone").is_some(),
            "ReFS fixture fell back to {format}"
        );
    }
    println!(
        "Recovery format {format}, base file {} bytes, draft {} bytes",
        fs::metadata(&base)?.len(),
        fs::metadata(&first)?.len()
    );
    if format == "Blocks" {
        assert!(fs::metadata(&base)?.len() < 2 * MIB as u64);
    } else {
        assert!(format.get("Clone").is_some());
    }
    let mut saving = d.clone();
    saving.save(&path)?;
    d.adopt_save(saving);
    let saved = file_hash(&path)?;
    d.replace(MIB as u64 - 2, 4, b"second edit")?;
    let wanted = doc_hash(&d)?;
    d.save_recovery(&next)?;
    assert_eq!(Document::recovery_archive(&next)?.as_ref(), Some(&base));
    assert!(header(&next)?["saved_patch"].is_object());
    assert!(fs::metadata(&next)?.len() < 4096);
    drop(d);
    let mut named = Document::recover(&next)?;
    assert!(named.path().is_some());
    assert_eq!(doc_hash(&named)?, wanted);
    assert!(named.undo());
    assert_eq!(doc_hash(&named)?, saved);
    drop(named);

    let mtime = fs::metadata(&path)?.modified()?;
    let mut other = OpenOptions::new().write(true).open(&path)?;
    other.seek(SeekFrom::Start(3 * MIB as u64))?;
    other.write_all(b"external")?;
    other.set_modified(mtime)?;
    drop(other);
    let external = file_hash(&path)?;
    let mut detached = Document::recover(&next)?;
    assert!(detached.path().is_none());
    assert_eq!(doc_hash(&detached)?, wanted);
    detached.replace(1, 0, b"!")?;
    assert!(detached.undo());
    detached.save_recovery(&migrated)?;
    assert!(fs::metadata(&migrated)?.len() < 4096);
    let linked = Document::recovery_archive(&migrated)?.unwrap();
    assert_eq!(linked.file_name(), base.file_name());
    drop(detached);
    let outside_name = dir.path().join("external-writer-copy.txt");
    fs::rename(&path, &outside_name)?;
    assert_eq!(doc_hash(&Document::recover(&next)?)?, wanted);
    fs::remove_file(&base)?;
    let mut restored = Document::recover(&migrated)?;
    assert!(restored.path().is_none());
    let copy = dir.path().join("recovered.txt");
    restored.save(&copy)?;
    assert_eq!(file_hash(&copy)?, wanted);
    assert_eq!(file_hash(&outside_name)?, external);
    drop(restored);
    fs::write(linked, b"damaged private base")?;
    assert!(Document::recover(&migrated).is_err());
    Ok(())
}

/// Saving edits at opposite ends must not duplicate the untouched middle of the file.
#[test]
fn distant_saved_edits_reuse_one_archive_and_survive_losing_the_original() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("edited.txt");
    let draft = dir.path().join("dirty.lwr");
    let checkpoint = dir.path().join("first.lwr");
    let mut data = vec![0; 8 * MIB];
    let mut random = 136505867u32;
    for byte in &mut data {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        *byte = b' ' + (random % 95) as u8;
    }
    fs::write(&path, &data)?;
    let mut doc = Document::open(&path)?;
    doc.replace(11, 1, b"AB")?;
    doc.save_recovery(&checkpoint)?;
    let base = Document::recovery_archive(&checkpoint)?.unwrap();
    doc.save(&path)?;
    data.splice(11..12, *b"AB");
    let near_end = data.len() - 113;
    doc.replace(near_end as u64, 3, b"Z")?;
    doc.save(&path)?;
    data.splice(near_end..near_end + 3, *b"Z");
    let saved_hash: [u8; 32] = Sha256::digest(&data).into();
    doc.replace((MIB - 1) as u64, 2, b"dirty")?;
    data.splice(MIB - 1..MIB + 1, *b"dirty");
    let draft_hash: [u8; 32] = Sha256::digest(&data).into();
    doc.save_recovery(&draft)?;
    let new_base = Document::recovery_archive(&draft)?.unwrap();
    let archives = fs::read_dir(dir.path())?.collect::<Result<Vec<_>, _>>()?;
    let sizes = archives
        .iter()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "lwbase"))
        .map(|entry| entry.metadata().map(|m| m.len()))
        .collect::<Result<Vec<_>, _>>()?;
    println!("Archive lengths {sizes:?}, draft {} bytes, saved {saved_hash:x?}, recovered {draft_hash:x?}", fs::metadata(&draft)?.len());
    assert_eq!(new_base, base, "two distant edits produced another archive");
    assert_eq!(sizes.len(), 1);
    assert!(fs::metadata(&draft)?.len() < 4096);
    drop(doc);
    let hidden = base.with_extension("held");
    fs::rename(&base, &hidden)?;
    let mut named = Document::recover(&draft)?;
    assert!(named.path().is_some());
    assert_eq!(doc_hash(&named)?, draft_hash);
    assert!(named.undo());
    assert_eq!(doc_hash(&named)?, saved_hash);
    drop(named);
    fs::rename(&hidden, &base)?;
    fs::remove_file(&path)?;
    let detached = Document::recover(&draft)?;
    assert!(detached.path().is_none());
    assert_eq!(doc_hash(&detached)?, draft_hash);
    drop(detached);
    let bytes = fs::read(&draft)?;
    assert_eq!(&bytes[..4], b"LWR4");
    let n = u64::from_le_bytes(bytes[4..12].try_into().unwrap()) as usize;
    let mut h: serde_json::Value = serde_json::from_slice(&bytes[12..12 + n])?;
    h["saved_patch"]["spans"][0] = serde_json::json!({"Copy":[u64::MAX,1]});
    let corrupt = serde_json::to_vec(&h)?;
    let mut bad = File::create(&draft)?;
    bad.write_all(b"LWR4")?;
    bad.write_all(&(corrupt.len() as u64).to_le_bytes())?;
    bad.write_all(&corrupt)?;
    bad.write_all(&bytes[12 + n..])?;
    drop(bad);
    assert!(Document::recover(&draft).is_err());
    Ok(())
}

#[test]
fn distant_edits_above_the_literal_budget_create_a_verified_base() -> io::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("large-changes.txt");
    let checkpoint = dir.path().join("before.lwr");
    let draft = dir.path().join("after.lwr");
    let mut expected = vec![b'a'; 2 * MIB];
    fs::write(&path, &expected)?;
    let mut doc = Document::open(&path)?;
    doc.save_recovery(&checkpoint)?;
    for (at, byte) in [(500usize, b'b'), (MIB, b'c')] {
        let replacement = vec![byte; 32 * 1024 + 1];
        doc.replace(at as u64, replacement.len() as u64, &replacement)?;
        expected[at..at + replacement.len()].copy_from_slice(&replacement);
    }
    doc.save(&path)?;
    doc.replace(3, 1, b"!")?;
    expected[3] = b'!';
    doc.save_recovery(&draft)?;
    assert_ne!(
        Document::recovery_archive(&checkpoint)?,
        Document::recovery_archive(&draft)?
    );
    drop(doc);
    fs::remove_file(path)?;
    let restored = Document::recover(&draft)?;
    assert!(restored.path().is_none());
    assert_eq!(
        doc_hash(&restored)?,
        <[u8; 32]>::from(Sha256::digest(&expected))
    );
    Ok(())
}

#[test]
fn previous_deflate_checkpoint_still_recovers_after_the_original_disappears() -> io::Result<()> {
    use flate2::{write::DeflateEncoder, Compression};
    let dir = tempfile::tempdir()?;
    let source = "caf\u{e9}\n".as_bytes();
    let mut encoder = DeflateEncoder::new(
        File::create(dir.path().join("base-old.lwbase"))?,
        Compression::fast(),
    );
    encoder.write_all(source)?;
    encoder.finish()?.sync_all()?;
    let hash: [u8; 32] = Sha256::digest(source).into();
    let h = serde_json::to_vec(&serde_json::json!({
        "base":{"file":"base-old.lwbase","bytes":source.len(),"digest":hash},
        "path":dir.path().join("gone.txt"), "stamp":null, "saved_encoding":"UTF-8",
        "encoding":"UTF-8 BOM", "bom":true, "length":source.len()+1
    }))?;
    let p = dir.path().join("old.lwr");
    let mut f = File::create(&p)?;
    f.write_all(b"LWR2")?;
    f.write_all(&(h.len() as u64).to_le_bytes())?;
    f.write_all(&h)?;
    f.write_all(&[1])?;
    f.write_all(&0u64.to_le_bytes())?;
    f.write_all(&(source.len() as u64).to_le_bytes())?;
    f.write_all(&[2])?;
    f.write_all(&1u64.to_le_bytes())?;
    f.write_all(b"!\0")?;
    drop(f);
    let mut d = Document::recover(&p)?;
    assert!(d.path().is_none());
    let saved = dir.path().join("recovered.txt");
    d.save(&saved)?;
    assert_eq!(
        fs::read(&saved)?,
        [b"\xef\xbb\xbf".as_slice(), source, b"!"].concat()
    );
    Ok(())
}

/// Exercise crash recovery with the original pathname gone and a scratch volume
/// too small to contain a decompressed 8 GiB copy. The source fixture stays read-only.
#[cfg(windows)]
#[test]
#[ignore = "requires the independent 8 GiB reference fixture and a dedicated small volume"]
fn detached_eight_gib_recovers_on_a_small_volume() -> io::Result<()> {
    use std::{env, process::Command, time::Instant};
    const NEEDLE: &[u8] = b"NEAR_PERSONAL_ID";
    let required =
        |key: &str| env::var_os(key).ok_or_else(|| io::Error::other(format!("Missing {key}")));
    let at: u64 = required("LINEWORK_TEST_8G_OFFSET")?
        .to_string_lossy()
        .parse()
        .map_err(io::Error::other)?;
    if let Some(alias) = env::var_os("LINEWORK_TEST_WRITER_ALIAS") {
        let alias = Path::new(&alias);
        let mut doc = Document::open(alias)?;
        assert_eq!(doc.len(), 8 << 30);
        assert_eq!(doc.read(at, NEEDLE.len())?, NEEDLE);
        doc.replace(at, NEEDLE.len() as u64, "λ".as_bytes())?;
        doc.save_recovery(Path::new(&required("LINEWORK_TEST_WRITER_DRAFT")?))?;
        // Simulate loss of process-local state immediately after atomic publication.
        std::process::exit(0);
    }
    let fixture = std::path::PathBuf::from(required("LINEWORK_TEST_8G_FIXTURE")?);
    let expected = required("LINEWORK_TEST_8G_EDITED_SHA")?
        .to_string_lossy()
        .into_owned();
    let clean = required("LINEWORK_TEST_8G_ORIGINAL_SHA")?
        .to_string_lossy()
        .into_owned();
    let begin = Instant::now();
    let first = tempfile::tempdir()?;
    let second = tempfile::tempdir()?;
    let available = free_bytes(first.path())?;
    assert!(
        ((1u64 << 20)..(256u64 << 20)).contains(&available),
        "Unexpected free scratch bytes {available}"
    );
    let names = tempfile::tempdir_in(fixture.parent().unwrap())?;
    let alias = names.path().join("disposable.txt");
    fs::hard_link(&fixture, &alias)?;
    let draft = first.path().join("draft.lwr");
    println!("Writing durable recovery with {available} bytes free on the scratch volume");
    let status = Command::new(env::current_exe()?)
        .args([
            "detached_eight_gib_recovers_on_a_small_volume",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LINEWORK_TEST_WRITER_ALIAS", &alias)
        .env("LINEWORK_TEST_WRITER_DRAFT", &draft)
        .status()?;
    assert!(status.success(), "checkpoint writer exited {status}");
    fs::remove_file(&alias)?;
    println!(
        "Writer exited and the source name was removed at {:.3}s",
        begin.elapsed().as_secs_f64()
    );
    let recovered = Document::recover(&draft)?;
    assert!(recovered.path().is_none());
    let digest = |value: [u8; 32]| value.iter().map(|b| format!("{b:02x}")).collect::<String>();
    assert_eq!(digest(doc_hash(&recovered)?), expected);
    let next = second.path().join("next.lwr");
    recovered.save_recovery(&next)?;
    drop(recovered);
    let size = fs::metadata(&next)?.len();
    assert!(size < 4096);
    drop(first);
    println!(
        "Verified detached draft and removed the first session at {:.3}s",
        begin.elapsed().as_secs_f64()
    );
    let reopened = Document::recover(&next)?;
    assert!(reopened.path().is_none());
    assert_eq!(digest(doc_hash(&reopened)?), expected);
    assert_eq!(digest(file_hash(&fixture)?), clean);
    println!("Reopened draft SHA-256 {expected}, untouched source {clean}, new draft {size} bytes, free scratch {} bytes, elapsed {:.3}s",free_bytes(second.path())?,begin.elapsed().as_secs_f64());
    Ok(())
}

#[cfg(windows)]
#[test]
#[ignore = "requires a large fixture file and a dedicated ReFS temporary volume"]
fn refs_eight_gib_unaligned_clone_recovers_after_destruction() -> io::Result<()> {
    use std::{env, process::Command, time::Instant};
    const TAIL: &[u8] = b"ReFS-tail!\n";
    const NEEDLE: &[u8] = b"NEAR_PERSONAL_ID";
    const EDIT: &[u8] = "λ".as_bytes();
    let var = |key| env::var(key).map_err(io::Error::other);
    let hex = |v: [u8; 32]| v.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let pos: u64 = var("LINEWORK_TEST_8G_OFFSET")?
        .parse()
        .map_err(io::Error::other)?;
    assert!(pos > 4 << 30);
    if let Ok(name) = env::var("LINEWORK_TEST_REFS_WRITER") {
        let mut doc = Document::open(Path::new(&name))?;
        assert_eq!(doc.read(pos, NEEDLE.len())?, NEEDLE);
        doc.replace(pos, NEEDLE.len() as u64, EDIT)?;
        let draft = Path::new(&name).with_extension("lwr");
        let before = free_bytes(draft.parent().unwrap())?;
        doc.save_recovery(&draft)?;
        let after = free_bytes(draft.parent().unwrap())?;
        let h = header(&draft)?;
        assert_eq!(h["base"]["format"]["Clone"]["offset"], 0);
        let used = i128::from(before) - i128::from(after);
        println!(
            "Durable ReFS checkpoint. Net used-volume change {used} bytes, draft {} bytes",
            fs::metadata(&draft)?.len()
        );
        assert!(used < (16 * MIB) as i128);
        assert!(doc.undo());
        let undo = hex(doc_hash(&doc)?);
        assert_eq!(undo, var("LINEWORK_TEST_REFS_UNDO_SHA")?);
        println!("Undo SHA-256 {undo}. Writer exiting");
        std::process::exit(0);
    }
    let begin = Instant::now();
    let src = var("LINEWORK_TEST_8G_FIXTURE")?;
    let mut input = File::open(src)?;
    assert_eq!(input.metadata()?.len(), 8 << 30);
    let dir = tempfile::tempdir()?;
    let disposable = dir.path().join("private-source.txt");
    let mut out = File::create_new(&disposable)?;
    let mut clean = Sha256::new();
    let mut buf = vec![0; 4 * MIB];
    let mut left = pos;
    println!("Preparing independent 8 GiB reference with an unaligned tail");
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        input.read_exact(&mut buf[..n])?;
        clean.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        left -= n as u64;
    }
    let mut edit = clean.clone();
    input.read_exact(&mut buf[..NEEDLE.len()])?;
    assert_eq!(&buf[..NEEDLE.len()], NEEDLE);
    out.write_all(NEEDLE)?;
    clean.update(NEEDLE);
    edit.update(EDIT);
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        clean.update(&buf[..n]);
        edit.update(&buf[..n]);
    }
    assert_eq!(
        hex(clean.clone().finalize().into()),
        var("LINEWORK_TEST_8G_ORIGINAL_SHA")?
    );
    assert_eq!(
        hex(edit.clone().finalize().into()),
        var("LINEWORK_TEST_8G_EDITED_SHA")?
    );
    out.write_all(TAIL)?;
    out.sync_all()?;
    drop(out);
    clean.update(TAIL);
    edit.update(TAIL);
    let clean = hex(clean.finalize().into());
    let edited = hex(edit.finalize().into());
    println!(
        "Reference ready at {:.3}s. Launching checkpoint writer for {} bytes",
        begin.elapsed().as_secs_f64(),
        fs::metadata(&disposable)?.len()
    );
    let status = Command::new(env::current_exe()?)
        .args([
            "refs_eight_gib_unaligned_clone_recovers_after_destruction",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LINEWORK_TEST_REFS_WRITER", &disposable)
        .env("LINEWORK_TEST_REFS_UNDO_SHA", &clean)
        .status()?;
    assert!(status.success(), "Writer exited {status}");
    let mut outside = OpenOptions::new().write(true).open(&disposable)?;
    outside.seek(SeekFrom::Start((4u64 << 30) + 7))?;
    outside.write_all(b"external overwrite")?;
    outside.seek(SeekFrom::End(-3))?;
    outside.write_all(b"END")?;
    outside.sync_all()?;
    drop(outside);
    fs::remove_file(&disposable)?;
    println!(
        "External overwrite and removal complete at {:.3}s. Recovering detached draft",
        begin.elapsed().as_secs_f64()
    );
    let restored = Document::recover(&disposable.with_extension("lwr"))?;
    assert!(restored.path().is_none());
    assert_eq!(hex(doc_hash(&restored)?), edited);
    println!(
        "Detached SHA-256 {edited}, original after Undo {clean}, elapsed {:.3}s",
        begin.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(windows)]
#[test]
#[ignore = "requires the 8 GiB reference and an owned NTFS scratch volume"]
fn ntfs_eight_gib_distant_saves_keep_one_recovery_base() -> io::Result<()> {
    use std::{env, process::Command, time::Instant};
    const SIZE: u64 = 8 << 30;
    const SAVED: [(u64, u8); 2] = [(31, b'~'), (SIZE - 32, b'!')];
    const UNSAVED: (u64, u8) = (SIZE / 2 + 42, b'@');
    let var = |key| env::var(key).map_err(io::Error::other);
    let hex = |v: [u8; 32]| v.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if let Ok(name) = env::var("LINEWORK_TEST_NTFS_WRITER") {
        let path = Path::new(&name);
        let draft = path.with_extension("lwr");
        let first = path.with_extension("first");
        let mut doc = Document::open(path)?;
        for (i, (pos, byte)) in SAVED.into_iter().enumerate() {
            assert_ne!(doc.read(pos, 1)?, [byte]);
            doc.replace(pos, 1, &[byte])?;
            if i == 0 {
                doc.save_recovery(&first)?;
                assert_eq!(header(&first)?["base"]["format"], "Blocks");
                println!("First private recovery archive published");
            }
            let mut worker = doc.clone();
            worker.save(path)?;
            doc.adopt_save(worker);
            println!("Completed Save {} at byte {pos}", i + 1);
        }
        doc.replace(UNSAVED.0, 1, &[UNSAVED.1])?;
        doc.save_recovery(&draft)?;
        let base = Document::recovery_archive(&draft)?.unwrap();
        assert_eq!(Some(&base), Document::recovery_archive(&first)?.as_ref());
        let archives = fs::read_dir(path.parent().unwrap())?.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            archives
                .iter()
                .filter(|e| e.path().extension().is_some_and(|v| v == "lwbase"))
                .count(),
            1
        );
        assert_eq!(fs::read(&draft)?.get(..4), Some(b"LWR4".as_slice()));
        let size = fs::metadata(&draft)?.len();
        assert!(size < 4096);
        println!(
            "Both checkpoints reference one {}-byte archive, updated draft {size} bytes",
            fs::metadata(base)?.len()
        );
        assert!(doc.undo());
        assert_eq!(hex(doc_hash(&doc)?), var("LINEWORK_TEST_NTFS_SAVED_HASH")?);
        println!("Undo hash verified. Writer exiting");
        std::process::exit(0);
    }
    let start = Instant::now();
    let mut reference = File::open(var("LINEWORK_TEST_8G_FIXTURE")?)?;
    assert_eq!(reference.metadata()?.len(), SIZE);
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("owned.txt");
    let mut private = File::create_new(&path)?;
    let mut clean = Sha256::new();
    let mut saved = Sha256::new();
    let mut dirty = Sha256::new();
    let mut buf = vec![0; 4 * MIB];
    println!("Preparing independent input and three full-file hashes");
    for at in (0..SIZE).step_by(buf.len()) {
        reference.read_exact(&mut buf)?;
        private.write_all(&buf)?;
        clean.update(&buf);
        let end = at + buf.len() as u64;
        for (pos, byte) in SAVED {
            if (at..end).contains(&pos) {
                assert_ne!(buf[(pos - at) as usize], byte);
                buf[(pos - at) as usize] = byte;
            }
        }
        saved.update(&buf);
        if (at..end).contains(&UNSAVED.0) {
            assert_ne!(buf[(UNSAVED.0 - at) as usize], UNSAVED.1);
            buf[(UNSAVED.0 - at) as usize] = UNSAVED.1;
        }
        dirty.update(&buf);
    }
    private.sync_all()?;
    drop(private);
    let clean = hex(clean.finalize().into());
    let saved = hex(saved.finalize().into());
    let dirty = hex(dirty.finalize().into());
    assert_eq!(clean, var("LINEWORK_TEST_8G_ORIGINAL_SHA")?);
    println!("Launching writer at {:.3}s", start.elapsed().as_secs_f64());
    let status = Command::new(env::current_exe()?)
        .args([
            "ntfs_eight_gib_distant_saves_keep_one_recovery_base",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LINEWORK_TEST_NTFS_WRITER", &path)
        .env("LINEWORK_TEST_NTFS_SAVED_HASH", &saved)
        .status()?;
    assert!(status.success(), "Writer exited {status}");
    println!("Checking the complete saved file after the writer exited");
    assert_eq!(hex(file_hash(&path)?), saved);
    fs::remove_file(&path)?;
    println!(
        "Original pathname removed at {:.3}s. Reopening the detached draft",
        start.elapsed().as_secs_f64()
    );
    let restored = Document::recover(&path.with_extension("lwr"))?;
    assert!(restored.path().is_none());
    assert_eq!(hex(doc_hash(&restored)?), dirty);
    println!(
        "Detached SHA-256 {dirty}, disk and Undo {saved}, reference {clean}, elapsed {:.3}s",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
