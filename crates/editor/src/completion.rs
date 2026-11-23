use super::{Editor, Notification, sci::*};
use std::collections::BTreeMap;
use std::ops::Range;

struct Snippet {
    label: &'static str,
    body: &'static str,
}
const RUST: &[Snippet] = &[
    Snippet {
        label: "fn · function",
        body: "fn \x01name\x02(\x01args\x02) {\n    \x01\x02\n}",
    },
    Snippet {
        label: "for · loop",
        body: "for \x01item\x02 in \x01items\x02 {\n    \x01\x02\n}",
    },
    Snippet {
        label: "match · expression",
        body: "match \x01value\x02 {\n    \x01pattern\x02 => \x01result\x02,\n}",
    },
    Snippet {
        label: "test · unit test",
        body: "#[test]\nfn \x01test_name\x02() {\n    \x01\x02\n}",
    },
];
const PYTHON: &[Snippet] = &[
    Snippet {
        label: "def · function",
        body: "def \x01name\x02(\x01args\x02):\n    \x01pass\x02",
    },
    Snippet {
        label: "for · loop",
        body: "for \x01item\x02 in \x01items\x02:\n    \x01pass\x02",
    },
    Snippet {
        label: "if · condition",
        body: "if \x01condition\x02:\n    \x01pass\x02",
    },
    Snippet {
        label: "class · declaration",
        body: "class \x01Name\x02:\n    \x01pass\x02",
    },
];
const JS: &[Snippet] = &[
    Snippet {
        label: "function · declaration",
        body: "function \x01name\x02(\x01args\x02) {\n    \x01\x02\n}",
    },
    Snippet {
        label: "for · of loop",
        body: "for (const \x01item\x02 of \x01items\x02) {\n    \x01\x02\n}",
    },
    Snippet {
        label: "if · condition",
        body: "if (\x01condition\x02) {\n    \x01\x02\n}",
    },
    Snippet {
        label: "const · arrow function",
        body: "const \x01name\x02 = (\x01args\x02) => {\n    \x01\x02\n};",
    },
];
const C_LIKE: &[Snippet] = &[
    Snippet {
        label: "if · condition",
        body: "if (\x01condition\x02) {\n    \x01\x02\n}",
    },
    Snippet {
        label: "while · loop",
        body: "while (\x01condition\x02) {\n    \x01\x02\n}",
    },
    Snippet {
        label: "for · loop",
        body: "for (\x01int i = 0\x02; \x01i < count\x02; \x01++i\x02) {\n    \x01\x02\n}",
    },
];
const HTML: &[Snippet] = &[
    Snippet {
        label: "html · document",
        body: "<!doctype html>\n<html lang=\"en\">\n<head>\n    <meta charset=\"utf-8\">\n    <title>\x01Title\x02</title>\n</head>\n<body>\n    \x01\x02\n</body>\n</html>",
    },
    Snippet {
        label: "a · link",
        body: "<a href=\"\x01url\x02\">\x01text\x02</a>",
    },
];
const MD: &[Snippet] = &[
    Snippet {
        label: "link · Markdown",
        body: "[\x01text\x02](\x01url\x02)",
    },
    Snippet {
        label: "task · checklist",
        body: "- [ ] \x01task\x02",
    },
];
const GO: &[Snippet] = &[Snippet {
    label: "func · declaration",
    body: "func \x01name\x02(\x01args\x02) {\n    \x01\x02\n}",
}];
fn snippets(lang: &str) -> &'static [Snippet] {
    match lang {
        "Rust" => RUST,
        "Python" => PYTHON,
        "HTML" => HTML,
        "Markdown" => MD,
        "Go" => GO,
        "JavaScript" | "TypeScript" => JS,
        "C" | "C++" | "C#" | "Java" => C_LIKE,
        _ => &[],
    }
}
pub(super) struct SnippetSession {
    fields: Vec<(u16, Range<usize>)>,
    stops: Vec<u16>,
    index: usize,
    pending: Vec<(usize, usize, bool)>,
}
fn identifier(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}
fn formatted(
    body: &str,
    start: usize,
    eol: &str,
    indent: &str,
    unit: &str,
) -> (String, Vec<Range<usize>>) {
    let mut text = String::new();
    let mut fields = Vec::new();
    let mut begin = 0;
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            text.push_str(eol);
            text.push_str(indent);
        }
        let mut rest = line;
        while let Some(t) = rest.strip_prefix("    ") {
            text.push_str(unit);
            rest = t;
        }
        for c in rest.chars() {
            match c {
                '\x01' => begin = start + text.len(),
                '\x02' => fields.push(begin..start + text.len()),
                c => text.push(c),
            }
        }
    }
    (text, fields)
}
impl Editor {
    pub fn complete(&self) -> bool {
        self.completions(true)
    }
    pub(super) fn completions(&self, explicit: bool) -> bool {
        let caret = self.caret();
        let start = self.send(SCI_WORDSTARTPOSITION, caret, 1).max(0) as usize;
        if caret.saturating_sub(start) > 100
            || (!explicit
                && (caret - start < self.completion_threshold.get()
                    || self.language() == "Plain Text"))
        {
            return false;
        }
        let prefix = self.text_range(start, caret);
        let key = prefix.to_lowercase();
        let lang = self.language.get();
        // Ordinary Markdown text is prose. A list of the page's own words
        // opening under every word typed gets in the way there, so unasked
        // it offers only the user's own snippets. Code in a Markdown page
        // completes as before, and asking with the shortcut lists everything.
        let mut quiet = false;
        if !explicit {
            if lang.name == "Markdown" {
                // The character just typed has no style until its line is
                // lexed, and its style is what says prose or code.
                let line = self.send(SCI_LINEFROMPOSITION, caret, 0).max(0) as usize;
                let from = self.send(SCI_POSITIONFROMLINE, line, 0).max(0) as usize;
                self.send(SCI_COLOURISE, from, caret as isize);
            }
            let style = self.send(SCI_GETSTYLEAT, caret.saturating_sub(1), 0) as usize;
            if self.prose_styles.get().get(style).copied().unwrap_or(false) {
                if lang.name != "Markdown" {
                    return false;
                }
                quiet = true;
            }
        }
        let mut matches: BTreeMap<String, u8> = BTreeMap::new();
        let mut add = |word: &str, rank: u8| {
            if !word.is_empty()
                && word.len() <= 96
                && word != prefix
                && word
                    .get(..key.len())
                    .is_some_and(|s| s.eq_ignore_ascii_case(&key))
            {
                let e = matches.entry(word.to_owned()).or_insert(rank);
                *e = (*e).min(rank);
            }
        };
        if quiet {
            let name = self.language();
            for snippet in self.user_snippets.borrow().iter().filter(|s| s.matches(&name)) {
                add(&snippet.label(), 0);
            }
        } else {
            for label in self.snippet_labels() {
                add(&label, 0);
            }
            let local = self.text_range(caret.saturating_sub(8192), caret + 8192);
            for word in local.split(|c| !identifier(c)).filter(|s| s.len() > 1) {
                add(word, 1);
            }
            for word in self.workspace_words.borrow().iter().take(10000) {
                add(word, 2);
            }
            for (_slot, words) in &lang.keywords {
                for word in words
                    .split_whitespace()
                    .filter(|w| w.chars().all(identifier))
                {
                    add(word, 3);
                }
            }
        }
        if matches.is_empty() {
            return false;
        }
        let mut words: Vec<_> = matches.into_iter().collect();
        words.sort_by(|(a, ra), (b, rb)| ra.cmp(rb).then_with(|| a.cmp(b)));
        words.truncate(64);
        // The control handles incremental filtering, arrows, Enter, Esc and
        // multi-selection completion. Custom order keeps local symbols on top.
        self.send(SCI_AUTOCSETORDER, 2, 0);
        self.put_str(
            SCI_AUTOCSHOW,
            caret - start,
            &words
                .into_iter()
                .map(|(s, _)| s)
                .collect::<Vec<_>>()
                .join("\n"),
        );
        true
    }
    pub fn set_user_snippets(&self, snippets: &[crate::UserSnippet]) {
        self.user_snippets.replace(
            snippets
                .iter()
                .filter(|s| s.validate().is_ok())
                .cloned()
                .collect(),
        );
    }
    pub fn snippet_labels(&self) -> Vec<String> {
        let mut labels: Vec<_> = snippets(&self.language())
            .iter()
            .map(|s| s.label.to_owned())
            .collect();
        labels.extend(
            self.user_snippets
                .borrow()
                .iter()
                .filter(|s| s.matches(&self.language()))
                .map(|s| s.label()),
        );
        labels.sort_unstable();
        labels.dedup();
        labels
    }
    fn template(&self, label: &str) -> Option<crate::user_snippet::Template> {
        if let Some(s) = self
            .user_snippets
            .borrow()
            .iter()
            .find(|s| s.matches(&self.language()) && s.label() == label)
        {
            return s.template().ok();
        }
        let s = snippets(&self.language())
            .iter()
            .find(|s| s.label == label)?;
        let n = s.body.matches('\x01').count();
        Some(crate::user_snippet::Template {
            body: s.body.to_owned(),
            stops: (1..=n as u16).collect(),
        })
    }
    pub fn insert_snippet(&self, label: &str) -> bool {
        let Some(template) = self.template(label) else {
            return false;
        };
        self.expand_snippet(self.selections(), &template)
    }
    pub(super) fn expand_completed_snippet(&self, position: isize) {
        if position < 0 {
            return;
        }
        let pos = position as usize;
        let caret = self.caret();
        if caret < pos || caret - pos > 128 {
            return;
        }
        let label = self.text_range(pos, caret);
        let Some(template) = self.template(&label) else {
            return;
        };
        let ranges = self
            .selections()
            .into_iter()
            .filter_map(|(_, caret)| {
                let a = caret.checked_sub(label.len())?;
                (self.text_range(a, caret) == label).then_some((a, caret))
            })
            .collect();
        self.expand_snippet(ranges, &template);
    }
    fn expand_snippet(
        &self,
        mut ranges: Vec<(usize, usize)>,
        template: &crate::user_snippet::Template,
    ) -> bool {
        if ranges.is_empty() {
            return false;
        }
        for (a, b) in &mut ranges {
            if *b < *a {
                std::mem::swap(a, b);
            }
        }
        ranges.sort_unstable();
        ranges.dedup();
        if ranges.windows(2).any(|v| v[0].1 > v[1].0)
            || ranges.len().saturating_mul(template.body.len()) > 64 * 1024 * 1024
        {
            return false;
        }
        let eol = match self.send(SCI_GETEOLMODE, 0, 0) {
            0 => "\r\n",
            1 => "\r",
            _ => "\n",
        };
        let width = self.send(SCI_GETINDENT, 0, 0).clamp(1, 32) as usize;
        let unit = if self.send(SCI_GETUSETABS, 0, 0) != 0 {
            "\t".into()
        } else {
            " ".repeat(width)
        };
        let mut delta = 0isize;
        let mut edits = Vec::new();
        let mut fields = Vec::new();
        let mut pending = Vec::new();
        for (a, b) in ranges {
            let line = self.send(SCI_LINEFROMPOSITION, a, 0) as usize;
            let line_start = self.send(SCI_POSITIONFROMLINE, line, 0) as usize;
            let indent: String = self
                .text_range(line_start, a.min(line_start + 4096))
                .chars()
                .take_while(|c| matches!(c, ' ' | '\t'))
                .collect();
            let (text, spans) = formatted(
                &template.body,
                a.saturating_add_signed(delta),
                eol,
                &indent,
                &unit,
            );
            fields.extend(template.stops.iter().copied().zip(spans));
            delta += text.len() as isize - (b - a) as isize;
            if b > a {
                pending.push((a, b - a, true));
            }
            if !text.is_empty() {
                pending.push((a, text.len(), false));
            }
            edits.push((a, b, text));
        }
        self.snippet.replace(None);
        self.send(SCI_BEGINUNDOACTION, 0, 0);
        for (a, b, value) in edits.iter().rev() {
            self.replace_range(*a, *b, value);
        }
        self.send(SCI_ENDUNDOACTION, 0, 0);
        let mut stops = template.stops.clone();
        stops.sort_by_key(|&s| (s == 0, s));
        stops.dedup();
        let session = SnippetSession {
            fields,
            stops,
            index: 0,
            pending,
        };
        self.select_fields(&session);
        self.snippet.replace(Some(session));
        true
    }
    fn select_fields(&self, session: &SnippetSession) {
        if let Some(&n) = session.stops.get(session.index) {
            let mut fields = session.fields.iter().filter(|f| f.0 == n).map(|f| &f.1);
            if let Some(r) = fields.next() {
                self.set_selection(r.start, r.end);
                for r in fields {
                    self.send(SCI_ADDSELECTION, r.end, r.start as isize);
                }
                self.send(SCI_SETMAINSELECTION, 0, 0);
                self.send(SCI_SCROLLCARET, 0, 0);
            }
        }
    }
    pub fn snippet_next(&self) -> bool {
        // Take the session out before sending native messages, which can
        // synchronously re-enter the host's WM_NOTIFY handler.
        let Some(mut session) = self.snippet.take() else {
            return false;
        };
        let (a, b) = self.selection();
        let n = session.stops[session.index];
        if !session
            .fields
            .iter()
            .any(|(group, r)| *group == n && a.min(b) >= r.start && a.max(b) <= r.end)
        {
            return false;
        }
        session.index += 1;
        if session.index >= session.stops.len() {
            return false;
        }
        self.select_fields(&session);
        self.snippet.replace(Some(session));
        true
    }
    pub(super) fn track_snippet_edit(&self, notice: &Notification) {
        let mut slot = self.snippet.borrow_mut();
        let Some(session) = slot.as_mut() else { return };
        let pos = notice.position.max(0) as usize;
        let length = notice.length.max(0) as usize;
        let deletion = notice.modification_type & 2 != 0;
        if let Some(i) = session
            .pending
            .iter()
            .position(|&sig| sig == (pos, length, deletion))
        {
            session.pending.remove(i);
            return;
        }
        session.pending.clear();
        let group = session.stops[session.index];
        let Some(i) = session.fields.iter().position(|(n, r)| {
            *n == group
                && r.start <= pos
                && pos <= r.end
                && (!deletion || pos.saturating_add(length) <= r.end)
        }) else {
            return;
        };
        let delta = if deletion {
            -(length as isize)
        } else {
            length as isize
        };
        for (j, (_, r)) in session.fields.iter_mut().enumerate().skip(i) {
            if i != j {
                r.start = r.start.saturating_add_signed(delta);
            }
            r.end = r.end.saturating_add_signed(delta);
        }
    }
}
