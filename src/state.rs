use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Step {
    Native {
        msg: u32,
        arg: usize,
        text: Option<String>,
    },
    Command(String),
    Find {
        query: String,
        replace: Option<String>,
        regex: bool,
        case: bool,
        whole: bool,
        back: bool,
        #[serde(default = "default_wrap")]
        wrap: bool,
        #[serde(default)]
        selection: bool,
    },
}
#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileState {
    pub path: Option<PathBuf>,
    pub name: String,
    pub offset: u64,
    pub language: String,
    pub marks: Vec<u64>,
    pub recovery: Option<String>,
}
/// How the Markdown preview points at the text that was just edited.
/// One settings key holds every choice, so a settings file written before
/// this existed loads with the defaults and a saved choice is never reset.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(default)]
pub struct PreviewCues {
    /// "pill", "beam", "underline", "block" or "off".
    pub style: String,
    /// Mark only the changed characters instead of the whole word.
    pub letters: bool,
    /// Also mark the edited line in the page margin.
    pub margin: bool,
    /// How far the surrounding text fades toward the page color, 0 to 45 percent.
    pub dim: u8,
    /// Show and hide the cue without movement.
    pub calm: bool,
    /// A plain click on rendered text moves the source caret there.
    pub click_source: bool,
}
impl PreviewCues {
    pub const STYLES: [&'static str; 5] = ["pill", "beam", "underline", "block", "off"];
    pub const MAX_DIM: u8 = 45;
    /// A hand-edited settings file may hold any text or number. Use the
    /// nearest valid choice instead of rejecting the whole file.
    pub fn checked(&self) -> Self {
        let style = self.style.trim().to_ascii_lowercase();
        Self {
            style: if Self::STYLES.contains(&style.as_str()) { style } else { "pill".into() },
            dim: self.dim.min(Self::MAX_DIM),
            ..self.clone()
        }
    }
}
impl Default for PreviewCues {
    fn default() -> Self {
        Self {
            style: "pill".into(),
            letters: false,
            margin: false,
            dim: 23,
            calm: false,
            click_source: true,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub sidebar_width: i32,
    pub document_map: bool,
    pub document_preview: bool,
    pub preview_width: i32,
    pub preview_cues: PreviewCues,
    pub stacked: bool,
    pub sync_scroll: bool,
    pub language_rules: BTreeMap<String, String>,
    pub snippets: Vec<linework_editor::UserSnippet>,
    pub appearance: crate::appearance::Appearance,
    pub tab_width: usize,
    pub use_tabs: Option<bool>,
    pub auto_indent: bool,
    pub auto_complete: bool,
    pub completion_threshold: usize,
    pub current_line: bool,
    pub dictionary: Option<PathBuf>,
    pub light: bool,
    pub wrap: bool,
    pub spell: bool,
    pub whitespace: bool,
    pub highlight: bool,
    pub caret: String,
    pub width: usize,
    pub blink: u32,
    pub zoom: i32,
    pub sidebar: bool,
    pub root: Option<PathBuf>,
    pub recent: Vec<PathBuf>,
    pub tabs: Vec<FileState>,
    pub active_pane: usize,
    pub view_positions: Option<[u64; 2]>,
    pub active: usize,
    pub second: Option<usize>,
    pub builds: BTreeMap<PathBuf, String>,
    pub shortcuts: BTreeMap<String, String>,
    pub macros: BTreeMap<String, Vec<Step>>,
    pub last_macro: Vec<Step>,
}
impl Default for Prefs {
    fn default() -> Self {
        Self {
            sidebar_width: 270,
            document_map: false,
            document_preview: false,
            preview_width: 0,
            preview_cues: Default::default(),
            stacked: false,
            sync_scroll: false,
            language_rules: BTreeMap::new(),
            snippets: vec![],
            appearance: Default::default(),
            tab_width: 4,
            use_tabs: None,
            auto_indent: true,
            auto_complete: true,
            completion_threshold: 3,
            current_line: true,
            dictionary: None,
            light: false,
            wrap: false,
            spell: false,
            whitespace: false,
            highlight: true,
            caret: "Line".into(),
            width: 2,
            blink: 550,
            zoom: 0,
            sidebar: true,
            root: None,
            recent: vec![],
            tabs: vec![],
            active_pane: 0,
            view_positions: None,
            active: 0,
            second: None,
            builds: BTreeMap::new(),
            shortcuts: BTreeMap::new(),
            macros: BTreeMap::new(),
            last_macro: vec![],
        }
    }
}
pub fn profile() -> PathBuf {
    std::env::var_os("LINEWORK_PROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("Linework")
        })
}
pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}
pub fn load_prefs() -> (Prefs, Option<String>) {
    let path = profile().join("settings.json");
    match fs::read(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Prefs::default(), None),
        Err(e) => (
            Prefs::default(),
            Some(format!("Could not read settings: {e}")),
        ),
        Ok(raw) => match serde_json::from_slice::<Prefs>(&raw) {
            Ok(p) => (p, None),
            Err(e) => {
                let mut p = read::<Prefs>(&path.with_extension("bak")).unwrap_or_default();
                // Recover individual valid preferences even if a single value is malformed.
                if let Ok(serde_json::Value::Object(fields)) = serde_json::from_slice(&raw) {
                    let mut value = serde_json::to_value(&p).unwrap();
                    for (key, field) in fields {
                        let mut candidate = value.clone();
                        candidate[&key] = field;
                        if let Ok(valid) = serde_json::from_value(candidate.clone()) {
                            p = valid;
                            value = candidate;
                        }
                    }
                }
                (
                    p,
                    Some(format!(
                        "Recovered readable settings. The damaged settings file will be preserved on the next write. {e}"
                    )),
                )
            }
        },
    }
}
fn merge_changes(
    base: &serde_json::Value,
    local: &serde_json::Value,
    disk: &mut serde_json::Value,
) {
    if local == base {
        return;
    }
    if let (Some(b), Some(l), Some(d)) = (base.as_object(), local.as_object(), disk.as_object_mut())
    {
        for k in b.keys().filter(|k| !l.contains_key(*k)) {
            d.remove(k);
        }
        for (k, v) in l {
            if b.get(k) != Some(v) {
                merge_changes(
                    b.get(k).unwrap_or(&serde_json::Value::Null),
                    v,
                    d.entry(k.clone()).or_insert(serde_json::Value::Null),
                );
            }
        }
    } else {
        *disk = local.clone();
    }
}
pub fn store<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Expected a settings or manifest filename")
    })?;
    fs::create_dir_all(parent)?;
    // Canonicalize the directory, since a first checkpoint has no destination file.
    // MoveFileExW needs Windows' extended path form for long profile paths.
    let path = fs::canonicalize(parent)?.join(name);
    let bytes = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    use std::io::Write;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    atomic_move(&tmp, &path)
}
pub fn atomic_move(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let a: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            a.as_ptr(),
            b.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
pub fn key(p: &Path) -> String {
    linework_project::path_key(p)
}
pub fn size(n: u64) -> String {
    let mut v = n as f64;
    let mut i = 0;
    let u = ["B", "KiB", "MiB", "GiB", "TiB"];
    while v >= 1024. && i < 4 {
        v /= 1024.;
        i += 1
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", u[i])
    }
}
pub fn pretty(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',')
        }
        out.push(c)
    }
    out
}

fn default_wrap() -> bool {
    true
}

/// Serialize the settings merge between windows. A preference change must not
/// revert another window's persisted session, and a closing window only owns
/// drafts it restored itself.
pub fn save_settings(
    prefs: &Prefs,
    baseline: &Prefs,
    final_save: bool,
    claimed: Option<&std::collections::HashSet<String>>,
) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = profile();
    fs::create_dir_all(&dir)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let _lock = loop {
        match fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .share_mode(0)
            .open(dir.join("settings.lock"))
        {
            Ok(lock) => break lock,
            Err(e) if e.raw_os_error() == Some(32) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(12))
            }
            Err(e) => return Err(e),
        }
    };
    let path = dir.join("settings.json");
    let mut next = prefs.clone();
    if let Some(previous) = read::<Prefs>(&path) {
        let mut merged = serde_json::to_value(&previous)?;
        merge_changes(
            &serde_json::to_value(baseline)?,
            &serde_json::to_value(prefs)?,
            &mut merged,
        );
        next = serde_json::from_value(merged)?;
        if let Some(claimed) = claimed.filter(|_| final_save) {
            next.tabs = prefs.tabs.clone();
            next.active = prefs.active;
            next.active_pane = prefs.active_pane;
            next.second = prefs.second;
            next.view_positions = prefs.view_positions;
            let known: std::collections::HashSet<_> = next
                .tabs
                .iter()
                .filter_map(|s| s.recovery.clone())
                .collect();
            next.tabs.extend(previous.tabs.into_iter().filter(|s| {
                s.recovery
                    .as_ref()
                    .is_some_and(|r| !claimed.contains(r) && !known.contains(r))
            }));
        } else {
            next.tabs = previous.tabs;
            if let Some(claimed) = claimed {
                next.tabs
                    .retain(|s| s.recovery.as_ref().is_none_or(|r| !claimed.contains(r)));
            }
            next.active = previous.active;
            next.active_pane = previous.active_pane;
            next.second = previous.second;
            next.view_positions = previous.view_positions;
        }
    }
    if path.try_exists()? {
        let data = fs::read(&path)?;
        if serde_json::from_slice::<Prefs>(&data).is_ok() {
            // A settings backup must not resurrect intentionally closed drafts.
            let mut backup: Prefs = serde_json::from_slice(&data)?;
            backup.tabs = next.tabs.clone();
            store(&path.with_extension("bak"), &backup)?;
        } else {
            let mut backup = tempfile::Builder::new()
                .prefix("settings-damaged-")
                .suffix(".json")
                .tempfile_in(&dir)?;
            std::io::Write::write_all(&mut backup, &data)?;
            backup.as_file().sync_all()?;
            backup.keep().map_err(|e| e.error)?;
        }
    }
    store(&path, &next)
}
