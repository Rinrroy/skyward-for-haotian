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

use skyward_ui::app::{Payload, PLUGIN_FOLDER, PLUGIN_STORE, Plugin, PluginList, PluginRow};
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
    // The folder this installs into is made by the first download, so its
    // being absent says nothing about the partition: what is looked at is
    // `/data` itself, which is a different filesystem from `/` exactly when
    // something is mounted there, and something is mounted there when it is
    // open. Looking at the folder instead made a phone whose data was open
    // and whose plugins folder was not made yet say the data was shut, and
    // the download refuse to start.
    let (Ok(inside), Ok(root)) = (fs::metadata("/data"), fs::metadata("/")) else {
        return false;
    };
    inside.dev() != root.dev()
}

/// Where a download puts an image: `payload.<format>` beside the manifest,
/// written as `payload.<format>.part` while it comes down and renamed only
/// after the digest matches. This is the name this program gives a file.
pub fn image(dir: &Path, payload: &Payload) -> PathBuf {
    dir.join(format!("payload.{}", payload.format))
}

/// The program an image starts when nothing says otherwise: the installer this
/// project builds its own payloads with, which is what a hand-copied image of
/// the browser holds too.
pub const PROGRAM: &str = "/opt/skyward/start.sh";

/// The image a plugin carries, wherever it is: the name a download gives it
/// first, and otherwise any one file beside the manifest that is an image of
/// that format — which is what a person who copied an image in from the system
/// has, since the file keeps the name it was downloaded with.
///
/// A folder holding two images of the same format is not guessed at: the one
/// a download would have written is taken, and otherwise the first by name, so
/// that what runs does not change from one look at the page to the next.
pub fn found_image(dir: &Path, payload: &Payload) -> Option<PathBuf> {
    let written = image(dir, payload);
    if written.is_file() {
        return Some(written);
    }
    wanted(dir, &payload.format)
}

/// The first file in `dir` whose extension is `format`, by name.
fn wanted(dir: &Path, format: &str) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case(format)))
        .collect();
    found.sort();
    found.into_iter().next()
}

/// A plugin made from an image alone, with no manifest to read: what a person
/// has after downloading the browser in the system and copying the file in.
///
/// The name is the file's own, the program is the one every payload of this
/// project starts, and nothing is claimed about a version or an author, since
/// nothing is known. It carries no `protocol`, which is what says it cannot be
/// run: an image with no manifest beside it does not say what to start inside
/// it, so the page lists it and says why it does nothing.
fn imagined(name: &str, file: &Path, format: &str) -> Plugin {
    Plugin {
        title: Some(name.to_string()),
        version: None,
        author: None,
        about: Some("an image with no plugin.json beside it".to_string()),
        needs: Vec::new(),
        protocol: None,
        payload: Some(Payload {
            url: String::new(),
            sha256: String::new(),
            size: fs::metadata(file).map(|meta| meta.len()).unwrap_or(0),
            format: format.to_string(),
            program: PROGRAM.to_string(),
        }),
    }
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
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // No hidden ones: a plugin is what someone put there, not the marks a
        // filesystem leaves.
        if name.starts_with('.') || rows.iter().any(|row| row.name == name) {
            continue;
        }
        let at = entry.path();
        if kind.is_dir() {
            // A folder is a plugin when it says so, or when it holds one image
            // and nothing else: an image is the whole of what a plugin needs
            // to run, and a manifest is what it says about itself.
            let read = manifest(&at, lang);
            let (manifest, image) = match read {
                Ok(plugin) => {
                    // An image is only believed where the manifest says one is
                    // carried and a file of that format is really there.
                    let found = plugin.payload.as_ref().and_then(|payload| found_image(&at, payload));
                    (Ok(plugin), found)
                }
                Err(why) => match lone(&at) {
                    Some((file, format)) => (Ok(imagined(&name, &file, &format)), Some(file)),
                    None => (Err(why), None),
                },
            };
            // The whole path is kept: the page draws its last part and the
            // handover needs the whole of it.
            let image = image.map(|file| file.to_string_lossy().into_owned());
            rows.push(PluginRow { manifest, name, image });
        } else if kind.is_file() {
            // An image dropped in the folder itself, with no folder around it:
            // the shortest way to put a plugin on a phone is to copy the file
            // in, so that is a plugin too.
            match format_of(&at) {
                Some(format) => rows.push(PluginRow { manifest: Ok(imagined(&name, &at, &format)), name, image: Some(at.to_string_lossy().into_owned()) }),
                None => continue,
            }
        }
    }
    Ok(())
}

/// The one image in `dir`, and its format, where the folder holds exactly one
/// image and no manifest: the shape a hand-copied plugin has.
fn lone(dir: &Path) -> Option<(PathBuf, String)> {
    let mut images: Vec<(PathBuf, String)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let path = entry.path();
            let format = format_of(&path)?;
            Some((path, format))
        })
        .collect();
    images.sort();
    match images.len() {
        1 => images.pop(),
        _ => None,
    }
}

/// `path`'s format, where the extension is one an image may be mounted as.
fn format_of(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    skyward_ui::plugin::FORMATS.iter().find(|format| **format == ext).map(|format| (*format).to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use skyward_ui::lang::Lang;

    /// A folder of plugins of its own, gone at the end of the test.
    struct Tree(PathBuf);

    impl Tree {
        fn made(name: &str) -> Tree {
            let root = std::env::temp_dir().join(format!("skyward-plugins-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("a folder of plugins");
            Tree(root)
        }

        fn file(&self, path: &str) {
            let full = self.0.join(path);
            fs::create_dir_all(full.parent().expect("a file has a folder")).expect("a folder for a file");
            fs::write(&full, b"image").expect("a file to list");
        }

        fn text(&self, path: &str, text: &str) {
            let full = self.0.join(path);
            fs::create_dir_all(full.parent().expect("a file has a folder")).expect("a folder for a file");
            fs::write(&full, text).expect("a manifest to read");
        }

        fn listed(&self) -> Vec<PluginRow> {
            let mut rows = Vec::new();
            listed(&self.0, Lang::En, &mut rows).expect("a folder that reads");
            rows
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const MANIFEST: &str = r#"{"title":"Browser","version":"1.0","payload":{"url":"https://example.invalid/w.erofs","sha256":"00","size":1,"format":"erofs","program":"/opt/skyward/start.sh"}}"#;

    #[test]
    fn an_image_named_as_a_download_names_it_is_the_one_a_plugin_carries() {
        let tree = Tree::made("named");
        tree.file("browser/payload.erofs");
        tree.text("browser/plugin.json", MANIFEST);
        let rows = tree.listed();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].image.as_ref().is_some_and(|image| image.ends_with("payload.erofs")));
    }

    #[test]
    fn an_image_kept_under_the_name_it_was_downloaded_with_is_still_the_one() {
        // What a person has after fetching the browser in the system: the
        // release's own file name, no payload.erofs anywhere.
        let tree = Tree::made("kept");
        tree.file("browser/webview-1.1-arm64.erofs");
        tree.text("browser/plugin.json", MANIFEST);
        let rows = tree.listed();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].image.as_ref().is_some_and(|image| image.ends_with("webview-1.1-arm64.erofs")), "{:?}", rows[0].image);
    }

    #[test]
    fn a_folder_holding_only_an_image_is_a_plugin_and_starts_where_every_payload_starts() {
        let tree = Tree::made("lone");
        tree.file("webview/browser.erofs");
        let rows = tree.listed();
        assert_eq!(rows.len(), 1, "a folder with one image and no manifest is a plugin");
        let plugin = rows[0].manifest.as_ref().expect("a plugin made from the image");
        assert_eq!(plugin.title.as_deref(), Some("webview"));
        let payload = plugin.payload.as_ref().expect("an image to mount");
        assert_eq!(payload.program, PROGRAM);
        assert_eq!(payload.format, "erofs");
        assert!(rows[0].image.is_some());
    }

    #[test]
    fn an_image_dropped_in_the_folder_itself_is_a_plugin_too() {
        // The shortest way to put one on a phone: copy the file in, with no
        // folder around it.
        let tree = Tree::made("dropped");
        tree.file("webview-1.1-arm64.erofs");
        let rows = tree.listed();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "webview-1.1-arm64.erofs");
        assert!(rows[0].image.is_some(), "and it is what runs");
        assert_eq!(rows[0].manifest.as_ref().map(|plugin| plugin.payload.as_ref().map(|p| p.format.clone())), Ok(Some("erofs".to_string())));
    }

    #[test]
    fn a_file_that_is_not_an_image_is_not_a_plugin() {
        let tree = Tree::made("other");
        tree.file("notes.txt");
        tree.file("browser/payload.erofs");
        let rows = tree.listed();
        assert_eq!(rows.len(), 1, "only the folder with an image in it: {:?}", rows.iter().map(|r| &r.name).collect::<Vec<_>>());
        assert_eq!(rows[0].name, "browser");
    }

    #[test]
    fn a_folder_with_two_images_and_no_manifest_is_not_guessed_at() {
        let tree = Tree::made("two");
        tree.file("browser/a.erofs");
        tree.file("browser/b.erofs");
        let rows = tree.listed();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].manifest.is_err(), "which of the two runs is not something to guess");
        assert!(rows[0].image.is_none());
    }
}
