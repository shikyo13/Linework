use super::{Editor, sci::*};
use spellbook::Dictionary;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

static DICTIONARY: OnceLock<Result<Arc<Dictionary>, String>> = OnceLock::new();
static LEARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
fn dictionary() -> Result<Arc<Dictionary>, String> {
    DICTIONARY
        .get_or_init(|| {
            Dictionary::new(
                include_str!("../assets/en_US.aff"),
                include_str!("../assets/en_US.dic"),
            )
            .map(Arc::new)
            .map_err(|e| format!("English dictionary could not load: {e:?}"))
        })
        .clone()
}
fn user_words_path() -> Option<std::path::PathBuf> {
    let folder = std::env::var_os("LINEWORK_PROFILE")
        .or_else(|| std::env::var_os("LINEWORK_STATE_DIRECTORY"))
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA").map(|p| std::path::PathBuf::from(p).join("Linework"))
        })?;
    Some(folder.join("words.txt"))
}
fn learned() -> &'static Mutex<HashSet<String>> {
    LEARNED.get_or_init(|| {
        Mutex::new(
            user_words_path()
                .and_then(|p| fs::read_to_string(p).ok())
                .unwrap_or_default()
                .lines()
                .map(str::to_lowercase)
                .collect(),
        )
    })
}
#[derive(Default)]
pub(super) struct Spelling {
    enabled: bool,
    revision: u64,
    shown: Option<(usize, usize, u64)>,
    ignored: HashSet<String>,
    custom_dictionary: Option<Arc<Dictionary>>,
}
impl Spelling {
    pub fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}
fn spans(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    std::iter::from_fn(move || {
        let word_start = text[offset..]
            .char_indices()
            .find(|(_, c)| c.is_alphabetic())?
            .0
            + offset;
        let mut end = word_start;
        for (i, c) in text[word_start..].char_indices() {
            if !(c.is_alphabetic() || c == '\'' || c == '’') {
                break;
            }
            end = word_start + i + c.len_utf8();
        }
        offset = end;
        let word = text[word_start..end].trim_end_matches(['\'', '’']);
        Some((word_start, word))
    })
}
impl Editor {
    fn current_dictionary(&self) -> Result<Arc<Dictionary>, String> {
        self.spelling
            .borrow()
            .custom_dictionary
            .as_ref()
            .cloned()
            .map_or_else(dictionary, Ok)
    }
    /// Load a UTF-8 Hunspell pair. Selection commits only after both files parse.
    pub fn set_dictionary(&self, path: Option<&std::path::Path>) -> Result<(), String> {
        let next = if let Some(path) = path {
            let aff = fs::read_to_string(path.with_extension("aff"))
                .map_err(|e| format!("Could not read companion .aff file: {e}"))?;
            let words = fs::read_to_string(path)
                .map_err(|e| format!("Could not read .dic word list: {e}"))?;
            Some(Arc::new(Dictionary::new(&aff, &words).map_err(|e| {
                format!("Invalid Hunspell dictionary: {e:?}")
            })?))
        } else {
            None
        };
        {
            let mut sp = self.spelling.borrow_mut();
            sp.custom_dictionary = next;
            sp.ignored.clear();
            sp.invalidate();
        }
        self.refresh_spelling();
        Ok(())
    }
    pub fn set_spellcheck(&self, on: bool) -> Result<(), String> {
        if on {
            self.current_dictionary()?;
        }
        {
            let mut sp = self.spelling.borrow_mut();
            sp.enabled = on;
            sp.shown = None;
        }
        if on {
            self.refresh_spelling();
        } else {
            self.send(SCI_SETINDICATORCURRENT, 8, 0);
            self.send(SCI_INDICATORCLEARRANGE, 0, self.len() as isize);
        }
        Ok(())
    }
    pub fn spellcheck_enabled(&self) -> bool {
        self.spelling.borrow().enabled
    }
    /// Called on idle after edits or scrolling. Only visible prose/comment
    /// bytes are read. A giant unbroken line is sampled around the caret.
    pub fn refresh_spelling(&self) {
        let (start, mut end) = self.visible_range();
        let start = if end.saturating_sub(start) > 32768 && (start..end).contains(&self.caret()) {
            self.caret().saturating_sub(8192).max(start)
        } else {
            start
        };
        end = end.min(start.saturating_add(32768));
        let rev = {
            let sp = self.spelling.borrow();
            if !sp.enabled || sp.shown == Some((start, end, sp.revision)) {
                return;
            }
            sp.revision
        };
        let Ok(dict) = self.current_dictionary() else {
            return;
        };
        self.send(SCI_COLOURISE, start, end as isize);
        let styled = self.bytes(start, end, true);
        let accepted = self.prose_styles.get();
        let raw: Vec<u8> = styled
            .chunks_exact(2)
            .map(|pair| {
                if accepted[pair[1] as usize] {
                    pair[0]
                } else {
                    b' '
                }
            })
            .collect();
        // A viewport boundary can bisect a Unicode scalar. Replacing invalid
        // bytes by spaces keeps all indicator offsets in document coordinates.
        let mut raw = raw;
        while let Err(e) = std::str::from_utf8(&raw) {
            let i = e.valid_up_to();
            let count = e.error_len().unwrap_or(raw.len() - i);
            raw[i..i + count].fill(b' ');
        }
        let text = std::str::from_utf8(&raw).expect("validated UTF-8");
        let ranges = {
            let sp = self.spelling.borrow();
            let known = learned().lock().unwrap_or_else(|p| p.into_inner());
            spans(text)
                .filter(|(_, w)| {
                    w.len() >= 3 && w.len() <= 64 && !w.chars().all(|c| c.is_uppercase())
                })
                .filter(|(_, word)| {
                    !sp.ignored.contains(&word.to_lowercase())
                        && !known.contains(&word.to_lowercase())
                        && !dict.check(word)
                })
                .take(500)
                .map(|(pos, word)| (start + pos, word.len()))
                .collect::<Vec<_>>()
        };
        self.spelling.borrow_mut().shown = Some((start, end, rev));
        self.send(SCI_SETINDICATORCURRENT, 8, 0);
        self.send(SCI_INDICATORCLEARRANGE, 0, self.len() as isize);
        for (p, n) in ranges {
            self.send(SCI_INDICATORFILLRANGE, p, n as isize);
        }
    }
    fn spelling_target(&self) -> Option<(usize, usize, String)> {
        let pos = self.caret();
        let anchor = pos.saturating_sub(256);
        let bytes = self.bytes(anchor, (pos + 256).min(self.len()), false);
        let first = (0..bytes.len())
            .take(4)
            .find(|i| bytes[*i] & 0xc0 != 0x80)
            .unwrap_or(0);
        let rest = &bytes[first..];
        let valid = std::str::from_utf8(rest).map_or_else(|e| e.valid_up_to(), |s| s.len());
        let text = std::str::from_utf8(&rest[..valid]).ok()?;
        for (i, word) in spans(text) {
            let a = anchor + first + i;
            let b = a + word.len();
            if a <= pos && pos <= b && word.len() <= 64 {
                return Some((a, b, word.to_owned()));
            }
        }
        None
    }
    pub fn spelling_suggestions(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some((a, _, word)) = self.spelling_target() {
            if !self.prose_styles.get()[self.send(SCI_GETSTYLEAT, a, 0).clamp(0, 255) as usize] {
                return out;
            }
            if let Ok(dict) = self.current_dictionary() {
                if !dict.check(&word) {
                    dict.suggest(&word, &mut out);
                    out.truncate(8);
                }
            }
        }
        out
    }
    pub fn replace_spelling(&self, suggestion: &str) -> bool {
        if let Some((a, b, _)) = self.spelling_target() {
            self.replace_range(a, b, suggestion);
            self.set_selection(a, a + suggestion.len());
            return true;
        }
        false
    }
    pub fn learn_word(&self) -> bool {
        let Some((_, _, word)) = self.spelling_target() else {
            return false;
        };
        let Some(path) = user_words_path() else {
            return false;
        };
        let mut words = learned().lock().unwrap_or_else(|p| p.into_inner());
        if !words.contains(&word.to_lowercase()) {
            let result = (|| -> std::io::Result<()> {
                fs::create_dir_all(path.parent().unwrap())?;
                let mut f = OpenOptions::new().create(true).append(true).open(path)?;
                writeln!(f, "{word}")
            })();
            if result.is_err() {
                return false;
            }
            words.insert(word.to_lowercase());
        }
        drop(words);
        self.spelling.borrow_mut().invalidate();
        self.refresh_spelling();
        true
    }
    pub fn ignore_word(&self) -> bool {
        let Some((_, _, word)) = self.spelling_target() else {
            return false;
        };
        {
            let mut sp = self.spelling.borrow_mut();
            sp.ignored.insert(word.to_lowercase());
            sp.invalidate();
        }
        self.refresh_spelling();
        true
    }
}
