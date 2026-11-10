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
    partitions_in(Path::new(BY_NAME), slot)
}

/// [`partitions`], under `base`, where a test keeps stand-in partitions.
fn partitions_in(base: &Path, slot: Slot) -> Vec<PathBuf> {
    ["vbmeta", "vbmeta_system", "vbmeta_vendor"]
        .iter()
        .map(|name| base.join(format!("{name}{}", slot.suffix())))
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
    // The read-back below goes through the page cache, which would pass
    // even if the write had not reached the disk: this is what makes the
    // block survive the power going away mid-change.
    file.sync_all().with_context(|| format!("sync {}", block.path.display()))?;
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
///
/// The file and its parent directory are synced to disk before returning,
/// so the copy is durable even if the subsequent partition write is
/// interrupted by a power loss.
pub fn stash(block: &Block, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("{}-0x{:08x}-{at}.img", block.name(), block.flags()));
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        file.write_all(&block.bytes).with_context(|| format!("write {}", path.display()))?;
        file.flush().with_context(|| format!("flush {}", path.display()))?;
        file.sync_all().with_context(|| format!("sync {}", path.display()))?;
    }
    // Also sync the directory so the new directory entry is durable.
    if let Ok(dir_file) = std::fs::OpenOptions::new().read(true).open(dir) {
        let _ = dir_file.sync_all();
    }
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
    #[cfg(test)]
    if let Ok(base) = std::env::var("SKYWARD_VAB_TEST_BLOCKS") {
        return blocks_in(Path::new(&base), slot);
    }
    blocks_in(Path::new(BY_NAME), slot)
}

/// [`blocks`], out of `base`, where a test keeps stand-in partitions.
fn blocks_in(base: &Path, slot: Slot) -> Result<Vec<Block>> {
    let paths = partitions_in(base, slot);
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
///
/// Only blocks that are recognizably the same as the backup with only the
/// flags flipped are restored: a different ROM's vbmeta with the same flag
/// values must not be overwritten by an older backup.
///
/// All blocks that need restoration are checked for a valid matching
/// backup before any write takes place, so a missing or mismatched backup
/// on a later block does not leave earlier blocks partially changed.
///
/// Returns changes for blocks that were successfully restored. Blocks
/// already in the target state are reported with `kept: None`. The first
/// missing or mismatched backup stops further processing with no blocks
/// written yet.
pub fn restore_all(slot: Slot, dir: &Path) -> Result<Vec<Change>> {
    let all_blocks = blocks(slot)?;

    // First pass: identify blocks that need restoration and verify their
    // backups exist and match. No writes happen in this pass, so a
    // problem on the second or third block does not leave the first one
    // half-changed.
    let mut pending: Vec<(Block, Vec<u8>, PathBuf)> = Vec::new();
    let mut noops: Vec<Change> = Vec::new();

    for block in all_blocks {
        let name = block.name();
        let before = block.flags();
        // A block whose flags are already both on needs no restore.
        if before & (HASHTREE_DISABLED | VERIFICATION_DISABLED) == 0 {
            noops.push(Change { name, before, after: before, kept: None });
            continue;
        }
        let Some(copy) = newest_copy(dir, &name)? else {
            bail!(
                "{} has checks off but no backup is kept of what it held first; \
                 cannot restore it",
                name
            );
        };
        let kept = std::fs::read(&copy).with_context(|| format!("read {}", copy.display()))?;
        if kept.len() < HEADER || &kept[..4] != MAGIC {
            bail!("{} does not hold a vbmeta block", copy.display());
        }

        // Verify the current block is the backup with only the flags
        // changed to the "both off" value this recovery writes. If the
        // rest of the block differs (e.g. another ROM was installed),
        // the backup does not belong to the current system and must not
        // be written back.
        let mut expected = kept.clone();
        expected[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(
            &(HASHTREE_DISABLED | VERIFICATION_DISABLED).to_be_bytes()
        );
        if block.bytes != expected {
            bail!(
                "the backup of {} does not match the current block beyond \
                 the flags; it may belong to a different ROM and was not \
                 restored",
                name
            );
        }

        pending.push((block, kept, copy));
    }

    // Second pass: all backups verified; now actually restore each one.
    let mut changes = noops;
    for (mut block, kept, copy) in pending {
        let name = block.name();
        let before = block.flags();
        block.bytes = kept;
        let after = block.flags();
        store(&block)?;
        changes.push(Change { name, before, after, kept: Some(copy) });
    }
    Ok(changes)
}

/// Turn both checks back on: put the copy of each block back out of `dir`
/// where one was kept. Where no copy is kept, the blocks are left as they
/// are unless every one already says both checks are on: the flags a
/// phone's own build set are not always the two zeros this writes, and a
/// build that ships its hashtree off cannot start with them on.
///
/// If `dir` is provided, all backups are verified before any block is
/// written. After restoration, every block is checked to have both
/// checks enabled; if any restored backup itself shipped with a check
/// off, the operation fails rather than claiming success.
pub fn enable(slot: Slot, dir: Option<&Path>) -> Result<Vec<Change>> {
    if let Some(dir) = dir {
        let restored = restore_all(slot, dir)?;
        // After restoring from backup, verify every block actually has
        // both checks on. A backup taken from a build that ships its
        // hashtree off would not result in a fully enabled state.
        let all_on = restored.iter().all(|c| {
            c.after & (HASHTREE_DISABLED | VERIFICATION_DISABLED) == 0
        });
        if !all_on {
            let still_off: Vec<&str> = restored
                .iter()
                .filter(|c| c.after & (HASHTREE_DISABLED | VERIFICATION_DISABLED) != 0)
                .map(|c| c.name.as_str())
                .collect();
            bail!(
                "restored backups of {} still have at least one check off; \
                 cannot fully enable AVB",
                still_off.join(", ")
            );
        }
        if restored.iter().any(|c| c.kept.is_some()) {
            return Ok(restored);
        }
    }
    for block in blocks(slot)? {
        if block.flags() & (HASHTREE_DISABLED | VERIFICATION_DISABLED) != 0 {
            bail!(
                "no copy is kept of what {} held first, and a check is off in it; writing \
                 zeros there could leave the phone unable to start, so nothing was changed",
                block.name()
            );
        }
    }
    Ok(Vec::new())
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

    /// A stand-in vbmeta partition as a build ships it, with `flags`.
    fn shipped(dir: &Path, name: &str, flags: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 4096];
        bytes[..4].copy_from_slice(MAGIC);
        bytes[ALGORITHM_AT..ALGORITHM_AT + 4].copy_from_slice(&2u32.to_be_bytes());
        bytes[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(&flags.to_be_bytes());
        std::fs::write(dir.join(name), &bytes).unwrap();
        bytes
    }

    fn flags_of(path: &Path) -> u32 {
        let bytes = std::fs::read(path).unwrap();
        u32::from_be_bytes([bytes[FLAGS_AT], bytes[FLAGS_AT + 1], bytes[FLAGS_AT + 2], bytes[FLAGS_AT + 3]])
    }

    /// The two chains the recovery offers, over stand-in partitions, the
    /// way the phone would run them: the checks go off with a copy of each
    /// block kept, a package is installed with them off, and putting them
    /// back writes the copies whole. Where no copy is kept, a block with a
    /// check off is left as it is — writing zeros is only for blocks that
    /// already say both checks are on.
    #[test]
    fn the_change_is_made_and_taken_back_through_the_whole_chain() {
        let scratch = std::env::temp_dir().join(format!("skyward-avb-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let base = scratch.join("by-name");
        std::fs::create_dir_all(&base).unwrap();
        let factory = [
            ("vbmeta_a", shipped(&base, "vbmeta_a", 0)),
            ("vbmeta_system_a", shipped(&base, "vbmeta_system_a", 0)),
            ("vbmeta_vendor_a", shipped(&base, "vbmeta_vendor_a", 0)),
        ];
        std::env::set_var("SKYWARD_VAB_TEST_BLOCKS", &base);
        let both = HASHTREE_DISABLED | VERIFICATION_DISABLED;

        // Turning the checks off: each block changes only in its flag word,
        // and a whole copy of what it held goes somewhere that outlives it.
        let kept = scratch.join("kept");
        let changes = set_all(Slot::A, both, &kept).unwrap();
        assert_eq!(changes.len(), 3, "every block is told of");
        for (name, was) in &factory {
            let change = changes.iter().find(|c| c.name == *name).unwrap();
            let copy = change.kept.as_ref().expect("a copy was kept");
            assert_eq!(std::fs::read(copy).unwrap(), *was, "the copy is the block as shipped");
            assert_eq!(flags_of(&base.join(name)), both, "{name} has both checks off");
        }

        // A package is installed with the checks off; the checks the
        // bootloader would make are what the flags say they are.
        let readings = read(Slot::A).unwrap();
        assert!(readings.iter().all(|r| !r.verity() && !r.verification()));

        // Putting them back: the copies go back whole, the phone's own
        // flags and not zeros, and the copies stay where they were.
        let back = enable(Slot::A, Some(&kept)).unwrap();
        assert_eq!(back.len(), 3, "every block is put back");
        for (name, was) in &factory {
            assert_eq!(std::fs::read(base.join(name)).unwrap(), *was, "{name} is as it shipped");
        }

        // With every block already on, a further enable has nothing to do.
        assert!(enable(Slot::A, Some(&kept)).unwrap().is_empty());

        // A build that ships its hashtree off, and no copy kept: the block
        // is left as it is rather than written zeros it cannot start with.
        shipped(&base, "vbmeta_a", HASHTREE_DISABLED);
        let refused = enable(Slot::A, None).unwrap_err();
        assert!(refused.to_string().contains("vbmeta_a"), "{refused:#}");
        assert_eq!(flags_of(&base.join("vbmeta_a")), HASHTREE_DISABLED, "the block was not written");

        // An update package wrote the block again, with its own flags: it
        // is not dragged back over, which would count the phone backwards.
        shipped(&base, "vbmeta_a", 0);
        let untouched = enable(Slot::A, Some(&kept)).unwrap();
        assert!(untouched.is_empty(), "a block that does not say the recovery left it saying is skipped");
        assert_eq!(flags_of(&base.join("vbmeta_a")), 0);

        std::env::remove_var("SKYWARD_VAB_TEST_BLOCKS");
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// F01: a different ROM's vbmeta with the same flags=3 must NOT be
    /// overwritten by an older backup taken from another ROM. The backup
    /// only matches if the entire block is identical except for the flags
    /// word having been flipped to "both off" by this recovery.
    #[test]
    fn restore_rejects_foreign_rom_with_same_flags() {
        let scratch = std::env::temp_dir().join(format!("skyward-avb-foreign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let base = scratch.join("by-name");
        std::fs::create_dir_all(&base).unwrap();

        // ROM A: original vbmeta (algorithm=2, flags=0).
        shipped(&base, "vbmeta_a", 0);
        std::env::set_var("SKYWARD_VAB_TEST_BLOCKS", &base);
        let both = HASHTREE_DISABLED | VERIFICATION_DISABLED;

        // Turn AVB off under ROM A — this creates the backup.
        let kept = scratch.join("kept");
        let changes = set_all(Slot::A, both, &kept).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(flags_of(&base.join("vbmeta_a")), both);

        // ROM B is installed: same flags=3 but different body (algorithm=3
        // instead of 2, and different rollback/index bytes further in).
        let mut rom_b = vec![0u8; 4096];
        rom_b[..4].copy_from_slice(MAGIC);
        rom_b[ALGORITHM_AT..ALGORITHM_AT + 4].copy_from_slice(&3u32.to_be_bytes());
        rom_b[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(&both.to_be_bytes());
        rom_b[200] = 0xAB; // different body content
        std::fs::write(base.join("vbmeta_a"), &rom_b).unwrap();

        // Enable AVB should fail: the backup belongs to ROM A, not ROM B.
        let result = enable(Slot::A, Some(&kept));
        assert!(result.is_err(), "foreign ROM block must not be overwritten by old backup");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("does not match"), "error should mention mismatch, got: {err}");

        // The block must be unchanged — still ROM B's content.
        let current = std::fs::read(base.join("vbmeta_a")).unwrap();
        assert_eq!(current, rom_b, "foreign block was not left untouched");

        std::env::remove_var("SKYWARD_VAB_TEST_BLOCKS");
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// F16: when only some blocks have backups and others don't, the
    /// operation must fail before writing any block, so we never end up
    /// with a partial restore.
    #[test]
    fn partial_backup_fails_before_writing_anything() {
        let scratch = std::env::temp_dir().join(format!("skyward-avb-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let base = scratch.join("by-name");
        std::fs::create_dir_all(&base).unwrap();

        let both = HASHTREE_DISABLED | VERIFICATION_DISABLED;
        // Three blocks, all off.
        let vbmeta = shipped(&base, "vbmeta_a", both);
        let vbmeta_sys = shipped(&base, "vbmeta_system_a", both);
        let _vbmeta_vnd = shipped(&base, "vbmeta_vendor_a", both);

        std::env::set_var("SKYWARD_VAB_TEST_BLOCKS", &base);
        let kept = scratch.join("kept");

        // Make a backup for only the first two blocks (simulating a case
        // where vendor was never backed up).
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::write(kept.join("vbmeta_a-0x00000003-1000.img"), &vbmeta).unwrap();
        std::fs::write(kept.join("vbmeta_system_a-0x00000003-1000.img"), &vbmeta_sys).unwrap();

        // Enable should fail: vbmeta_vendor_a has checks off but no backup.
        let result = enable(Slot::A, Some(&kept));
        assert!(result.is_err(), "missing backup for a disabled block must fail");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("vbmeta_vendor_a"), "error should name the missing block, got: {err}");

        // No block should have been written — all three are still flags=3.
        assert_eq!(flags_of(&base.join("vbmeta_a")), both, "first block was written despite later failure");
        assert_eq!(flags_of(&base.join("vbmeta_system_a")), both, "second block was written despite later failure");
        assert_eq!(flags_of(&base.join("vbmeta_vendor_a")), both, "third block was changed");

        std::env::remove_var("SKYWARD_VAB_TEST_BLOCKS");
        std::fs::remove_dir_all(&scratch).unwrap();
    }
}
