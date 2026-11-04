//! Where the copy of a vbmeta block goes before it is changed, and the way
//! back written beside it.
//!
//! Turning the two checks off is the one thing this recovery does that can
//! leave a phone unable to start, and the copy is what undoes it. So the
//! copy may not live anywhere that goes with the next start, and the phone
//! says how to put it back in the same folder it kept it in.

use anyhow::{bail, Context, Result};
use skyward_vab::Slot;
use std::fs;
use std::path::{Path, PathBuf};

/// The folders a copy may go in, the best first.
const FOLDERS: [&str; 2] = ["/mnt/logfs/skyward/avb", "/sdcard/Skyward/avb"];

/// The first of [`FOLDERS`] that takes a file.
///
/// The log disk comes first: it is a partition of its own, not encrypted, and
/// writable from the start, so its copy is still readable on a phone that
/// will not start — which is what turning these checks off can leave behind.
/// Internal storage is second, where a computer sees it. A copy kept in the
/// recovery's own memory would be gone at the next start, which is exactly
/// when it is wanted, so a phone that offers neither folder is not changed at
/// all.
pub fn folder() -> Result<PathBuf> {
    let mut refused = Vec::new();
    for name in FOLDERS {
        let at = Path::new(name);
        if name.starts_with("/sdcard") && !skyward_sys::internal_storage_ready() {
            refused.push(format!("{name}: the data partition is not open"));
            continue;
        }
        match write_and_take_away(at) {
            Ok(()) => return Ok(at.to_path_buf()),
            Err(e) => refused.push(format!("{name}: {e:#}")),
        }
    }
    bail!("no folder would keep a copy of the blocks, so nothing was changed ({})", refused.join("; "))
}

/// The folder the copies were kept in, where one still holds any: what a
/// change is undone out of.
pub fn existing() -> Option<PathBuf> {
    FOLDERS.iter().map(PathBuf::from).find(|dir| holds_copies(dir))
}

/// Whether `dir` holds any of the copies [`folder`] hands to [`note`]'s
/// neighbours.
fn holds_copies(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.ends_with(".img") && name.contains("-0x")
            })
        })
        .unwrap_or(false)
}

/// Whether `at` can be made and written to: a file written and taken away
/// again, so a folder that only looks writable is not taken for one.
fn write_and_take_away(at: &Path) -> Result<()> {
    fs::create_dir_all(at).with_context(|| format!("make {}", at.display()))?;
    let mark = at.join(".skyward-probe");
    fs::write(&mark, b"probe").with_context(|| format!("write {}", mark.display()))?;
    fs::remove_file(&mark).with_context(|| format!("take {} away", mark.display()))
}

/// Say how to put a block back, beside the copies themselves: a phone that
/// will not start is read from another machine or from a shell, never from
/// the page that made the change.
pub fn note(folder: &Path, slot: Slot) -> Result<()> {
    const NOTE: &str = "\
Skyward turned the vbmeta checks off (or back on) and kept each block as it was first.
Every file is one whole vbmeta partition, named for the block and for the flags it
held before the change, and for when it was copied:

  vbmeta{suffix}-0x00000000-1793771814.img

Put one back from a recovery that can read this folder:

  dd if=<file>.img of=/dev/block/by-name/vbmeta{suffix}

Or with the phone in the bootloader:

  fastboot flash vbmeta{suffix} <file>.img

The same holds for the vbmeta_system and vbmeta_vendor files beside this one.
This folder is on the log disk, which is not encrypted: it stays readable when the
data partition cannot be opened, and it goes with neither a restart nor a wipe.
";
    let at = folder.join("RESTORE.txt");
    fs::write(&at, NOTE.replace("{suffix}", slot.suffix())).with_context(|| format!("write {}", at.display()))
}
