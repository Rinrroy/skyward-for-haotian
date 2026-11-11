//! Clearing what ART compiled ahead of time, once after each new image.
//!
//! A recovery is flashed over the phone's own system, and the compilations
//! the system left in the data partition were made by a different build of
//! the system it will run next: the phone then works from that older code,
//! re-verifies it, and compiles it again as it goes. It is slower and draws
//! more power for as long as that lasts, and it lasts until something clears
//! the cache. Nothing in the phone's own system does it for a recovery,
//! because the system does not know a recovery was flashed.
//!
//! So the first start of a new Skyward clears it, and writes down which image
//! did, on a partition that outlives a boot and that a factory reset does not
//! remake. `persist` is the one: it is a real partition of a few megabytes, a
//! wipe leaves it alone, and it already holds what Android's time daemon
//! keeps, so the way to mount it is known.
//!
//! Nothing here may fail the recovery. A phone whose data partition will not
//! open, or whose persist cannot be written, starts as it always did: what it
//! could not do is left unrecorded on purpose, so that the next start tries
//! again.

// The decision is what the tests are about, and the clearing it decides is
// done only on Android: a host build has the first and not the second.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

#[cfg(target_os = "android")]
use anyhow::{Context, Result};
#[cfg(target_os = "android")]
use std::fs;
#[cfg(target_os = "android")]
use std::path::{Path, PathBuf};

/// The vendor persist partition, as the phone's by-name links name it.
#[cfg(target_os = "android")]
const PERSIST: &str = "/dev/block/by-name/persist";

/// Where persist is mounted while this reads and writes its marker.
#[cfg(target_os = "android")]
const MOUNT: &str = "/skyward/persist";

/// The marker, relative to persist's root: which image last cleared the
/// cache. In a folder of Skyward's own, so nothing the phone keeps there is
/// touched.
#[cfg(target_os = "android")]
const MARKER: &str = "skyward/art-cleared";

/// Where ART keeps on the data partition what it compiles ahead of time: the
/// system's own apps, what the ART module compiles for the boot classpath and
/// the system server, and what is compiled for the slot an update was
/// installed to. The system compiles all of it again as it needs it.
///
/// These are the three folders the wipe page's ART cache step names, kept
/// here so that clearing them needs no page and nobody at the screen.
#[cfg(target_os = "android")]
const ART_CACHES: [&str; 3] = ["/data/dalvik-cache", "/data/misc/apexdata/com.android.art/dalvik-cache", "/data/ota"];

/// Whether this start should clear the cache, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Nothing was recorded: the first start of this image, or one whose
    /// marker a reflash of the recovery partition took away with it.
    First,
    /// Another image cleared it last, and this one compiles differently, so
    /// what is there is stale again.
    OtherVersion,
    /// The marker holds nothing that names an image, so what it means is not
    /// known. Clearing costs a recompilation; running on a cache nobody can
    /// account for costs longer.
    Unreadable,
}

/// What this start should do, given what the marker holds: `None` where the
/// marker names this same image.
pub fn due(marker: Option<&str>, image: &str) -> Option<Due> {
    match marker {
        None => Some(Due::First),
        Some(held) => match held.trim() {
            "" => Some(Due::Unreadable),
            held if held == image => None,
            _ => Some(Due::OtherVersion),
        },
    }
}

/// The line the marker holds once this image has cleared the cache.
pub fn marker_text(image: &str) -> String {
    format!("{image}\n")
}

/// Clear the ART caches, once, if this image has not done it yet.
///
/// `open_data` opens the data partition as far as it opens with nothing a
/// user knows, which is all this needs: what ART compiled is nobody's own
/// file. Without it the caches are not there to clear, and the marker is left
/// as it was so that the next start tries again. What happened, or why
/// nothing did, goes to the kernel log: a recovery puts nothing on a terminal
/// for a person to read.
///
/// Called from the recovery's start, before the pages are up, so that the
/// phone is never in a session whose cache goes away under it.
#[cfg(target_os = "android")]
pub fn clear_once(open_data: impl FnOnce() -> Result<()>) {
    let image = skyward_info::VERSION;
    let Some(why) = due(read_marker().as_deref(), image) else { return };
    let said = match why {
        Due::First => "the first start of this image",
        Due::OtherVersion => "another image cleared it last",
        Due::Unreadable => "what the marker holds names no image",
    };
    let _ = skyward_sys::kmsg(&format!("what ART compiled is cleared on this start: {said}"));
    if let Err(e) = open_data() {
        // The phone keeps its files closed: nothing here can be done, and the
        // marker stays as it is so that the next start tries again.
        let _ = skyward_sys::kmsg(&format!("what ART compiled is not cleared: {said}, and the data partition did not open: {e:#}"));
        return;
    }
    match cleared() {
        Ok((entries, bytes)) => {
            // Written before the line that says so, so that a start cut off
            // in between does not clear the cache a second time.
            if let Err(e) = write_marker(image) {
                let _ = skyward_sys::kmsg(&format!(
                    "what ART compiled is cleared ({entries} entries, {bytes} bytes), and which image did it is not written: {e:#}"
                ));
                return;
            }
            let _ = skyward_sys::kmsg(&format!(
                "what ART compiled is cleared for {image} ({said}): {entries} entries, {bytes} bytes, to be compiled again as the system needs them"
            ));
        }
        Err(e) => {
            let _ = skyward_sys::kmsg(&format!("what ART compiled is not cleared: {said}, and clearing it failed: {e:#}"));
        }
    }
}

/// Delete what ART compiled, and give how many entries went and how many bytes
/// they held. A folder that is not there holds nothing to delete.
#[cfg(target_os = "android")]
fn cleared() -> Result<(u64, u64)> {
    let folders: Vec<PathBuf> = ART_CACHES.iter().map(PathBuf::from).collect();
    let mut last = skyward_files::Progress::default();
    // Never asked to stop: this runs before the pages are up, and a phone
    // that wanted it stopped has no way to say so yet.
    let cancel = std::sync::atomic::AtomicBool::new(false);
    skyward_files::delete_files(&folders, &cancel, &mut |p| last = p)?;
    Ok((last.entries, last.bytes))
}

/// What the marker holds, or `None` where there is none, or where persist
/// could not be read at all.
///
/// A recovery that cannot read persist still starts, so a failure here gives
/// `None` and the clearing is then done: clearing a cache twice costs one
/// recompilation, and never clearing it costs the fault this exists to answer.
#[cfg(target_os = "android")]
fn read_marker() -> Option<String> {
    let read = with_persist(|persist| {
        let marker = persist.at.join(MARKER);
        match fs::read_to_string(&marker) {
            Ok(held) => Ok(held),
            // No marker is a first start, which is not a failure to say
            // anything about.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("read {}", marker.display())),
        }
    });
    match read {
        Ok(held) => Some(held),
        Err(e) => {
            let _ = skyward_sys::kmsg(&format!("which image last cleared what ART compiled is not read: {e:#}"));
            None
        }
    }
}

/// Write down that this image cleared the cache.
#[cfg(target_os = "android")]
fn write_marker(image: &str) -> Result<()> {
    with_persist(|persist| {
        persist.writable(|at| {
            let marker = at.join(MARKER);
            let folder = marker.parent().expect("the marker is in a folder of its own");
            fs::create_dir_all(folder).with_context(|| format!("make {}", folder.display()))?;
            fs::write(&marker, marker_text(image)).with_context(|| format!("write {}", marker.display()))?;
            // Flushed out to the partition, so that a start cut off right
            // after this one still reads it back.
            fs::File::open(&marker)?.sync_all().with_context(|| format!("flush {}", marker.display()))
        })
    })
}

/// Persist, mounted where this opened it.
#[cfg(target_os = "android")]
struct Persist {
    at: PathBuf,
    /// Whether this opened the mount, and so has to take it away again: a
    /// mount the phone's own programs left is theirs to keep.
    ours: bool,
}

#[cfg(target_os = "android")]
impl Persist {
    /// Persist as it is mounted, mounting it read-only first where it is not.
    /// An earlier run of the recovery service may have left persist mounted,
    /// writable, where the phone's own programs use it; a second mount of the
    /// same device would be refused, so what is there is used as it is.
    fn open() -> Result<Persist> {
        let device = skyward_block::devnum(Path::new(PERSIST))?;
        if let Some(at) = skyward_block::mount::points_of(device)?.first() {
            return Ok(Persist { at: at.clone(), ours: false });
        }
        let at = PathBuf::from(MOUNT);
        fs::create_dir_all(&at).with_context(|| format!("make {}", at.display()))?;
        skyward_block::mount::mount_ro(Path::new(PERSIST), &at)?;
        Ok(Persist { at, ours: true })
    }

    /// Do `work` with persist writable, and make it read-only again where
    /// this made it. Only a few bytes are ever written: the partition holds
    /// the phone's own settings and is a few megabytes in all.
    fn writable<T>(&self, work: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
        if self.ours {
            skyward_block::mount::remount_writable(&self.at, true)?;
        }
        let done = work(&self.at);
        // A mount this did not make belongs to the phone's own programs, and
        // is left as it was found; it took this image's few bytes as it was.
        let back = match self.ours {
            true => skyward_block::mount::remount_writable(&self.at, false),
            false => Ok(()),
        };
        match (done, back) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(e)) => Err(e.context(format!("make the mount on {MOUNT} read-only again"))),
            (Err(e), _) => Err(e),
        }
    }
}

#[cfg(target_os = "android")]
impl Drop for Persist {
    /// Whatever went wrong above, the mount this made goes away: it is not
    /// one the phone's own programs would find again, and leaving it up would
    /// keep them from mounting persist themselves.
    fn drop(&mut self) {
        if self.ours {
            let _ = skyward_block::mount::umount(&self.at);
        }
    }
}

/// Do `work` with persist, mounted where it is not mounted already.
#[cfg(target_os = "android")]
fn with_persist<T>(work: impl FnOnce(&Persist) -> Result<T>) -> Result<T> {
    let persist = Persist::open()?;
    work(&persist)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_that_has_cleared_the_cache_does_not_clear_it_again() {
        assert_eq!(due(Some(&marker_text("0.2.3")), "0.2.3"), None);
        assert_eq!(due(Some("0.2.3\n"), "0.2.3"), None, "the newline a file ends with is not part of the version");
    }

    #[test]
    fn the_first_start_of_an_image_clears_the_cache() {
        assert_eq!(due(None, "0.2.3"), Some(Due::First), "nothing was ever recorded");
        assert_eq!(due(Some("0.2.2\n"), "0.2.3"), Some(Due::OtherVersion), "another image cleared it last");
    }

    #[test]
    fn a_marker_that_names_no_image_clears_the_cache() {
        // A marker left empty, or holding only white space, says an image
        // wrote it and not which: clearing costs one recompilation and is
        // the safe way to be wrong about it.
        assert_eq!(due(Some(""), "0.2.3"), Some(Due::Unreadable));
        assert_eq!(due(Some(" \n\t"), "0.2.3"), Some(Due::Unreadable));
        assert_eq!(due(Some("half a line"), "0.2.3"), Some(Due::OtherVersion), "a name that is no version still says an image ran");
    }

    #[test]
    fn what_one_start_reads_back_is_what_the_one_before_it_wrote() {
        let written = marker_text("0.2.3");
        assert_eq!(written, "0.2.3\n");
        assert_eq!(due(Some(&written), "0.2.3"), None, "the start after it clears nothing");
        assert_eq!(due(Some(&written), "0.2.4"), Some(Due::OtherVersion), "and a newer image clears it again");
    }
}
