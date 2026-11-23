use super::*;

// Native and Python extension APIs exchange arrays of coordinates. Refuse a
// request too large for that representation without truncating the editor's
// disk-backed selection set.
const MAX_EXTENSION_REGIONS: u64 = 65_536;

#[derive(Clone, Copy)]
struct OffsetChange {
    start: u64,
    end: u64,
    new_start: u64,
    added: u64,
}
struct OffsetMap(Vec<OffsetChange>);
impl OffsetMap {
    fn new(input: &[(u64, u64, u64)], old_len: u64, new_len: u64) -> Result<Self, String> {
        let (mut old, mut new) = (0, 0u64);
        let mut table = Vec::with_capacity(input.len());
        for &(start, del, added) in input {
            if start < old || start > old_len || del > old_len - start {
                return Err("Invalid extension edit range".into());
            }
            let new_start = new
                .checked_add(start - old)
                .ok_or("Invalid extension edit length")?;
            new = new_start
                .checked_add(added)
                .ok_or("Invalid extension edit length")?;
            old = start + del;
            table.push(OffsetChange {
                start,
                end: old,
                new_start,
                added,
            });
        }
        if new.checked_add(old_len - old) != Some(new_len) {
            return Err("Extension edit lengths disagree".into());
        }
        Ok(Self(table))
    }
    fn point(&self, pos: u64) -> u64 {
        let i = self.0.partition_point(|e| e.start < pos);
        if i == 0 {
            return pos;
        }
        let e = self.0[i - 1];
        if pos < e.end {
            e.new_start
        } else {
            e.new_start + e.added + (pos - e.end)
        }
    }
    fn fold(&self, f: Fold) -> Option<Fold> {
        let i = self.0.partition_point(|e| e.end <= f.header);
        if self.0.get(i).is_some_and(|e| e.start < f.end) {
            return None;
        }
        // An insertion exactly at the header belongs before the folded block.
        let delta = |pos: u64| {
            if i == 0 {
                pos
            } else {
                let e = self.0[i - 1];
                e.new_start + e.added + (pos - e.end)
            }
        };
        Some(Fold {
            header: delta(f.header),
            start: delta(f.start),
            end: delta(f.end),
        })
    }
}

impl App {
    /// Global UTF-8 (anchor, caret) pairs, primary selection first.
    pub(crate) fn extension_selections(&self, p: usize) -> Result<Vec<(u64, u64)>, String> {
        let id = self
            .assigned
            .get(p)
            .copied()
            .flatten()
            .ok_or("No document in this pane")?;
        let i = self.idx(id).ok_or("Document closed")?;
        let e = &self.editors[p];
        if let Some(cursors) = self.cursors(p) {
            if cursors.set.len() > MAX_EXTENSION_REGIONS {
                return Err("The extension selection list exceeds 65,536 ranges".into());
            }
            if let Some(order) = cursors.extension_order {
                return Ok(order.to_vec());
            }
            let backward = {
                let panes = &*self.views.panes.borrow();
                panes[p].backward
            };
            let pair = |r: &Range<u64>| {
                if backward {
                    (r.end, r.start)
                } else {
                    (r.start, r.end)
                }
            };
            let mut result = Vec::with_capacity(cursors.set.len() as usize);
            result.push(pair(&cursors.main));
            cursors
                .set
                .visit(0, |r| {
                    if r != cursors.main {
                        result.push(pair(&r));
                    }
                    Ok(true)
                })
                .map_err(|e| e.to_string())?;
            return Ok(result);
        }
        if let Some((bid, rev, range, _)) = &self.wide_selection[p] {
            if *bid == id && *rev == self.buffers[i].rev {
                let (a, b) = e.selection();
                return Ok(vec![if a > b {
                    (range.end, range.start)
                } else {
                    (range.start, range.end)
                }]);
            }
        }
        if e.send(GETSELECTIONS, 0, 0) > MAX_EXTENSION_REGIONS as isize {
            return Err("The extension selection list exceeds 65,536 ranges".into());
        }
        let main = e
            .send(linework_editor::sci::SCI_GETMAINSELECTION, 0, 0)
            .max(0) as usize;
        let mut ranges: Vec<_> = e
            .selections()
            .into_iter()
            .map(|(a, b)| {
                if !self.buffers[i].large {
                    return (a as u64, b as u64);
                }
                let lo = events::point(p, a.min(b) as u64);
                let hi = if a == b {
                    lo
                } else {
                    events::end_point(p, a.max(b) as u64)
                };
                if a > b { (hi, lo) } else { (lo, hi) }
            })
            .collect();
        if main < ranges.len() {
            ranges[..=main].rotate_right(1);
        }
        Ok(ranges)
    }
    /// Validate the result before any buffer in a native command is committed.
    pub(crate) fn validate_extension_selections(
        len: u64,
        regions: &[(u64, u64)],
    ) -> Result<(), String> {
        if regions.is_empty() {
            return Err("An editor view needs a caret".into());
        }
        if regions.len() as u64 > MAX_EXTENSION_REGIONS {
            return Err("The extension selection list exceeds 65,536 ranges".into());
        }
        let mut ordered = Vec::with_capacity(regions.len());
        for &(a, b) in regions {
            if a > len || b > len {
                return Err("Extension selection is outside the document".into());
            }
            ordered.push((a.min(b), a.max(b)));
        }
        ordered.sort_unstable();
        if ordered
            .windows(2)
            .any(|pair| pair[0].1 > pair[1].0 && pair[0] != pair[1])
        {
            return Err("Extension selections overlap".into());
        }
        Ok(())
    }
    pub(crate) fn set_extension_selections(
        &mut self,
        p: usize,
        regions: &[(u64, u64)],
    ) -> Result<(), String> {
        let id = self
            .assigned
            .get(p)
            .copied()
            .flatten()
            .ok_or("No document in this pane")?;
        let i = self.idx(id).ok_or("Document closed")?;
        if regions.len() as u64 > MAX_EXTENSION_REGIONS {
            return Err("The extension selection list exceeds 65,536 ranges".into());
        }
        let &(anchor, caret) = regions.first().ok_or("An editor view needs a caret")?;
        let len = if self.buffers[i].large {
            self.buffers[i].doc.len()
        } else {
            self.editors[p].len() as u64
        };
        if regions.iter().any(|&(a, b)| a > len || b > len) {
            return Err("Extension selection is outside the document".into());
        }
        if !self.buffers[i].large {
            quiet(|| {
                let e = &self.editors[p];
                for (n, &(a, b)) in regions.iter().enumerate() {
                    e.send(
                        if n == 0 {
                            linework_editor::sci::SCI_SETSELECTION
                        } else {
                            linework_editor::sci::SCI_ADDSELECTION
                        },
                        b as usize,
                        a as isize,
                    );
                }
                e.send(linework_editor::sci::SCI_SETMAINSELECTION, 0, 0);
            });
            return Ok(());
        }
        let primary = anchor.min(caret)..anchor.max(caret);
        let set = if regions.len() > 1 {
            Some(
                SelectionSet::from_ranges(
                    regions.iter().map(|&(a, b)| a.min(b)..a.max(b)).collect(),
                )
                .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        let active = self.active;
        self.active = p;
        self.clear_cursors(p);
        if let Some(set) = set {
            self.install_cursors(p, i, set, primary, false);
            {
                let pane = &mut self.views.panes.borrow_mut()[p];
                pane.backward = anchor > caret;
                if let Some(s) = &mut pane.cursors {
                    s.extension_order = Some(Arc::from(regions));
                }
            }
        } else {
            self.select_span(primary, anchor <= caret);
        }
        self.active = active;
        self.refresh();
        Ok(())
    }
    pub(crate) fn can_commit_extension_snapshot(&self, i: usize) -> Result<(), String> {
        let b = self.buffers.get(i).ok_or("Document closed")?;
        if !b.large || b.saving || b.follow || b.preparing.is_some() {
            return Err("This document cannot accept a paged extension edit".into());
        }
        if PENDING.with_borrow(|jobs| jobs.contains_key(&b.id)) {
            return Err("A previous file-wide edit is still running".into());
        }
        Ok(())
    }
    /// Apply a successful grouped snapshot atomically, then rebind both
    /// viewports to their corresponding positions in the edited document.
    pub(crate) fn commit_extension_snapshot(
        &mut self,
        i: usize,
        edited: Document,
        changes: &[(u64, u64, u64)],
    ) -> Result<(), String> {
        self.can_commit_extension_snapshot(i)?;
        let b = &self.buffers[i];
        let id = b.id;
        let old_len = b.doc.len();
        let mapping = OffsetMap::new(changes, old_len, edited.len())?;
        let mut positions = [None; 2];
        for (p, slot) in positions.iter_mut().enumerate() {
            if self.assigned[p] == Some(id) {
                *slot = Some(mapping.point(events::point(p, self.editors[p].caret() as u64)));
            }
        }
        let folds: Vec<_> = self
            .closed(id, b.rev)
            .into_iter()
            .filter_map(|f| mapping.fold(f))
            .collect();
        let folded = if folds.is_empty() {
            None
        } else {
            let map = Arc::new(FoldMap::new(folds.clone()));
            let view = map.project(&edited).map_err(|e| e.to_string())?;
            Some((map, view))
        };
        let b = &mut self.buffers[i];
        b.doc.apply_snapshot(edited).map_err(|e| e.to_string())?;
        b.rev += 1;
        b.dirty = b.doc.dirty();
        b.last = mapping.point(b.last.min(old_len)).min(b.doc.len());
        b.marks = b.marks.iter().map(|at| mapping.point(*at)).collect();
        if let Some((map, view)) = folded {
            self.views.folds.insert(
                id,
                Closed {
                    rev: b.rev,
                    folds,
                    map,
                    view,
                },
            );
        } else {
            self.views.folds.remove(&id);
        }
        let rev = b.rev;
        for (p, pos) in positions.into_iter().enumerate() {
            if let Some(at) = pos {
                let at = self
                    .projection(id, rev)
                    .and_then(|(map, _)| map.hidden(at))
                    .map_or(at, |f| f.header);
                self.bind(p, i, at);
            }
        }
        self.track_edit();
        self.refresh();
        Ok(())
    }
}
