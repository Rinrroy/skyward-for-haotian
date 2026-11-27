//! avbctl as install scripts run it before they write a system that
//! verified boot would turn away: to tell, and to turn on or off,
//! dm-verity, which checks each block of a partition as it is read, and
//! verification, the bootloader's checking of the partitions at all. Each
//! is a flag in the header of a top-level vbmeta image, which an unlocked
//! bootloader honours: the image of the slot the phone runs from, and, on a
//! phone with two slots, the other slot's as well, so that turning a check
//! off before writing the slot the phone is not running from leaves the
//! bootloader nothing to turn away when it starts that one. What it prints
//! and how it ends are what those scripts read.

use super::Io;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// How avbctl ends, numbered as sysexits.h numbers them.
const DONE: i32 = 0;
const MISUSED: i32 = 64;
const FAILED: i32 = 70;
const REFUSED: i32 = 77;

/// A vbmeta image's header: 256 bytes starting with its magic, its flags a
/// big-endian 32-bit word 120 bytes in.
const HEADER_LEN: usize = 256;
const MAGIC: &[u8] = b"AVB0";
const FLAGS_AT: usize = 120;

/// The footer ending a partition whose image carries its vbmeta image
/// along: 64 bytes starting with its magic, where the vbmeta image starts
/// in the partition a big-endian 64-bit word 20 bytes in, and how long it
/// says the image is one 28 bytes in.
const FOOTER_LEN: usize = 64;
const FOOTER_MAGIC: &[u8] = b"AVBf";
const VBMETA_AT: usize = 20;
const VBMETA_SIZE_AT: usize = 28;

const USAGE: &str = "\
avbctl - command-line tool for AVB.

Usage:
  avbctl [--force] COMMAND

Commands:
  avbctl get-verity           - Prints whether verity is enabled in current slot.
  avbctl disable-verity       - Disable verity in current slot.
  avbctl enable-verity        - Enable verity in current slot.
  avbctl get-verification     - Prints whether verification is enabled in current slot.
  avbctl disable-verification - Disable verification in current slot.
  avbctl enable-verification  - Enable verification in current slot.
";

const LOCKED: &str = "\
avbctl: the bootloader is locked, and a locked bootloader does not boot a slot
whose vbmeta image has been changed. Run with --force to change it all the same.
";

/// What avbctl turns on and off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Check {
    Verity,
    Verification,
}

impl Check {
    fn name(self) -> &'static str {
        match self {
            Check::Verity => "verity",
            Check::Verification => "verification",
        }
    }

    /// The flag in the header that turns it off.
    fn off(self) -> u32 {
        match self {
            Check::Verity => 1 << 0,
            Check::Verification => 1 << 1,
        }
    }
}

/// What avbctl is asked: whether a check is on, or to turn it on or off.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Command {
    Get(Check),
    Set(Check, bool),
}

/// The phone avbctl works on: the folder its partitions are named in, the
/// suffix of the slot it runs from, empty on a phone with one slot, whether
/// its bootloader is locked, and whether it has two slots at all.
struct Phone<'a> {
    by_name: &'a Path,
    suffix: &'a str,
    locked: bool,
    ab: bool,
}

pub(super) fn run(args: &[OsString], io: &mut Io) -> i32 {
    let suffix = skyward_sys::prop("ro.boot.slot_suffix").unwrap_or_default();
    let locked = skyward_sys::prop("ro.boot.vbmeta.device_state").as_deref() == Some("locked");
    let ab = two_slots(&suffix);
    avbctl(args, io, &Phone { by_name: Path::new(skyward_block::BY_NAME), suffix: &suffix, locked, ab })
}

/// Whether the phone has two slots, as its own configuration says. A
/// configuration that cannot be read leaves the bootloader's word for it,
/// which names a slot only where there are two.
fn two_slots(suffix: &str) -> bool {
    let config = Path::new("/").join(skyward_config::ramdisk::CONFIG);
    match skyward_config::DeviceConfig::load(&config) {
        Ok(configured) => configured.device.ab,
        Err(e) => {
            // A diagnostic may never fail what it tells of.
            let _ = skyward_sys::kmsg(&format!("avbctl: {e}; going by the slot the bootloader booted from"));
            !suffix.is_empty()
        }
    }
}

/// The slots a change covers: both of a phone with two, so that turning a
/// check off before writing the slot the phone is not running from leaves the
/// bootloader nothing to turn away when it starts that one; and the one slot
/// of a phone with one, which is the slot the bootloader booted from.
fn slots(phone: &Phone) -> &'static [&'static str] {
    if phone.ab {
        return &["_a", "_b"];
    }
    match phone.suffix {
        "_a" => &["_a"],
        "_b" => &["_b"],
        _ => &[""],
    }
}

fn avbctl(args: &[OsString], io: &mut Io, phone: &Phone) -> i32 {
    let (mut force, mut command) = (false, None);
    // Of the commands given the last counts, and anything else is passed
    // over.
    for arg in args {
        match arg.to_str() {
            Some("--force") => force = true,
            Some("get-verity") => command = Some(Command::Get(Check::Verity)),
            Some("disable-verity") => command = Some(Command::Set(Check::Verity, false)),
            Some("enable-verity") => command = Some(Command::Set(Check::Verity, true)),
            Some("get-verification") => command = Some(Command::Get(Check::Verification)),
            Some("disable-verification") => command = Some(Command::Set(Check::Verification, false)),
            Some("enable-verification") => command = Some(Command::Set(Check::Verification, true)),
            _ => {}
        }
    }
    let Some(command) = command else {
        let _ = io.errors.write_all(USAGE.as_bytes());
        return MISUSED;
    };
    let (Command::Get(check) | Command::Set(check, _)) = command;
    let slot = match phone.suffix {
        "" => String::new(),
        suffix => format!(" on slot with suffix {suffix}"),
    };
    let state = |on: bool| if on { "enabled" } else { "disabled" };
    match command {
        Command::Get(_) => match is_on(phone, check) {
            Ok(on) => say(io, &format!("{} is {}{slot}.\n", check.name(), state(on))),
            Err(e) => unread(io, check, e),
        },
        Command::Set(_, want) => {
            // Every vbmeta a change covers is read before one is written: the
            // slot the phone booted from may already hold what was asked for
            // while the other one does not, and that is not a reason to leave
            // the other one as it was.
            let already = match all_are(phone, check, want) {
                Ok(already) => already,
                Err(e) => return unread(io, check, e),
            };
            if already {
                return say(io, &format!("{} is already {}{slot}.\n", check.name(), state(want)));
            }
            if phone.locked && !force {
                let _ = io.errors.write_all(LOCKED.as_bytes());
                return REFUSED;
            }
            if let Err(e) = set(phone, check, want) {
                let _ = writeln!(io.errors, "avbctl: {e}");
                let _ = writeln!(io.errors, "Error setting {}.", check.name());
                return FAILED;
            }
            say(io, &format!("Successfully {} {}{slot}. Reboot the device for changes to take effect.\n", state(want), check.name()))
        }
    }
}

/// Tell why a check could not be read, in the words scripts match on.
fn unread(io: &mut Io, check: Check, e: io::Error) -> i32 {
    let _ = writeln!(io.errors, "avbctl: {e}");
    let _ = writeln!(io.errors, "Error getting whether {} is enabled.", check.name());
    FAILED
}

fn say(io: &mut Io, text: &str) -> i32 {
    match io.output.write_all(text.as_bytes()).and_then(|()| io.output.flush()) {
        Ok(()) => DONE,
        Err(_) => FAILED,
    }
}

/// Whether `check` is on in the slot's top-level vbmeta image.
fn is_on(phone: &Phone, check: Check) -> io::Result<bool> {
    let located = top_level(phone.by_name, phone.suffix)?;
    let mut file = File::open(&located.path).map_err(|e| about(&located.path, e))?;
    let header = header(&mut file, &located.path, located.at)?;
    Ok(flags(&header) & check.off() == 0)
}

/// Whether every vbmeta a change covers already holds `on`.
fn all_are(phone: &Phone, check: Check, on: bool) -> io::Result<bool> {
    for suffix in slots(phone) {
        let located = top_level(phone.by_name, suffix)?;
        let mut file = File::open(&located.path).map_err(|e| about(&located.path, e))?;
        let header = header(&mut file, &located.path, located.at)?;
        if (flags(&header) & check.off() == 0) != on {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Turn `check` on or off in every vbmeta the phone holds for its slots,
/// leaving the rest of each as it was — or in none of them: every target is
/// opened and held before one is written, and a write that does not read back
/// as it was written takes the ones already written back with it.
fn set(phone: &Phone, check: Check, on: bool) -> io::Result<()> {
    let mut targets = plan(phone, check, on)?;
    commit(&mut targets)
}

/// A partition to change: its file, opened and held against other writers,
/// where in it the image starts, the header it holds now, and the header it
/// must hold after the change.
struct Target {
    path: PathBuf,
    at: u64,
    file: File,
    before: [u8; HEADER_LEN],
    after: [u8; HEADER_LEN],
}

/// Every target a change covers, opened and held and read before any of them
/// is written: a phone that cannot give one of them is told before anything
/// has changed rather than after.
fn plan(phone: &Phone, check: Check, on: bool) -> io::Result<Vec<Target>> {
    let mut targets: Vec<Target> = Vec::new();
    for suffix in slots(phone) {
        let located = top_level(phone.by_name, suffix)?;
        // One partition may be the top-level image of both slots, with each
        // slot's own images chained under it, and is then one target: a second
        // open of it would be the first write's rival.
        if targets.iter().any(|target| target.path == located.path && target.at == located.at) {
            continue;
        }
        targets.push(open(&located, check, on)?);
    }
    Ok(targets)
}

/// `located` opened for writing and held against every other writer, with the
/// header it holds now and the header it must hold after the change.
fn open(located: &Located, check: Check, on: bool) -> io::Result<Target> {
    let path = &located.path;
    // The header is the whole of what a flag change writes, and it has to be
    // inside the partition: a damaged footer may not put the write past the
    // end of it.
    let fits = located.at.checked_add(HEADER_LEN as u64).is_some_and(|end_of_header| end_of_header <= located.end);
    if !fits {
        return Err(io::Error::new(ErrorKind::InvalidData, format!("{} has no room for a vbmeta header at {}", path.display(), located.at)));
    }
    let mut file = OpenOptions::new().read(true).write(true).open(path).map_err(|e| about(path, e))?;
    hold(&file, path)?;
    let before = header(&mut file, path, located.at)?;
    let flags = match on {
        true => flags(&before) & !check.off(),
        false => flags(&before) | check.off(),
    };
    let mut after = before;
    after[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(&flags.to_be_bytes());
    Ok(Target { path: path.clone(), at: located.at, file, before, after })
}

/// Hold `file` against every other writer of the same partition. A writer that
/// is already there is reported rather than waited for: two installs racing on
/// one vbmeta is not a state to spin in.
fn hold(file: &File, path: &Path) -> io::Result<()> {
    // The descriptor is open, so it is one `flock` may be given.
    match unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } {
        0 => Ok(()),
        _ => {
            let e = io::Error::last_os_error();
            Err(io::Error::new(e.kind(), format!("{} is held by another writer ({e})", path.display())))
        }
    }
}

/// Write every target's header and read it back, or put back the ones that
/// were written and answer with the failure that stopped the change.
fn commit(targets: &mut [Target]) -> io::Result<()> {
    let mut next = 0;
    while next < targets.len() {
        // The target the change has come to, which is one of the targets' own
        // places: the loop ends with the last of them.
        let failed = match targets.get_mut(next) {
            Some(target) => write(target).err(),
            None => break,
        };
        if let Some(e) = failed {
            // The ones before this were written, and this one may be half
            // written: all of them are put back, the last first.
            return Err(restore(targets, next + 1, e));
        }
        next += 1;
    }
    Ok(())
}

/// Write one target's header, flush it, and read it back.
fn write(target: &mut Target) -> io::Result<()> {
    if target.before == target.after {
        // Already what it must be: nothing to write, and nothing to read back.
        return Ok(());
    }
    let after = target.after;
    put(target, &after)
}

/// Put one target's header back as it was, flush it, and read it back.
fn put_back(target: &mut Target) -> io::Result<()> {
    if target.before == target.after {
        // Nothing was written to it, so nothing is put back.
        return Ok(());
    }
    let before = target.before;
    put(target, &before)
}

/// Write `wanted` over the header of `target`'s partition, flush it, and read
/// it back: a write that does not read back as what was written is a failure
/// like any other.
fn put(target: &mut Target, wanted: &[u8; HEADER_LEN]) -> io::Result<()> {
    let Target { path, at, file, .. } = target;
    file.seek(SeekFrom::Start(*at)).and_then(|_| file.write_all(wanted)).and_then(|()| file.sync_all()).map_err(|e| about(path, e))?;
    let read_back = header(file, path, *at)?;
    match read_back == *wanted {
        true => Ok(()),
        false => Err(io::Error::new(ErrorKind::InvalidData, format!("{} did not read back as it was written", path.display()))),
    }
}

/// Put back the headers of the first `written` targets, the last first, and
/// answer with `first` — the failure that stopped the change — with what went
/// wrong putting them back appended to it.
fn restore(targets: &mut [Target], written: usize, first: io::Error) -> io::Error {
    let mut failures = Vec::new();
    for target in targets.iter_mut().take(written).rev() {
        if let Err(e) = put_back(target) {
            failures.push(e.to_string());
        }
    }
    match failures.is_empty() {
        true => first,
        false => io::Error::new(first.kind(), format!("{first}; putting back what was written failed: {}", failures.join("; "))),
    }
}

/// Where a slot's top-level vbmeta image is: the partition it is in, where in
/// it the image starts, and where the room it may occupy ends — the end of the
/// partition, or, for an image a footer placed, the start of that footer.
struct Located {
    path: PathBuf,
    at: u64,
    end: u64,
}

/// Where the slot's top-level vbmeta image starts: at the start of its vbmeta
/// partition, or, on a phone without one, in its boot partition where the
/// footer ending that partition says.
///
/// A phone with two slots may name a vbmeta partition after each slot, or keep
/// one image for both and chain each slot's own images under it; both are
/// looked for, the slot's own first.
///
/// The numbers a footer gives are checked against the partition it ends before
/// they are used: a damaged footer may not put the image, and so a write, past
/// the end of the partition.
fn top_level(by_name: &Path, suffix: &str) -> io::Result<Located> {
    // The suffix names a partition, so nothing but a slot's own name may reach
    // the path.
    if !matches!(suffix, "" | "_a" | "_b") {
        return Err(io::Error::other(format!("{suffix:?} is not a slot suffix")));
    }
    let named = by_name.join(format!("vbmeta{suffix}"));
    let shared = by_name.join("vbmeta");
    for vbmeta in [&named, &shared] {
        if vbmeta.exists() {
            return Ok(Located { end: length(vbmeta)?, path: vbmeta.clone(), at: 0 });
        }
    }
    let boot = by_name.join(format!("boot{suffix}"));
    let mut file = File::open(&boot).map_err(|e| about(&boot, e))?;
    // How long the partition is, as the kernel says: a block device's length
    // is not a file's, and seeking to its end is the one way to ask both.
    let len = file.seek(SeekFrom::End(0)).map_err(|e| about(&boot, e))?;
    // The image may occupy what comes before the footer, and no more: the
    // footer is the partition's last 64 bytes.
    let end = len.checked_sub(FOOTER_LEN as u64).ok_or_else(|| missing(&named, &boot))?;
    let mut footer = [0; FOOTER_LEN];
    file.seek(SeekFrom::Start(end)).and_then(|_| file.read_exact(&mut footer)).map_err(|e| about(&boot, e))?;
    if !footer.starts_with(FOOTER_MAGIC) {
        return Err(missing(&named, &boot));
    }
    // Fixed places in a 64-byte footer, so both are eight bytes.
    let at = u64::from_be_bytes(footer[VBMETA_AT..VBMETA_AT + 8].try_into().expect("eight bytes"));
    let size = u64::from_be_bytes(footer[VBMETA_SIZE_AT..VBMETA_SIZE_AT + 8].try_into().expect("eight bytes"));
    // What the footer says the image takes has to be inside the partition it
    // ends, and long enough to hold the header that is to be written into it.
    let inside = size >= HEADER_LEN as u64 && at.checked_add(size).is_some_and(|end_of_image| end_of_image <= end);
    if !inside {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("{} has a vbmeta footer that puts the image outside the partition", boot.display()),
        ));
    }
    Ok(Located { path: boot, at, end })
}

/// How long `path` is, as the kernel says: a block device's length is not a
/// file's, and seeking to its end is the one way to ask about both.
fn length(path: &Path) -> io::Result<u64> {
    let mut file = File::open(path).map_err(|e| about(path, e))?;
    file.seek(SeekFrom::End(0)).map_err(|e| about(path, e))
}

/// The error for a slot with neither a vbmeta partition of its own nor a boot
/// partition ending in a footer that says where its image is.
fn missing(vbmeta: &Path, boot: &Path) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, format!("there is no {}, and {} does not end in a vbmeta footer", vbmeta.display(), boot.display()))
}

/// The vbmeta header `at` bytes into `file`, the partition at `path`.
fn header(file: &mut File, path: &Path, at: u64) -> io::Result<[u8; HEADER_LEN]> {
    let mut header = [0; HEADER_LEN];
    file.seek(SeekFrom::Start(at)).and_then(|_| file.read_exact(&mut header)).map_err(|e| about(path, e))?;
    if !header.starts_with(MAGIC) {
        return Err(io::Error::new(ErrorKind::InvalidData, format!("{} holds no vbmeta header {at} bytes in", path.display())));
    }
    Ok(header)
}

fn flags(header: &[u8; HEADER_LEN]) -> u32 {
    u32::from_be_bytes(header[FLAGS_AT..FLAGS_AT + 4].try_into().expect("four bytes"))
}

/// `e`, saying which partition it came from.
fn about(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A vbmeta image whose flags are `flags`: its header, and the
    /// authentication block after it, which avbctl leaves as it is. The header
    /// says how long both variable blocks are, as a real image's does, so that
    /// the extent the image occupies is the extent these bytes are.
    fn vbmeta(flags: u32) -> Vec<u8> {
        let (auth, aux) = (64, 64);
        let mut image = vec![0u8; 256 + auth + aux];
        image[..4].copy_from_slice(b"AVB0");
        image[12..20].copy_from_slice(&(auth as u64).to_be_bytes());
        image[20..28].copy_from_slice(&(aux as u64).to_be_bytes());
        image[112..120].copy_from_slice(&7u64.to_be_bytes());
        image[120..124].copy_from_slice(&flags.to_be_bytes());
        image[128..141].copy_from_slice(b"avbtool 1.3.0");
        for (i, b) in image[256..].iter_mut().enumerate() {
            *b = i as u8 + 1;
        }
        image
    }

    fn ran(args: &[&str], phone: &Phone) -> (i32, String, String) {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let code = avbctl(&args, &mut Io { input: &mut io::empty(), output: &mut output, errors: &mut errors }, phone);
        (code, String::from_utf8(output).unwrap(), String::from_utf8(errors).unwrap())
    }

    #[test]
    fn verity_and_verification_are_flags_in_the_header_of_the_slot_s_vbmeta_image() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("vbmeta_a"), dir.path().join("vbmeta_b"));
        fs::write(&a, vbmeta(0)).unwrap();
        fs::write(&b, vbmeta(0)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: false, ab: false };
        assert_eq!(ran(&["get-verity"], &phone), (0, "verity is enabled on slot with suffix _a.\n".into(), String::new()));
        assert_eq!(
            ran(&["--force", "disable-verity"], &phone),
            (0, "Successfully disabled verity on slot with suffix _a. Reboot the device for changes to take effect.\n".into(), String::new())
        );
        assert_eq!(fs::read(&a).unwrap(), vbmeta(1), "the verity flag set and nothing else");
        assert_eq!(ran(&["disable-verity"], &phone), (0, "verity is already disabled on slot with suffix _a.\n".into(), String::new()));
        assert_eq!(ran(&["disable-verification", "--force"], &phone).0, 0, "the command and --force in either order");
        assert_eq!(fs::read(&a).unwrap(), vbmeta(3));
        assert_eq!(ran(&["get-verification"], &phone).1, "verification is disabled on slot with suffix _a.\n");
        assert_eq!(ran(&["get-verity"], &phone).1, "verity is disabled on slot with suffix _a.\n");
        assert_eq!(
            ran(&["enable-verity"], &phone).1,
            "Successfully enabled verity on slot with suffix _a. Reboot the device for changes to take effect.\n"
        );
        assert_eq!(fs::read(&a).unwrap(), vbmeta(2));
        assert_eq!(ran(&["enable-verification"], &phone).0, 0);
        assert_eq!(fs::read(&a).unwrap(), vbmeta(0));
        assert_eq!(fs::read(&b).unwrap(), vbmeta(0), "the other slot's as it was");
    }

    #[test]
    fn a_phone_with_one_slot_names_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("vbmeta"), vbmeta(2)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "", locked: false, ab: false };
        assert_eq!(ran(&["get-verification"], &phone).1, "verification is disabled.\n");
        assert_eq!(ran(&["enable-verification"], &phone).1, "Successfully enabled verification. Reboot the device for changes to take effect.\n");
        assert_eq!(fs::read(dir.path().join("vbmeta")).unwrap(), vbmeta(0));
    }

    #[test]
    fn a_locked_bootloader_s_flags_change_only_when_forced() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("vbmeta_a");
        fs::write(&a, vbmeta(2)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: true, ab: false };
        assert_eq!(ran(&["disable-verity"], &phone), (77, String::new(), LOCKED.into()));
        assert_eq!(fs::read(&a).unwrap(), vbmeta(2), "nothing written");
        assert_eq!(ran(&["get-verity"], &phone).0, 0, "telling needs no force");
        assert_eq!(ran(&["disable-verification"], &phone).1, "verification is already disabled on slot with suffix _a.\n");
        assert_eq!(ran(&["--force", "disable-verity"], &phone).0, 0);
        assert_eq!(fs::read(&a).unwrap(), vbmeta(3));
    }

    #[test]
    fn without_a_vbmeta_partition_the_image_is_where_the_boot_partition_s_footer_says() {
        let dir = tempfile::tempdir().unwrap();
        let boot = dir.path().join("boot_b");
        let partition = |flags: u32| {
            let (kernel, image) = (vec![0x11u8; 4096], vbmeta(flags));
            let mut boot = kernel.clone();
            boot.extend_from_slice(&image);
            boot.resize(4096 * 3, 0);
            let mut footer = b"AVBf".to_vec();
            footer.extend_from_slice(&1u32.to_be_bytes());
            footer.extend_from_slice(&0u32.to_be_bytes());
            footer.extend_from_slice(&(kernel.len() as u64).to_be_bytes());
            footer.extend_from_slice(&4096u64.to_be_bytes());
            footer.extend_from_slice(&(image.len() as u64).to_be_bytes());
            footer.resize(64, 0);
            let end = boot.len() - 64;
            boot[end..].copy_from_slice(&footer);
            boot
        };
        fs::write(&boot, partition(0)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_b", locked: false, ab: false };
        assert_eq!(ran(&["get-verity"], &phone).1, "verity is enabled on slot with suffix _b.\n");
        assert_eq!(ran(&["disable-verity"], &phone).0, 0);
        assert_eq!(fs::read(&boot).unwrap(), partition(1));
    }

    #[test]
    fn what_holds_no_vbmeta_image_fails_and_stays_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: false, ab: false };
        let (code, output, errors) = ran(&["get-verity"], &phone);
        assert_eq!((code, output.as_str()), (70, ""), "no partition to look in");
        assert!(errors.ends_with("Error getting whether verity is enabled.\n"), "{errors}");
        fs::write(dir.path().join("boot_a"), vec![0u8; 8192]).unwrap();
        assert_eq!(ran(&["disable-verity"], &phone).0, 70, "a boot partition without a footer");
        let a = dir.path().join("vbmeta_a");
        let other = b"ANDROID!".repeat(64);
        fs::write(&a, &other).unwrap();
        let (code, _, errors) = ran(&["disable-verification"], &phone);
        assert_eq!(code, 70);
        assert!(errors.contains("holds no vbmeta header"), "{errors}");
        assert_eq!(fs::read(&a).unwrap(), other, "nothing written");
    }

    #[test]
    fn without_a_command_it_tells_how_it_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: false, ab: false };
        for args in [&[][..], &["--force"], &["disable-dm-verity"]] {
            assert_eq!(ran(args, &phone), (64, String::new(), USAGE.into()), "{args:?}");
        }
    }

    #[test]
    fn a_flag_change_covers_both_slots_of_a_phone_with_two() {
        // The point of the whole exercise: a script that turns verification off
        // before writing slot B has turned it off for slot B, not only for the
        // slot the recovery happens to be running from.
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("vbmeta_a"), dir.path().join("vbmeta_b"));
        fs::write(&a, vbmeta(0)).unwrap();
        fs::write(&b, vbmeta(0)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: false, ab: true };
        assert_eq!(
            ran(&["disable-verification"], &phone),
            (0, "Successfully disabled verification on slot with suffix _a. Reboot the device for changes to take effect.\n".into(), String::new())
        );
        assert_eq!(fs::read(&a).unwrap(), vbmeta(2), "the slot the phone booted from");
        assert_eq!(fs::read(&b).unwrap(), vbmeta(2), "and the one it did not, which is the one being written");
        // The booted slot already holding what was asked for is not a reason
        // to leave the other one as it was.
        fs::write(&a, vbmeta(0)).unwrap();
        assert_eq!(ran(&["disable-verification"], &phone).0, 0);
        assert_eq!(fs::read(&a).unwrap(), vbmeta(2));
        assert_eq!(fs::read(&b).unwrap(), vbmeta(2));
        assert_eq!(ran(&["disable-verification"], &phone), (0, "verification is already disabled on slot with suffix _a.\n".into(), String::new()));
        // And on again, in both.
        assert_eq!(ran(&["enable-verification"], &phone).0, 0);
        assert_eq!(fs::read(&a).unwrap(), vbmeta(0));
        assert_eq!(fs::read(&b).unwrap(), vbmeta(0));
    }

    #[test]
    fn one_vbmeta_image_chained_under_by_both_slots_is_written_once() {
        // The other shape of a phone with two slots: one top-level image, with
        // each slot's own images chained under it. It is one target — a second
        // open of it would be the first write's rival, and would fail.
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("vbmeta");
        fs::write(&shared, vbmeta(0)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_b", locked: false, ab: true };
        assert_eq!(ran(&["get-verification"], &phone).1, "verification is enabled on slot with suffix _b.\n");
        assert_eq!(ran(&["disable-verification"], &phone).0, 0);
        assert_eq!(fs::read(&shared).unwrap(), vbmeta(2));
        assert_eq!(ran(&["get-verification"], &phone).1, "verification is disabled on slot with suffix _b.\n");
    }

    #[test]
    fn a_slot_with_no_image_to_change_stops_the_change_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("vbmeta_a");
        fs::write(&a, vbmeta(0)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: false, ab: true };
        // The slot that has an image is already what it must be, so the reading
        // comes to the other one — which has none — before anything is opened
        // for writing, and says so as a reading that failed.
        let (code, output, errors) = ran(&["enable-verity"], &phone);
        assert_eq!((code, output.as_str()), (70, ""), "the other slot has nothing to read");
        assert!(errors.ends_with("Error getting whether verity is enabled.\n"), "{errors}");
        assert_eq!(fs::read(&a).unwrap(), vbmeta(0), "and the slot that has one is as it was");
        // And asked for a change the booted slot does not hold yet, the change
        // is planned before it is written: the empty slot is found there, and
        // the one that could have been changed is left alone.
        let (code, output, errors) = ran(&["disable-verity"], &phone);
        assert_eq!((code, output.as_str()), (70, ""), "the other slot has nothing to change");
        assert!(errors.ends_with("Error setting verity.\n"), "{errors}");
        assert_eq!(fs::read(&a).unwrap(), vbmeta(0), "and the slot that has one is as it was");
    }

    #[test]
    fn a_failure_on_the_second_slot_puts_the_first_back() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("vbmeta_a"), dir.path().join("vbmeta_b"));
        fs::write(&a, vbmeta(0)).unwrap();
        fs::write(&b, vbmeta(0)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: false, ab: true };
        let mut targets = plan(&phone, Check::Verification, false).unwrap();
        assert_eq!(targets.len(), 2, "both slots, the booted one first");
        // The second write is made to give back something that is not the
        // header that was written, as a partition that takes a write and keeps
        // other bytes would.
        targets[1].after[..4].copy_from_slice(b"XXXX");
        let failed = commit(&mut targets).unwrap_err();
        assert!(failed.to_string().contains("holds no vbmeta header"), "{failed}");
        assert_eq!(fs::read(&a).unwrap(), vbmeta(0), "the first slot, put back rather than left changed");
        assert_eq!(fs::read(&b).unwrap(), vbmeta(0), "and the second, whose write was not believed");
    }

    #[test]
    fn a_second_writer_is_told_rather_than_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("vbmeta_a");
        fs::write(&a, vbmeta(0)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: false, ab: false };
        let held = plan(&phone, Check::Verity, false).unwrap();
        assert!(plan(&phone, Check::Verity, false).is_err(), "the second writer is told, not waited for");
        drop(held);
        assert!(plan(&phone, Check::Verity, false).is_ok(), "and is let in once the first has let go");
        assert_eq!(fs::read(&a).unwrap(), vbmeta(0), "neither wrote anything");
    }

    #[test]
    fn a_footer_that_puts_the_image_past_the_partition_is_refused_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let boot = dir.path().join("boot_b");
        // A boot partition with the image in it, and a footer saying the image
        // is longer than what is left of the partition it ends.
        let partition = |size: u64| {
            let (kernel, image) = (vec![0x11u8; 4096], vbmeta(0));
            let mut boot = kernel.clone();
            boot.extend_from_slice(&image);
            boot.resize(4096 * 3, 0);
            let mut footer = FOOTER_MAGIC.to_vec();
            footer.extend_from_slice(&1u32.to_be_bytes());
            footer.extend_from_slice(&0u32.to_be_bytes());
            footer.extend_from_slice(&(kernel.len() as u64).to_be_bytes());
            footer.extend_from_slice(&4096u64.to_be_bytes());
            footer.extend_from_slice(&size.to_be_bytes());
            footer.resize(FOOTER_LEN, 0);
            let end = boot.len() - FOOTER_LEN;
            boot[end..].copy_from_slice(&footer);
            boot
        };
        let phone = Phone { by_name: dir.path(), suffix: "_b", locked: false, ab: false };
        for size in [HEADER_LEN as u64 - 1, 4096 * 3] {
            let content = partition(size);
            fs::write(&boot, &content).unwrap();
            let (code, output, errors) = ran(&["disable-verity"], &phone);
            assert_eq!((code, output.as_str()), (70, ""), "a footer that puts the image past the partition is not a place to write");
            assert!(errors.ends_with("Error getting whether verity is enabled.\n"), "{errors}");
            assert_eq!(fs::read(&boot).unwrap(), content, "nothing written");
        }
    }

    // -- what the pages and the installer use -------------------------------

}
