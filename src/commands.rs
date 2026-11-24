#[derive(Clone, Copy)]
pub struct Command {
    pub id: &'static str,
    pub name: &'static str,
    pub group: &'static str,
    pub key: &'static str,
}
macro_rules! cmds {($(($id:literal,$name:literal,$group:literal,$key:literal)),* $(,)?)=>{&[$(Command{id:$id,name:$name,group:$group,key:$key}),*]};}
pub const COMMANDS: &[Command] = cmds![
    ("new", "New file", "File", "Ctrl+N"),
    ("open", "Open file…", "File", "Ctrl+O"),
    ("folder", "Open folder…", "File", "Ctrl+Shift+O"),
    ("save", "Save", "File", "Ctrl+S"),
    ("saveas", "Save as…", "File", "Ctrl+Shift+S"),
    (
        "comparesaved",
        "Compare changes with saved file",
        "File",
        ""
    ),
    ("comparefile", "Compare with another file…", "File", ""),
    ("saveall", "Save all", "File", "Ctrl+Shift+F12"),
    ("close", "Close tab", "File", "Ctrl+W"),
    ("reopen", "Reopen closed file", "File", "Ctrl+Shift+T"),
    ("encoding", "Convert encoding…", "File", ""),
    ("reload", "Reload from disk…", "File", ""),
    ("recode", "Reopen with encoding…", "File", ""),
    ("encodingdetect", "Detect encoding of files that are not UTF-8", "File", ""),
    ("encodingfallback", "Encoding for files that are not UTF-8…", "File", ""),
    ("encodingnew", "Encoding for new files…", "File", ""),
    ("eol", "Convert line endings…", "File", ""),
    ("follow", "Follow growing file", "File", ""),
    ("exit", "Exit Linework", "File", "Alt+F4"),
    ("undo", "Undo", "Edit", "Ctrl+Z"),
    ("redo", "Redo", "Edit", "Ctrl+Y"),
    ("cut", "Cut", "Edit", "Ctrl+X"),
    ("copy", "Copy", "Edit", "Ctrl+C"),
    ("paste", "Paste", "Edit", "Ctrl+V"),
    ("dup", "Duplicate selection or line", "Edit", "Ctrl+Shift+D"),
    ("moveup", "Move selected lines up", "Edit", "Alt+Up"),
    ("movedown", "Move selected lines down", "Edit", "Alt+Down"),
    ("upper", "Uppercase selections", "Edit", ""),
    ("lower", "Lowercase selections", "Edit", ""),
    (
        "columnfill",
        "Fill selections with a number series…",
        "Edit",
        "Alt+C"
    ),
    ("complete", "Show completions", "Edit", "Ctrl+Space"),
    ("snippets", "Insert snippet…", "Edit", ""),
    ("spell", "Check spelling as you type", "Edit", "F6"),
    ("correct", "Spelling suggestions…", "Edit", "Ctrl+."),
    ("selall", "Select all", "Selection", "Ctrl+A"),
    ("selnext", "Select next occurrence", "Selection", "Ctrl+D"),
    (
        "selmatches",
        "Select all occurrences",
        "Selection",
        "Ctrl+Shift+L"
    ),
    (
        "selskipped",
        "Skip to next occurrence",
        "Selection",
        "Ctrl+K"
    ),
    ("undoselect", "Undo last selection", "Selection", "Ctrl+U"),
    (
        "sellines",
        "Split selection into lines",
        "Selection",
        "Alt+Shift+L"
    ),
    ("caretup", "Add caret above", "Selection", "Ctrl+Alt+Up"),
    ("caretdown", "Add caret below", "Selection", "Ctrl+Alt+Down"),
    (
        "rectangle",
        "Rectangular selection",
        "Selection",
        "Alt+Shift+R"
    ),
    ("find", "Find in file", "Search", "Ctrl+F"),
    ("replace", "Replace in file", "Search", "Ctrl+H"),
    ("findnext", "Next match", "Search", "F3"),
    ("findprev", "Previous match", "Search", "Shift+F3"),
    ("findproject", "Find in folder", "Search", "Ctrl+Shift+F"),
    (
        "replaceproject",
        "Preview replacements in folder",
        "Search",
        "Ctrl+Shift+H"
    ),
    ("applypreview", "Apply checked replacements", "Search", ""),
    (
        "togglehit",
        "Include or exclude selected match",
        "Search",
        ""
    ),
    ("goto", "Go to line, byte or percentage…", "Go", "Ctrl+G"),
    ("opentabs", "Switch open editor…", "Go", ""),
    ("quick", "Quick open…", "Go", "Ctrl+P"),
    ("symbols", "Symbols in file", "Go", "Ctrl+R"),
    ("wsymbols", "Symbols in workspace…", "Go", "Ctrl+Shift+R"),
    ("def", "Go to definition", "Go", "F12"),
    ("refs", "Find references", "Go", "Shift+F12"),
    ("back", "Go back", "Go", "Alt+Left"),
    ("forward", "Go forward", "Go", "Alt+Right"),
    ("bookmark", "Toggle bookmark", "Go", "Ctrl+F2"),
    ("nextmark", "Next bookmark", "Go", "F2"),
    ("marks", "List bookmarks…", "Go", ""),
    ("palette", "Command palette…", "View", "Ctrl+Shift+P"),
    ("sidebar", "Toggle sidebar", "View", "Ctrl+B"),
    ("split", "Split editor", "View", "Ctrl+Alt+2"),
    ("single", "Single editor", "View", "Ctrl+Alt+1"),
    ("other", "Focus other pane", "View", "F8"),
    (
        "openother",
        "Open file in other pane…",
        "View",
        "Alt+Shift+O"
    ),
    ("nexttab", "Next tab", "View", "Ctrl+Tab"),
    ("prevtab", "Previous tab", "View", "Ctrl+Shift+Tab"),
    ("wrap", "Toggle word wrap", "View", "Alt+Z"),
    ("fold", "Fold or unfold block", "View", "Ctrl+Shift+["),
    ("foldall", "Fold all blocks", "View", ""),
    ("unfold", "Unfold all blocks", "View", ""),
    ("theme", "Color theme…", "View", ""),
    ("language", "Syntax language…", "View", ""),
    ("cursor", "Cursor appearance…", "View", ""),
    ("whitespace", "Show whitespace", "View", ""),
    ("markmatches", "Highlight selected occurrences", "View", ""),
    ("zoom+", "Increase font size", "View", "Ctrl+="),
    ("zoom-", "Decrease font size", "View", "Ctrl+-"),
    ("zoom0", "Reset font size", "View", "Ctrl+0"),
    ("findopen", "Find in open tabs…", "Search", ""),
    (
        "replaceopen",
        "Preview replacements in open tabs…",
        "Search",
        ""
    ),
    ("findselection", "Find in selection…", "Search", "Ctrl+F3"),
    ("findoptions", "Find options…", "Search", ""),
    ("tokencolors", "Syntax colors…", "View", ""),
    ("highlights", "Highlighting options…", "View", ""),
    ("markcolor", "Mark phrase with a color…", "Search", ""),
    ("nextcolor", "Go to next colored mark…", "Go", ""),
    ("clearmarks", "Clear colored marks…", "Search", ""),
    ("managesnippets", "Manage snippets…", "Edit", ""),
    ("minimap", "Document map", "View", ""),
    ("docpreview", "Show document preview to the right", "View", "Ctrl+Shift+V"),
    ("previewcue", "Preview effects…", "View", ""),
    ("previewfollow", "Preview follows the source caret", "View", ""),
    ("editoreffects", "Editor effects…", "View", ""),
    ("paragraphfocus", "Paragraph focus", "View", ""),
    ("mdbold", "Markdown: bold", "Edit", ""),
    ("mditalic", "Markdown: italic", "Edit", ""),
    ("previewshow", "Show the latest edit in the preview", "View", ""),
    ("previewsync", "Show the caret position in the preview", "View", ""),
    (
        "previewsource",
        "Go to the source of the preview's reading position",
        "View",
        ""
    ),
    ("splitbelow", "Stack editors vertically", "View", ""),
    ("syncscroll", "Link scrolling between panes", "View", ""),
    ("langrules", "Filename and language rules…", "View", ""),
    ("managemacros", "Manage macros…", "Macro", ""),
    (
        "comment",
        "Toggle comment on selected lines",
        "Edit",
        "Ctrl+/"
    ),
    ("indent", "Indent selection", "Edit", ""),
    ("outdent", "Outdent selection", "Edit", ""),
    ("sortlines", "Sort lines ascending", "Edit", ""),
    ("sortdesc", "Sort lines descending", "Edit", ""),
    ("dedup", "Remove duplicate lines", "Edit", ""),
    ("trimlines", "Trim trailing spaces", "Edit", ""),
    (
        "importgrammar",
        "Import Sublime syntax grammar…",
        "View",
        ""
    ),
    ("typing", "Typing and completion preferences…", "Edit", ""),
    ("dictionary", "Spelling dictionary…", "Edit", ""),
    ("currentline", "Highlight current line", "View", ""),
    (
        "toggleall",
        "Include or exclude all preview matches",
        "Search",
        ""
    ),
    ("previewhit", "Inspect selected replacement…", "Search", ""),
    (
        "record",
        "Start or stop macro recording",
        "Macro",
        "Ctrl+Shift+M"
    ),
    ("play", "Replay macro", "Macro", "F9"),
    ("repeat", "Repeat macro…", "Macro", ""),
    ("savemacro", "Save named macro…", "Macro", ""),
    ("loadmacro", "Load named macro…", "Macro", ""),
    ("build", "Run build", "Tools", "F7"),
    ("buildconfig", "Set build command…", "Tools", ""),
    ("nppinstall", "Install Notepad++ plugin…", "Tools", ""),
    ("nppcommands", "Notepad++ plugin commands…", "Tools", ""),
    ("sublimeinstall", "Install Sublime package…", "Tools", ""),
    ("sublimecommands", "Sublime package commands…", "Tools", ""),
    ("sublimepackages", "Manage Sublime packages…", "Tools", ""),
    ("nextdiag", "Next diagnostic", "Tools", "F4"),
    ("tools", "Run external tool…", "Tools", ""),
    ("toolconfig", "Configure external tools…", "Tools", ""),
    ("bindings", "Change keyboard shortcut…", "Tools", ""),
    ("cancel", "Cancel background operation", "Tools", ""),
    ("help", "Keyboard shortcuts…", "Help", "F1")
];
pub fn label(id: &str) -> &str {
    COMMANDS
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.name)
        .unwrap_or(id)
}
pub fn shortcut_index(overrides: &std::collections::BTreeMap<String, String>)
    -> std::collections::HashMap<String, &'static Command>
{
    let mut keys = std::collections::HashMap::new();
    for c in COMMANDS {
        let key = overrides.get(c.id).map(String::as_str).unwrap_or(c.key);
        if !key.is_empty() {
            // Preserve the first binding if an imported settings file has duplicates.
            keys.entry(key.to_ascii_lowercase()).or_insert(c);
        }
    }
    keys
}
pub fn chord(vk: u32, ctrl: bool, shift: bool, alt: bool) -> String {
    let key = match vk {
        0x41..=0x5a | 0x30..=0x39 => (vk as u8 as char).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6f),
        0x20 => "Space".into(),
        9 => "Tab".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0xbf => "/".into(),
        0xbc => ",".into(),
        0xbe => ".".into(),
        0xbb | 0x6b => "=".into(),
        0xbd | 0x6d => "-".into(),
        0xdb => "[".into(),
        0xdd => "]".into(),
        _ => return String::new(),
    };
    format!(
        "{}{}{}{}",
        if ctrl { "Ctrl+" } else { "" },
        if alt { "Alt+" } else { "" },
        if shift { "Shift+" } else { "" },
        key
    )
}

pub fn normalize_chord(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        return Some(String::new());
    }
    let mut parts: Vec<_> = value.split('+').map(str::trim).collect();
    let key = parts.pop()?;
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    for modifier in parts {
        let flag = match modifier.to_ascii_lowercase().as_str() {
            "ctrl" => &mut ctrl,
            "alt" => &mut alt,
            "shift" => &mut shift,
            _ => return None,
        };
        if *flag {
            return None;
        }
        *flag = true;
    }
    (0..=255)
        .find(|vk| !key.is_empty() && chord(*vk, false, false, false).eq_ignore_ascii_case(key))
        .map(|vk| chord(vk, ctrl, shift, alt))
}
