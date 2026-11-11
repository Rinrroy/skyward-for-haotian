//! The networks the installed system remembers, read from its
//! WifiConfigStore: the names and passwords the phone joined with, so the
//! recovery joins them again without the password being typed. Nothing is
//! copied or kept anywhere by this crate -- the store is read once the
//! data stands decrypted, and used on the spot.
//!
//! The store has been written in two shapes. The older one puts a network in
//! a `<WifiConfiguration>` block and its values in elements:
//!
//! ```text
//! <WifiConfiguration>
//!   <string name="SSID">"Home"</string>
//!   <string name="PreSharedKey">"secret"</string>
//!   <int name="SecurityType">2</int>
//! </WifiConfiguration>
//! ```
//!
//! Android 13 and later put it in a `<Network>` block and its values in
//! attributes of the element itself:
//!
//! ```text
//! <Network>
//!   <string name="SSID" value="&quot;Home&quot;" />
//!   <string name="PreSharedKey" value="&quot;secret&quot;" />
//!   <int name="SecurityType" value="2" />
//! </Network>
//! ```
//!
//! Both are read here, so the store of whichever phone the recovery runs on
//! gives its networks up.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::supplicant::Security;

/// Where Android 13 and later keep the store, inside the apex module.
const STORE: &str = "/data/misc/apexdata/com.android.wifi/WifiConfigStore.xml";
/// Where older builds kept it.
const LEGACY: &str = "/data/misc/wifi/WifiConfigStore.xml";
/// The folders the store is kept in, whatever the phone calls the file.
const FOLDERS: &[&str] = &[
    "/data/misc/apexdata/com.android.wifi",
    "/data/misc/wifi",
    "/data/misc/apexdata/com.android.wifi/softap.conf",
];

/// One network the system remembers: its name, how it is protected, and
/// the password it joins with when it takes one.
pub struct Saved {
    pub ssid: Vec<u8>,
    pub security: Security,
    pub psk: Option<String>,
}

impl Saved {
    /// Whether the recovery can join this network right away: an open
    /// one, or one whose password the store carries. The enterprise kinds
    /// it cannot join at all, and the reader leaves them out already.
    pub fn joinable(&self) -> bool {
        match self.security {
            Security::Open | Security::Owe => true,
            _ => self.psk.is_some(),
        }
    }
}

/// Read the store the installed system keeps. A phone whose data is not
/// decrypted yet, or whose store is of a shape this reader does not know,
/// gives an empty list -- the password can then be typed as always.
pub fn load() -> Vec<Saved> {
    for path in paths() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(saved) = parse(&text) else {
            continue;
        };
        if !saved.is_empty() {
            return saved;
        }
    }
    Vec::new()
}

/// Every file the store may be kept in: the two names it is known by, and
/// whatever else the folders it lives in hold.
fn paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![PathBuf::from(STORE), PathBuf::from(LEGACY)];
    for folder in FOLDERS {
        let Ok(entries) = std::fs::read_dir(folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "xml") && !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// The tags a saved network is written under, each with the tag that closes
/// it: Android 13 and later call it a `Network`, older builds a
/// `WifiConfiguration`.
const BLOCKS: &[(&str, &str)] = &[
    ("<Network>", "</Network>"),
    ("<WifiConfiguration>", "</WifiConfiguration>"),
];

/// The kinds of network the recovery cannot join: the enterprise ones the
/// store names by number.
const UNJOINABLE: &[i32] = &[3, 5, 7, 8, 9];

/// Walk every block of the store and name what it keeps.
fn parse(text: &str) -> Result<Vec<Saved>> {
    let mut out: Vec<Saved> = Vec::new();
    for (open, close) in BLOCKS {
        let mut rest = text;
        while let Some(at) = rest.find(open) {
            let tail = &rest[at..];
            let Some(end) = tail.find(close) else {
                break;
            };
            let block = &tail[..end + close.len()];
            rest = &tail[end + close.len()..];
            let Some(one) = one(block) else {
                continue;
            };
            if out.iter().any(|s: &Saved| s.ssid == one.ssid) {
                continue;
            }
            out.push(one);
        }
        if !out.is_empty() {
            break;
        }
    }
    Ok(out)
}

/// What one block of the store says: the network's name, how it is
/// protected, and the password it joins with.
fn one(block: &str) -> Option<Saved> {
    let kind = int_field(block, "SecurityType");
    if kind.is_some_and(|k| UNJOINABLE.contains(&k)) {
        return None;
    }
    let raw = field(block, "SSID")?;
    let ssid = unquote(&raw);
    if ssid.is_empty() {
        return None;
    }
    let psk = field(block, "PreSharedKey")
        .map(|v| String::from_utf8_lossy(&unquote(&v)).into_owned())
        .filter(|p| !p.is_empty());
    let security = match kind {
        Some(0) => Security::Open,
        Some(1) => Security::Wep,
        Some(4) => Security::Sae,
        _ => Security::Psk,
    };
    Some(Saved { ssid, security, psk })
}

/// One `<string name="...">value</string>` inside a block of the store, or
/// the same value written as an attribute of the element, which is how
/// Android 13 and later write it.
fn field(block: &str, name: &str) -> Option<String> {
    let open = format!(r#"<string name="{name}">"#);
    if let Some(value) = between(block, &open, "</string>") {
        return Some(entities(&value));
    }
    let attr = format!(r#"<string name="{name}" value=""#);
    let rest = block.find(&attr)? + attr.len();
    let end = block[rest..].find('"')? + rest;
    Some(entities(block[rest..end].trim()))
}

/// One `<int name="...">value</int>`, likewise.
fn int_field(block: &str, name: &str) -> Option<i32> {
    let open = format!(r#"<int name="{name}">"#);
    if let Some(value) = between(block, &open, "</int>") {
        return value.trim().parse().ok();
    }
    let attr = format!(r#"<int name="{name}" value=""#);
    let rest = block.find(&attr)? + attr.len();
    let end = block[rest..].find('"')? + rest;
    block[rest..end].trim().parse().ok()
}

/// What stands between an opening tag and the one that closes it.
fn between(block: &str, open: &str, close: &str) -> Option<String> {
    let at = block.find(open)? + open.len();
    let end = block[at..].find(close)? + at;
    Some(block[at..end].to_string())
}

/// The escapes an XML document writes its text with.
fn entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let Some(end) = rest.find(';') else {
            out.push('&');
            break;
        };
        let name = &rest[..end];
        let ch = match name {
            "quot" => Some('"'),
            "amp" => Some('&'),
            "apos" => Some('\''),
            "lt" => Some('<'),
            "gt" => Some('>'),
            _ => name
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => out.push('&'),
        }
    }
    out.push_str(rest);
    out
}

/// Strip the quotes the store writes its strings with. A name the phone
/// wrote as hex -- one with odd characters in it -- comes back through the
/// same escape the supplicant's own scan answers speak.
fn unquote(value: &str) -> Vec<u8> {
    let t = value.trim();
    if let Some(inner) = t.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return inner.as_bytes().to_vec();
    }
    if let Some(hex) = t.strip_prefix("0x") {
        if let Ok(b) = crate::supplicant::unescape(hex) {
            return b;
        }
    }
    t.as_bytes().to_vec()
}

/// Whether the store of the installed system can be read at all: the data
/// partition has to stand open, and the file has to be there.
pub fn readable() -> bool {
    paths().iter().any(|p| Path::new(p).exists())
}
