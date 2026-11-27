//! Orders for the search results: file order, or sorted by the matched text,
//! the text from the match, a field, the line's length, or how often a line
//! occurs (identical lines collapsed into one row with a count).
use crate::sort_spec::{self, SortSpec};
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Order {
    #[default]
    File,
    Matched,
    FromMatch,
    Field,
    Length,
    Frequent,
}

impl Order {
    pub const ALL: [Order; 6] = [
        Order::File,
        Order::Matched,
        Order::FromMatch,
        Order::Field,
        Order::Length,
        Order::Frequent,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Order::File => "File order",
            Order::Matched => "Matched text",
            Order::FromMatch => "Text from the match",
            Order::Field => "Field…",
            Order::Length => "Line length",
            Order::Frequent => "Most frequent first",
        }
    }
    /// How the results heading names the order.
    pub fn caption(self) -> &'static str {
        match self {
            Order::File => "file order",
            Order::Matched => "matched text",
            Order::FromMatch => "text from the match",
            Order::Field => "field",
            Order::Length => "line length",
            Order::Frequent => "most frequent",
        }
    }
}

/// The order chosen for the results, with the field settings and switches
/// (ignore case, numbers, reverse, one row per value) of a `SortSpec`.
#[derive(Clone, Debug, Default)]
pub struct ResultSort {
    pub order: Order,
    pub spec: SortSpec,
}

/// One matching line: its text and its matches as byte ranges in the text.
pub struct Item<'a> {
    pub text: &'a str,
    pub ranges: &'a [(usize, usize)],
}

fn folded(s: &str, fold: bool) -> String {
    if fold { s.to_lowercase() } else { s.to_owned() }
}

/// The leading number of `s`, as GNU sort's -n reads one (none counts as 0).
fn number(s: &str) -> f64 {
    let t = s.trim_start();
    let end = t
        .char_indices()
        .take_while(|(i, c)| c.is_ascii_digit() || *c == '.' || (*i == 0 && *c == '-'))
        .last()
        .map_or(0, |(i, c)| i + c.len_utf8());
    t[..end].parse().unwrap_or(0.0)
}

fn key<'a>(item: &Item<'a>, sort: &ResultSort) -> &'a str {
    let first = item.ranges.first().copied();
    let at = |r: Option<(usize, usize)>| r.filter(|r| r.1 <= item.text.len() && item.text.is_char_boundary(r.0) && item.text.is_char_boundary(r.1));
    match sort.order {
        Order::Matched => at(first).map_or(item.text, |r| &item.text[r.0..r.1]),
        Order::FromMatch => at(first).map_or(item.text, |r| &item.text[r.0..]),
        // Most frequent counts by the field holding the match, from the
        // separator before it to the end of the line: many hashes cracked to
        // one password become one row, and mrcandycane7 stays apart from
        // candycane7.
        Order::Frequent => at(first).map_or(item.text, |r| {
            let start = item.text[..r.0]
                .rfind([':', char::from(9), ',', ';', '|', ' '])
                .map_or(0, |p| p + 1);
            &item.text[start..]
        }),
        Order::Field => {
            let s = &sort.spec;
            if s.field == 0 {
                return item.text;
            }
            let fields = sort_spec::split(item.text, s.separator);
            match fields.get(s.field - 1) {
                None => "",
                Some(f) if s.to_end => {
                    let start = f.as_ptr() as usize - item.text.as_ptr() as usize;
                    &item.text[start..]
                }
                Some(f) => f,
            }
        }
        _ => item.text,
    }
}

/// The rows in the chosen order: each an index into `items` and how many
/// lines it stands for (more than 1 only when identical lines are collapsed).
pub fn arrange(items: &[Item<'_>], sort: &ResultSort) -> Vec<(usize, usize)> {
    let spec = &sort.spec;
    if sort.order == Order::File {
        return (0..items.len()).map(|i| (i, 1)).collect();
    }
    let keys: Vec<String> = items
        .iter()
        .map(|it| {
            let k = key(it, sort);
            let k = if spec.decode_hex {
                String::from_utf8_lossy(&linework_search::hexcode::decode(k.as_bytes())).into_owned()
            } else {
                k.to_owned()
            };
            folded(&k, spec.fold)
        })
        .collect();
    let collapse = sort.order == Order::Frequent || spec.unique;
    let mut rows: Vec<(usize, usize)> = if collapse {
        let mut first: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let mut rows: Vec<(usize, usize)> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            match first.get(k.as_str()) {
                Some(&row) => rows[row].1 += 1,
                None => {
                    first.insert(k.as_str(), rows.len());
                    rows.push((i, 1));
                }
            }
        }
        rows
    } else {
        (0..items.len()).map(|i| (i, 1)).collect()
    };
    let by_key = |a: &(usize, usize), b: &(usize, usize)| -> Ordering {
        let (ka, kb) = (&keys[a.0], &keys[b.0]);
        let first = match sort.order {
            Order::Frequent => b.1.cmp(&a.1),
            Order::Length => ka.chars().count().cmp(&kb.chars().count()),
            _ if spec.numeric => number(ka).total_cmp(&number(kb)),
            _ => Ordering::Equal,
        };
        first.then_with(|| ka.cmp(kb))
    };
    rows.sort_by(|a, b| {
        let o = by_key(a, b);
        if spec.reverse { o.reverse() } else { o }
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    fn items<'a>(lines: &'a [(&'a str, Vec<(usize, usize)>)]) -> Vec<Item<'a>> {
        lines.iter().map(|(t, r)| Item { text: t, ranges: r }).collect()
    }
    fn texts(lines: &[(&str, Vec<(usize, usize)>)], sort: &ResultSort) -> Vec<String> {
        let it = items(lines);
        arrange(&it, sort).into_iter().map(|(i, n)| format!("{}{}", lines[i].0, if n > 1 { format!(" x{n}") } else { String::new() })).collect()
    }
    #[test]
    fn results_sort_by_match_from_match_field_length_and_frequency() {
        let lines = [
            ("e9:Candycane7", vec![(3, 12)]),
            ("8d:candycanees", vec![(3, 12)]),
            ("2b:CANDYCANES03", vec![(3, 12)]),
            ("ad:mrcandycane1", vec![(5, 14)]),
            ("8d:candycanees", vec![(3, 12)]),
            ("ff:candycanees", vec![(3, 12)]),
        ];
        let mut sort = ResultSort { order: Order::FromMatch, ..Default::default() };
        assert_eq!(texts(&lines, &sort), ["2b:CANDYCANES03", "e9:Candycane7", "ad:mrcandycane1", "8d:candycanees", "8d:candycanees", "ff:candycanees"]);
        sort.spec.fold = true;
        assert_eq!(texts(&lines, &sort)[0], "ad:mrcandycane1");
        sort = ResultSort { order: Order::Frequent, ..Default::default() };
        // Different hashes, one password: one row, counted by the field.
        assert_eq!(texts(&lines, &sort)[0], "8d:candycanees x3");
        assert_eq!(texts(&lines, &sort).len(), 4);
        let near = [("a1:candycane7", vec![(3, 12)]), ("a2:mrcandycane7", vec![(5, 14)]), ("a3:candycane7", vec![(3, 12)])];
        assert_eq!(texts(&near, &sort), ["a1:candycane7 x2", "a2:mrcandycane7"]);
        sort = ResultSort { order: Order::Length, ..Default::default() };
        assert_eq!(texts(&lines, &sort)[0], "e9:Candycane7");
        sort.spec.reverse = true;
        // Equal lengths fall back to the text, and reverse turns that round too.
        assert_eq!(texts(&lines, &sort)[..2], ["ad:mrcandycane1", "2b:CANDYCANES03"]);
        sort = ResultSort {
            order: Order::Field,
            spec: SortSpec { separator: Some(b':'), field: 1, ..Default::default() },
        };
        assert_eq!(texts(&lines, &sort)[0], "2b:CANDYCANES03");
        sort.spec.unique = true;
        assert_eq!(texts(&lines, &sort).len(), 5);
        let hexes = [("h1:$HEX[7a6170]", vec![(3, 15)]), ("h2:yes", vec![(3, 6)])];
        sort = ResultSort { order: Order::FromMatch, ..Default::default() };
        assert_eq!(texts(&hexes, &sort)[0], "h1:$HEX[7a6170]");
        sort.spec.decode_hex = true;
        assert_eq!(texts(&hexes, &sort)[0], "h2:yes");
        let nums = [("a:10", vec![]), ("a:9", vec![]), ("a:-1", vec![])];
        sort = ResultSort {
            order: Order::Field,
            spec: SortSpec { separator: Some(b':'), field: 2, to_end: true, numeric: true, ..Default::default() },
        };
        assert_eq!(texts(&nums, &sort), ["a:-1", "a:9", "a:10"]);
    }
}
