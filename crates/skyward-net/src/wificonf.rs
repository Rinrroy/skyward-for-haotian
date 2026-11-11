//! The networks the installed system remembers, read from its
//! WifiConfigStore: the names and passwords the phone joined with, so the
//! recovery joins them again without the password being typed. Nothing is
//! copied or kept anywhere by this crate -- the store is read once the
//! data stands decrypted, and used on the spot.

use anyhow::{Context, Result};
use std::path::Path;

use crate::supplicant::Security;

/// Where Android 13 and later keep the store, inside the apex module.
const STORE: &str = "/data/misc/apexdata/com.android.wifi/WifiConfigStore.xml";
/// Where older builds kept it.
const LEGACY: &str = "/data/misc/wifi/WifiConfigStore.xml";

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
    for path in [STORE, LEGACY] {
        if !Path::new(path).exists() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Ok(saved) = parse(&text) {
            if !saved.is_empty() {
                return saved;
            }
        }
    }
    Vec::new()
}

/// One `<string name="...">value</string>` inside a block of the store.
fn field(block: &str, name: &str) -> Option<String> {
    let open = format!(r#"<string name="{name}">"#);
    let at = block.find(&open)?;
    let rest = &block[at + open.len()..];
    let end = rest.find("</string>")?;
    Some(rest[..end].to_string())
}

/// One `<int name="...">value</int>` likewise.
fn int_field(block: &str, name: &str) -> Option<i32> {
    let open = format!(r#"<int name="{name}">"#);
    let at = block.find(&open)?;
    let rest = &block[at + open.len()..];
    let end = rest.find("</int>")?;
    rest[..end].trim().parse().ok()
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

/// The kinds of network the recovery cannot join: the enterprise ones the
/// store names by number.
const UNJOINABLE: &[i32] = &[3, 5, 7, 8, 9];

/// Walk every `<WifiConfiguration>` of the store and name what it keeps.
fn parse(text: &str) -> Result<Vec<Saved>> {
    let mut out: Vec<Saved> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("<WifiConfiguration>") {
        let tail = &rest[at..];
        let end = tail
            .find("</WifiConfiguration>")
            .context("a WifiConfiguration that never closes")?
            + "</WifiConfiguration>".len();
        let block = &tail[..end];
        rest = &tail[end..];
        let kind = int_field(block, "SecurityType");
        if kind.is_some_and(|k| UNJOINABLE.contains(&k)) {
            continue;
        }
        let Some(ssid_raw) = field(block, "SSID") else {
            continue;
        };
        let ssid = unquote(&ssid_raw);
        if ssid.is_empty() {
            continue;
        }
        let psk = field(block, "PreSharedKey").map(|v| {
            String::from_utf8_lossy(&unquote(&v)).into_owned()
        });
        let psk = psk.filter(|p| !p.is_empty());
        let security = match kind {
            Some(0) => Security::Open,
            Some(1) => Security::Wep,
            Some(4) => Security::Sae,
            _ => Security::Psk,
        };
        if out.iter().any(|s: &Saved| s.ssid == ssid) {
            continue;
        }
        out.push(Saved { ssid, security, psk });
    }
    Ok(out)
}
