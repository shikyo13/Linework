use crate::{app::App, forms};
use linework_editor::UserSnippet;

impl App {
    pub(crate) fn apply_user_snippets(&self) {
        for e in &self.editors {
            e.set_user_snippets(&self.prefs.snippets);
        }
    }
    pub(crate) fn manage_snippets(&mut self) {
        let mut choices = vec!["Create a snippet…".to_owned()];
        choices.extend(self.prefs.snippets.iter().map(|s| {
            format!(
                "{}   [{}]",
                s.label(),
                if s.language == "*" {
                    "All languages"
                } else {
                    &s.language
                }
            )
        }));
        let Some(i) = forms::pick(self.h, "Manage snippets", &choices, "") else {
            return;
        };
        let existing = i.checked_sub(1);
        let mut draft = if let Some(i) = existing {
            let old = &self.prefs.snippets[i];
            let choices = vec!["Edit snippet…".into(), "Delete snippet…".into()];
            let Some(action) = forms::pick(self.h, &old.label(), &choices, "") else {
                return;
            };
            if action == 1 {
                let choices = vec!["Keep snippet".into(), format!("Delete {}", old.label())];
                if forms::pick(self.h, "Delete saved snippet?", &choices, "") == Some(1) {
                    self.prefs.snippets.remove(i);
                    self.apply_user_snippets();
                    self.save_prefs();
                    self.notice("Snippet deleted");
                }
                return;
            }
            old.clone()
        } else {
            UserSnippet {
                name: String::new(),
                trigger: String::new(),
                language: self.edit().language().into(),
                body: "${1:name}$0".into(),
            }
        };
        loop {
            let Some(edited) = forms::snippet_form(self.h, &draft) else {
                return;
            };
            draft = edited;
            let error = draft.validate().err().or_else(|| {
                self.prefs
                    .snippets
                    .iter()
                    .enumerate()
                    .any(|(j, s)| {
                        Some(j) != existing
                            && s.label() == draft.label()
                            && (s.language == "*"
                                || draft.language == "*"
                                || s.language == draft.language)
                    })
                    .then(|| {
                        "A snippet with this trigger and name already applies to this language."
                            .into()
                    })
            });
            if let Some(msg) = error {
                crate::win::alert(self.h, "Edit snippet", &msg);
                continue;
            }
            if let Some(i) = existing {
                self.prefs.snippets[i] = draft;
            } else {
                self.prefs.snippets.push(draft);
            }
            self.apply_user_snippets();
            self.save_prefs();
            self.notice("Snippet saved");
            return;
        }
    }
}
