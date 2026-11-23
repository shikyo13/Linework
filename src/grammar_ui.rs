use crate::{
    app::{App, language_for},
    dialogs, state,
    win::alert,
};
impl App {
    pub(crate) fn load_grammars(&mut self) {
        let dir = state::profile().join("grammars");
        match linework_editor::load_user_grammars(&dir) {
            Ok(issues) if !issues.is_empty() => {
                self.notice(format!("User syntax: {}", issues.join(" · ")))
            }
            Err(e) => self.notice(format!("User syntax: {e}")),
            _ => {}
        }
        self.refresh_user_grammar_views();
    }
    pub(crate) fn import_grammar(&mut self) {
        let Some(file) = dialogs::files(self.h)
            .set_title("Import syntax grammar")
            .add_filter("Sublime Text syntax grammar", &["sublime-syntax"])
            .pick_file()
        else {
            return;
        };
        let name =
            match linework_editor::import_user_grammar(&state::profile().join("grammars"), &file) {
                Ok(s) => s,
                Err(e) => {
                    alert(self.h, "Grammar could not be imported", &e);
                    return;
                }
            };
        self.refresh_user_grammar_views();
        self.notice(format!("Imported {name} · available in Syntax language"));
    }
    fn refresh_user_grammar_views(&mut self) {
        let languages = linework_editor::languages();
        for i in 0..self.buffers.len() {
            let b = &self.buffers[i];
            let lang = if b.manual && languages.contains(&b.lang) {
                b.lang.clone()
            } else {
                let bytes = b.doc.read(0, 8192).unwrap_or_default();
                language_for(
                    &self.prefs,
                    b.path.as_deref(),
                    &String::from_utf8_lossy(&bytes),
                )
            };
            let id = b.id;
            self.buffers[i].lang = lang.clone();
            for p in 0..2 {
                if self.assigned[p] == Some(id)
                    && (self.editors[p].language() != lang || self.editors[p].is_custom_grammar())
                {
                    self.editors[p].set_language(&lang);
                    self.indentation(p, &lang);
                }
            }
        }
    }
}
