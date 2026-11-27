//! Sorting lines by a field, and sorting any range too large to sort in
//! memory, through the `lw sort` engine shipped beside Linework. The sorted
//! text stays in a temporary file the document reads from, and replaces the
//! lines as one undo step.
use super::*;
use crate::sort_spec::{self, SortSpec};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

/// Ranges up to this size, sorted by whole lines, are sorted in memory.
pub(super) const IN_MEMORY: u64 = 32 * 1024 * 1024;

pub(super) type SortResult = Result<(Document, u64, BulkView), String>;

impl App {
    /// The lines the selections cover, or the whole document when nothing is
    /// selected.
    fn sort_span(&mut self, i: usize) -> Result<Range<u64>, String> {
        let d = &self.buffers[i].doc;
        if let Some(r) = self.wide_span() {
            return Ok(r);
        }
        let sels = self.edit().selections();
        let a = sels.iter().map(|(x, y)| (*x).min(*y)).min().unwrap_or(0);
        let b = sels.iter().map(|(x, y)| (*x).max(*y)).max().unwrap_or(0);
        if sels.len() <= 1 && a == b {
            return Ok(0..d.len());
        }
        let from = events::point(self.active, a as u64);
        let to = events::end_point(self.active, b as u64).max(from + 1).min(d.len());
        if d.indexed_bytes() < to {
            return Err("Wait for the line index before sorting these lines".into());
        }
        Ok(d.line_start(d.line_of(from))..d.line_start(d.line_of(to - 1) + 1).max(to).min(d.len()))
    }
    /// The line at the caret, for the field picker, and the field the first
    /// selection starts in when there are several (a rectangle or carets).
    fn sort_sample(&mut self, i: usize, range: &Range<u64>) -> (String, String, Option<usize>) {
        let d = &self.buffers[i].doc;
        let caret = events::point(self.active, self.edit().caret() as u64).min(d.len());
        let line = line_around(d, caret);
        let head = d
            .read(range.start, 64 * 1024)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let sels = self.edit().selections();
        let column = (sels.len() > 1).then(|| {
            let e = self.edit();
            let a = sels.iter().map(|(x, y)| (*x).min(*y)).min().unwrap_or(0);
            let start = e.send(POSITIONFROMLINE, e.send(LINEFROMPOSITION, a, 0) as usize, 0) as usize;
            e.text_range(start, a).len()
        });
        (line, head, column)
    }
    pub(super) fn sort_by_field(&mut self) {
        let Some(i) = self.current() else { return };
        if !self.writable(i) {
            return;
        }
        if let Err(e) = self.sync_buffer(i) {
            self.notice(e);
            return;
        }
        let range = match self.sort_span(i) {
            Ok(r) => r,
            Err(e) => {
                self.notice(e);
                return;
            }
        };
        let (line, head, column) = self.sort_sample(i, &range);
        let separator = sort_spec::guess_separator(&head);
        let field = column.map_or(0, |c| sort_spec::field_at(&line, separator, c));
        let spec = match forms::sort_form(self.h, &line, separator, field) {
            None => return,
            Some(Err(e)) => {
                self.notice(e);
                return;
            }
            Some(Ok(spec)) => spec,
        };
        self.start_lw_sort(i, range, &spec);
    }
    /// Sort `range` of buffer `i` with `lw sort` on a worker thread.
    pub(super) fn start_lw_sort(&mut self, i: usize, range: Range<u64>, spec: &SortSpec) {
        let args = sort_spec::lw_args(spec);
        let unique = spec.unique;
        let b = &self.buffers[i];
        let (id, rev, doc) = (b.id, b.rev, b.doc.clone());
        let layout = BulkView::capture(self, i);
        let (job, cancel) = self.new_job("sort");
        let tx = self.tx.clone();
        let host = self.h;
        let span = range.clone();
        std::thread::spawn(move || {
            let r = sort_with_lw(doc, span.clone(), &args, unique, &cancel)
                .map(|(doc, n)| (doc, n, layout));
            let r = if cancel.load(Ordering::Relaxed) { Err("Sort canceled".into()) } else { r };
            let _ = tx.send(Worker::Sorted(job, id, rev, span, r));
            post(host, WM_TIMER, 1, 0);
        });
        self.notice("Sorting lines…");
    }
    pub(super) fn finish_sort(&mut self, id: u64, rev: u64, range: Range<u64>, result: SortResult) {
        self.drain_edits();
        let Some(i) = self.idx(id) else { return };
        let r = (|| {
            let b = &self.buffers[i];
            if b.rev != rev || b.saving || b.follow {
                return Err("The document changed while sorting. Sort again.".to_owned());
            }
            let (doc, n, layout) = result?;
            if b.large || doc.len() > LARGE {
                let promote = !b.large;
                self.buffers[i].doc.apply_snapshot(doc).map_err(|e| e.to_string())?;
                self.buffers[i].rev += 1;
                self.buffers[i].dirty = true;
                if promote {
                    self.promote(i)
                } else {
                    self.reload_views(i)
                }
                self.track_edit();
            } else {
                let raw = doc.read(range.start, n as usize).map_err(|e| e.to_string())?;
                let value = String::from_utf8(raw).map_err(|_| "The sorted text is not valid UTF-8")?;
                self.change_open(i, vec![(range.start as usize, range.end as usize, value)])?;
            }
            layout.restore(self, i);
            Ok(range.end - range.start - n)
        })();
        match r {
            Ok(0) => self.notice("Sorted lines"),
            Ok(removed) => self.notice(format!("Sorted lines, removing {removed} bytes of repeated lines")),
            Err(e) => self.notice(e),
        }
    }
}

/// The text of the line holding `pos`, read from at most 4 KiB either side.
fn line_around(d: &Document, pos: u64) -> String {
    let from = pos.saturating_sub(4096);
    let bytes = d.read(from, 8192).unwrap_or_default();
    let at = (pos - from) as usize;
    let at = at.min(bytes.len());
    let start = bytes[..at].iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let end = bytes[at..].iter().position(|b| *b == b'\n' || *b == b'\r').map_or(bytes.len(), |p| at + p);
    String::from_utf8_lossy(&bytes[start..end]).into_owned()
}

/// Free bytes on the volume holding `folder`.
fn free_space(folder: &Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut free = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(wide(&folder.to_string_lossy()).as_ptr(), &mut free, null_mut(), null_mut()) };
    (ok != 0).then_some(free)
}

/// Removes a path when dropped, unless disarmed.
struct Cleanup(Option<PathBuf>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = fs::remove_file(p);
        }
    }
}

/// Sort the lines of `range` of `doc` with `lw sort ARGS`, and return the
/// document with that range replaced (one undo step) and the new range length.
pub(super) fn sort_with_lw(
    mut doc: Document,
    range: Range<u64>,
    args: &[String],
    unique: bool,
    cancel: &AtomicBool,
) -> Result<(Document, u64), String> {
    let lw = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name("lw.exe");
    if !lw.is_file() {
        return Err(format!("Sorting this needs lw.exe beside Linework ({})", lw.display()));
    }
    let len = range.end - range.start;
    if len == 0 {
        return Err("There are no lines to sort".into());
    }
    let encoding = doc.encoding_name();
    if encoding.starts_with("UTF-16") || encoding == "Hexadecimal" {
        return Err(format!("Lines in {encoding} cannot be sorted this way"));
    }
    let io = |e: io::Error| e.to_string();
    let ends_in_newline = doc.read(range.end - 1, 1).map_err(io)? == b"\n";
    // The ending given to a last line that has none: the one the lines
    // themselves use, else the document's.
    let tail_from = range.end.saturating_sub(64 * 1024).max(range.start);
    let tail = doc.read(tail_from, (range.end - tail_from) as usize).map_err(io)?;
    let newline: &[u8] = match tail.iter().rposition(|b| *b == b'\n') {
        Some(p) if p > 0 && tail[p - 1] == b'\r' => b"\r\n",
        Some(_) => b"\n",
        None => doc.newline().as_bytes(),
    };
    if newline == b"\r" {
        return Err("Sorting this way needs \\n or \\r\\n line endings".into());
    }
    let whole_clean_file = range == (0..doc.len())
        && !doc.dirty()
        && ends_in_newline
        && encoding == "UTF-8"
        && doc.path().and_then(|p| fs::metadata(p).ok()).is_some_and(|m| m.len() == doc.len())
        && matches!(doc.disk_changed(), Ok(false));
    let temp = std::env::temp_dir();
    let need = len + if whole_clean_file { 0 } else { len + newline.len() as u64 };
    if let Some(free) = free_space(&temp) {
        if free < need {
            return Err(format!(
                "Sorting needs {} MB free in {}, and {} MB are free",
                need >> 20,
                temp.display(),
                free >> 20
            ));
        }
    }
    // The input: the file itself when the document is exactly that file,
    // otherwise the range written out (with a final newline if it has none).
    let export;
    let input = if whole_clean_file {
        doc.path().unwrap().to_path_buf()
    } else {
        let mut f = tempfile::NamedTempFile::new_in(&temp).map_err(io)?;
        let mut buf = vec![0u8; 4 << 20];
        let mut at = range.start;
        while at < range.end {
            if cancel.load(Ordering::Relaxed) {
                return Err("Sort canceled".into());
            }
            let n = (range.end - at).min(buf.len() as u64) as usize;
            doc.read_into(at, &mut buf[..n]).map_err(io)?;
            f.write_all(&buf[..n]).map_err(io)?;
            at += n as u64;
        }
        if !ends_in_newline {
            f.write_all(newline).map_err(io)?;
        }
        f.flush().map_err(io)?;
        let path = f.path().to_path_buf();
        export = f;
        let _ = &export;
        path
    };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let out = temp.join(format!("linework-sort-{}-{stamp}.txt", std::process::id()));
    let mut cleanup = Cleanup(Some(out.clone()));
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    let mut child = Command::new(&lw)
        .arg("sort")
        .args(args)
        .arg("-o")
        .arg(&out)
        .arg(&input)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("Could not start lw sort: {e}"))?;
    let mut stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_string(&mut s);
        }
        s
    });
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Sort canceled".into());
        }
        match child.try_wait().map_err(io)? {
            Some(s) => break s,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let message = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("lw sort failed: {}", message.trim()));
    }
    // The document reads the sorted text from this file, which Windows
    // deletes once the last version that uses it is gone.
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    };
    let file = OpenOptions::new()
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
        .open(&out)
        .map_err(io)?;
    cleanup.0 = None;
    let mut n = file.metadata().map_err(io)?.len();
    // A last line that had no ending gets none again. Whichever line sorts
    // last loses its ending; with mixed endings that can move one \r.
    let mut slack = 0;
    if !ends_in_newline {
        use std::os::windows::fs::FileExt;
        let mut end = [0u8; 2];
        let have = n.min(2) as usize;
        file.seek_read(&mut end[..have], n - have as u64).map_err(io)?;
        let cut = if newline.len() == 2 && end[..have] == *b"\r\n" {
            2
        } else if have > 0 && end[have - 1] == b'\n' {
            1
        } else {
            return Err("lw sort did not end the last line as expected; nothing was changed".into());
        };
        slack = (newline.len() as u64).abs_diff(cut);
        n -= cut;
        file.set_len(n).map_err(io)?;
    }
    if !unique && n.abs_diff(len) > slack {
        return Err(format!("lw sort returned {n} bytes for {len}; nothing was changed"));
    }
    doc.replace_from_file(range.start, len, file).map_err(io)?;
    Ok((doc, n))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lw_beside_tests() -> bool {
        std::env::current_exe().is_ok_and(|p| {
            p.parent().and_then(|d| d.parent()).is_some_and(|d| d.join("lw.exe").is_file())
        })
    }
    #[test]
    fn a_range_sorts_by_its_second_field_through_lw_and_undoes() -> Result<(), String> {
        // Test binaries live in target/<profile>/deps; lw.exe sits one level up.
        if !lw_beside_tests() {
            eprintln!("lw.exe not built beside the tests; skipped");
            return Ok(());
        }
        let exe = std::env::current_exe().unwrap();
        let lw = exe.parent().unwrap().parent().unwrap().join("lw.exe");
        let here = exe.with_file_name("lw.exe");
        fs::copy(&lw, &here).map_err(|e| e.to_string())?;
        let mut d = Document::empty();
        d.replace(0, 0, b"keep\nh3:b:c\nh1:a\nh2:B\nend").map_err(|e| e.to_string())?;
        let spec = SortSpec { separator: Some(b':'), field: 2, to_end: true, ..Default::default() };
        let cancel = AtomicBool::new(false);
        let (mut sorted, n) = sort_with_lw(d.clone(), 5..22, &sort_spec::lw_args(&spec), false, &cancel)?;
        assert_eq!(n, 17);
        assert_eq!(sorted.read(0, 99).unwrap(), b"keep\nh2:B\nh1:a\nh3:b:c\nend");
        // Without a final newline, the last line stays without one.
        let (tail, _) = sort_with_lw(d.clone(), 17..25, &[], false, &cancel)?;
        assert_eq!(tail.read(0, 99).unwrap(), b"keep\nh3:b:c\nh1:a\nend\nh2:B");
        assert!(sorted.undo());
        assert_eq!(sorted.read(0, 99).unwrap(), d.read(0, 99).unwrap());
        let _ = fs::remove_file(here);
        Ok(())
    }
}
