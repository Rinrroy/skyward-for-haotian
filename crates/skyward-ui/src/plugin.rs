//! What a plugin's manifest says: the `plugin.json` in its folder, read
//! into the words a page draws.
//!
//! This is the reading half, kept apart from the filesystem so that it can be
//! tested on its own: the folder is found and the file read by the runtime,
//! and every judgement about what the file says is made here. What a plugin
//! that speaks [`PROTOCOL`] is then handed — the panel, the fingers, the log —
//! is the plugin host's business, not this file's.
//!
//! The manifest is written by whoever makes the plugin, not by this
//! program, so nothing in it is trusted: every string is trimmed and cut
//! to a length a page can draw, and a key of the wrong type is an error
//! rather than a silently missing field.

use crate::app::{Payload, Plugin};
use serde::Deserialize;

/// The file a plugin folder is read from.
pub const FILE: &str = "plugin.json";

/// The only plugin protocol this release knows: the frame protocol. A plugin
/// that says `"protocol": 1` draws into a frame file the recovery maps and
/// puts on the panel, and takes its fingers from a FIFO of commands. One that
/// says nothing, or says another number, is listed and not run.
pub const PROTOCOL: u32 = 1;

/// The most a manifest may be. It comes from a folder anyone can put on
/// internal storage, and a page draws what it holds.
pub const MOST: u64 = 16 * 1024;

/// The longest one of its free-text fields may stand. A page draws these in
/// one line or one paragraph, and a manifest is not the place to make a
/// page of them.
const LONGEST: usize = 120;

/// How many things a plugin may say it needs.
const NEEDS_MOST: usize = 16;

/// The most an image may say it is. A payload is downloaded into the data
/// partition, so a manifest claiming more than the phone could ever hold is
/// a manifest that is wrong, and saying so now is better than filling the
/// phone to find out.
const SIZE_MOST: u64 = 8 * 1024 * 1024 * 1024;

/// The longest a download address may be. A URL is a string from a file
/// anyone may write, and a page is not the place to draw a kilobyte of it.
const URL_LONGEST: usize = 2048;

/// The ways an image may be mounted. The first is what a payload is built
/// as; the rest are here because an image someone made by hand may be one.
pub const FORMATS: [&str; 4] = ["squashfs", "erofs", "ext4", "img"];

/// The manifest as it is written, before any of it is believed.
#[derive(Deserialize)]
struct Written {
    /// What it calls itself. `name` is taken for it too: a plugin is a
    /// folder with a name, and the two words are the same thing here.
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    about: Option<String>,
    /// What it needs of the recovery, by name.
    #[serde(default)]
    needs: Vec<String>,
    /// Which plugin protocol it speaks: 1 is the frame protocol, and a
    /// manifest with none is a plugin that is listed and not run.
    #[serde(default)]
    protocol: Option<u32>,
    /// The image it carries, where it is more than a manifest.
    #[serde(default)]
    payload: Option<WrittenPayload>,
}

/// A manifest's `payload` as it is written, before any of it is believed.
#[derive(Deserialize)]
struct WrittenPayload {
    url: String,
    sha256: String,
    size: u64,
    #[serde(default)]
    format: Option<String>,
    program: String,
}

/// Read a payload out of a manifest, or say what is wrong with it.
///
/// Every one of these is a refusal rather than a correction: a plugin whose
/// image cannot be fetched, checked or found again is not a plugin that
/// should be offered to anyone.
fn payload(written: WrittenPayload) -> Result<Payload, String> {
    let url = written.url.trim();
    if url.len() > URL_LONGEST {
        return Err(format!("payload.url: longer than {URL_LONGEST} characters"));
    }
    if !url.starts_with("https://") {
        return Err("payload.url: an image is fetched over https, and this address is not".into());
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("payload.url: an address has no spaces in it".into());
    }
    let sha256 = written.sha256.trim().to_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("payload.sha256: a digest is 64 hexadecimal characters".into());
    }
    if written.size == 0 || written.size > SIZE_MOST {
        return Err(format!("payload.size: {} is not a size an image can be", written.size));
    }
    let format = written.format.as_deref().unwrap_or("squashfs").trim().to_lowercase();
    if !FORMATS.contains(&format.as_str()) {
        return Err(format!("payload.format: {format:?} is not one of {FORMATS:?}"));
    }
    let program = written.program.trim();
    if !program.starts_with('/') || program.split('/').any(|part| part == "..") {
        return Err(format!("payload.program: {program:?} is not a path inside the image"));
    }
    Ok(Payload { url: url.to_string(), sha256, size: written.size, format, program: program.to_string() })
}

/// Read a manifest's text. What it says, or why it cannot be read as one.
pub fn manifest(text: &str) -> Result<Plugin, String> {
    // A manifest is a JSON object, and is said so before serde sees it: serde
    // reads a struct out of a sequence too, by position, so `[]` would come
    // back as a plugin that says nothing rather than as the error it is.
    if !text.trim_start().starts_with('{') {
        return Err("plugin.json: a manifest is a JSON object, and this is not one".into());
    }
    let written: Written = serde_json::from_str(text).map_err(|e| format!("plugin.json: {e}"))?;
    plugin_of(written)
}

/// One entry of the published index as it is written: a manifest with the
/// folder name added to it.
#[derive(Deserialize)]
struct WrittenIndex {
    #[serde(default)]
    plugins: Vec<serde_json::Value>,
}

/// How many plugins the index may offer. A page draws them all, and an index
/// that names more than this is not an index this program needs.
pub const INDEX_MOST: usize = 32;

/// One plugin to install: the folder it goes in and what it says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    /// The folder name under the plugin store, as the index wrote it.
    pub name: String,
    pub plugin: Plugin,
    /// The entry's own text, written as the installed `plugin.json` so the
    /// plugin is read the same way after the next start as it was now.
    pub text: String,
}

/// Read the published index. Each entry is held to the same rules as a
/// manifest on the phone, because it comes from the same place: someone
/// else's file, fetched over the network.
///
/// An entry is a manifest with one more key, `name`, which is the folder it
/// is installed into. Each entry is read by [`manifest`] on its own text, so
/// there is one parser and not two.
pub fn index(text: &str) -> Result<Vec<Offered>, String> {
    if !text.trim_start().starts_with('{') {
        return Err("the plugin index is a JSON object, and this is not one".into());
    }
    let written: WrittenIndex = serde_json::from_str(text).map_err(|e| format!("the plugin index: {e}"))?;
    let mut offered = Vec::new();
    for value in written.plugins.iter().take(INDEX_MOST) {
        let name = value.get("name").and_then(|n| n.as_str()).and_then(kept).ok_or("an index entry has no name")?;
        // A folder name is one path part: `..` would install outside the
        // store, and a slash would install into a folder that is not one.
        if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            return Err(format!("{name:?} is not a folder name"));
        }
        let text = value.to_string();
        let plugin = manifest(&text)?;
        if plugin.payload.is_none() {
            // A plugin to download that carries no image is a plugin this
            // page could install and then have nothing to run.
            return Err(format!("{name}: the index offers it without an image"));
        }
        offered.push(Offered { name, plugin, text });
    }
    Ok(offered)
}

/// A manifest as the pages and the runtime hold it.
fn plugin_of(written: Written) -> Result<Plugin, String> {
    let needs = written.needs.iter().filter_map(|need| kept(need)).take(NEEDS_MOST).collect();
    let payload = written.payload.map(payload).transpose()?;
    Ok(Plugin {
        title: written.title.as_deref().and_then(kept).or_else(|| written.name.as_deref().and_then(kept)),
        version: written.version.as_deref().and_then(kept),
        author: written.author.as_deref().and_then(kept),
        about: written.about.as_deref().and_then(kept),
        needs,
        protocol: written.protocol,
        payload,
    })
}

/// One of the manifest's strings as a page may draw it: trimmed, cut to
/// [`LONGEST`], and nothing at all where it says nothing.
fn kept(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(LONGEST).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_names_the_plugin_and_what_it_is_for() {
        let plugin = manifest(r#"{"title":"主题包","version":"1.2","author":"某人","about":"换一套界面","needs":["theme","audio"]}"#).expect("a plugin");
        assert_eq!(plugin.title.as_deref(), Some("主题包"));
        assert_eq!(plugin.version.as_deref(), Some("1.2"));
        assert_eq!(plugin.author.as_deref(), Some("某人"));
        assert_eq!(plugin.about.as_deref(), Some("换一套界面"));
        assert_eq!(plugin.needs, ["theme", "audio"]);
    }

    #[test]
    fn a_manifest_may_leave_everything_out() {
        let plugin = manifest("{}").expect("a plugin with nothing said");
        assert_eq!(plugin, Plugin::default());
        // Whitespace says nothing rather than standing as a name.
        let blank = manifest(r#"{"title":"  ","about":"\n"}"#).expect("a plugin");
        assert_eq!(blank, Plugin::default());
    }

    #[test]
    fn a_name_stands_for_the_title() {
        let plugin = manifest(r#"{"name":"theme-pack"}"#).expect("a plugin");
        assert_eq!(plugin.title.as_deref(), Some("theme-pack"));
        // A title said too takes the place of the name.
        let both = manifest(r#"{"name":"theme-pack","title":"主题包"}"#).expect("a plugin");
        assert_eq!(both.title.as_deref(), Some("主题包"));
    }

    #[test]
    fn what_is_not_a_manifest_is_an_error_rather_than_a_plugin() {
        for text in ["", "{", "[]", "null", "3", r#"{"needs":"theme"}"#, r#"{"title":3}"#] {
            assert!(manifest(text).is_err(), "{text:?} is not a manifest");
        }
    }

    #[test]
    fn a_string_no_page_could_draw_is_cut_to_one_it_can() {
        let long = "长".repeat(500);
        let plugin = manifest(&format!(r#"{{"about":"{long}"}}"#)).expect("a plugin");
        assert_eq!(plugin.about.as_deref().map(|a| a.chars().count()), Some(LONGEST));
        let needs = format!(r#"{{"needs":{}}}"#, serde_json::to_string(&vec!["a"; 50]).expect("json"));
        assert_eq!(manifest(&needs).expect("a plugin").needs.len(), NEEDS_MOST);
    }

    /// A manifest with the given payload fields, all of them good unless the
    /// test says otherwise.
    fn with_payload(fields: &str) -> String {
        format!(r#"{{"title":"浏览器","payload":{{{fields}}}}}"#)
    }

    const DIGEST: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn a_payload_says_where_the_image_is_and_what_runs_in_it() {
        let text = with_payload(&format!(
            r#""url":"https://example.invalid/webview.squashfs","sha256":"{DIGEST}","size":734003200,"format":"squashfs","program":"/usr/bin/chromium""#
        ));
        let plugin = manifest(&text).expect("a plugin with an image");
        let payload = plugin.payload.expect("the image it carries");
        assert_eq!(payload.url, "https://example.invalid/webview.squashfs");
        assert_eq!(payload.sha256, DIGEST);
        assert_eq!(payload.size, 734003200);
        assert_eq!(payload.format, "squashfs");
        assert_eq!(payload.program, "/usr/bin/chromium");
    }

    #[test]
    fn a_manifest_without_a_payload_carries_none() {
        assert_eq!(manifest(r#"{"title":"主题包"}"#).expect("a plugin").payload, None);
    }

    #[test]
    fn a_plugin_is_run_only_when_it_speaks_the_protocol_and_carries_an_image() {
        let image = format!(r#""url":"https://example.invalid/w.squashfs","sha256":"{DIGEST}","size":1,"program":"/opt/skyward/run.sh""#);
        let frames = manifest(&format!(r#"{{"title":"浏览器","protocol":1,"payload":{{{image}}}}}"#)).expect("a plugin");
        assert_eq!(frames.protocol, Some(PROTOCOL));
        assert!(frames.runs(), "the frame protocol, with an image");
        // No protocol at all is a plugin from before this one: listed, and not
        // run, because the recovery does not guess how a program wants the
        // phone.
        let mute = manifest(&format!(r#"{{"title":"浏览器","payload":{{{image}}}}}"#)).expect("a plugin");
        assert_eq!(mute.protocol, None);
        assert!(!mute.runs(), "a plugin that says nothing about a protocol");
        // A protocol this release does not know is not one to guess at either.
        let later = manifest(&format!(r#"{{"title":"浏览器","protocol":2,"payload":{{{image}}}}}"#)).expect("a plugin");
        assert!(!later.runs(), "a protocol from a later release");
        // A protocol with nothing to run is still nothing to run.
        let bare = manifest(&format!(r#"{{"protocol":{PROTOCOL}}}"#)).expect("a plugin");
        assert!(!bare.runs(), "a plugin with no image");
        // A protocol that is not a number is an error, not a plugin.
        assert!(manifest(r#"{"protocol":"one"}"#).is_err());
    }

    #[test]
    fn a_payload_the_phone_could_not_fetch_or_check_is_refused() {
        // Plain http: an image that is executed is not fetched where anyone
        // on the way could rewrite it.
        let plain = with_payload(&format!(
            r#""url":"http://example.invalid/a.img","sha256":"{DIGEST}","size":1,"program":"/a""#
        ));
        assert!(manifest(&plain).is_err(), "http is not https");
        // An address with a space in it is two words, not one address.
        let spaced = with_payload(&format!(
            r#""url":"https://example.invalid/a b.img","sha256":"{DIGEST}","size":1,"program":"/a""#
        ));
        assert!(manifest(&spaced).is_err(), "an address has no spaces");
        // A digest that is not a digest cannot be checked against anything.
        for digest in ["", "abc", &"z".repeat(64)] {
            let text = with_payload(&format!(
                r#""url":"https://example.invalid/a.img","sha256":"{digest}","size":1,"program":"/a""#
            ));
            assert!(manifest(&text).is_err(), "{digest:?} is not a digest");
        }
        // Nothing to download, or more than the phone could hold.
        for size in [0, SIZE_MOST + 1] {
            let text = with_payload(&format!(
                r#""url":"https://example.invalid/a.img","sha256":"{DIGEST}","size":{size},"program":"/a""#
            ));
            assert!(manifest(&text).is_err(), "{size} is not a size an image can be");
        }
        // A format nothing mounts.
        let odd = with_payload(&format!(
            r#""url":"https://example.invalid/a.img","sha256":"{DIGEST}","size":1,"format":"zip","program":"/a""#
        ));
        assert!(manifest(&odd).is_err(), "zip is not mounted");
        // A program that is not a path inside the image.
        for program in ["chromium", "/../../etc/passwd", "/usr/../../bin/sh", ""] {
            let text = with_payload(&format!(
                r#""url":"https://example.invalid/a.img","sha256":"{DIGEST}","size":1,"program":"{program}""#
            ));
            assert!(manifest(&text).is_err(), "{program:?} is not a path inside the image");
        }
        // A payload that leaves out what it needs is an error, not a plugin
        // that quietly carries nothing.
        let missing = with_payload(r#""url":"https://example.invalid/a.img","size":1,"program":"/a""#);
        assert!(manifest(&missing).is_err(), "a payload without a digest");
    }

    #[test]
    fn a_digest_is_read_in_either_case_and_a_format_defaults_to_the_one_we_build() {
        let upper = with_payload(&format!(
            r#""url":"https://example.invalid/a.img","sha256":"{}","size":1,"program":"/a""#,
            DIGEST.to_uppercase()
        ));
        let payload = manifest(&upper).expect("a plugin").payload.expect("an image");
        assert_eq!(payload.sha256, DIGEST, "the digest is compared in one case");
        assert_eq!(payload.format, "squashfs", "the format we build is the one assumed");
    }

    #[test]
    fn an_index_offers_plugins_by_folder_name_and_refuses_what_it_could_not_install() {
        let text = format!(
            r#"{{"version":1,"plugins":[{{"name":"webview","title":"浏览器","version":"1.0","payload":{{"url":"https://example.invalid/w.squashfs","sha256":"{DIGEST}","size":734003200,"format":"squashfs","program":"/opt/skyward/start.sh"}}}}]}}"#
        );
        let offered = index(&text).expect("an index");
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].name, "webview");
        assert_eq!(offered[0].plugin.title.as_deref(), Some("浏览器"));
        // The text kept is the entry's own, and it is a manifest this program
        // reads back to the same thing.
        assert_eq!(manifest(&offered[0].text).expect("the entry's own text"), offered[0].plugin);
        // A name that is not one folder, and an entry with nothing to run.
        for name in ["../etc", "a/b", "..", "."] {
            let bad = format!(r#"{{"plugins":[{{"name":"{name}","payload":{{"url":"https://example.invalid/a","sha256":"{DIGEST}","size":1,"program":"/a"}}}}]}}"#);
            assert!(index(&bad).is_err(), "{name:?} is not a folder name");
        }
        let bare = r#"{"plugins":[{"name":"webview","title":"浏览器"}]}"#;
        assert!(index(bare).is_err(), "an entry with no image is not one to install");
        assert!(index("[]").is_err(), "an index is an object");
        assert_eq!(index(r#"{"plugins":[]}"#).expect("an empty index").len(), 0);
    }
}

