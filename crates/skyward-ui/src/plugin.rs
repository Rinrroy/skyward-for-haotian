//! What a plugin's manifest says: the `plugin.json` in its folder, read
//! into the words a page draws.
//!
//! Nothing runs a plugin in this release. This is the reading half, kept
//! apart from the filesystem so that it can be tested on its own: the
//! folder is found and the file read by the runtime, and every judgement
//! about what the file says is made here.
//!
//! The manifest is written by whoever makes the plugin, not by this
//! program, so nothing in it is trusted: every string is trimmed and cut
//! to a length a page can draw, and a key of the wrong type is an error
//! rather than a silently missing field.

use crate::app::Plugin;
use serde::Deserialize;

/// The file a plugin folder is read from.
pub const FILE: &str = "plugin.json";

/// The most a manifest may be. It comes from a folder anyone can put on
/// internal storage, and a page draws what it holds.
pub const MOST: u64 = 16 * 1024;

/// The longest one of its free-text fields may stand. A page draws these in
/// one line or one paragraph, and a manifest is not the place to make a
/// page of them.
const LONGEST: usize = 120;

/// How many things a plugin may say it needs.
const NEEDS_MOST: usize = 16;

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
}

/// Read a manifest's text. What it says, or why it cannot be read as one.
pub fn manifest(text: &str) -> Result<Plugin, String> {
    let written: Written = serde_json::from_str(text).map_err(|e| format!("plugin.json: {e}"))?;
    let needs = written.needs.iter().filter_map(|need| kept(need)).take(NEEDS_MOST).collect();
    Ok(Plugin {
        title: written.title.as_deref().and_then(kept).or_else(|| written.name.as_deref().and_then(kept)),
        version: written.version.as_deref().and_then(kept),
        author: written.author.as_deref().and_then(kept),
        about: written.about.as_deref().and_then(kept),
        needs,
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
}
