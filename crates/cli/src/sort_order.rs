//! The orders lw sort can put lines in: GNU sort's keys (-k) and options
//! (-b -d -f -g -h -i -M -n -R -r -V), with the byte order of the C locale.
use std::cmp::Ordering;

/// How one key, or the whole line, is compared.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Opts {
    pub blanks_start: bool,
    pub blanks_end: bool,
    pub dictionary: bool,
    pub fold: bool,
    pub general: bool,
    pub human: bool,
    pub nonprinting: bool,
    pub month: bool,
    pub numeric: bool,
    pub random: bool,
    pub reverse: bool,
    pub version: bool,
}

impl Opts {
    /// GNU sort's test for a key that takes the global options: none of its own but -r... and not -r either.
    fn plain(&self) -> bool {
        !(self.dictionary || self.nonprinting || self.fold || self.blanks_start || self.blanks_end || self.month || self.numeric || self.general || self.human || self.version || self.random)
    }

    /// Applies one option letter; false for a letter that is not one.
    pub fn set(&mut self, letter: char, end: bool) -> bool {
        match letter {
            'b' => {
                if end {
                    self.blanks_end = true
                } else {
                    self.blanks_start = true
                }
            }
            'd' => self.dictionary = true,
            'f' => self.fold = true,
            'g' => self.general = true,
            'h' => self.human = true,
            'i' => self.nonprinting = true,
            'M' => self.month = true,
            'n' => self.numeric = true,
            'R' => self.random = true,
            'r' => self.reverse = true,
            'V' => self.version = true,
            _ => return false,
        }
        true
    }

    fn kinds(&self) -> usize {
        [self.general, self.human, self.month, self.numeric, self.random, self.version].iter().filter(|&&k| k).count()
    }
}

/// One -k key: fields and characters counted from 0; `end_field` None is the end of the line, `end_char` 0 the end of its field.
#[derive(Clone, Debug)]
pub struct Key {
    pub start_field: usize,
    pub start_char: usize,
    pub end_field: Option<usize>,
    pub end_char: usize,
    pub opts: Opts,
}

impl Key {
    /// Reads KEYDEF: F[.C][OPTS][,F[.C][OPTS]].
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = |why: &str| format!("-k {text:?}: {why}");
        let mut opts = Opts::default();
        let (start, end) = match text.split_once(',') {
            Some((a, b)) => (a, Some(b)),
            None => (text, None),
        };
        let number = |t: &str| -> (Option<usize>, usize) {
            let digits = t.bytes().take_while(u8::is_ascii_digit).count();
            (t[..digits].parse().ok(), digits)
        };
        let (field, used) = number(start);
        let field = field.ok_or_else(|| bad("a key starts with a field number"))?;
        if field == 0 {
            return Err(bad("field numbers are counted from 1"));
        }
        let mut rest = &start[used..];
        let mut start_char = 0;
        if let Some(r) = rest.strip_prefix('.') {
            let (c, used) = number(r);
            let c = c.ok_or_else(|| bad("a character position follows the dot"))?;
            if c == 0 {
                return Err(bad("character positions are counted from 1"));
            }
            start_char = c - 1;
            rest = &r[used..];
        }
        for letter in rest.chars() {
            if !opts.set(letter, false) {
                return Err(bad(&format!("{letter} is not an ordering option")));
            }
        }
        let (mut end_field, mut end_char) = (None, 0);
        if let Some(end) = end {
            let (f, used) = number(end);
            let f = f.ok_or_else(|| bad("a field number follows the comma"))?;
            if f == 0 {
                return Err(bad("field numbers are counted from 1"));
            }
            end_field = Some(f - 1);
            let mut rest = &end[used..];
            if let Some(r) = rest.strip_prefix('.') {
                let (c, used) = number(r);
                end_char = c.ok_or_else(|| bad("a character position follows the dot"))?;
                rest = &r[used..];
            }
            for letter in rest.chars() {
                if !opts.set(letter, true) {
                    return Err(bad(&format!("{letter} is not an ordering option")));
                }
            }
        }
        if opts.kinds() > 1 {
            return Err(bad("only one of g, h, M, n, R and V may be given for a key"));
        }
        Ok(Self { start_field: field - 1, start_char, end_field, end_char, opts })
    }
}

/// Everything that decides the order of two lines.
#[derive(Clone)]
pub struct Order {
    pub keys: Vec<Key>,
    pub global: Opts,
    /// -t: the field separator; None for GNU's default (a run of blanks begins each field).
    pub tab: Option<u8>,
    pub stable: bool,
    pub unique: bool,
    /// The whole line compared as bytes, perhaps reversed: the fast path.
    pub plain: bool,
    /// Salt for -R, so its order changes from run to run.
    pub salt: (u64, u64),
    /// --decode-hex: keys are compared with $HEX[...] decoded.
    pub decode_hex: bool,
}

fn blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

impl Order {
    /// Sets the keys up as GNU sort does: keys without options take the global ones, and with no keys but global options, the whole line is one key.
    pub fn new(mut keys: Vec<Key>, global: Opts, tab: Option<u8>, stable: bool, unique: bool, salt: (u64, u64)) -> Self {
        for key in &mut keys {
            if key.opts.plain() && !key.opts.reverse {
                key.opts = global;
            }
        }
        if keys.is_empty() && (!global.plain() || global.reverse) {
            keys.push(Key { start_field: 0, start_char: 0, end_field: None, end_char: 0, opts: global });
        }
        let plain = keys.is_empty() || (keys.len() == 1 && keys[0].start_field == 0 && keys[0].start_char == 0 && keys[0].end_field.is_none() && keys[0].opts.plain());
        Self { keys, global, tab, stable, unique, plain, salt, decode_hex: false }
    }

    /// Compare keys with hashcat's $HEX[...] decoded to the bytes it stands
    /// for (lines are written as they are). The whole line is then a key.
    pub fn decode_hex(&mut self) {
        if self.keys.is_empty() {
            self.keys.push(Key { start_field: 0, start_char: 0, end_field: None, end_char: 0, opts: self.global });
        }
        self.plain = false;
        self.decode_hex = true;
    }

    pub fn reversed(&self) -> bool {
        self.global.reverse
    }

    /// The order of two lines (without their end bytes).
    pub fn cmp(&self, a: &[u8], b: &[u8]) -> Ordering {
        if self.plain {
            let o = a.cmp(b);
            return if self.global.reverse { o.reverse() } else { o };
        }
        let o = self.key_cmp(a, b);
        if o != Ordering::Equal || self.unique || self.stable {
            return o;
        }
        let o = a.cmp(b);
        if self.global.reverse {
            o.reverse()
        } else {
            o
        }
    }

    /// Whether two lines are the same for -u: equal by their keys.
    pub fn same(&self, a: &[u8], b: &[u8]) -> bool {
        if self.plain {
            return a == b;
        }
        self.key_cmp(a, b) == Ordering::Equal
    }

    fn key_cmp(&self, a: &[u8], b: &[u8]) -> Ordering {
        for key in &self.keys {
            let ka = self.key_text(a, key);
            let kb = self.key_text(b, key);
            let o = if self.decode_hex {
                compare_key(&linework_search::hexcode::decode(ka), &linework_search::hexcode::decode(kb), &key.opts, self.salt)
            } else {
                compare_key(ka, kb, &key.opts, self.salt)
            };
            if o != Ordering::Equal {
                return if key.opts.reverse { o.reverse() } else { o };
            }
        }
        Ordering::Equal
    }

    /// The part of `line` a key covers, as GNU sort's begfield and limfield find it.
    fn key_text<'a>(&self, line: &'a [u8], key: &Key) -> &'a [u8] {
        let lim = line.len();
        // Start.
        let mut ptr = 0;
        let mut words = key.start_field;
        match self.tab {
            Some(t) => {
                while ptr < lim && words > 0 {
                    words -= 1;
                    while ptr < lim && line[ptr] != t {
                        ptr += 1;
                    }
                    if ptr < lim {
                        ptr += 1;
                    }
                }
            }
            None => {
                while ptr < lim && words > 0 {
                    words -= 1;
                    while ptr < lim && blank(line[ptr]) {
                        ptr += 1;
                    }
                    while ptr < lim && !blank(line[ptr]) {
                        ptr += 1;
                    }
                }
            }
        }
        if key.opts.blanks_start {
            while ptr < lim && blank(line[ptr]) {
                ptr += 1;
            }
        }
        let start = (ptr + key.start_char).min(lim);
        // End.
        let end = match key.end_field {
            None => lim,
            Some(field) => {
                let mut ptr = 0;
                let mut words = field + usize::from(key.end_char == 0);
                match self.tab {
                    Some(t) => {
                        while ptr < lim && words > 0 {
                            words -= 1;
                            while ptr < lim && line[ptr] != t {
                                ptr += 1;
                            }
                            if ptr < lim && (words > 0 || key.end_char != 0) {
                                ptr += 1;
                            }
                        }
                    }
                    None => {
                        while ptr < lim && words > 0 {
                            words -= 1;
                            while ptr < lim && blank(line[ptr]) {
                                ptr += 1;
                            }
                            while ptr < lim && !blank(line[ptr]) {
                                ptr += 1;
                            }
                        }
                    }
                }
                if key.end_char != 0 {
                    if key.opts.blanks_end {
                        while ptr < lim && blank(line[ptr]) {
                            ptr += 1;
                        }
                    }
                    ptr = (ptr + key.end_char).min(lim);
                }
                ptr
            }
        };
        if end <= start {
            &line[start..start]
        } else {
            &line[start..end]
        }
    }
}

fn compare_key(a: &[u8], b: &[u8], o: &Opts, salt: (u64, u64)) -> Ordering {
    if o.random {
        return random_cmp(a, b, salt);
    }
    if o.numeric {
        return numeric_cmp(a, b);
    }
    if o.general {
        return general_cmp(a, b);
    }
    if o.human {
        return human_cmp(a, b);
    }
    if o.month {
        return month(a).cmp(&month(b));
    }
    if o.version {
        return version_cmp(a, b);
    }
    let ignore: Option<fn(u8) -> bool> = if o.dictionary {
        Some(|c: u8| !(c.is_ascii_alphanumeric() || blank(c)))
    } else if o.nonprinting {
        Some(|c: u8| !(0x20..0x7f).contains(&c))
    } else {
        None
    };
    let tr = |c: u8| if o.fold { c.to_ascii_uppercase() } else { c };
    match ignore {
        Some(skip) => {
            let (mut i, mut j) = (0, 0);
            loop {
                while i < a.len() && skip(a[i]) {
                    i += 1;
                }
                while j < b.len() && skip(b[j]) {
                    j += 1;
                }
                if i >= a.len() || j >= b.len() {
                    break;
                }
                let d = tr(a[i]).cmp(&tr(b[j]));
                if d != Ordering::Equal {
                    return d;
                }
                i += 1;
                j += 1;
            }
            (i < a.len()).cmp(&(j < b.len()))
        }
        None if o.fold => {
            for (x, y) in a.iter().zip(b) {
                let d = tr(*x).cmp(&tr(*y));
                if d != Ordering::Equal {
                    return d;
                }
            }
            a.len().cmp(&b.len())
        }
        None => a.cmp(b),
    }
}

/// -n: an optional minus, digits and a decimal point after leading blanks; anything else counts as zero.
pub fn numeric_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (na, ia, fa) = number_parts(a);
    let (nb, ib, fb) = number_parts(b);
    let za = ia.is_empty() && fa.is_empty();
    let zb = ib.is_empty() && fb.is_empty();
    let sa = if za { 0 } else if na { -1 } else { 1 };
    let sb = if zb { 0 } else if nb { -1 } else { 1 };
    if sa != sb {
        return sa.cmp(&sb);
    }
    if sa == 0 {
        return Ordering::Equal;
    }
    let magnitude = ia.len().cmp(&ib.len()).then_with(|| ia.cmp(ib)).then_with(|| fa.cmp(fb));
    if sa < 0 {
        magnitude.reverse()
    } else {
        magnitude
    }
}

/// Sign, integer digits without leading zeros, fraction digits without trailing zeros.
fn number_parts(s: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut i = 0;
    while i < s.len() && blank(s[i]) {
        i += 1;
    }
    let negative = s.get(i) == Some(&b'-');
    if negative {
        i += 1;
    }
    let start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let mut int = &s[start..i];
    while let [b'0', rest @ ..] = int {
        int = rest;
    }
    let mut frac: &[u8] = &[];
    if s.get(i) == Some(&b'.') {
        let f = i + 1;
        let mut j = f;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        frac = &s[f..j];
        while let [rest @ .., b'0'] = frac {
            frac = rest;
        }
    }
    (negative, int, frac)
}

/// -h: by the unit (K, M, G, T, P, E, Z, Y, R, Q) first, then as -n.
fn human_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let skip = |s: &[u8]| -> usize { s.iter().take_while(|&&c| blank(c)).count() };
    let (a, b) = (&a[skip(a)..], &b[skip(b)..]);
    unit_order(a).cmp(&unit_order(b)).then_with(|| numeric_cmp(a, b))
}

fn unit_order(s: &[u8]) -> i32 {
    let minus = s.first() == Some(&b'-');
    let mut i = usize::from(minus);
    let mut max_digit = b'0' - 1;
    while i < s.len() && (s[i].is_ascii_digit() || s[i] == b'.') {
        if s[i].is_ascii_digit() {
            max_digit = max_digit.max(s[i]);
        }
        i += 1;
    }
    if max_digit > b'0' {
        let order = match s.get(i) {
            Some(b'K' | b'k') => 1,
            Some(b'M') => 2,
            Some(b'G') => 3,
            Some(b'T') => 4,
            Some(b'P') => 5,
            Some(b'E') => 6,
            Some(b'Z') => 7,
            Some(b'Y') => 8,
            Some(b'R') => 9,
            Some(b'Q') => 10,
            _ => 0,
        };
        return if minus { -order } else { order };
    }
    0
}

/// -M: JAN to DEC after leading blanks, in any letter case; anything else before JAN.
fn month(s: &[u8]) -> u8 {
    const MONTHS: [&[u8; 3]; 12] = [b"JAN", b"FEB", b"MAR", b"APR", b"MAY", b"JUN", b"JUL", b"AUG", b"SEP", b"OCT", b"NOV", b"DEC"];
    let i = s.iter().take_while(|&&c| blank(c)).count();
    let word = &s[i..];
    if word.len() < 3 {
        return 0;
    }
    let up = [word[0].to_ascii_uppercase(), word[1].to_ascii_uppercase(), word[2].to_ascii_uppercase()];
    MONTHS.iter().position(|m| **m == up).map_or(0, |p| p as u8 + 1)
}

/// -g: numbers as C's strtod reads them; text that is not a number first, then NaN, then numbers in order.
fn general_cmp(a: &[u8], b: &[u8]) -> Ordering {
    match (strtod(a), strtod(b)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => match x.partial_cmp(&y) {
            Some(o) => o,
            None => match (x.is_nan(), y.is_nan()) {
                (true, true) => x.to_bits().cmp(&y.to_bits()),
                (true, false) => Ordering::Less,
                _ => Ordering::Greater,
            },
        },
    }
}

/// The number at the start of `s`, as C's strtod reads it, or None when there is none.
pub fn strtod(s: &[u8]) -> Option<f64> {
    let mut i = s.iter().take_while(|&&c| matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')).count();
    let mut negative = false;
    if let Some(&c @ (b'+' | b'-')) = s.get(i) {
        negative = c == b'-';
        i += 1;
    }
    let rest = &s[i..];
    let lower = |n: usize| -> Vec<u8> { rest.iter().take(n).map(u8::to_ascii_lowercase).collect() };
    let sign = |v: f64| if negative { -v } else { v };
    if lower(3) == b"inf" {
        return Some(sign(f64::INFINITY));
    }
    if lower(3) == b"nan" {
        return Some(sign(f64::NAN));
    }
    if lower(2) == b"0x" {
        // Hexadecimal: digits, an optional point and fraction, an optional binary exponent.
        let mut j = 2;
        let mut value = 0f64;
        let mut digits = 0;
        while let Some(d) = rest.get(j).and_then(|c| (*c as char).to_digit(16)) {
            value = value * 16.0 + d as f64;
            j += 1;
            digits += 1;
        }
        let mut scale = 0i32;
        if rest.get(j) == Some(&b'.') {
            j += 1;
            while let Some(d) = rest.get(j).and_then(|c| (*c as char).to_digit(16)) {
                value = value * 16.0 + d as f64;
                scale -= 4;
                j += 1;
                digits += 1;
            }
        }
        if digits == 0 {
            // "0x" alone reads as 0 followed by text.
            return Some(sign(0.0));
        }
        if let Some(b'p' | b'P') = rest.get(j) {
            let mut k = j + 1;
            let mut eneg = false;
            if let Some(&c @ (b'+' | b'-')) = rest.get(k) {
                eneg = c == b'-';
                k += 1;
            }
            let estart = k;
            let mut e = 0i32;
            while let Some(c) = rest.get(k).filter(|c| c.is_ascii_digit()) {
                e = e.saturating_mul(10).saturating_add((c - b'0') as i32);
                k += 1;
            }
            if k > estart {
                scale += if eneg { -e } else { e };
            }
        }
        return Some(sign(value * 2f64.powi(scale)));
    }
    let mut j = 0;
    let mut digits = 0;
    while rest.get(j).is_some_and(u8::is_ascii_digit) {
        j += 1;
        digits += 1;
    }
    if rest.get(j) == Some(&b'.') {
        j += 1;
        while rest.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    let mut end = j;
    if let Some(b'e' | b'E') = rest.get(j) {
        let mut k = j + 1;
        if let Some(b'+' | b'-') = rest.get(k) {
            k += 1;
        }
        let estart = k;
        while rest.get(k).is_some_and(u8::is_ascii_digit) {
            k += 1;
        }
        if k > estart {
            end = k;
        }
    }
    let text = std::str::from_utf8(&rest[..end]).ok()?;
    let text = if text.starts_with('.') { format!("0{text}") } else { text.to_owned() };
    let text = text.replace(".e", ".0e").replace(".E", ".0E");
    let text = if text.ends_with('.') { format!("{text}0") } else { text };
    text.parse::<f64>().ok().map(sign)
}

/// -V: gnulib's filevercmp, as GNU sort uses it.
pub fn version_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (ae, be) = (a.is_empty(), b.is_empty());
    if ae {
        return if be { Ordering::Equal } else { Ordering::Less };
    }
    if be {
        return Ordering::Greater;
    }
    if a[0] == b'.' {
        if b[0] != b'.' {
            return Ordering::Less;
        }
        let (adot, bdot) = (a.len() == 1, b.len() == 1);
        if adot || bdot {
            return (!adot).cmp(&!bdot);
        }
        let adotdot = a[1] == b'.' && a.len() == 2;
        let bdotdot = b[1] == b'.' && b.len() == 2;
        if adotdot || bdotdot {
            return (!adotdot).cmp(&!bdotdot);
        }
    } else if b[0] == b'.' {
        return Ordering::Greater;
    }
    let ap = prefix_len(a);
    let bp = prefix_len(b);
    let one_pass = ap == a.len() && bp == b.len();
    let r = verrevcmp(&a[..ap], &b[..bp]);
    if r != 0 || one_pass {
        return r.cmp(&0);
    }
    verrevcmp(a, b).cmp(&0)
}

/// The length of `s` without a file suffix matching (\.[A-Za-z~][A-Za-z0-9~]*)*$.
fn prefix_len(s: &[u8]) -> usize {
    let n = s.len();
    let mut prefix = 0;
    let mut i = 0;
    loop {
        if i == n {
            return prefix;
        }
        i += 1;
        prefix = i;
        while i + 1 < n && s[i] == b'.' && (s[i + 1].is_ascii_alphabetic() || s[i + 1] == b'~') {
            i += 2;
            while i < n && (s[i].is_ascii_alphanumeric() || s[i] == b'~') {
                i += 1;
            }
        }
    }
}

fn ver_order(s: &[u8], pos: usize) -> i32 {
    if pos == s.len() {
        return -1;
    }
    let c = s[pos];
    if c.is_ascii_digit() {
        0
    } else if c.is_ascii_alphabetic() {
        c as i32
    } else if c == b'~' {
        -2
    } else {
        c as i32 + 256
    }
}

fn verrevcmp(s1: &[u8], s2: &[u8]) -> i32 {
    let (mut p1, mut p2) = (0, 0);
    while p1 < s1.len() || p2 < s2.len() {
        let mut first_diff = 0i32;
        while (p1 < s1.len() && !s1[p1].is_ascii_digit()) || (p2 < s2.len() && !s2[p2].is_ascii_digit()) {
            let c1 = ver_order(s1, p1);
            let c2 = ver_order(s2, p2);
            if c1 != c2 {
                return c1 - c2;
            }
            p1 += 1;
            p2 += 1;
        }
        while p1 < s1.len() && s1[p1] == b'0' {
            p1 += 1;
        }
        while p2 < s2.len() && s2[p2] == b'0' {
            p2 += 1;
        }
        while p1 < s1.len() && p2 < s2.len() && s1[p1].is_ascii_digit() && s2[p2].is_ascii_digit() {
            if first_diff == 0 {
                first_diff = s1[p1] as i32 - s2[p2] as i32;
            }
            p1 += 1;
            p2 += 1;
        }
        if p1 < s1.len() && s1[p1].is_ascii_digit() {
            return 1;
        }
        if p2 < s2.len() && s2[p2].is_ascii_digit() {
            return -1;
        }
        if first_diff != 0 {
            return first_diff;
        }
    }
    0
}

/// -R: by a salted hash of the key, so equal keys stay together; equal hashes by bytes.
fn random_cmp(a: &[u8], b: &[u8], salt: (u64, u64)) -> Ordering {
    let h = |s: &[u8]| -> u64 {
        // FNV-1a over the salt and the key, then a final mix.
        let mut x = 0xcbf29ce484222325u64 ^ salt.0;
        for &c in s {
            x ^= c as u64;
            x = x.wrapping_mul(0x100000001b3);
        }
        x ^= salt.1;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
        x
    };
    h(a).cmp(&h(b)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_compare_as_gnu_sort_does() {
        assert_eq!(numeric_cmp(b"10", b"9"), Ordering::Greater);
        assert_eq!(numeric_cmp(b"-3", b"2.5"), Ordering::Less);
        assert_eq!(numeric_cmp(b"-0", b"0.000"), Ordering::Equal);
        assert_eq!(numeric_cmp(b"abc", b"0"), Ordering::Equal);
        assert_eq!(numeric_cmp(b" 007.50", b"7.5"), Ordering::Equal);
        assert_eq!(numeric_cmp(b"-10", b"-9"), Ordering::Less);
        assert_eq!(human_cmp(b"2K", b"1M"), Ordering::Less);
        assert_eq!(human_cmp(b"1024", b"1K"), Ordering::Less);
        assert_eq!(strtod(b" 1e3x"), Some(1000.0));
        assert_eq!(strtod(b"0x10"), Some(16.0));
        assert_eq!(strtod(b"abc"), None);
        assert_eq!(strtod(b".5"), Some(0.5));
        assert_eq!(month(b" feb"), 2);
        assert_eq!(version_cmp(b"a2", b"a10"), Ordering::Less);
        assert_eq!(version_cmp(b"1.0~rc1", b"1.0"), Ordering::Less);
        assert_eq!(version_cmp(b"file.tar.gz", b"file2.tar.gz"), Ordering::Less);
    }

    #[test]
    fn keys_cover_what_gnu_sort_finds() {
        let k = Key::parse("2,2").unwrap();
        let o = Order::new(vec![k.clone()], Opts::default(), Some(b':'), false, false, (0, 0));
        assert_eq!(o.key_text(b"a:bb:c", &o.keys[0]), b"bb");
        let o = Order::new(vec![Key::parse("2").unwrap()], Opts::default(), None, false, false, (0, 0));
        assert_eq!(o.key_text(b"a  bb c", &o.keys[0]), b"  bb c");
        let o = Order::new(vec![Key::parse("1.2,1.3").unwrap()], Opts::default(), Some(b':'), false, false, (0, 0));
        assert_eq!(o.key_text(b"abcdef:x", &o.keys[0]), b"bc");
        assert!(Key::parse("0").is_err() && Key::parse("1.0").is_err() && Key::parse("1nV").is_err() && Key::parse("1q").is_err());
    }
}
