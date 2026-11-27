//! Sorting lines by a field: guessing the separator, listing a line's fields
//! for the picker, and the `lw sort` arguments that carry out the choice.

/// The separators the picker guesses between, most specific first.
const CANDIDATES: [u8; 5] = [b'\t', b':', b',', b';', b'|'];

/// What the person chose in the sort dialog.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SortSpec {
    /// None: fields begin at runs of blanks, as GNU sort's default.
    pub separator: Option<u8>,
    /// 0 sorts by the whole line; otherwise the field number, from 1.
    pub field: usize,
    /// The chosen field is the last one: it runs to the end of the line, so
    /// a value that contains the separator still sorts as a whole.
    pub to_end: bool,
    pub fold: bool,
    pub numeric: bool,
    pub reverse: bool,
    pub unique: bool,
}

/// Read what was typed in the separator box: empty means blanks, `\t` or
/// `tab` a tab, `space` a space, otherwise exactly one ASCII character.
pub fn parse_separator(text: &str) -> Result<Option<u8>, String> {
    let t = text.trim_matches(|c| c == '\r' || c == '\n');
    match t.to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "\\t" | "tab" | "\t" => Ok(Some(b'\t')),
        "space" | " " => Ok(Some(b' ')),
        _ if t.len() == 1 && t.is_ascii() => Ok(Some(t.as_bytes()[0])),
        _ => Err("The separator must be one character, \\t for a tab, or empty for blanks".into()),
    }
}

/// How the separator is shown in the box.
pub fn separator_text(separator: Option<u8>) -> String {
    match separator {
        None => String::new(),
        Some(b'\t') => "\\t".into(),
        Some(b) => (b as char).to_string(),
    }
}

/// The separator most of the sample's lines contain at least once, if any
/// candidate is in at least half of them.
pub fn guess_separator(sample: &str) -> Option<u8> {
    let lines: Vec<&str> = sample
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(200)
        .collect();
    if lines.is_empty() {
        return None;
    }
    let mut best = None;
    let mut best_count = 0;
    for c in CANDIDATES {
        let n = lines.iter().filter(|l| l.as_bytes().contains(&c)).count();
        if n * 2 >= lines.len() && n > best_count {
            best = Some(c);
            best_count = n;
        }
    }
    best
}

/// The fields of one line as the sort sees them.
pub fn split(line: &str, separator: Option<u8>) -> Vec<&str> {
    let line = line.trim_end_matches(['\r', '\n']);
    match separator {
        Some(s) => line.split(s as char).collect(),
        None => line.split_whitespace().collect(),
    }
}

/// The picker's rows: the whole line, then each field with its value.
pub fn field_labels(line: &str, separator: Option<u8>) -> Vec<String> {
    let fields = split(line, separator);
    let mut rows = vec!["Whole line".to_owned()];
    for (i, f) in fields.iter().enumerate() {
        let mut shown: String = f.chars().take(60).collect();
        if f.chars().count() > 60 {
            shown.push('…');
        }
        let tail = if i + 1 == fields.len() && fields.len() > 1 {
            "   (to the end of the line)"
        } else {
            ""
        };
        rows.push(format!("Field {}:  {shown}{tail}", i + 1));
    }
    rows
}

/// The field (from 1) that holds byte `column` of `line`.
pub fn field_at(line: &str, separator: Option<u8>, column: usize) -> usize {
    let before = &line.as_bytes()[..column.min(line.len())];
    match separator {
        Some(s) => 1 + before.iter().filter(|b| **b == s).count(),
        None => {
            let text = String::from_utf8_lossy(before);
            let words = text.split_whitespace().count();
            if text.ends_with(char::is_whitespace) || words == 0 { words + 1 } else { words }
        }
    }
}

/// The `lw sort` options for a choice (input and output are added by the caller).
pub fn lw_args(spec: &SortSpec) -> Vec<String> {
    let mut args = Vec::new();
    let mut opts = String::new();
    if spec.separator.is_none() && spec.field > 0 {
        opts.push('b');
    }
    if spec.fold {
        opts.push('f');
    }
    if spec.numeric {
        opts.push('n');
    }
    if spec.reverse {
        opts.push('r');
    }
    if spec.field == 0 {
        args.extend(opts.chars().map(|c| format!("-{c}")));
    } else {
        if let Some(s) = spec.separator {
            args.push("-t".into());
            args.push((s as char).to_string());
        }
        let key = if spec.to_end {
            format!("{}{opts}", spec.field)
        } else {
            format!("{0}{opts},{0}{opts}", spec.field)
        };
        args.push("-k".into());
        args.push(key);
    }
    if spec.unique {
        args.push("-u".into());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_hash_list_guesses_the_colon_even_when_passwords_hold_commas() {
        let sample = "0000a:zap16201\n0000b:a,b,c,d\n0000c:Monty603\n";
        assert_eq!(guess_separator(sample), Some(b':'));
        assert_eq!(guess_separator("one two\nthree four\n"), None);
        assert_eq!(guess_separator("a\tb,c\nd\te\n"), Some(b'\t'));
    }
    #[test]
    fn fields_are_listed_and_found_by_column() {
        let line = "0000a:zap:16201";
        assert_eq!(split(line, Some(b':')), ["0000a", "zap", "16201"]);
        assert_eq!(field_labels(line, Some(b':'))[3], "Field 3:  16201   (to the end of the line)");
        assert_eq!(field_at(line, Some(b':'), 6), 2);
        assert_eq!(field_at(line, Some(b':'), 0), 1);
        assert_eq!(field_at("  a  b", None, 5), 2);
        assert_eq!(field_at("  a  b", None, 2), 1);
    }
    #[test]
    fn choices_become_gnu_sort_keys() {
        let spec = SortSpec { separator: Some(b':'), field: 2, to_end: true, fold: true, ..Default::default() };
        assert_eq!(lw_args(&spec), ["-t", ":", "-k", "2f"]);
        let spec = SortSpec { separator: Some(b','), field: 1, numeric: true, reverse: true, unique: true, ..Default::default() };
        assert_eq!(lw_args(&spec), ["-t", ",", "-k", "1nr,1nr", "-u"]);
        let spec = SortSpec { field: 0, fold: true, reverse: true, ..Default::default() };
        assert_eq!(lw_args(&spec), ["-f", "-r"]);
        let spec = SortSpec { field: 2, ..Default::default() };
        assert_eq!(lw_args(&spec), ["-k", "2b,2b"]);
        assert_eq!(parse_separator("\\t"), Ok(Some(b'\t')));
        assert_eq!(parse_separator(""), Ok(None));
        assert!(parse_separator("::").is_err());
    }
}
