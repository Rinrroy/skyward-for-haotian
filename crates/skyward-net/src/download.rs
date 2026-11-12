//! Downloading a package whole: from where an earlier download of it
//! stopped, over connections that may drop, and checked against the digest
//! its server gave before it takes its own name.

use anyhow::{bail, Context, Result};
use md5::{Digest, Md5};
use sha2::Sha256;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// How much is read and written at a time.
const CHUNK: usize = 1 << 20;

/// The digest a package's server gives of it, in hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sum {
    Md5(String),
    Sha256(String),
}

/// Where a package is downloaded to until it is whole and checked.
pub fn partial(path: &Path) -> PathBuf {
    let mut name = path.file_name().expect("a package has a name").to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// The package called `name` in `folder`, where `name` is a package's own
/// name: one plain path component, with nothing in it that leads out of the
/// folder. The name comes from the update server, and what it names is
/// deleted, so a name that does not hold up here is no package at all.
pub fn package(folder: &Path, name: &str) -> Option<PathBuf> {
    let only = Path::new(name);
    let plain = matches!(only.components().next(), Some(std::path::Component::Normal(_))) && only.components().count() == 1;
    plain.then(|| folder.join(only))
}

/// How far a download has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// What an earlier download left is read for its digest, before the
    /// rest is added to it.
    Checking { done: u64, total: u64 },
    Downloading { done: u64, total: u64 },
}

/// Why a download did not end with the package whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The folder has too little room for what is left of it.
    NoRoom { needed: u64, free: u64 },
    /// What was downloaded was not the package its server described, and
    /// is gone.
    Damaged,
    /// The server now offers another system than the one downloaded.
    Changed,
    /// The connection kept failing, or anything else: why.
    Other(String),
}

/// The server turned the download down: what a signed address that has
/// run out gets, or one it no longer serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused(pub u16);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server answered {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// The whole package does not have the digest its server gave; what was
/// downloaded of it is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Damaged;

impl std::fmt::Display for Damaged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the package downloaded does not have the digest its server gave")
    }
}

impl std::error::Error for Damaged {}

/// The folder has too little room for what is left to download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoRoom {
    pub needed: u64,
    pub free: u64,
}

impl std::fmt::Display for NoRoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes more are needed, and {} are free", self.needed, self.free)
    }
}

impl std::error::Error for NoRoom {}

/// The bytes free for anyone to write in the filesystem `dir` is on.
pub fn free(dir: &Path) -> Result<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).context("a path with a NUL")?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("read the free space of {}", dir.display()));
    }
    Ok(s.f_bavail as u64 * s.f_frsize as u64)
}

/// The digest, by `D`, of what `file` holds, read from its start.
fn hash_of<D: Digest>(file: &mut File, total: u64, stop: &AtomicBool, progress: &mut dyn FnMut(Progress)) -> Result<D> {
    let mut hash = D::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            bail!(crate::Stopped);
        }
        let n = file.read(&mut buf).context("read what was downloaded before")?;
        if n == 0 {
            return Ok(hash);
        }
        hash.update(&buf[..n]);
        done += n as u64;
        progress(Progress::Checking { done, total });
    }
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// How much of the package a download already holds, to go on from: what is
/// in the part, which is what the range it asks for starts at. A part longer
/// than the package cannot be of it — the server now offers something else —
/// so nothing of it is kept.
fn resume_from(have: u64, total: u64) -> u64 {
    if have > total {
        0
    } else {
        have
    }
}

/// How many addresses a server may hand on before the last one is asked.
const HOPS: usize = 5;

/// The address `url` ends at, and how long what it sends is.
///
/// A release asset is not served where its name says: GitHub answers with the
/// address of the machine that holds it, and that answer carries no length.
/// Following it here, rather than trusting the client to, is what makes a
/// download from a release work at all; the address followed to is also the
/// one the bytes are asked from, so the two cannot disagree.
fn locate(agent: &ureq::Agent, url: &str) -> Result<(String, u64)> {
    let mut at = url.to_string();
    for _ in 0..HOPS {
        let head = agent.head(&at).call().context("ask the server about the package")?;
        match head.status().as_u16() {
            200 => {
                let total = content_length(head.headers()).context("the server does not say how long the package is")?;
                return Ok((at, total));
            }
            301 | 302 | 303 | 307 | 308 => {
                let next = head.headers().get(ureq::http::header::LOCATION).and_then(|v| v.to_str().ok()).context("a server answered with no address to go to")?;
                // Only an address is followed: one said relative would need a
                // URL taken apart and put together again, which is more than
                // a download is for.
                if !next.starts_with("https://") {
                    bail!("a server answered with an address that is not one: {next}");
                }
                at = next.to_string();
            }
            status => bail!(Refused(status)),
        }
    }
    bail!("a server handed the download on more than {HOPS} times")
}

/// Download `url` to `path`, carrying on from what an earlier download
/// left, and give it its name once it has the digest `sum`. A part as long
/// as the package is only checked; a server that will not send from the
/// middle sends it all again.
pub fn fetch(agent: &ureq::Agent, url: &str, path: &Path, sum: &Sum, stop: &AtomicBool, progress: &mut dyn FnMut(Progress)) -> Result<()> {
    match sum {
        Sum::Md5(hex) => fetch_as::<Md5>(agent, url, path, hex, stop, progress),
        Sum::Sha256(hex) => fetch_as::<Sha256>(agent, url, path, hex, stop, progress),
    }
}

/// [`fetch`], with the digest by `D` in hex.
fn fetch_as<D: Digest>(agent: &ureq::Agent, url: &str, path: &Path, sum: &str, stop: &AtomicBool, progress: &mut dyn FnMut(Progress)) -> Result<()> {
    let part = partial(path);
    let dir = path.parent().context("a package's path is in a folder")?;
    fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    let head = agent.head(url).call().context("ask the server about the package")?;
    let (url, total) = match head.status().as_u16() {
        200 => (url.to_string(), content_length(head.headers()).context("the server does not say how long the package is")?),
        // Handed on to where the package is: the address followed to is what
        // is asked for the bytes as well.
        _ => locate(agent, url)?,
    };
    let url = url.as_str();
    let mut file = OpenOptions::new().create(true).read(true).append(true).open(&part).with_context(|| format!("open {}", part.display()))?;
    let held = file.metadata().with_context(|| format!("read {}", part.display()))?.len();
    let mut have = resume_from(held, total);
    if have != held {
        file.set_len(0).with_context(|| format!("empty {}", part.display()))?;
    }
    let room = free(dir)?;
    if total - have > room {
        bail!(NoRoom { needed: total - have, free: room });
    }
    let mut hash = hash_of::<D>(&mut file, have, stop, progress)?;
    if have < total {
        let mut asked = agent.get(url);
        if have > 0 {
            asked = asked.header("Range", format!("bytes={have}-"));
        }
        let mut response = asked.call().context("start the download")?;
        match response.status().as_u16() {
            206 if have > 0 => {}
            200 => {
                if have > 0 {
                    file.set_len(0).with_context(|| format!("empty {}", part.display()))?;
                    have = 0;
                    hash = D::new();
                }
            }
            status => bail!(Refused(status)),
        }
        let mut body = response.body_mut().with_config().limit(u64::MAX).reader();
        let mut buf = vec![0u8; CHUNK];
        let mut done = have;
        progress(Progress::Downloading { done, total });
        while done < total {
            if stop.load(Ordering::Relaxed) {
                bail!(crate::Stopped);
            }
            let n = body.read(&mut buf).context("download the package")?;
            if n == 0 {
                bail!("the server stopped sending at {done} of {total} bytes");
            }
            let n = n.min((total - done) as usize);
            file.write_all(&buf[..n]).with_context(|| format!("write {}", part.display()))?;
            hash.update(&buf[..n]);
            done += n as u64;
            progress(Progress::Downloading { done, total });
        }
        file.sync_all().with_context(|| format!("write {} through", part.display()))?;
    }
    if hex(&hash.finalize()) != sum.to_lowercase() {
        drop(file);
        fs::remove_file(&part).with_context(|| format!("delete {}", part.display()))?;
        bail!(Damaged);
    }
    fs::rename(&part, path).with_context(|| format!("name {} {}", part.display(), path.display()))
}

/// The length a response says its body has.
pub(crate) fn content_length(headers: &ureq::http::HeaderMap) -> Option<u64> {
    headers.get(ureq::http::header::CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_is_named_after_its_package() {
        assert_eq!(partial(Path::new("/sdcard/downloaded_rom/a.zip")), Path::new("/sdcard/downloaded_rom/a.zip.part"));
    }

    #[test]
    fn a_package_is_named_by_one_component_that_cannot_lead_out_of_its_folder() {
        let folder = Path::new("/sdcard/downloaded_rom");
        assert_eq!(package(folder, "ishtar-ota_full-OS3.0.310.0.WMACNXM.zip"), Some(folder.join("ishtar-ota_full-OS3.0.310.0.WMACNXM.zip")));
        assert_eq!(package(folder, ""), None, "nothing is no package");
        for led_away in ["../locksettings.db", "..", ".", "/sdcard/a.zip", "a/../../b.zip", "a/b.zip"] {
            assert_eq!(package(folder, led_away), None, "{led_away} leads out of the folder");
        }
    }

    #[test]
    fn a_download_goes_on_from_what_its_part_holds_but_not_from_more_than_the_package() {
        assert_eq!(resume_from(0, 100), 0);
        assert_eq!(resume_from(40, 100), 40, "a paused download goes on from its part's own length");
        assert_eq!(resume_from(100, 100), 100, "a part as long as the package is only checked");
        assert_eq!(resume_from(101, 100), 0, "a longer part is of another package and is let go");
    }

    #[test]
    fn what_is_there_is_read_for_its_digest() {
        let dir = std::env::temp_dir().join(format!("skyward-net-hash-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.part");
        fs::write(&path, vec![7u8; CHUNK + 5]).unwrap();
        let mut seen = Vec::new();
        let hash = hash_of::<Md5>(&mut File::open(&path).unwrap(), (CHUNK + 5) as u64, &AtomicBool::new(false), &mut |p| seen.push(p)).unwrap();
        assert_eq!(hex(&hash.finalize()), hex(&Md5::digest(vec![7u8; CHUNK + 5])));
        assert_eq!(seen.last(), Some(&Progress::Checking { done: (CHUNK + 5) as u64, total: (CHUNK + 5) as u64 }));
        let hash = hash_of::<Sha256>(&mut File::open(&path).unwrap(), (CHUNK + 5) as u64, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(hex(&hash.finalize()), hex(&Sha256::digest(vec![7u8; CHUNK + 5])));
        let stopped = hash_of::<Md5>(&mut File::open(&path).unwrap(), 1, &AtomicBool::new(true), &mut |_| {});
        assert!(stopped.unwrap_err().is::<crate::Stopped>());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn free_space_is_read_from_the_filesystem() {
        assert!(free(&std::env::temp_dir()).unwrap() > 0);
        assert!(free(Path::new("/no/such/folder")).is_err());
    }
}
