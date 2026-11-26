//! Replacing across files: a preview of every changed line, a record of every
//! edit kept before a file is saved, and undo of a whole change from that record.
//!
//! Each file is opened, changed in memory and saved by the editor's own engine,
//! so it is saved as the editor saves: in its encoding, and refused when the
//! file changed on disk since it was opened. A change record lives in the
//! `changes` folder of the profile, one JSON object per line: the change, then
//! for each file its edits (written before the file is saved) and a line once
//! it is saved. Undo reverts a saved file only when every changed place still
//! holds the text the change put there.
use linework_storage::{Document, SearchOptions};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

/// One edit as it stands after the change.
pub struct Edit {
    /// Where the new text starts in the changed file's text.
    pub at: u64,
    pub new: Vec<u8>,
    /// The text it replaced.
    pub old: Vec<u8>,
}

/// A file changed in memory, not yet saved.
pub struct Planned {
    pub doc: Document,
    pub path: PathBuf,
    pub edits: Vec<Edit>,
    /// For a preview: each changed line's number from 1, its text before and
    /// after, and where its first change starts in it.
    pub lines: Vec<(u64, Vec<u8>, Vec<u8>, usize)>,
}

/// Whether `at` is the end of a text that ends with a line break: the place
/// after the last line, which belongs to no line (as grep and sed count lines).
pub fn after_last_line(doc: &Document, at: u64) -> io::Result<bool> {
    Ok(at != 0 && at == doc.len() && matches!(doc.read(at - 1, 1)?.first(), Some(b'\n' | b'\r')))
}

/// Replaces every match of `old` in one file in memory. None when nothing matched.
pub fn plan(path: &Path, old: &str, new: &str, options: SearchOptions, preview: bool, widest: usize, cancel: &AtomicBool) -> io::Result<Option<Planned>> {
    let mut doc = Document::open(path)?;
    let mut before = Vec::new();
    if preview {
        doc.find_lines(old, options, widest, linework_storage::LineEnds::LineFeed, cancel, &mut |line| {
            before.push(line.owned());
            Ok(false)
        })?;
    }
    let length = doc.len();
    let phantom = after_last_line(&doc, length)?;
    let mut ranges: Vec<(Range<u64>, u64)> = Vec::new();
    let count = doc.replace_observed(old, new, options, 0..length, cancel, |hit, len| ranges.push((hit, len)))?;
    if count != ranges.len() {
        return Err(io::Error::other(format!("the engine replaced {count} matches but reported {}", ranges.len())));
    }
    if ranges.is_empty() {
        return Ok(None);
    }
    // An empty match after the last line (a pattern like ^ or .* in a text that
    // ends with a line break) is in no line: sed leaves it, and so does lw. It
    // is taken out after the old text is read, which needs the change on top
    // of the engine's undo history.
    let phantom = phantom && ranges.last().is_some_and(|(hit, _)| hit.is_empty() && hit.start == length);
    if phantom && ranges.len() == 1 {
        doc.undo();
        return Ok(None);
    }
    // The text each match replaced, read from the state before the change.
    if !doc.undo() {
        return Err(io::Error::other("the change left no state to read the old text from"));
    }
    let olds = ranges.iter().map(|(r, _)| doc.read(r.start, (r.end - r.start) as usize)).collect::<io::Result<Vec<_>>>();
    doc.redo();
    let olds = olds?;
    let mut edits = Vec::with_capacity(ranges.len());
    let mut shift = 0i64;
    for ((range, len), old) in ranges.iter().zip(olds) {
        let at = (range.start as i64 + shift) as u64;
        edits.push(Edit { at, new: doc.read(at, *len as usize)?, old });
        shift += *len as i64 - (range.end - range.start) as i64;
    }
    if phantom {
        let edit = edits.pop().expect("more than one edit");
        ranges.pop();
        doc.replace(edit.at, edit.new.len() as u64, b"")?;
    }
    // Each changed line before and after: the line's text with the edits that start in it.
    let mut lines = Vec::new();
    let mut next = 0;
    for line in &before {
        // The preview shows lines without their ending, so the CR of a CRLF line is left off.
        let text = line.text.strip_suffix(b"\r").unwrap_or(&line.text);
        // A very long line is read in part: positions count from where its text starts.
        let (start, end) = (line.text_start, line.text_start + text.len() as u64);
        while next < ranges.len() && ranges[next].0.start < start {
            next += 1;
        }
        let mut after = Vec::new();
        let mut taken = 0usize;
        let first = ranges.get(next).map_or(0, |(r, _)| r.start.saturating_sub(start) as usize);
        let mut i = next;
        while i < ranges.len() && ranges[i].0.start <= end {
            let (from, to) = ((ranges[i].0.start - start) as usize, ((ranges[i].0.end.min(end)) - start) as usize);
            after.extend_from_slice(&text[taken.min(from)..from]);
            after.extend_from_slice(&edits[i].new);
            taken = to.max(from);
            i += 1;
        }
        after.extend_from_slice(&text[taken.min(text.len())..]);
        lines.push((line.line + 1, text.to_vec(), after, first));
    }
    // The path as a person would write it, absolute so undo works from any folder.
    let path = std::path::absolute(path)?;
    Ok(Some(Planned { doc, path, edits, lines }))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    (0..text.len()).step_by(2).map(|i| text.get(i..i + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok())).collect()
}

/// An open change record.
pub struct Record {
    pub id: String,
    file: File,
}

impl Record {
    /// Starts a record in `folder`, named by the time, holding the command.
    pub fn start(folder: &Path, command: &[String]) -> io::Result<Self> {
        fs::create_dir_all(folder)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let id = format!("{}{:03}", now.as_secs(), now.subsec_millis());
        let file = OpenOptions::new().create_new(true).append(true).open(folder.join(format!("{id}.jsonl")))?;
        let mut record = Self { id, file };
        record.line(&json!({"change": record.id, "seconds": now.as_secs(), "command": command}))?;
        Ok(record)
    }
    fn line(&mut self, value: &Value) -> io::Result<()> {
        let mut text = serde_json::to_vec(value)?;
        text.push(b'\n');
        self.file.write_all(&text)
    }
    fn line_now(&mut self, value: &Value) -> io::Result<()> {
        self.line(value)?;
        self.file.sync_data()
    }
    /// Keeps the edits of files about to be saved, on disk before any of them
    /// is saved, so a crash cannot leave a changed file without its record.
    pub fn keep(&mut self, planned: &[&Planned]) -> io::Result<()> {
        for p in planned {
            let edits: Vec<Value> = p.edits.iter().map(|e| json!([e.at, hex(&e.new), hex(&e.old)])).collect();
            self.line(&json!({"file": p.path, "length": p.doc.len(), "edits": edits}))?;
        }
        self.file.sync_data()
    }
    /// Keeps one file's edits, as `keep` does, for a file changed outside a `Planned`.
    pub fn keep_file(&mut self, path: &Path, length: u64, edits: &[Edit]) -> io::Result<()> {
        let edits: Vec<Value> = edits.iter().map(|e| json!([e.at, hex(&e.new), hex(&e.old)])).collect();
        self.line(&json!({"file": path, "length": length, "edits": edits}))?;
        self.file.sync_data()
    }
    /// Notes that a kept file was saved. Undo checks a file's text before it
    /// reverts it, so this note may be lost in a crash without harm.
    pub fn saved(&mut self, path: &Path) -> io::Result<()> {
        self.line(&json!({"saved": path}))
    }
    pub fn finish(&mut self) -> io::Result<()> {
        self.line_now(&json!({"finished": true}))
    }
}

/// `work` taking each item, on up to 12 threads, results in the items' order.
pub fn each_owned<T: Send, R: Send>(items: Vec<T>, work: impl Fn(T) -> R + Sync) -> Vec<R> {
    let count = items.len();
    let queue = std::sync::Mutex::new(items.into_iter().enumerate());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get() * 3 / 4).clamp(1, 12).min(count.max(1));
    let mut done: Vec<(usize, R)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let next = queue.lock().unwrap().next();
                        let Some((i, item)) = next else { return mine };
                        mine.push((i, work(item)));
                    }
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().expect("a worker stopped")).collect()
    });
    done.sort_unstable_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

/// `work` for every item on up to 12 threads, results in the items' order.
pub fn each<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get() * 3 / 4).clamp(1, 12).min(items.len().max(1));
    let mut done: Vec<(usize, R)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { return mine };
                        mine.push((i, work(item)));
                    }
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().expect("a worker stopped")).collect()
    });
    done.sort_unstable_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

/// One file of a change, as its record describes it.
pub struct Changed {
    pub path: PathBuf,
    /// The file's length after the change.
    pub length: u64,
    pub edits: Vec<Edit>,
    /// False when the record ends before the save was marked done: the save
    /// failed, or the program stopped during or just after it.
    pub saved: bool,
}

/// What one record holds, as read back.
pub struct Kept {
    pub id: String,
    pub seconds: u64,
    pub command: Vec<String>,
    pub files: Vec<Changed>,
    pub undone: bool,
}

/// The records in `folder`, oldest first.
pub fn records(folder: &Path) -> io::Result<Vec<Kept>> {
    let mut names: Vec<PathBuf> = match fs::read_dir(folder) {
        Ok(entries) => entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    names.sort();
    let mut kept = Vec::new();
    for name in names {
        let mut record = Kept { id: String::new(), seconds: 0, command: Vec::new(), files: Vec::new(), undone: false };
        for line in io::BufReader::new(File::open(&name)?).lines() {
            let Ok(value) = serde_json::from_str::<Value>(&line?) else { continue };
            if let Some(id) = value["change"].as_str() {
                record.id = id.to_owned();
                record.seconds = value["seconds"].as_u64().unwrap_or(0);
                record.command = value["command"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
            } else if let Some(path) = value["file"].as_str() {
                let edits = value["edits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| Some(Edit { at: e[0].as_u64()?, new: unhex(e[1].as_str()?)?, old: unhex(e[2].as_str()?)? }))
                    .collect();
                // Unsaved until a note says the save got through; a file whose
                // save was never noted is kept too.
                record.files.push(Changed { path: PathBuf::from(path), length: value["length"].as_u64().unwrap_or(0), edits, saved: false });
            } else if let Some(path) = value["saved"].as_str() {
                // Notes follow their batch's entries, so match them by path.
                if let Some(file) = record.files.iter_mut().rev().find(|f| f.path.as_os_str() == path) {
                    file.saved = true;
                }
            } else if value["undone"].as_bool() == Some(true) {
                record.undone = true;
            }
        }
        kept.push(record);
    }
    Ok(kept)
}

/// Marks a record undone.
pub fn mark_undone(folder: &Path, id: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().append(true).open(folder.join(format!("{id}.jsonl")))?;
    file.write_all(b"{\"undone\":true}\n")?;
    file.sync_data()
}

/// Reverts one saved file of a change. Err when the file no longer holds the
/// changed text exactly where the change left it.
pub fn revert(path: &Path, length: u64, edits: &[Edit]) -> io::Result<()> {
    let mut doc = Document::open(path)?;
    if doc.len() != length {
        return Err(io::Error::other(format!("its length is {} now, not the {length} the change left", doc.len())));
    }
    for edit in edits {
        if doc.read(edit.at, edit.new.len())? != edit.new {
            return Err(io::Error::other(format!("the text at byte {} is no longer what the change wrote", edit.at)));
        }
    }
    let back: Vec<(u64, u64, Vec<u8>)> = edits.iter().map(|e| (e.at, e.new.len() as u64, e.old.clone())).collect();
    doc.replace_many(&back)?;
    doc.save(path)
}

/// What undoing a change did.
pub struct Undone {
    pub id: String,
    /// The files put back as they were.
    pub restored: Vec<PathBuf>,
    /// The files left as they are, and why.
    pub refused: Vec<(PathBuf, String)>,
}

/// Undoes the change `id`, or the last one not undone, from the records in
/// `folder`. Each file is checked before it is touched and left alone when it
/// no longer holds what the change wrote, or when `keep` says to leave it (a
/// file open with unsaved edits). The change is marked undone when no file was
/// left. lw undo and the editor both undo with this.
pub fn undo(folder: &Path, id: Option<&str>, keep: &(dyn Fn(&Path) -> bool + Sync)) -> Result<Undone, String> {
    let records = records(folder).map_err(|e| e.to_string())?;
    let chosen = match id {
        Some(id) => records.iter().find(|r| r.id == id).ok_or(format!("no change {id}; lw undo --list shows them"))?,
        None => records.iter().rev().find(|r| !r.undone && !r.files.is_empty()).ok_or("no change to undo")?,
    };
    if chosen.undone {
        return Err(format!("change {} was already undone", chosen.id));
    }
    let files: Vec<&Changed> = chosen.files.iter().rev().collect();
    let results = each(&files, |file| {
        if keep(&file.path) {
            return Err(io::Error::other("it is open with unsaved changes"));
        }
        revert(&file.path, file.length, &file.edits)
    });
    let mut done = Undone { id: chosen.id.clone(), restored: Vec::new(), refused: Vec::new() };
    for (file, result) in files.iter().zip(results) {
        match result {
            Ok(()) => done.restored.push(file.path.clone()),
            // A save that was never marked done: the file holds the change only
            // if the save got through; otherwise there is nothing to undo.
            Err(_) if !file.saved => {}
            Err(e) => done.refused.push((file.path.clone(), e.to_string())),
        }
    }
    if done.refused.is_empty() {
        mark_undone(folder, &chosen.id).map_err(|e| e.to_string())?;
    }
    Ok(done)
}
