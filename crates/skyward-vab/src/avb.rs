//! The Android Verified Boot block a partition carries, and the flags in it
//! that say whether the bootloader and the kernel check what they read.
//!
//! Turning them off lets a build that is not the phone's own start, at the
//! price of nothing checking that what starts is what was meant to. The
//! blocks are signed, so a flag changed here no longer matches its
//! signature; an unlocked bootloader does not look at that, a locked one
//! does.

use crate::Slot;
use anyhow::{bail, Context, Result};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Where the phone names its partitions.
pub const BY_NAME: &str = "/dev/block/by-name";

/// The flags, as AVB 2.0 numbers them.
pub const HASHTREE_DISABLED: u32 = 1;
pub const VERIFICATION_DISABLED: u32 = 2;

const MAGIC: &[u8; 4] = b"AVB0";
const ALGORITHM_AT: usize = 28;
const FLAGS_AT: usize = 120;
const HEADER: usize = 256;

/// What the header's algorithm field says.
pub fn algorithm_name(algorithm: u32) -> &'static str {
    match algorithm {
        0 => "none",
        1 => "sha256_rsa2048",
        2 => "sha256_rsa4096",
        3 => "sha256_rsa8192",
        _ => "other",
    }
}

/// One vbmeta block, whole, as its partition holds it.
pub struct Block {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

impl Block {
    /// The flag word, big-endian like the rest of the header.
    pub fn flags(&self) -> u32 {
        u32::from_be_bytes([self.bytes[FLAGS_AT], self.bytes[FLAGS_AT + 1], self.bytes[FLAGS_AT + 2], self.bytes[FLAGS_AT + 3]])
    }

    pub fn algorithm(&self) -> u32 {
        u32::from_be_bytes([self.bytes[ALGORITHM_AT], self.bytes[ALGORITHM_AT + 1], self.bytes[ALGORITHM_AT + 2], self.bytes[ALGORITHM_AT + 3]])
    }

    /// Whether the kernel checks the filesystems this block describes.
    pub fn verity(&self) -> bool {
        self.flags() & HASHTREE_DISABLED == 0
    }

    /// Whether the bootloader checks what it starts.
    pub fn verification(&self) -> bool {
        self.flags() & VERIFICATION_DISABLED == 0
    }

    pub fn set_flags(&mut self, flags: u32) {
        self.bytes[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(&flags.to_be_bytes());
    }

    /// The name the partition goes by, for telling one from another.
    pub fn name(&self) -> String {
        self.path.file_name().and_then(|n| n.to_str()).unwrap_or("vbmeta").to_string()
    }
}

/// The vbmeta partitions this phone keeps for `slot`: its own, and those a
/// chain in it points at. A phone that keeps none of the others simply has
/// fewer entries.
pub fn partitions(slot: Slot) -> Vec<PathBuf> {
    ["vbmeta", "vbmeta_system", "vbmeta_vendor"]
        .iter()
        .map(|name| Path::new(BY_NAME).join(format!("{name}{}", slot.suffix())))
        .filter(|path| path.exists())
        .collect()
}

/// Read a whole vbmeta partition.
pub fn open(path: &Path) -> Result<Block> {
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if bytes.len() < HEADER {
        bail!("{} holds {} bytes, too few for a vbmeta header", path.display(), bytes.len());
    }
    if &bytes[..4] != MAGIC {
        bail!("{} does not begin a vbmeta block: {:02x?}", path.display(), &bytes[..4]);
    }
    Ok(Block { path: path.to_path_buf(), bytes })
}

/// Write a block back whole, then read it again to be sure the partition
/// holds what was meant for it.
pub fn store(block: &Block) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(&block.path)
        .with_context(|| format!("open {} to write", block.path.display()))?;
    file.write_all(&block.bytes)
        .with_context(|| format!("write {}", block.path.display()))?;
    file.flush().with_context(|| format!("flush {}", block.path.display()))?;
    drop(file);
    let back = open(&block.path)?;
    if back.bytes != block.bytes {
        bail!("{} does not hold what was written to it", block.path.display());
    }
    Ok(())
}

/// Keep a copy of a block beside the recovery's own files, so what it held
/// before a change is not lost. The name carries the flags it was taken at
/// and the second it was taken, so a later change keeps its own copy rather
/// than writing over the one taken before the first.
pub fn stash(block: &Block, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("{}-0x{:08x}-{at}.img", block.name(), block.flags()));
    std::fs::write(&path, &block.bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// What one block says about the checks it names, for a page or a command
/// line to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub name: String,
    pub flags: u32,
    pub algorithm: u32,
}

impl Reading {
    /// Whether the kernel checks the filesystems this block describes.
    pub fn verity(&self) -> bool {
        self.flags & HASHTREE_DISABLED == 0
    }

    /// Whether the bootloader checks what it starts.
    pub fn verification(&self) -> bool {
        self.flags & VERIFICATION_DISABLED == 0
    }

    /// Whether both checks are on, as the phone came from its maker.
    pub fn whole(&self) -> bool {
        self.verity() && self.verification()
    }
}

/// A block whose flags were set, and what they said before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub name: String,
    pub before: u32,
    pub after: u32,
    /// Where a copy of the block as it was went, when it was written.
    pub kept: Option<PathBuf>,
}

/// Every vbmeta block the slot keeps, read whole.
fn blocks(slot: Slot) -> Result<Vec<Block>> {
    let paths = partitions(slot);
    if paths.is_empty() {
        bail!("no vbmeta partition for slot {}", slot.letter());
    }
    paths.iter().map(|p| open(p)).collect()
}

/// Read what every vbmeta block of `slot` says about the checks it names.
pub fn read(slot: Slot) -> Result<Vec<Reading>> {
    blocks(slot)?
        .iter()
        .map(|b| Ok(Reading { name: b.name(), flags: b.flags(), algorithm: b.algorithm() }))
        .collect()
}

/// Put back what each block held before it was changed, out of the copies
/// kept under `dir`.
///
/// The flags a phone's own build sets are not always the two this recovery
/// writes: a build that ships its hashtree off and its verification on
/// cannot be brought back by writing zeros. So undoing a change means
/// writing the copy back whole, not a number chosen here.
pub fn restore_all(slot: Slot, dir: &Path) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for mut block in blocks(slot)? {
        let name = block.name();
        let Some(copy) = newest_copy(dir, &name)? else { continue };
        let kept = std::fs::read(&copy).with_context(|| format!("read {}", copy.display()))?;
        if kept.len() < HEADER || &kept[..4] != MAGIC {
            bail!("{} does not hold a vbmeta block", copy.display());
        }
        let before = block.flags();
        block.bytes = kept;
        let after = block.flags();
        store(&block)?;
        changes.push(Change { name, before, after, kept: Some(copy) });
    }
    Ok(changes)
}

/// The copy of `name` taken most recently under `dir`, out of the files
/// [`stash`] writes, which are named for the block, the flags it held and
/// the second it was taken.
fn newest_copy(dir: &Path, name: &str) -> Result<Option<PathBuf>> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(None) };
    let mut newest: Option<(u64, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(rest) = file.strip_prefix(name).and_then(|r| r.strip_prefix("-0x")) else { continue };
        let Some((_, stamp)) = rest.split_once('-') else { continue };
        let Ok(seconds) = stamp.trim_end_matches(".img").parse::<u64>() else { continue };
        if newest.as_ref().is_none_or(|(best, _)| seconds > *best) {
            newest = Some((seconds, path));
        }
    }
    Ok(newest.map(|(_, path)| path))
}

/// Set the same flags on every vbmeta block of `slot`, keeping a copy of
/// each one it changes first: what a block held before is not to be lost
/// with the change. Blocks that already say `flags` are left untouched, and
/// still told of.
pub fn set_all(slot: Slot, flags: u32, kept: &Path) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for mut block in blocks(slot)? {
        let before = block.flags();
        let name = block.name();
        if before == flags {
            changes.push(Change { name, before, after: before, kept: None });
            continue;
        }
        let copy = stash(&block, kept)?;
        block.set_flags(flags);
        store(&block)?;
        changes.push(Change { name, before, after: flags, kept: Some(copy) });
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Vec<u8> {
        let mut bytes = vec![0u8; 4096];
        bytes[..4].copy_from_slice(MAGIC);
        bytes[ALGORITHM_AT..ALGORITHM_AT + 4].copy_from_slice(&1u32.to_be_bytes());
        bytes
    }

    #[test]
    fn the_flags_are_read_and_written_big_endian() {
        let mut block = Block { path: PathBuf::from("x"), bytes: header() };
        assert_eq!(block.flags(), 0);
        assert!(block.verity() && block.verification());
        block.set_flags(HASHTREE_DISABLED | VERIFICATION_DISABLED);
        assert_eq!(block.flags(), 3);
        assert!(!block.verity() && !block.verification());
        assert_eq!(&block.bytes[FLAGS_AT..FLAGS_AT + 4], &[0, 0, 0, 3]);
    }

    #[test]
    fn only_the_flags_change() {
        let mut block = Block { path: PathBuf::from("x"), bytes: header() };
        let before = block.bytes.clone();
        block.set_flags(2);
        let differing: Vec<usize> = (0..before.len()).filter(|i| before[*i] != block.bytes[*i]).collect();
        assert_eq!(differing, vec![FLAGS_AT + 3], "only the lowest byte of the flag word moves");
    }

    #[test]
    fn a_partition_that_is_not_vbmeta_is_refused() {
        let dir = std::env::temp_dir().join(format!("skyward-avb-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("not-vbmeta");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        assert!(open(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
