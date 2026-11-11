//! The plugins kept on the phone: folders under `Skyward/plugins` on
//! internal storage.
//!
//! Nothing here runs a plugin, and nothing here is allowed to: this release
//! is the page and the reading of what a plugin will be. What a plugin says
//! about itself is read and drawn; a folder someone put there with no
//! `plugin.json` in it is reported as such rather than passed over, so that
//! the page never looks empty when it is not.
//!
//! Reading is best-effort in the one way that matters: a folder that cannot
//! be read is a row saying so, not an error that takes the page away, and
//! the whole folder being absent is simply no plugins at all.

use skyward_ui::app::{PLUGIN_FOLDER, PluginRow};
use skyward_ui::lang::Lang;
use std::fs;
use std::path::{Path, PathBuf};

/// The most plugins listed. A page draws them all, and a folder with more
/// than this in it is not a folder of plugins.
const MOST: usize = 64;

/// The folder the plugins are kept in, under internal storage.
pub fn folder() -> PathBuf {
    Path::new(skyward_crypto::data::SDCARD).join(PLUGIN_FOLDER)
}

/// The plugins in [`folder`], by name, each with what its manifest says or
/// why there is nothing to read.
pub fn read(lang: Lang) -> Result<Vec<PluginRow>, String> {
    let dir = folder();
    let listing = match fs::read_dir(&dir) {
        Ok(listing) => listing,
        // No folder is no plugins: the page says what a plugin is and where
        // it goes, and nothing has been put there yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut rows = Vec::new();
    for entry in listing.flatten() {
        if rows.len() == MOST {
            let _ = skyward_sys::kmsg(&format!("only the first {MOST} plugins are listed"));
            break;
        }
        // Folders only, and no hidden ones: what a plugin is, is a folder
        // someone put there, not the marks a filesystem leaves.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                rows.push(PluginRow { manifest: manifest(&entry.path(), lang), name });
            }
        }
    }
    // One order, decided here: the page draws them as they come.
    rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.name.cmp(&b.name)));
    Ok(rows)
}

/// What the plugin in `dir` says it is, or why there is nothing to read.
fn manifest(dir: &Path, lang: Lang) -> Result<skyward_ui::app::Plugin, String> {
    let file = dir.join(skyward_ui::plugin::FILE);
    let unread = lang.pick(["读不了 plugin.json", "讀不了 plugin.json", "plugin.json can't be read"]);
    let meta = fs::metadata(&file).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => lang.pick(["文件夹里没有 plugin.json", "資料夾裡沒有 plugin.json", "The folder holds no plugin.json"]).to_string(),
        _ => format!("{unread}: {e}"),
    })?;
    if !meta.is_file() {
        return Err(lang.pick(["plugin.json 不是一个文件", "plugin.json 不是一個檔案", "plugin.json is not a file"]).to_string());
    }
    if meta.len() > skyward_ui::plugin::MOST {
        return Err(lang.pick(["plugin.json 太大", "plugin.json 太大", "plugin.json is too large"]).to_string());
    }
    let text = fs::read_to_string(&file).map_err(|e| format!("{unread}: {e}"))?;
    skyward_ui::plugin::manifest(&text).map_err(|why| format!("{}: {why}", lang.pick(["plugin.json 读不了", "plugin.json 讀不了", "plugin.json is not a manifest"])))
}
