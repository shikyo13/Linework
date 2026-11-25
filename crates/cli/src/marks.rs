//! What changed in a folder since the last look: a snapshot of the folder's
//! listing (each file's path, size and last write time) kept in the profile,
//! compared with the folder as it is now.
//!
//! The listing gives every file's size and time without opening it, so a
//! comparison costs one walk of the folder. A change that keeps both a file's
//! size and its write time is not seen.
use linework_search::walk;
use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// A folder's files as the walk lists them: relative path to (size, write time).
pub type Listing = HashMap<String, (u64, u64)>;

/// The folder's files now, by the same rules as a folder search.
pub fn list(root: &Path, options: walk::Options) -> io::Result<Listing> {
    let parts = walk::walk(root, options, Vec::new, |found: &mut Vec<(String, u64, u64)>, file| {
        found.push((file.path.to_owned(), file.size, file.modified));
    })?;
    Ok(parts.into_iter().flatten().map(|(path, size, time)| (path, (size, time))).collect())
}

/// Where a folder's snapshot under `name` is kept.
fn file(profile: &Path, root: &Path, name: &str) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(root)?;
    let key = absolute.to_string_lossy().to_lowercase().replace('\\', "/");
    let key = key.trim_end_matches('/');
    // FNV-1a: a stable name for the folder, which is also written inside.
    let hash = key.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    let name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    Ok(profile.join("marks").join(format!("{hash:016x}-{name}.tsv")))
}

/// The snapshot kept under `name`, or None when there is none.
pub fn load(profile: &Path, root: &Path, name: &str) -> io::Result<Option<Listing>> {
    let text = match fs::read(file(profile, root, name)?) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut listing = Listing::new();
    // The first line names the folder; each other line is path, size, time.
    for line in text.split(|&b| b == b'\n').skip(1) {
        let fields: Vec<&[u8]> = line.rsplitn(3, |&b| b == b'\t').collect();
        if let [time, size, path] = fields[..] {
            let number = |b: &[u8]| std::str::from_utf8(b).ok().and_then(|s| s.parse().ok());
            if let (Some(size), Some(time)) = (number(size), number(time)) {
                listing.insert(String::from_utf8_lossy(path).into_owned(), (size, time));
            }
        }
    }
    Ok(Some(listing))
}

/// Keeps `listing` as the snapshot under `name`, replacing the old one whole.
pub fn save(profile: &Path, root: &Path, name: &str, listing: &Listing) -> io::Result<()> {
    let target = file(profile, root, name)?;
    fs::create_dir_all(target.parent().expect("a marks folder"))?;
    let mut text = Vec::with_capacity(listing.len() * 64);
    writeln!(text, "{}", std::path::absolute(root)?.display())?;
    let mut paths: Vec<_> = listing.iter().collect();
    paths.sort_unstable_by(|a, b| a.0.cmp(b.0));
    for (path, (size, time)) in paths {
        writeln!(text, "{path}\t{size}\t{time}")?;
    }
    let staged = target.with_extension("tmp");
    fs::write(&staged, &text)?;
    fs::rename(&staged, &target)
}

/// What differs between a snapshot and now, each list sorted by path.
#[derive(Default)]
pub struct Changes {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

pub fn compare(before: &Listing, now: &Listing) -> Changes {
    let mut changes = Changes::default();
    for (path, stamp) in now {
        match before.get(path) {
            None => changes.added.push(path.clone()),
            Some(old) if old != stamp => changes.changed.push(path.clone()),
            Some(_) => {}
        }
    }
    changes.removed = before.keys().filter(|path| !now.contains_key(*path)).cloned().collect();
    for list in [&mut changes.added, &mut changes.changed, &mut changes.removed] {
        list.sort_unstable();
    }
    changes
}
