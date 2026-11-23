use super::*;
use crate::npp;

fn copy_package(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    for entry in src.read_dir()? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(io::Error::other(
                "Plugin packages cannot include reparse points",
            ));
        }
        let to = dst.join(entry.file_name());
        if meta.is_dir() {
            fs::create_dir(&to)?;
            copy_package(&path, &to)?
        } else if meta.is_file() {
            fs::copy(path, to)?;
        }
    }
    Ok(())
}
impl App {
    pub(crate) fn npp_style_view(&self, p: usize) {
        crate::chrome::theme_child(self.editors[p].hwnd(), self.prefs.light);
        if let Some(path) = self.prefs.dictionary.as_deref() {
            let _ = self.editors[p].set_dictionary(Some(path));
        }
        self.style(p);
        self.apply_appearance();
        self.apply_user_snippets();
    }
    pub(crate) fn npp_manage(&mut self, cmd: &str) {
        if cmd == "nppinstall" {
            let Some(file) = crate::dialogs::files(self.h)
                .set_title("Install a trusted x64 Notepad++ plugin")
                .add_filter("Notepad++ plugin", &["dll"])
                .pick_file()
            else {
                return;
            };
            match self.npp_install(&file) {
                Ok(()) => self.notice("Plugin installed. Its commands are available in Tools."),
                Err(e) => alert(self.h, "Install Notepad++ plugin", &e),
            }
        } else {
            let rows = npp::rows();
            if rows.is_empty() {
                self.notice("Use Tools → Install Notepad++ plugin to add a plugin DLL.");
                return;
            }
            if let Some(i) = forms::pick(
                self.h,
                "Notepad++ plugin commands",
                &rows.iter().map(|r| r.1.clone()).collect::<Vec<_>>(),
                "",
            ) {
                npp::invoke(self, rows[i].0);
            }
        }
    }
    pub(super) fn npp_install(&mut self, src: &Path) -> Result<(), String> {
        let stem = src.file_stem().ok_or("Missing DLL name")?;
        let file = src.file_name().ok_or("Missing DLL name")?;
        let dest = npp::home().join(stem);
        if dest.exists() && !dest.join(".linework-disabled").is_file() {
            return Err(format!(
                "{} already exists. To upgrade it, close Linework and replace the plugin folder.",
                dest.display()
            ));
        }
        let stage = tempfile::tempdir_in(npp::home()).map_err(|e| e.to_string())?;
        let package = stage.path().join("package");
        fs::create_dir(&package).map_err(|e| e.to_string())?;
        if let Some(parent) = src
            .parent()
            .filter(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(stem)))
        {
            copy_package(parent, &package).map_err(|e| e.to_string())?;
        } else {
            fs::copy(src, package.join(file)).map_err(|e| e.to_string())?;
        }
        // The marker prevents startup loading an incomplete installation.
        fs::write(package.join(".linework-disabled"), b"").map_err(|e| e.to_string())?;
        // Preserve the files of a previously failed install, including user changes.
        let previous = if dest.exists() {
            let archive = state::profile().join("Removed Plugins");
            fs::create_dir_all(&archive).map_err(|e| e.to_string())?;
            let backup = tempfile::tempdir_in(&archive)
                .map_err(|e| e.to_string())?
                .keep()
                .join(stem);
            fs::rename(&dest, &backup).map_err(|e| e.to_string())?;
            Some(backup)
        } else {
            None
        };
        let restore_previous = |error: String| -> String {
            if let Some(old) = &previous
                && let Err(e) = fs::rename(old, &dest)
            {
                return format!("{error}. Previous files remain in {}: {e}", old.display());
            }
            error
        };
        fs::rename(package, &dest).map_err(|e| restore_previous(e.to_string()))?;
        if let Err(error) = npp::attach_new(self, &dest.join(file)) {
            fs::rename(&dest, stage.path().join("failed"))
                .map_err(|e| format!("{error}. Could not roll back installation: {e}"))?;
            return Err(restore_previous(error));
        }
        fs::remove_file(dest.join(".linework-disabled")).map_err(|e| e.to_string())
    }
    pub(crate) fn npp_open(&mut self, path: PathBuf, open: bool) -> bool {
        let Ok(path) = std::path::absolute(path) else {
            return false;
        };
        let key = state::key(&path);
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| b.path.as_ref().is_some_and(|p| state::key(p) == key))
        {
            self.select(i, self.active, None);
            return true;
        }
        if !open
            || self.pending_open(&path)
            || self.commit_paths.iter().any(|p| state::key(p) == key)
        {
            return false;
        }
        let Ok(doc) = Document::open(&path) else {
            return false;
        };
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let n = self.buffers.len();
        self.add(doc, Some(path), name, self.active, None);
        self.buffers.len() > n
    }
    pub(crate) fn npp_language(&mut self, id: u64, name: &str) -> bool {
        let Some(i) = self.idx(id) else { return false };
        let Some(name) = linework_editor::languages()
            .into_iter()
            .find(|s| s.eq_ignore_ascii_case(name))
        else {
            return false;
        };
        self.buffers[i].lang = name.clone();
        self.buffers[i].manual = true;
        for p in 0..2 {
            if self.assigned[p] == Some(id) {
                self.editors[p].set_language(&name);
                self.indentation(p, &name);
            }
        }
        self.saved_state = false;
        true
    }
    pub(crate) fn npp_save(&mut self, id: u64, path: Option<PathBuf>, copy_only: bool) -> bool {
        self.dispatch_pending();
        if !npp::checkpoint_pages(self) {
            return false;
        }
        let Some(i) = self.idx(id) else { return false };
        if self.buffers[i].saving || global_edit::pending(id) || self.buffers[i].preparing.is_some()
        {
            return false;
        }
        let Some(path) = path.or_else(|| self.buffers[i].path.clone()).or_else(|| {
            crate::dialogs::files(self.h)
                .set_title("Save file")
                .set_file_name(&self.buffers[i].name)
                .save_file()
        }) else {
            return false;
        };
        let key = state::key(&path);
        if self.commit_paths.iter().any(|p| state::key(p) == key)
            || self
                .buffers
                .iter()
                .enumerate()
                .any(|(j, b)| j != i && b.path.as_ref().is_some_and(|p| state::key(p) == key))
        {
            return false;
        }
        npp::file_event(self, 1007, id);
        if self.sync_buffer(i).is_err() {
            return false;
        }
        let mut doc = self.buffers[i].doc.clone();
        if let Err(e) = doc.save(&path) {
            self.notice(e.to_string());
            return false;
        }
        if !copy_only {
            self.worker(Worker::Saved(id, Ok(doc), path, false));
        }
        true
    }
}
