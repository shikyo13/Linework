use super::*;
use std::io::{BufWriter, Write};

impl App {
    fn page_span(&self, p: usize) -> Option<Range<u64>> {
        let (id, rev, _, visible) = self.wide_selection[p].as_ref()?;
        let b = &self.buffers[self.idx(*id)?];
        if self.assigned[p] != Some(*id) || b.rev != *rev {
            return None;
        }
        Some(visible.clone())
    }
    pub(crate) fn wide_span(&self) -> Option<Range<u64>> {
        let p = self.active;
        if self.page_span(p).is_some() {
            return self.wide_selection[p].as_ref().map(|x| x.2.clone());
        }
        if !events::binding(p).large {
            return None;
        }
        let (a, b) = self.editors[p].selection();
        let (a, b) = (a.min(b), a.max(b));
        if a == b {
            return None;
        }
        let range = events::point(p, a as u64)..events::end_point(p, b as u64);
        (range.end - range.start > (b - a) as u64).then_some(range)
    }
    pub(super) fn check_wide_selection(&mut self, p: usize) {
        let Some(span) = self.page_span(p) else {
            self.wide_selection[p] = None;
            return;
        };
        let (a, b) = self.editors[p].selection();
        if span != (events::point(p, a.min(b) as u64)..events::end_point(p, a.max(b) as u64)) {
            self.wide_selection[p] = None;
        }
    }
    pub(crate) fn select_span(&mut self, range: Range<u64>, at_end: bool) {
        let Some(i) = self.current() else { return };
        let p = self.active;
        let id = self.buffers[i].id;
        let rev = self.buffers[i].rev;
        let at = if at_end { range.end } else { range.start };
        self.go(at, 0);
        let base = self.pane_base();
        let n = self.edit().len() as u64;
        let start = events::offset(p, range.start).min(n) as usize;
        let end = events::offset(p, range.end).min(n) as usize;
        if range.start < base
            || range.end > events::end_point(p, n)
            || range.end - range.start > (end - start) as u64
        {
            self.wide_selection[p] = Some((
                id,
                rev,
                range,
                events::point(p, start as u64)..events::end_point(p, end as u64),
            ));
        }
        if at_end {
            self.edit().set_selection(start, end)
        } else {
            self.edit().set_selection(end, start)
        }
        self.refresh();
    }
    pub(super) fn matching_insert(p: usize, id: u64, pos: u64) -> Option<Vec<u8>> {
        events::EVENTS.with_borrow_mut(|q| {
            let next = q.iter().position(|ev| matches!(ev, Event::Edit(..)))?;
            let Event::Edit(pane, bind, at, del, _) = &q[next] else {
                return None;
            };
            if (*pane, bind.id, *at, *del) != (p, id, pos, 0) {
                return None;
            }
            match q.remove(next) {
                Some(Event::Edit(_, _, _, _, bytes)) => Some(bytes),
                _ => None,
            }
        })
    }
    pub(super) fn take_span_edit(&mut self, p: usize, b: Binding, pos: u64, delete: u64) -> bool {
        if delete == 0 {
            return false;
        }
        let Some(projected) = self.page_span(p) else {
            return false;
        };
        if projected != (pos..pos + delete) {
            self.wide_selection[p] = None;
            return false;
        }
        let Some((_, _, all, _)) = self.wide_selection[p].take() else {
            return false;
        };
        let Some(i) = self.idx(b.id) else {
            return false;
        };
        // The control reports deleting a selection and inserting its replacement as two
        // notifications in the same dispatch. Commit them together to preserve undo.
        let append = Self::matching_insert(p, b.id, pos).unwrap_or_default();
        match self.buffers[i]
            .doc
            .replace(all.start, all.end - all.start, &append)
        {
            Ok(()) => {
                let buf = &mut self.buffers[i];
                buf.move_marks(all.start, all.end - all.start, append.len() as u64);
                buf.rev += 1;
                buf.dirty = buf.doc.dirty();
                self.active = p;
                self.bind(p, i, all.start + append.len() as u64);
                let other = 1 - p;
                if self.split && self.assigned[other] == Some(b.id) {
                    self.bind(
                        other,
                        i,
                        self.buffers[i].last.min(self.buffers[i].doc.len()),
                    );
                }
                self.track_edit();
                self.refresh();
            }
            Err(e) => {
                self.reload_views(i);
                self.notice(e.to_string());
            }
        }
        true
    }
    pub(super) fn copy_span(&mut self, range: Range<u64>, cut: bool) {
        let Some(i) = self.current() else { return };
        let n = range.end - range.start;
        if n > 32 * 1024 * 1024 {
            if cut {
                self.notice("This selection exceeds the clipboard limit. Copy it to a file, then delete the selection.");
                return;
            }
            let Some(dest) = crate::dialogs::files(self.h)
                .set_title("Copy large selection to a UTF-8 file")
                .set_file_name("selection.txt")
                .save_file()
            else {
                return;
            };
            if self.buffers.iter().any(|b| {
                b.path
                    .as_ref()
                    .is_some_and(|p| state::key(p) == state::key(&dest))
            }) {
                self.notice("Choose a destination outside the open tabs");
                return;
            }
            let file = self.buffers[i].doc.clone();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result = (|| -> io::Result<()> {
                    #[cfg(windows)]
                    let mut temp =
                        tempfile::NamedTempFile::new_in(dest.parent().unwrap_or(Path::new(".")))?;
                    // Elsewhere a temporary file is the owner's alone unless asked
                    // otherwise; this one becomes an ordinary new file.
                    #[cfg(not(windows))]
                    let mut temp = tempfile::Builder::new()
                        .permissions(<std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o666))
                        .tempfile_in(dest.parent().unwrap_or(Path::new(".")))?;
                    // A file it replaces is not opened to more people than it was.
                    #[cfg(not(windows))]
                    if let Ok(had) = std::fs::metadata(&dest) {
                        let _ = temp.as_file().set_permissions(had.permissions());
                    }
                    {
                        let mut out = BufWriter::new(temp.as_file_mut());
                        let mut at = range.start;
                        while at < range.end {
                            let bytes = file.read(at, (range.end - at).min(512 * 1024) as usize)?;
                            out.write_all(&bytes)?;
                            at += bytes.len() as u64;
                        }
                        out.flush()?;
                    }
                    temp.as_file().sync_all()?;
                    temp.persist(dest)?;
                    Ok(())
                })();
                let _ = tx.send(Worker::Info(
                    result
                        .map(|_| "Copied selection to file".into())
                        .unwrap_or_else(|e| e.to_string()),
                ));
            });
            self.notice("Copying selection to file…");
            return;
        }
        match self.buffers[i].doc.read(range.start, n as usize) {
            Ok(bytes) => {
                msg(
                    self.edit().hwnd(),
                    COPYTEXT,
                    bytes.len(),
                    bytes.as_ptr() as isize,
                );
                if cut && self.writable(i) {
                    self.edit().send(CLEAR, 0, 0);
                }
            }
            Err(e) => self.notice(e.to_string()),
        }
    }
}
