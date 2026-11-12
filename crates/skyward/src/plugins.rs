//! The plugins kept on the phone: folders under `Skyward/plugins` on
//! internal storage, and installed plugins under `skyward/plugins` on the
//! data partition.
//!
//! The two places are two kinds of plugin. What is on internal storage is
//! what a person dropped there: a manifest, read and drawn. What is under
//! `/data` is what this program installed, which may be the same manifest
//! with an image beside it — and it is under `/data` because an image is a
//! gigabyte and because it has to be there at the next start, not only
//! while a card is in.
//!
//! Nothing here runs a plugin. This is the reading half; the handover is a
//! page and a runtime of its own.
//!
//! Reading is best-effort in the one way that matters: a folder that cannot
//! be read is a row saying so, not an error that takes the page away, and
//! the whole folder being absent is simply no plugins at all.

use skyward_ui::app::{Payload, PLUGIN_FOLDER, PLUGIN_STORE, PluginList, PluginRow};
use skyward_ui::lang::Lang;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The most plugins listed. A page draws them all, and a folder with more
/// than this in it is not a folder of plugins.
const MOST: usize = 64;

/// The folder plugins are dropped in by hand, under internal storage.
pub fn folder() -> PathBuf {
    Path::new(skyward_crypto::data::SDCARD).join(PLUGIN_FOLDER)
}

/// The folder this program installs plugins into, under the data partition.
pub fn store() -> PathBuf {
    PathBuf::from(PLUGIN_STORE)
}

/// Whether the data partition is mounted, and so whether an installed plugin
/// can be seen at all.
///
/// It is a different filesystem from the one `/` is on, which is what being
/// mounted means; nothing here needs to know what is mounted there, only
/// that something is.
pub fn data_open() -> bool {
    let (Ok(inside), Ok(root)) = (fs::metadata(PLUGIN_STORE), fs::metadata("/")) else {
        return false;
    };
    inside.dev() != root.dev()
}

/// The image a plugin carries, if it has one where it says it does.
///
/// The name is decided here and nowhere else: a payload is `payload.<format>`
/// beside the manifest, written as `payload.<format>.part` while it
/// downloads and renamed only after the digest matches.
pub fn image(dir: &Path, payload: &Payload) -> PathBuf {
    dir.join(format!("payload.{}", payload.format))
}

/// The plugins in both places, by name, each with what its manifest says or
/// why there is nothing to read.
///
/// An installed plugin comes first and a folder of the same name on internal
/// storage is not listed twice: what was installed is what runs, so it is
/// what the page shows.
pub fn read(lang: Lang) -> Result<PluginList, String> {
    let open = data_open();
    let mut rows = Vec::new();
    let mut trouble: Option<String> = None;
    for dir in [store(), folder()] {
        if let Err(why) = listed(&dir, lang, &mut rows) {
            trouble.get_or_insert(why);
        }
    }
    // One order, decided here: the page draws them as they come.
    rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.name.cmp(&b.name)));
    // Nothing listed and something unreadable is the one case the page
    // should say why rather than look empty.
    match (rows.is_empty(), trouble) {
        (true, Some(why)) => Err(why),
        _ => Ok(PluginList { rows, data_open: open }),
    }
}

/// Add the plugins in one folder to `rows`, skipping a name already there.
fn listed(dir: &Path, lang: Lang, rows: &mut Vec<PluginRow>) -> Result<(), String> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        // No folder is no plugins: the page says what a plugin is and where
        // it goes, and nothing has been put there yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    for entry in listing.flatten() {
        if rows.len() == MOST {
            let _ = skyward_sys::kmsg(&format!("only the first {MOST} plugins are listed"));
            break;
        }
        // Folders only, and no hidden ones: what a plugin is, is a folder
        // someone put there, not the marks a filesystem leaves.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') && !rows.iter().any(|row| row.name == name) {
                let at = entry.path();
                let manifest = manifest(&at, lang);
                // An image is only believed where the manifest says one is
                // carried and the file it names is really there. The whole
                // path is kept: the page draws its last part and the
                // handover needs the whole of it.
                let image = match &manifest {
                    Ok(plugin) => plugin
                        .payload
                        .as_ref()
                        .map(|payload| image(&at, payload))
                        .filter(|file| file.is_file())
                        .map(|file| file.to_string_lossy().into_owned()),
                    Err(_) => None,
                };
                rows.push(PluginRow { manifest, name, image });
            }
        }
    }
    Ok(())
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
