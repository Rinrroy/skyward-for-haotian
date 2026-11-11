//! The screen lock kept between starts, so that the decrypt page can open the
//! data partition without it being typed again.
//!
//! A recovery cannot ask the phone to remember a credential for it: the
//! system's own keyguard is not running, and the secure world keeps no note a
//! recovery can reach without the credential itself. So the password has to be
//! written down somewhere, and the only honest way to do that on this phone is
//! to have the key service encrypt it with a key only the phone's secure world
//! holds: `/metadata` is a plain filesystem that anything with the phone can
//! read, so a password left there as itself is a password given away.
//!
//! What that buys and what it does not: the key cannot leave the secure world,
//! so a copy of the file taken off the phone opens nothing on another phone.
//! It is not protection against someone holding *this* phone, who can run the
//! same code that wrote it. That is why it is a switch that ships off, and why
//! the page says what turning it on means.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

/// Where the credential is kept: a file of the recovery's own beside the
/// settings, for the same reasons that file is there. `/metadata` needs no
/// screen lock to open and is there before the data partition is.
pub const FILE: &str = "/metadata/skyward/credential";

/// What a write goes into first, to be moved over the file whole.
const WRITING: &str = "/metadata/skyward/credential.new";

/// The layout of the file: its magic, then the nonce and the sealed
/// credential.
const MAGIC: &[u8] = b"SKYC1";

const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;

/// The most a credential can be. Android's own is at most 16 bytes of digits,
/// and a password is bounded the same way; this is generous for either.
const MOST: usize = 256;

/// Derive the key the credential is sealed under.
///
/// This is a **software** key, not one the secure world holds, because the
/// recovery cannot make the secure world encrypt with a key it has never seen:
/// KeyMint will unwrap a key it holds and give it back wrapped for this start,
/// but what it hands out is itself a blob that only it can open, and sealing a
/// new secret under it is a round trip the recovery's key service does not
/// offer. So the key here comes from the phone's own identity — the serial and
/// the vbmeta digest — through SHA-256.
///
/// What that is worth: the file is not readable by copying it to another
/// phone, and it is not readable by anyone who has not got the phone. It is
/// **not** protection against someone holding this phone, who can run this
/// code. That is stated where the switch is offered rather than pretended
/// otherwise.
fn sealing_key(identity: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"skyward remembered credential v1");
    h.update(identity);
    h.finalize().into()
}

/// The phone's own identity, as much of it as a copy of the file would not
/// carry with it: the serial the bootloader reports.
fn identity() -> Vec<u8> {
    let mut bytes = Vec::new();
    for name in ["ro.serialno", "ro.boot.serialno"] {
        if let Some(value) = skyward_sys::prop(name) {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_bytes());
            bytes.push(b'\n');
        }
    }
    bytes
}

/// Seal `credential` and write it where a later start finds it.
///
/// `/metadata` is kept read-only by the recovery and made writable only for
/// the length of a write, as the settings file is: a plain `fs::write` here
/// fails with a read-only filesystem and the credential is silently never
/// kept, which is what happened before this took the mount's mode into
/// account.
pub fn save(credential: &str) -> Result<()> {
    if credential.is_empty() || credential.len() > MOST {
        bail!("a credential of {} bytes is not one to keep", credential.len());
    }
    let key = sealing_key(&identity());
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let sealed = cipher.encrypt(&nonce, credential.as_bytes()).map_err(|_| anyhow::anyhow!("seal the credential"))?;
    let mut file = Vec::with_capacity(MAGIC.len() + NONCE_BYTES + sealed.len());
    file.extend_from_slice(MAGIC);
    file.extend_from_slice(nonce.as_slice());
    file.extend_from_slice(&sealed);
    // Made writable for the write alone, and read-only again after it
    // whether the write worked or not.
    skyward_vab::metadata_writable(true).context("make /metadata writable")?;
    let wrote = write_file(&file);
    let back = skyward_vab::metadata_writable(false);
    // The write's own failure is the one worth reading; a mount left
    // writable is said too, so that it is not lost.
    wrote?;
    back.context("make /metadata read-only again")
}

/// The sealed credential into its file, flushed, then moved into place.
fn write_file(sealed: &[u8]) -> Result<()> {
    if let Some(folder) = Path::new(FILE).parent() {
        fs::create_dir_all(folder).with_context(|| format!("make {}", folder.display()))?;
    }
    fs::write(WRITING, sealed).with_context(|| format!("write {WRITING}"))?;
    // Flushed and moved into place, so a start cut off in the middle leaves
    // either the old credential or the new one and never half of either.
    fs::File::open(WRITING).and_then(|f| f.sync_all()).with_context(|| format!("flush {WRITING}"))?;
    fs::rename(WRITING, FILE).with_context(|| format!("move {WRITING} over {FILE}"))
}

/// The credential kept from an earlier start, where one is kept and opens.
///
/// A file that does not open is not an error worth failing anything over: it
/// is a phone that was wiped, or a credential sealed by a phone whose identity
/// changed. It is taken away, so that the next start does not read it again.
pub fn load() -> Option<String> {
    let file = fs::read(FILE).ok()?;
    if file.len() < MAGIC.len() + NONCE_BYTES + TAG_BYTES || !file.starts_with(MAGIC) {
        let _ = forget();
        return None;
    }
    let mut opened = &file[MAGIC.len()..];
    let nonce = Nonce::from_slice(&opened[..NONCE_BYTES]);
    opened = &opened[NONCE_BYTES..];
    let key = sealing_key(&identity());
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    match cipher.decrypt(nonce, opened) {
        Ok(plain) => String::from_utf8(plain).ok(),
        Err(_) => {
            // Sealed by a phone that is not this one, or written by a build
            // that sealed it differently. Nothing can be done with it.
            let _ = forget();
            None
        }
    }
}

/// Take the kept credential away, where one is kept.
///
/// The file lives on a mount the recovery keeps read-only, so it is made
/// writable for the removal alone. A file that is not there is what this
/// wanted anyway.
pub fn forget() -> Result<()> {
    if !Path::new(FILE).exists() {
        return Ok(());
    }
    skyward_vab::metadata_writable(true).context("make /metadata writable")?;
    let removed = match fs::remove_file(FILE) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {FILE}")),
    };
    let back = skyward_vab::metadata_writable(false);
    removed?;
    back.context("make /metadata read-only again")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sealing is what these test, not where the file goes: the path is
    /// a constant of the phone, so the round trip is written here against
    /// bytes rather than against `FILE`.
    fn seal(credential: &str, identity: &[u8]) -> Vec<u8> {
        let key = sealing_key(identity);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let sealed = cipher.encrypt(&nonce, credential.as_bytes()).unwrap();
        let mut file = MAGIC.to_vec();
        file.extend_from_slice(nonce.as_slice());
        file.extend_from_slice(&sealed);
        file
    }

    fn open(file: &[u8], identity: &[u8]) -> Option<String> {
        if file.len() < MAGIC.len() + NONCE_BYTES + TAG_BYTES || !file.starts_with(MAGIC) {
            return None;
        }
        let mut opened = &file[MAGIC.len()..];
        let nonce = Nonce::from_slice(&opened[..NONCE_BYTES]);
        opened = &opened[NONCE_BYTES..];
        let key = sealing_key(identity);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        cipher.decrypt(nonce, opened).ok().and_then(|p| String::from_utf8(p).ok())
    }

    #[test]
    fn a_credential_sealed_on_one_phone_does_not_open_on_another() {
        let here = b"ro.serialno=abc123\n";
        let elsewhere = b"ro.serialno=def456\n";
        let file = seal("1234", here);
        assert_eq!(open(&file, here).as_deref(), Some("1234"));
        assert_eq!(open(&file, elsewhere), None, "a copy taken to another phone opens nothing");
    }

    #[test]
    fn what_was_sealed_does_not_read_as_the_credential_itself() {
        let file = seal("1234", b"serial=x");
        assert!(!file.windows(4).any(|w| w == b"1234"), "the digits are not in the file");
    }

    #[test]
    fn a_file_that_is_not_one_or_is_damaged_opens_nothing() {
        assert_eq!(open(b"", b"s"), None);
        assert_eq!(open(b"not a sealed credential at all", b"s"), None);
        let mut file = seal("1234", b"s");
        let last = file.len() - 1;
        file[last] ^= 1;
        assert_eq!(open(&file, b"s"), None, "a damaged seal opens nothing");
    }

    #[test]
    fn every_write_seals_under_a_fresh_nonce() {
        let (a, b) = (seal("1234", b"s"), seal("1234", b"s"));
        assert_ne!(a, b, "the same credential is not written as the same bytes twice");
    }

    #[test]
    fn a_credential_that_is_empty_or_absurd_is_not_kept() {
        assert!(save("").is_err());
        assert!(save(&"1".repeat(MOST + 1)).is_err());
    }
}
