use crate::{
    app::{App, Buffer},
    forms, state,
};
use linework_editor::{ColorRole, ColorScheme, HighlightOptions, MARK_COLORS, Theme, sci::*};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use windows_sys::Win32::UI::Controls::Dialogs::{
    CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW,
};

type Terms = [String; 5];
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    scheme: ColorScheme,
    presets: BTreeMap<String, ColorScheme>,
    options: HighlightOptions,
    swatches: [u32; 16],
    files: BTreeMap<String, Terms>,
    #[serde(skip)]
    untitled: BTreeMap<u64, Terms>,
}
impl Default for Appearance {
    fn default() -> Self {
        Self {
            scheme: Default::default(),
            presets: Default::default(),
            options: Default::default(),
            swatches: [0xffffff; 16],
            files: Default::default(),
            untitled: Default::default(),
        }
    }
}
impl Appearance {
    fn terms(&self, b: &Buffer) -> Terms {
        b.path
            .as_deref()
            .and_then(|p| self.files.get(&state::key(p)))
            .or_else(|| self.untitled.get(&b.id))
            .cloned()
            .unwrap_or_default()
    }
    fn store_terms(&mut self, b: &Buffer, terms: Terms) {
        let empty = terms.iter().all(String::is_empty);
        if let Some(path) = &b.path {
            let path = state::key(path);
            if empty {
                self.files.remove(&path);
            } else {
                self.files.insert(path, terms);
            }
            self.untitled.remove(&b.id);
        } else if empty {
            self.untitled.remove(&b.id);
        } else {
            self.untitled.insert(b.id, terms);
        }
    }
}
fn invert_rgb(v: u32) -> u32 {
    (v >> 16 & 255) | (v & 0xff00) | (v & 255) << 16
}
fn choose_color(owner: isize, rgb: u32, swatches: &mut [u32; 16]) -> Option<u32> {
    let mut dialog: CHOOSECOLORW = unsafe { std::mem::zeroed() };
    dialog.lStructSize = std::mem::size_of::<CHOOSECOLORW>() as u32;
    dialog.hwndOwner = owner as _;
    dialog.rgbResult = invert_rgb(rgb);
    dialog.lpCustColors = swatches.as_mut_ptr();
    dialog.Flags = CC_RGBINIT | CC_FULLOPEN;
    (unsafe { ChooseColorW(&mut dialog) } != 0).then(|| invert_rgb(dialog.rgbResult))
}

impl App {
    pub(crate) fn apply_appearance(&self) {
        for e in &self.editors {
            e.set_appearance(
                &self.prefs.appearance.scheme,
                HighlightOptions {
                    current_line: self.prefs.current_line,
                    ..self.prefs.appearance.options
                },
            );
            e.set_occurrence_highlight(self.prefs.highlight);
        }
        for p in 0..2 {
            self.effect_styles(p);
        }
    }
    pub(crate) fn sync_marks(&self, pane: usize) {
        let terms = self.assigned[pane]
            .and_then(|id| self.buffers.iter().find(|b| b.id == id))
            .map(|b| self.prefs.appearance.terms(b))
            .unwrap_or_default();
        self.editors[pane].set_mark_terms(&terms);
    }
    pub(crate) fn appearance_command(&mut self, id: &str) {
        match id {
            "tokencolors" => self.token_colors(),
            "highlights" => self.highlight_options(),
            "markcolor" | "clearmarks" | "nextcolor" => self.color_mark_command(id),
            _ => {}
        }
    }
    fn token_colors(&mut self) {
        let theme = if self.prefs.light {
            Theme::Light
        } else {
            Theme::Dark
        };
        let a = &self.prefs.appearance;
        let mut choices: Vec<_> = ColorRole::ALL
            .iter()
            .map(|&role| format!("{}    #{:06X}", role.label(), a.scheme.color(theme, role)))
            .collect();
        choices.extend(
            [
                "Save named color preset…",
                "Load named color preset…",
                "Reset this color theme to defaults",
            ]
            .map(str::to_owned),
        );
        let Some(index) = forms::pick(self.h, "Edit theme colors", &choices, "") else {
            return;
        };
        let a = &mut self.prefs.appearance;
        if let Some(&role) = ColorRole::ALL.get(index) {
            let Some(rgb) = choose_color(self.h, a.scheme.color(theme, role), &mut a.swatches)
            else {
                return;
            };
            a.scheme.set(theme, role, rgb);
        } else {
            match index - ColorRole::ALL.len() {
                0 => {
                    let Some(name) = forms::input(
                        self.h,
                        "Save color preset",
                        "Name for the current dark and light colors",
                        "",
                    ) else {
                        return;
                    };
                    let name = name.trim();
                    if name.is_empty() {
                        return;
                    }
                    if a.presets.contains_key(name) {
                        let options =
                            vec!["Keep the existing preset".into(), format!("Replace {name}")];
                        if forms::pick(self.h, "This preset already exists", &options, "")
                            != Some(1)
                        {
                            return;
                        }
                    }
                    a.presets.insert(name.to_owned(), a.scheme.clone());
                }
                1 => {
                    let names: Vec<_> = a.presets.keys().cloned().collect();
                    if names.is_empty() {
                        self.notice("Save a color preset first");
                        return;
                    }
                    let Some(index) = forms::pick(self.h, "Load color preset", &names, "") else {
                        return;
                    };
                    a.scheme = a.presets[&names[index]].clone();
                }
                _ => match theme {
                    Theme::Light => a.scheme.light.clear(),
                    Theme::Dark => a.scheme.dark.clear(),
                },
            }
        }
        self.apply_appearance();
        self.save_prefs();
        self.notice("Theme colors saved");
    }
    fn highlight_options(&mut self) {
        let old = self.prefs.appearance.options;
        let Some((_, check)) = forms::form(
            self.h,
            "Highlighting",
            &[],
            &[
                ("Color syntax tokens", old.syntax),
                ("Highlight the current line", self.prefs.current_line),
                ("Highlight matching braces and brackets", old.brace_pairs),
                (
                    "Highlight occurrences of the selected word",
                    self.prefs.highlight,
                ),
            ],
            "Apply",
        ) else {
            return;
        };
        self.prefs.appearance.options = HighlightOptions {
            syntax: check[0],
            current_line: check[1],
            brace_pairs: check[2],
        };
        self.prefs.current_line = check[1];
        self.prefs.highlight = check[3];
        self.apply_appearance();
        self.save_prefs();
        self.notice("Highlighting updated");
    }
    fn color_mark_command(&mut self, id: &str) {
        let Some(i) = self.current() else { return };
        let mut terms = self.prefs.appearance.terms(&self.buffers[i]);
        let slots: Vec<usize> = (0..5)
            .filter(|&j| id == "markcolor" || !terms[j].is_empty())
            .collect();
        if slots.is_empty() {
            self.notice("This document has no color marks");
            return;
        }
        let mut labels: Vec<String> = slots
            .iter()
            .map(|&j| {
                let name = MARK_COLORS[j].0;
                if terms[j].is_empty() {
                    name.into()
                } else {
                    format!("{name}    {}", terms[j].replace(['\r', '\n'], " "))
                }
            })
            .collect();
        if id == "clearmarks" {
            labels.push("Clear every color".into());
        }
        let title = match id {
            "markcolor" => "Mark occurrences with color",
            "nextcolor" => "Next occurrence of a color mark",
            _ => "Clear color marks",
        };
        let Some(index) = forms::pick(self.h, title, &labels, "") else {
            return;
        };
        let selected = slots.get(index).copied();
        match (id, selected) {
            ("markcolor", Some(slot)) => {
                let e = self.edit();
                let (a, b) = e.selection();
                let (a, b) = if a == b {
                    (
                        e.send(SCI_WORDSTARTPOSITION, b, 1) as usize,
                        e.send(SCI_WORDENDPOSITION, b, 1) as usize,
                    )
                } else {
                    (a.min(b), a.max(b))
                };
                if a == b || b - a > 512 {
                    self.notice("Select a word or phrase of up to 512 bytes");
                    return;
                }
                terms[slot] = e.text_range(a, b);
            }
            ("nextcolor", Some(slot)) => {
                self.find_marked_term(terms[slot].clone());
                return;
            }
            ("clearmarks", Some(slot)) => terms[slot].clear(),
            ("clearmarks", None) => terms = Default::default(),
            _ => return,
        }
        self.prefs.appearance.store_terms(&self.buffers[i], terms);
        for pane in 0..2 {
            if self.assigned[pane] == Some(self.buffers[i].id) {
                self.sync_marks(pane);
            }
        }
        self.save_prefs();
        self.notice("Color marks updated");
    }
}
