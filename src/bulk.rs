use super::*;
use linework_storage::Fold;
use std::collections::BTreeMap;

#[derive(Default)]
struct View {
    pane: usize,
    regions: Vec<(u64, u64)>,
    top: u64,
    collapsed: Vec<u64>,
}

/// Only positions the UI needs survive a streaming rewrite. The storage engine
/// can report hundreds of millions of replacements without retaining a list.
pub(crate) struct BulkView {
    marks: BTreeSet<u64>,
    views: Vec<View>,
    folds: Vec<Fold>,
    points: Vec<(u64, u64)>,
    next: usize,
    old_end: u64,
    new_end: u64,
}
impl BulkView {
    pub(super) fn capture(app: &App, i: usize) -> Self {
        let b = &app.buffers[i];
        let mut points = b.marks.clone();
        points.insert(b.last);
        let folds = app.closed(b.id, b.rev);
        for f in &folds {
            points.extend([f.header, f.start, f.end]);
        }
        let mut views = Vec::new();
        for p in 0..2 {
            if app.assigned[p] != Some(b.id) {
                continue;
            }
            let ed = &app.editors[p];
            let regions = if app.global_count_for(p) {
                vec![]
            } else {
                app.extension_selections(p).unwrap_or_default()
            };
            let line = ed.send(
                DOCLINEFROMVISIBLE,
                ed.send(GETFIRSTVISIBLELINE, 0, 0).max(0) as usize,
                0,
            );
            let top = events::point(
                p,
                ed.send(POSITIONFROMLINE, line.max(0) as usize, 0).max(0) as u64,
            );
            points.insert(top);
            for (a, z) in &regions {
                points.extend([*a, *z]);
            }
            let mut collapsed = Vec::new();
            if !b.large {
                let mut line = ed.send(CONTRACTEDFOLDNEXT, 0, 0);
                while line >= 0 {
                    let pos = ed.send(POSITIONFROMLINE, line as usize, 0).max(0) as u64;
                    points.insert(pos);
                    collapsed.push(pos);
                    let next = ed.send(CONTRACTEDFOLDNEXT, line as usize + 1, 0);
                    if next <= line {
                        break;
                    }
                    line = next;
                }
            }
            views.push(View {
                pane: p,
                regions,
                top,
                collapsed,
            });
        }
        Self {
            marks: b.marks.clone(),
            views,
            folds,
            points: points.into_iter().map(|p| (p, p)).collect(),
            next: 0,
            old_end: 0,
            new_end: 0,
        }
    }
    pub(super) fn edit(&mut self, r: Range<u64>, add: u64) {
        if self.next == self.points.len() {
            return;
        }
        let new_start = self.new_end + (r.start - self.old_end);
        while let Some((p, mapped)) = self.points.get_mut(self.next) {
            if *p > r.start && *p >= r.end {
                break;
            }
            *mapped = if *p <= r.start {
                self.new_end + (*p - self.old_end)
            } else {
                new_start + (*p - r.start).min(add)
            };
            self.next += 1;
        }
        self.new_end = new_start + add;
        self.old_end = r.end;
    }
    pub(super) fn finish(&mut self) {
        for (p, mapped) in &mut self.points[self.next..] {
            *mapped = self.new_end + (*p - self.old_end);
        }
        self.next = self.points.len();
    }
    pub(super) fn map_lines(&mut self, map: impl Fn(u64) -> u64) {
        for (p, v) in &mut self.points {
            *v = map(*p);
        }
        self.next = self.points.len();
    }
    pub(super) fn restore(mut self, app: &mut App, i: usize) {
        self.finish();
        let mapped: BTreeMap<_, _> = self.points.into_iter().collect();
        let at = |p: u64| mapped.get(&p).copied().unwrap_or(p);
        app.buffers[i].marks = self.marks.into_iter().map(at).collect();
        let id = app.buffers[i].id;
        if app.buffers[i].large {
            let folds = self
                .folds
                .into_iter()
                .filter_map(|f| {
                    let f = Fold {
                        header: at(f.header),
                        start: at(f.start),
                        end: at(f.end),
                    };
                    (f.header < f.start && f.start < f.end).then_some(f)
                })
                .collect();
            app.apply_folds(
                i,
                folds,
                at(app.buffers[i].last).min(app.buffers[i].doc.len()),
            );
        }
        for view in self.views {
            let p = view.pane;
            if app.assigned[p] != Some(id) {
                continue;
            }
            let regions: Vec<_> = view
                .regions
                .into_iter()
                .map(|(a, b)| (at(a), at(b)))
                .collect();
            if !regions.is_empty() {
                let _ = app.set_extension_selections(p, &regions);
            }
            let ed = &app.editors[p];
            for pos in view.collapsed {
                let line = ed.send(LINEFROMPOSITION, at(pos) as usize, 0).max(0) as usize;
                ed.send(FOLDLINE, line, 0);
            }
            let line = ed.send(
                LINEFROMPOSITION,
                events::offset(p, at(view.top)) as usize,
                0,
            );
            ed.send(
                SETFIRSTVISIBLELINE,
                ed.send(VISIBLEFROMDOCLINE, line.max(0) as usize, 0).max(0) as usize,
                0,
            );
            app.paint_bookmarks(i, p);
        }
    }
}
