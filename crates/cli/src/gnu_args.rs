//! Options in the GNU style, for the commands that take the options of GNU's
//! file tools (split, cut, sort) and of BSD's look rather than ripgrep's:
//! `-abc` joins letters, `-l1000` and `-l 1000` give a value, `--name=value`
//! and `--name value` give a long one, `--` ends the options.

/// One option a command takes.
pub struct Spec {
    pub short: Option<char>,
    pub long: &'static str,
    /// Whether it takes a value: `Some(false)` an optional one (`--name[=value]`), `Some(true)` a required one.
    pub value: Option<bool>,
}

pub const fn flag(short: Option<char>, long: &'static str) -> Spec {
    Spec { short, long, value: None }
}

pub const fn valued(short: Option<char>, long: &'static str) -> Spec {
    Spec { short, long, value: Some(true) }
}

/// The options given, by long name and in order, and the other words.
pub struct Given {
    pub options: Vec<(&'static str, Option<String>)>,
    pub words: Vec<String>,
}

impl Given {
    pub fn has(&self, long: &str) -> bool {
        self.options.iter().any(|(l, _)| *l == long)
    }

    /// The last value given for `long`.
    pub fn last(&self, long: &str) -> Option<&str> {
        self.options.iter().rev().find(|(l, _)| *l == long).and_then(|(_, v)| v.as_deref())
    }
}

pub fn parse(command: &str, specs: &[Spec], args: &[String]) -> Result<Given, String> {
    let mut given = Given { options: Vec::new(), words: Vec::new() };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" {
            given.words.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (long, None),
            };
            // A unique beginning of a long name stands for it, as in GNU's tools.
            let exact = specs.iter().find(|s| s.long == name);
            let spec = match exact {
                Some(s) => s,
                None => {
                    let starts: Vec<&Spec> = specs.iter().filter(|s| s.long.starts_with(name)).collect();
                    match starts.as_slice() {
                        [one] => *one,
                        [] => return Err(format!("lw {command} has no option --{name}; lw help {command} lists them")),
                        _ => return Err(format!("--{name} could be any of {}", starts.iter().map(|s| format!("--{}", s.long)).collect::<Vec<_>>().join(", "))),
                    }
                }
            };
            let value = match (spec.value, inline) {
                (None, Some(_)) => return Err(format!("--{} takes no value", spec.long)),
                (None, None) => None,
                (Some(_), Some(v)) => Some(v),
                (Some(false), None) => None,
                (Some(true), None) => {
                    let v = args.get(i).ok_or(format!("--{} needs a value", spec.long))?;
                    i += 1;
                    Some(v.clone())
                }
            };
            given.options.push((spec.long, value));
            continue;
        }
        if arg.len() > 1 && arg.starts_with('-') {
            let letters: Vec<char> = arg[1..].chars().collect();
            let mut k = 0;
            while k < letters.len() {
                let c = letters[k];
                k += 1;
                let spec = specs.iter().find(|s| s.short == Some(c)).ok_or(format!("lw {command} has no option -{c}; lw help {command} lists them"))?;
                if spec.value == Some(true) {
                    let rest: String = letters[k..].iter().collect();
                    let value = if !rest.is_empty() {
                        rest
                    } else {
                        let v = args.get(i).ok_or(format!("-{c} needs a value"))?;
                        i += 1;
                        v.clone()
                    };
                    given.options.push((spec.long, Some(value)));
                    break;
                }
                // A letter whose long name takes an optional value (-d for --numeric-suffixes) gives none.
                given.options.push((spec.long, None));
            }
            continue;
        }
        given.words.push(arg.clone());
    }
    Ok(given)
}

/// A size as GNU's tools read it: digits, then an optional unit. K, M, G, T,
/// P (or KiB and the like) are powers of 1024; KB, MB, GB, TB, PB powers of
/// 1000; b is 512. Letter case of the unit's first letter does not matter.
pub fn size(text: &str, what: &str) -> Result<u64, String> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let (number, unit) = text.split_at(digits);
    let bad = || format!("{what}: {text:?} is not a size; give digits with an optional unit such as K, M, G (1024s) or KB, MB, GB (1000s)");
    if number.is_empty() {
        return Err(bad());
    }
    let n: u64 = number.parse().map_err(|_| bad())?;
    let factor: u64 = match unit {
        "" => 1,
        "b" => 512,
        _ => {
            let mut chars = unit.chars();
            let power = match chars.next().map(|c| c.to_ascii_uppercase()) {
                Some('K') => 1,
                Some('M') => 2,
                Some('G') => 3,
                Some('T') => 4,
                Some('P') => 5,
                Some('E') => 6,
                _ => return Err(bad()),
            };
            let base: u64 = match chars.as_str() {
                "" | "iB" => 1024,
                "B" => 1000,
                _ => return Err(bad()),
            };
            base.checked_pow(power).ok_or_else(bad)?
        }
    };
    n.checked_mul(factor).ok_or(format!("{what}: {text} is too large"))
}

/// A whole number.
pub fn number(text: &str, what: &str) -> Result<u64, String> {
    text.parse().map_err(|_| format!("{what}: {text:?} is not a whole number"))
}

/// The worker threads: `--threads` or `--parallel` when given, else the processors.
pub fn threads(given: &Given) -> Result<usize, String> {
    let named = given.last("threads").or(given.last("parallel"));
    let n = match named {
        Some(t) => number(t, "--threads")? as usize,
        None => 0,
    };
    Ok(if n > 0 { n } else { std::thread::available_parallelism().map_or(4, |n| n.get()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPECS: &[Spec] = &[
        flag(Some('u'), "unique"),
        flag(Some('r'), "reverse"),
        valued(Some('k'), "key"),
        valued(Some('t'), "field-separator"),
        Spec { short: None, long: "numeric-suffixes", value: Some(false) },
    ];

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn letters_join_and_values_attach() {
        let g = parse("sort", SPECS, &words(&["-ur", "-k2,2", "-t", ":", "in.txt", "--", "-u"])).unwrap();
        let names: Vec<&str> = g.options.iter().map(|(l, _)| *l).collect();
        assert_eq!(names, ["unique", "reverse", "key", "field-separator"]);
        assert_eq!(g.last("key"), Some("2,2"));
        assert_eq!(g.last("field-separator"), Some(":"));
        assert_eq!(g.words, ["in.txt", "-u"]);
        let g = parse("sort", SPECS, &words(&["--uniq", "--key=1", "--numeric-suffixes", "--numeric-suffixes=5"])).unwrap();
        assert!(g.has("unique"));
        assert_eq!(g.last("key"), Some("1"));
        assert_eq!(g.last("numeric-suffixes"), Some("5"));
        assert!(parse("sort", SPECS, &words(&["-q"])).is_err());
        assert!(parse("sort", SPECS, &words(&["--re"])).is_ok());
    }

    #[test]
    fn sizes_read_as_gnu_reads_them() {
        assert_eq!(size("10", "x").unwrap(), 10);
        assert_eq!(size("2K", "x").unwrap(), 2048);
        assert_eq!(size("2k", "x").unwrap(), 2048);
        assert_eq!(size("1MB", "x").unwrap(), 1_000_000);
        assert_eq!(size("1GiB", "x").unwrap(), 1 << 30);
        assert_eq!(size("3b", "x").unwrap(), 1536);
        assert!(size("1.5G", "x").is_err() && size("G", "x").is_err() && size("1Q", "x").is_err());
    }
}
