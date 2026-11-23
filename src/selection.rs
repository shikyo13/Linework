use linework_editor::{Editor, sci::*};
use std::{cell::RefCell, collections::VecDeque};

type Spans = Vec<(usize, usize)>;
struct Snapshot {
    key: (isize, isize, u64),
    spans: Spans,
    main: usize,
}
thread_local! {static HISTORY:RefCell<VecDeque<Snapshot>>=const{RefCell::new(VecDeque::new())};}
fn key(e: &Editor) -> (isize, isize, u64) {
    (
        e.hwnd(),
        e.send(SCI_GETDOCPOINTER, 0, 0),
        e.document_epoch(),
    )
}
fn remember(e: &Editor) {
    let s = Snapshot {
        key: key(e),
        spans: e.selections(),
        main: e.send(SCI_GETMAINSELECTION, 0, 0) as usize,
    };
    HISTORY.with_borrow_mut(|items| {
        if items.len() >= 32 {
            items.pop_front();
        }
        items.push_back(s);
    });
}
fn set_spans(e: &Editor, spans: &[(usize, usize)], main: usize) {
    let Some(&(a, b)) = spans.first() else {
        return;
    };
    e.send(SCI_CHANGESELECTIONMODE, 0, 0);
    e.send(SCI_SETSELECTION, b, a as isize);
    for &(a, b) in spans.iter().skip(1) {
        e.send(SCI_ADDSELECTION, b, a as isize);
    }
    e.send(SCI_SETMAINSELECTION, main.min(spans.len() - 1), 0);
    e.send(SCI_SCROLLCARET, 0, 0);
}
/// Match by exact selected UTF-8 bytes, starting after the primary selection
/// and wrapping once. Chunk overlap keeps matches on chunk boundaries visible.
fn occurrences(e: &Editor, all: bool, skip: bool) -> Result<(), String> {
    let (a, b) = e.selection();
    if a == b {
        let left = e.send(SCI_WORDSTARTPOSITION, b, 1) as usize;
        let right = e.send(SCI_WORDENDPOSITION, b, 1) as usize;
        if left == right {
            return Err("Place the caret on a word or select some text.".into());
        }
        e.set_selection(left, right);
        if !all && !skip {
            return Ok(());
        }
    }
    let (a, b) = e.selection();
    let left = a.min(b);
    let right = a.max(b);
    if right - left > 65536 {
        return Err("Select up to 64 KiB of text to match.".into());
    }
    let text = e.bytes_of_selection(); // exact UTF-8 selection
    if text.is_empty() {
        return Ok(());
    }
    let existing = e.selections();
    let mut ranges: Spans = if all { Vec::new() } else { existing.clone() };
    let mut occupied: Vec<_> = existing
        .iter()
        .map(|(a, b)| ((*a).min(*b), (*a).max(*b)))
        .collect();
    occupied.sort_unstable();
    let passes = if all {
        vec![(0, e.len())]
    } else {
        vec![(right, e.len()), (0, left)]
    };
    'scan: for (mut at, end) in passes {
        while at < end {
            let until = (at + 256 * 1024 + text.len() - 1).min(end);
            let data = e.raw_bytes(at, until);
            let capacity = if until == end {
                data.len()
            } else {
                data.len() - text.len() + 1
            };
            let mut n = 0;
            let mut it = linework_editor::literal_matches(&data, &text);
            for i in &mut it {
                if i >= capacity {
                    break;
                }
                n = i + text.len();
                let start = at + i;
                let stop = start + text.len();
                let j = occupied.partition_point(|&(_, b)| b <= start);
                if !all && occupied.get(j).is_some_and(|&(a, _)| a < stop) {
                    continue;
                }
                if ranges.len() >= 8192 {
                    return Err("More than 8,192 selections. Select a more specific phrase.".into());
                }
                if skip && !all {
                    ranges.remove(e.send(SCI_GETMAINSELECTION, 0, 0) as usize);
                }
                ranges.push((start, stop));
                if !all {
                    break 'scan;
                }
            }
            at += capacity.max(n);
        }
    }
    if !ranges.is_empty() {
        set_spans(e, &ranges, if all { 0 } else { ranges.len() - 1 });
    }
    Ok(())
}
fn split_lines(e: &Editor) -> Result<(), String> {
    let mut out = Vec::new();
    for (anchor, caret) in e.selections() {
        let (a, b) = (anchor.min(caret), anchor.max(caret));
        if a == b {
            out.push((a, b));
            continue;
        }
        let first = e.send(SCI_LINEFROMPOSITION, a, 0) as usize;
        let last = e.send(SCI_LINEFROMPOSITION, b - 1, 0) as usize;
        if last - first + out.len() >= 8192 {
            return Err("Select at most 8,192 lines at once.".into());
        }
        for line in first..=last {
            let begin = (e.send(SCI_POSITIONFROMLINE, line, 0) as usize).max(a);
            let end = (e.send(SCI_GETLINEENDPOSITION, line, 0) as usize)
                .min(b)
                .max(begin);
            out.push(if anchor <= caret {
                (begin, end)
            } else {
                (end, begin)
            });
        }
    }
    set_spans(e, &out, 0);
    Ok(())
}
fn adjacent(e: &Editor, delta: isize) {
    let caret = e.caret();
    let line = e.send(SCI_LINEFROMPOSITION, caret, 0) + delta;
    if line < 0 || line >= e.send(SCI_GETLINECOUNT, 0, 0) {
        return;
    }
    let active = e.send(SCI_GETMAINSELECTION, 0, 0) as usize;
    let col =
        e.send(SCI_GETCOLUMN, caret, 0) + e.send(SCI_GETSELECTIONNCARETVIRTUALSPACE, active, 0);
    let p = e.send(SCI_FINDCOLUMN, line as usize, col) as usize;
    if !e.selections().contains(&(p, p)) {
        e.send(SCI_ADDSELECTION, p, p as isize);
        let added = e.send(SCI_GETMAINSELECTION, 0, 0) as usize;
        let space = (col - e.send(SCI_GETCOLUMN, p, 0)).max(0);
        e.send(SCI_SETSELECTIONNANCHORVIRTUALSPACE, added, space);
        e.send(SCI_SETSELECTIONNCARETVIRTUALSPACE, added, space);
        e.send(SCI_SCROLLCARET, 0, 0);
    }
}
fn rewrite(
    e: &Editor,
    mut value: impl FnMut(usize, &str) -> String,
    only_nonempty: bool,
) -> Result<(), String> {
    let mut ranges: Vec<_> = e
        .selections()
        .iter()
        .enumerate()
        .map(|(i, &(a, b))| {
            let pad = e.send(SCI_GETSELECTIONNSTARTVIRTUALSPACE, i, 0).max(0) as usize;
            (a.min(b), a.max(b), pad)
        })
        .collect();
    ranges.sort_unstable();
    ranges.dedup();
    if ranges
        .iter()
        .any(|(a, b, pad)| b - a > 64 * 1024 * 1024 || *pad > 1024 * 1024)
    {
        return Err("Selection exceeds the 64 MiB transform limit.".into());
    }
    if ranges.windows(2).any(|v| v[0].1 > v[1].0) {
        return Err("Transform requires non-overlapping selections.".into());
    }
    let mut changes = Vec::new();
    let mut delta = 0isize;
    let mut updated = Vec::new();
    for (i, (a, b, pad)) in ranges.into_iter().enumerate() {
        if only_nonempty && a == b {
            updated.push((
                a.saturating_add_signed(delta),
                a.saturating_add_signed(delta),
            ));
            continue;
        }
        let mut text = " ".repeat(pad);
        let selected = if only_nonempty {
            String::from_utf8(e.raw_bytes(a,b)).map_err(|_|"The selection contains invalid UTF-8. Choose an encoding before changing case.")?
        } else {
            String::new()
        };
        text.push_str(&value(i, &selected));
        let at = a.saturating_add_signed(delta);
        updated.push((at + pad, at + text.len()));
        delta += text.len() as isize - (b - a) as isize;
        changes.push((a, b, text));
    }
    e.send(SCI_BEGINUNDOACTION, 0, 0);
    for (a, b, text) in changes.into_iter().rev() {
        e.replace_range(a, b, &text);
    }
    set_spans(e, &updated, 0);
    e.send(SCI_ENDUNDOACTION, 0, 0);
    Ok(())
}
fn move_lines(e: &Editor, delta: isize) {
    let start = |line: usize| {
        let at = e.send(SCI_POSITIONFROMLINE, line, 0);
        if at < 0 { e.len() } else { at as usize }
    };
    let end = |line: usize| e.send(SCI_GETLINEENDPOSITION, line, 0) as usize;
    let line_of = |pos: usize| e.send(SCI_LINEFROMPOSITION, pos, 0) as usize;
    let point = |pos: usize| {
        let line = line_of(pos);
        (line, pos - start(line))
    };
    let total = e.send(SCI_GETLINECOUNT, 0, 0) as usize;
    let main = e.send(SCI_GETMAINSELECTION, 0, 0) as usize;
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut selections = Vec::new();
    for (i, (a, b)) in e.selections().into_iter().enumerate() {
        let first = line_of(a.min(b));
        let last = line_of(if a == b { a } else { a.max(b) - 1 });
        groups.push((first, last));
        selections.push((
            first,
            point(a),
            point(b),
            e.send(SCI_GETSELECTIONNANCHORVIRTUALSPACE, i, 0),
            e.send(SCI_GETSELECTIONNCARETVIRTUALSPACE, i, 0),
        ));
    }
    groups.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (first, last) in groups {
        if let Some(previous) = merged.last_mut().filter(|v| first <= v.1 + 1) {
            previous.1 = previous.1.max(last);
        } else {
            merged.push((first, last));
        }
    }
    let can_move = |a: usize, b: usize| if delta < 0 { a > 0 } else { b + 1 < total };
    e.send(SCI_BEGINUNDOACTION, 0, 0);
    for &(a, b) in merged.iter().filter(|&&(a, b)| can_move(a, b)) {
        // Move only the neighboring line and its delimiter. Even a selection
        // spanning the entire document except one line needs one line of RAM.
        if delta < 0 {
            let neighbor = start(a - 1);
            let split = end(a - 1);
            let block_start = start(a);
            let content = e.raw_bytes(neighbor, split);
            let mut tail = e.raw_bytes(split, block_start);
            let destination = end(b) - (block_start - neighbor);
            tail.extend_from_slice(&content);
            e.replace_bytes(neighbor, block_start, &[]);
            e.replace_bytes(destination, destination, &tail);
        } else {
            let block_start = start(a);
            let split = end(b);
            let neighbor = start(b + 1);
            let finish = end(b + 1);
            let mut head = e.raw_bytes(neighbor, finish);
            head.extend_from_slice(&e.raw_bytes(split, neighbor));
            e.replace_bytes(split, finish, &[]);
            e.replace_bytes(block_start, block_start, &head);
        }
    }
    let updated: Vec<_> = selections
        .iter()
        .map(|&(first, a, b, _, _)| {
            let i = merged.partition_point(|x| x.1 < first);
            let (lo, hi) = merged[i];
            let shift = if can_move(lo, hi) { delta } else { 0 };
            let map = |(row, byte): (usize, usize)| {
                let row = row.saturating_add_signed(shift);
                if row >= total {
                    e.len()
                } else {
                    start(row) + byte.min(end(row) - start(row))
                }
            };
            (map(a), map(b))
        })
        .collect();
    set_spans(e, &updated, main);
    for (i, &(_, _, _, anchor, caret)) in selections.iter().enumerate() {
        e.send(SCI_SETSELECTIONNANCHORVIRTUALSPACE, i, anchor);
        e.send(SCI_SETSELECTIONNCARETVIRTUALSPACE, i, caret);
    }
    e.send(SCI_ENDUNDOACTION, 0, 0);
}
pub fn number(e: &Editor, start: i64, step: i64) -> Result<(), String> {
    rewrite(
        e,
        |i, _| {
            start
                .saturating_add((i as i64).saturating_mul(step))
                .to_string()
        },
        false,
    )
}
pub fn apply(e: &Editor, id: &str) -> Result<(), String> {
    if matches!(
        id,
        "selnext"
            | "selmatches"
            | "selskipped"
            | "sellines"
            | "caretup"
            | "caretdown"
            | "rectangle"
    ) {
        remember(e);
    }
    match id {
        "selnext" | "selmatches" | "selskipped" => {
            occurrences(e, id == "selmatches", id == "selskipped")?
        }
        "sellines" => split_lines(e)?,
        "caretup" | "caretdown" => adjacent(e, if id == "caretup" { -1 } else { 1 }),
        "rectangle" => {
            let now = e.send(SCI_GETSELECTIONMODE, 0, 0);
            e.send(
                SCI_SETSELECTIONMODE,
                if now == 1 || now == 3 { 0 } else { 1 },
                0,
            );
        }
        "undoselect" => {
            let old = HISTORY.with_borrow_mut(|h| {
                let i = h.iter().rposition(|s| s.key == key(e))?;
                h.remove(i)
            });
            if let Some(s) = old {
                set_spans(e, &s.spans, s.main);
            }
        }
        "upper" | "lower" => rewrite(
            e,
            |_, s| {
                if id == "upper" {
                    s.to_uppercase()
                } else {
                    s.to_lowercase()
                }
            },
            true,
        )?,
        "dup" => {
            e.send(SCI_SELECTIONDUPLICATE, 0, 0);
        }
        "moveup" | "movedown" => move_lines(e, if id == "moveup" { -1 } else { 1 }),
        _ => return Err("Unknown selection action.".into()),
    }
    Ok(())
}
