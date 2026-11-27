use super::*;
use std::collections::BTreeSet;

fn line_comment(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "Rust" | "JavaScript" | "TypeScript" | "C" | "C++" | "C#" | "Java" | "Kotlin" | "Go"
        | "Swift" | "PHP" | "D" | "Dart" | "Zig" | "Verilog" | "Groovy" | "Graphviz" | "F#" => "//",
        "Python" | "PowerShell" | "Ruby" | "Shell" | "YAML" | "TOML" | "R" | "CMake"
        | "Makefile" | "Dockerfile" | "Perl" | "Nim" | "Julia" | "Tcl" | "Apache Config" => "#",
        "Lua" | "SQL" | "Haskell" | "VHDL" => "--",
        "Visual Basic" => "'",
        "Batch" => "REM",
        "INI" | "Assembly" | "Lisp" | "Clojure" => ";",
        "LaTeX" | "Erlang" => "%",
        "Fortran" => "!",
        _ => return None,
    })
}

impl App {
    pub(super) fn line_action(&mut self, id: &str) {
        let Some(i) = self.current() else { return };
        if !self.writable(i) {
            return;
        }
        if id == "indent" || id == "outdent" {
            self.edit()
                .send(if id == "indent" { TAB } else { BACKTAB }, 0, 0);
            return;
        }
        if id == "comment" {
            let lang = self.buffers[i].lang.clone();
            if let Some(prefix) = line_comment(&lang) {
                if let Err(e) = self.comment_lines(i, prefix) {
                    self.notice(e)
                }
                return;
            }
            let wrap = match lang.as_str() {
                "HTML" | "XML" | "Markdown" => Some(("<!--", "-->")),
                "CSS" => Some(("/*", "*/")),
                "OCaml" | "Pascal" => Some(("(*", "*)")),
                _ => None,
            };
            if let Some((left, right)) = wrap {
                let e = self.edit();
                let mut edits = vec![];
                let physical=|offset:usize|events::point(self.active,offset as u64) as usize;
                for (a, b) in e.selections() {
                    let (a, b) = if a == b {
                        let l = e.send(LINEFROMPOSITION, a, 0) as usize;
                        (
                            e.send(POSITIONFROMLINE, l, 0) as usize,
                            e.send(GETLINEENDPOSITION, l, 0) as usize,
                        )
                    } else {
                        (a.min(b), a.max(b))
                    };
                    let t = e.text_range(a, b);
                    let replacement = if let Some(inner) =
                        t.strip_prefix(left).and_then(|s| s.strip_suffix(right))
                    {
                        inner.to_owned()
                    } else {
                        format!("{left}{t}{right}")
                    };
                    let start = physical(a);
                    let end = if a == b {
                        start
                    } else {
                        events::end_point(self.active, b as u64) as usize
                    };
                    if end - start != b - a {
                        self.notice("Expand the selection’s folded lines before wrapping them");
                        return;
                    }
                    edits.push((start, end, replacement));
                }
                edits.sort_by_key(|x| x.0);
                edits.dedup_by_key(|x| x.0);
                if let Err(err) = self.change_open(i, edits) {
                    self.notice(err)
                }
            } else {
                self.notice("This language has no configured comment syntax")
            }
            return;
        }
        if let Err(err) = self.rewrite_lines(i, id) {
            self.notice(err)
        }
    }
    fn comment_lines(&mut self, i: usize, prefix: &str) -> Result<(), String> {
        let e = self.edit();
        let mut lines = BTreeSet::new();
        for (a, b) in e.selections() {
            let first = e.send(LINEFROMPOSITION, a.min(b), 0) as usize;
            let last_pos = if a == b { a } else { a.max(b) - 1 };
            let last = e.send(LINEFROMPOSITION, last_pos, 0) as usize;
            if last.saturating_sub(first) > 100000 || lines.len() > 100000 {
                return Err("Select up to 100,000 lines to toggle comments".into());
            }
            lines.extend(first..=last);
        }
        let physical=|offset:usize|events::point(self.active,offset as u64) as usize;
        let mut heads = Vec::new();
        for l in lines {
            let start = e.send(POSITIONFROMLINE, l, 0) as usize;
            let end = e.send(GETLINEENDPOSITION, l, 0) as usize;
            let pos = e.send(GETLINEINDENTPOSITION, l, 0) as usize;
            if pos == end && end != start {
                continue;
            }
            let leading = e.text_range(pos, end.min(pos + prefix.len() + 1));
            heads.push((pos, leading));
        }
        let remove = !heads.is_empty() && heads.iter().all(|(_, s)| s.starts_with(prefix));
        let edits = heads
            .into_iter()
            .map(|(p, s)| {
                if remove {
                    let n =
                        prefix.len() + usize::from(s.as_bytes().get(prefix.len()) == Some(&b' '));
                    (
                        physical(p),
                        events::end_point(self.active, (p + n) as u64) as usize,
                        String::new(),
                    )
                } else {
                    (physical(p), physical(p), format!("{prefix} "))
                }
            })
            .collect();
        self.change_open(i, edits)
    }
    fn rewrite_lines(&mut self, i: usize, id: &str) -> Result<(), String> {
        self.sync_buffer(i)?;
        let d = &self.buffers[i].doc;
        let (a, b) = self.edit().selection();
        let r = if let Some(r) = self.wide_span() {
            r
        } else if a == b {
            0..d.len()
        } else {
            let from = events::point(self.active, a.min(b) as u64);
            let to = events::end_point(self.active, a.max(b) as u64);
            if d.indexed_bytes() < to {
                return Err("Wait for the line index before transforming these lines".into());
            }
            d.line_start(d.line_of(from))..d.line_start(d.line_of(to - 1) + 1).max(to).min(d.len())
        };
        let count = r.end - r.start;
        let many_lines = d.indexed_bytes() >= r.end
            && d.line_of(r.end) - d.line_of(r.start) > 1_000_000;
        if (id == "sortlines" || id == "sortdesc")
            && (count > super::sort_lines::IN_MEMORY || many_lines)
        {
            let spec = crate::sort_spec::SortSpec {
                reverse: id == "sortdesc",
                ..Default::default()
            };
            self.start_lw_sort(i, r, &spec);
            return Ok(());
        }
        if count > 32 * 1024 * 1024 {
            return Err("Select up to 32 MiB of lines for this transformation".into());
        }
        let (start, end) = (r.start as usize, r.end as usize);
        let document = d.clone();
        let name = id.to_owned();
        let mut layout = BulkView::capture(self, i);
        let run = move || -> Result<Option<(String, BulkView)>, String> {
            let bytes = document
                .read(start as u64, count as usize)
                .map_err(|e| e.to_string())?;
            let source = String::from_utf8(bytes)
                .map_err(|_| "Reopen with the correct encoding before transforming these lines")?;
            if source
                .bytes()
                .filter(|b| *b == 10 || *b == 13)
                .take(1_000_001)
                .count()
                > 1_000_000
            {
                return Err("Select a smaller range for this line transformation".into());
            }
            let (result, moves) = transform_mapped(&source, &name, document.newline());
            layout.map_lines(|p| {
                if p < start as u64 {
                    return p;
                }
                if p >= end as u64 {
                    return p - count + result.len() as u64;
                }
                let rel = p as usize - start;
                let idx = moves.partition_point(|m| m.0 <= rel).saturating_sub(1);
                let (old, new, len) = moves[idx];
                (start + new + (rel - old).min(len)) as u64
            });
            Ok((result != source).then_some((result, layout)))
        };
        if count > 512 * 1024 && !self.recording && !self.playing {
            let (job, cancel) = self.new_job("line tools");
            let id = self.buffers[i].id;
            let rev = self.buffers[i].rev;
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let result = run();
                let result = if cancel.load(Ordering::Relaxed) {
                    Err("Line transformation canceled".into())
                } else {
                    result
                };
                let _ = tx.send(Worker::Lines(job, id, rev, start, end, result));
            });
            self.notice("Transforming lines…");
        } else if let Some((new, layout)) = run()? {
            self.change_open(i, vec![(start, end, new)])?;
            layout.restore(self, i);
        }
        Ok(())
    }
}
#[cfg(test)]
fn transform(src: &str, id: &str, eol: &str) -> String {
    transform_mapped(src, id, eol).0
}
fn transform_mapped(
    src: &str,
    id: &str,
    default_eol: &str,
) -> (String, Vec<(usize, usize, usize)>) {
    // Endings belong to output positions, preserving mixed EOL order and whether
    // the document ended in a newline independently of how content is sorted.
    let mut values = Vec::new();
    let mut endings = Vec::new();
    let mut start = 0;
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\r' || b[i] == b'\n' {
            let n = if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
                2
            } else {
                1
            };
            values.push(&src[start..i]);
            endings.push(&src[i..i + n]);
            i += n;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < b.len() {
        values.push(&src[start..]);
        endings.push("");
    }
    let final_eol = endings.last().copied().unwrap_or("");
    let originals = values.clone();
    match id {
        "sortlines" => values.sort(),
        "sortdesc" => values.sort_by(|a, b| b.cmp(a)),
        "dedup" => {
            let mut seen = HashSet::new();
            values.retain(|s| seen.insert(*s));
        }
        "trimlines" => {
            for v in &mut values {
                *v = v.trim_end_matches([' ', '\t']);
            }
        }
        _ => return (src.into(), vec![(0, 0, src.len())]),
    }
    let mut out = String::with_capacity(src.len());
    let mut moves = std::collections::HashMap::new();
    let mut first = std::collections::HashMap::new();
    for (i, v) in values.iter().enumerate() {
        moves.insert(v.as_ptr(), (out.len(), v.len()));
        if id == "dedup" {
            first.entry(*v).or_insert((out.len(), v.len()));
        }
        out.push_str(v);
        let delim = if i + 1 == values.len() {
            final_eol
        } else {
            endings
                .get(i)
                .copied()
                .filter(|s| !s.is_empty())
                .unwrap_or(default_eol)
        };
        out.push_str(delim);
    }
    let positions = originals
        .iter()
        .map(|v| {
            let old = v.as_ptr() as usize - src.as_ptr() as usize;
            let (new, len) = moves
                .get(&v.as_ptr())
                .or_else(|| first.get(*v))
                .copied()
                .unwrap_or((old, v.len()));
            (old, new, len)
        })
        .collect();
    (out, positions)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn line_transforms_preserve_trailing_line_and_cr() {
        assert_eq!(transform("b\r\na\ra", "sortlines", "\n"), "a\r\na\rb");
        assert_eq!(transform("a\r\nb\r\na\r\n", "dedup", "\n"), "a\r\nb\r\n");
        assert_eq!(transform("x  \r\ny\t", "trimlines", "\n"), "x\r\ny");
    }
}
