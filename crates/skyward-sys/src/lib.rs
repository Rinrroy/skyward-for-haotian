use anyhow::{anyhow, bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub mod bcb;
pub mod clock;
pub mod control;
pub mod cpu;
#[cfg(target_os = "android")]
pub mod sleep;
#[cfg(target_os = "android")]
pub mod uevent;
pub mod ueventd;
pub mod usb;

/// A system property, or `None` when it is unset.
#[cfg(target_os = "android")]
pub fn prop(name: &str) -> Option<String> {
    use std::ffi::{CStr, CString};
    let key = CString::new(name).expect("property names contain no NUL");
    let mut buf = [0 as libc::c_char; libc::PROP_VALUE_MAX as usize];
    let n = unsafe { libc::__system_property_get(key.as_ptr(), buf.as_mut_ptr()) };
    if n <= 0 {
        return None;
    }
    Some(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

/// Hosts have no property area.
#[cfg(not(target_os = "android"))]
pub fn prop(_name: &str) -> Option<String> {
    None
}

/// A system property read as a yes or a no, the way Android reads one;
/// `default` where it is unset or says neither.
pub fn bool_prop(name: &str, default: bool) -> bool {
    yes_or_no(prop(name).as_deref()).unwrap_or(default)
}

/// What a property's value says, as Android reads a yes or a no.
fn yes_or_no(value: Option<&str>) -> Option<bool> {
    match value? {
        "1" | "y" | "yes" | "on" | "true" => Some(true),
        "0" | "n" | "no" | "off" | "false" => Some(false),
        _ => None,
    }
}

/// Every system property whose name starts with `prefix`, with its value.
#[cfg(target_os = "android")]
pub fn props(prefix: &str) -> Vec<(String, String)> {
    use std::ffi::{c_char, c_void, CStr};
    extern "C" {
        fn __system_property_read_callback(
            pi: *const libc::prop_info,
            callback: unsafe extern "C" fn(cookie: *mut c_void, name: *const c_char, value: *const c_char, serial: u32),
            cookie: *mut c_void,
        );
    }
    unsafe extern "C" fn take(cookie: *mut c_void, name: *const c_char, value: *const c_char, _serial: u32) {
        let all = unsafe { &mut *cookie.cast::<Vec<(String, String)>>() };
        let (name, value) = unsafe { (CStr::from_ptr(name), CStr::from_ptr(value)) };
        all.push((name.to_string_lossy().into_owned(), value.to_string_lossy().into_owned()));
    }
    unsafe extern "C" fn each(pi: *const libc::prop_info, cookie: *mut c_void) {
        unsafe { __system_property_read_callback(pi, take, cookie) };
    }
    let mut all: Vec<(String, String)> = Vec::new();
    unsafe { libc::__system_property_foreach(each, (&mut all as *mut Vec<(String, String)>).cast()) };
    all.retain(|(name, _)| name.starts_with(prefix));
    all
}

/// Hosts have no property area.
#[cfg(not(target_os = "android"))]
pub fn props(_prefix: &str) -> Vec<(String, String)> {
    Vec::new()
}

/// Ask init to set a property; init refuses names the caller's SELinux
/// domain may not set.
#[cfg(target_os = "android")]
pub fn set_prop(name: &str, value: &str) -> Result<()> {
    use std::ffi::CString;
    let (k, v) = (CString::new(name)?, CString::new(value)?);
    if unsafe { libc::__system_property_set(k.as_ptr(), v.as_ptr()) } != 0 {
        bail!("init refused {name}={value}");
    }
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub fn set_prop(name: &str, _value: &str) -> Result<()> {
    bail!("this host has no property service to set {name}")
}

/// Wait until property `name` reads `value`, for at most `timeout`: init
/// sets some properties only once it has done what another one asked of
/// it.
pub fn wait_prop(name: &str, value: &str, timeout: Duration) -> Result<()> {
    let until = Instant::now() + timeout;
    loop {
        let now = prop(name);
        if now.as_deref() == Some(value) {
            return Ok(());
        }
        if Instant::now() >= until {
            bail!("{name} is {now:?} after {} ms, not {value:?}", timeout.as_millis());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Where a power request sends the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    System,
    Recovery,
    /// The recovery's own userspace fastboot. On a phone with dynamic
    /// partitions init leaves `--fastboot` for the recovery in misc and
    /// boots it.
    Fastboot,
    Bootloader,
    /// Qualcomm emergency download mode.
    Edl,
    Off,
}

impl Power {
    pub fn parse(name: &str) -> Option<Power> {
        Some(match name {
            "system" => Power::System,
            "recovery" => Power::Recovery,
            "fastboot" => Power::Fastboot,
            "bootloader" => Power::Bootloader,
            "edl" => Power::Edl,
            "off" => Power::Off,
            _ => return None,
        })
    }

    /// The `sys.powerctl` value; init syncs and unmounts before it acts.
    pub fn command(self) -> &'static str {
        match self {
            Power::System => "reboot,",
            Power::Recovery => "reboot,recovery",
            Power::Fastboot => "reboot,fastboot",
            Power::Bootloader => "reboot,bootloader",
            Power::Edl => "reboot,edl",
            Power::Off => "shutdown,",
        }
    }
}

pub fn power(target: Power) -> Result<()> {
    set_prop("sys.powerctl", target.command())
}

/// The folder the recovery service keeps what it makes for itself in, for
/// as long as the phone stays in recovery: its logs, the screenshots that
/// wait for internal storage, the supplicant's settings. Install scripts
/// written for other recoveries often empty /tmp before they start; this
/// folder is out of their way.
pub const RUN: &str = "/skyward/run";

/// Where the recovery service keeps the lines it writes to the kernel log
/// for as long as the phone stays in recovery.
pub const LOG: &str = "/skyward/run/recovery.log";

/// The folder a session's lines are also kept in, once the data partition can
/// be written to. [`LOG`] lives in a memory filesystem and goes with the next
/// start, which is exactly when a session that ended badly is worth reading;
/// this copy outlives it.
pub const LOG_FOLDER: &str = "/sdcard/Skyward/logs";

/// The small unencrypted FAT partition the bootloader writes its own logs to.
/// While the data partition is still closed there is no other place a
/// session's lines can outlast a restart, and a restart is what a session
/// that ended badly leaves behind.
const LOG_DISK: &str = "/dev/block/by-name/logfs";
const LOG_DISK_AT: &str = "/mnt/logfs";
const LOG_DISK_FOLDER: &str = "/mnt/logfs/skyward";

/// How many sessions are kept where they outlast the start.
const SESSIONS_KEPT: usize = 20;

/// The bytes the sessions and kernel logs may take together on the
/// bootloader's log disk. The disk is a few megabytes and carries the
/// maker's own logs besides, and the kernel's log of one session can be
/// most of a megabyte, so the older files go by their size together and
/// not by their count alone: a count alone let the disk fill and every
/// write to it fail, which took the WLAN down with it.
const LOG_DISK_BUDGET: u64 = 2_500_000;

/// The file [`kmsg`] writes its lines to as well, once [`keep_log`] opened
/// it.
static KEPT: OnceLock<Mutex<File>> = OnceLock::new();

/// The lasting copy's file, opened the first time the data partition is
/// writable, with the folder it was opened in — the folder names the write
/// when it fails, which the message names truly or not at all.
static KEPT_LONG: OnceLock<Mutex<Option<(File, PathBuf)>>> = OnceLock::new();

/// The second past the start at which the lasting copy was last tried, so a
/// phone whose data stays closed pays one attempt a second rather than one
/// per line.
static TRIED_LONG: AtomicU64 = AtomicU64::new(0);

/// Keep every line [`kmsg`] writes from now on in the file at `path` as
/// well, after what an earlier run left there.
pub fn keep_log(path: &Path) -> Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("open {}", path.display()))?;
    KEPT.set(Mutex::new(file)).map_err(|_| anyhow!("the log is kept in a file already"))
}

/// The lasting copy, opened where it can be; `None` while neither the data
/// partition nor the bootloader's log disk can be written to, or between the
/// attempts to open one.
fn lasting_log() -> Option<(File, PathBuf)> {
    let held = KEPT_LONG.get_or_init(|| Mutex::new(None));
    let mut held = held.lock().ok()?;
    if held.is_none() {
        let now = since_boot().map(|d| d.as_secs()).unwrap_or(0);
        if now.saturating_sub(TRIED_LONG.load(Ordering::Relaxed)) < 1 {
            return None;
        }
        TRIED_LONG.store(now, Ordering::Relaxed);
        // The bootloader's own disk first: it holds whether or not the data
        // partition is open, and a session that ended badly is worth reading
        // precisely when that partition is closed. Internal storage is the
        // fallback, for a phone whose log disk will not mount.
        let folder = match mount_log_disk() {
            Some(_) => PathBuf::from(LOG_DISK_FOLDER),
            None if internal_storage_ready() => PathBuf::from(LOG_FOLDER),
            None => return None,
        };
        let file = session_in(&folder)?;
        *held = Some((file, folder));
    }
    let (file, folder) = held.as_ref()?;
    Some((file.try_clone().ok()?, folder.clone()))
}

/// A file for this session in `folder`, opened after what earlier sessions
/// left there and named by how long the phone had been up when it started,
/// which is all the clock says before the system sets it from the RTC.
fn session_in(folder: &Path) -> Option<File> {
    std::fs::create_dir_all(folder).ok()?;
    let at = since_boot().map(|d| d.as_secs()).unwrap_or(0);
    let name = format!("session-{at}.log");
    let mut file = OpenOptions::new().create(true).append(true).open(folder.join(name)).ok()?;
    // The lines are stamped with the same clock, which says nothing of the
    // day; how long the phone had been up when they started does.
    writeln!(file, "session started {at} s after the phone started").ok()?;
    forget_older_sessions(folder);
    Some(file)
}

/// Keep the kernel's own log of this session beside the session's lines.
///
/// The lines [`kmsg`] writes are ours; what the kernel said — the mounts,
/// the denials, the devices that were not there — is in its ring buffer,
/// and the ring buffer goes with the restart. A session that ended badly is
/// worth reading after the phone has started again, so it is written out
/// as well: to the log disk when there is one, to the internal storage
/// otherwise, and nowhere when neither can be written.
pub fn keep_kernel_log() -> Result<()> {
    let folder = match mount_log_disk() {
        Some(_) => PathBuf::from(LOG_DISK_FOLDER),
        None if internal_storage_ready() => PathBuf::from(LOG_FOLDER),
        None => return Ok(()),
    };
    std::fs::create_dir_all(&folder).ok();
    let at = since_boot().map(|d| d.as_secs()).unwrap_or(0);
    let read = std::process::Command::new("/system/bin/dmesg").output().context("read the kernel log")?;
    let text = String::from_utf8_lossy(&read.stdout);
    // A phone that has just started says much before we arrive; the last
    // part is where the trouble is, and the partition is small.
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.len() > 4000 {
        lines = lines.split_off(lines.len() - 4000);
    }
    let mut out = String::from("the kernel log of this session, as it stood when the devices were brought up\n\n");
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    std::fs::write(folder.join(format!("kernel-{at}.log")), out).with_context(|| format!("write the kernel log to {}", folder.display()))?;
    forget_older_sessions(&folder);
    Ok(())
}

/// Leave the newest sessions and kernel logs in `folder` and take the rest
/// away, so a small partition does not fill with them: newest first, while
/// their count and their size together stay under the budget. On the
/// bootloader's disk the kernel's log of a session past is the heavy part
/// and was never taken away before, which is what filled it.
fn forget_older_sessions(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("session-") || n.starts_with("kernel-")))
        .filter_map(|p| {
            let meta = std::fs::metadata(&p).ok()?;
            Some((meta.modified().ok()?, meta.len(), p))
        })
        .collect();
    files.sort();
    files.reverse();
    let mut kept = 0usize;
    let mut taken = 0u64;
    for (_when, size, path) in &files {
        if kept < SESSIONS_KEPT && taken + size <= LOG_DISK_BUDGET {
            kept += 1;
            taken += size;
            continue;
        }
        let _ = std::fs::remove_file(path);
    }
}

/// Whether internal storage is the data partition's own, rather than the
/// empty folder the ramdisk leaves where it is to be mounted. Writing into
/// that one succeeds and goes with the next start, which is the same as
/// writing to [`LOG`]; only a mounted data partition is worth a file.
pub fn internal_storage_ready() -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|mounts| {
            mounts.lines().any(|line| {
                let mut fields = line.split_whitespace();
                let _device = fields.next();
                matches!(fields.next(), Some("/data") | Some("/sdcard"))
            })
        })
        .unwrap_or(false)
}

/// Write a line straight to the kernel log, without going through [`kmsg`],
/// which would come back to the code that is writing this.
fn note(text: &str) {
    if let Ok(mut log) = OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = log.write_all(format!("<6>skyward: {text}\n").as_bytes());
    }
}

/// The first few attempts to reach the log disk are worth a line each, so a
/// phone where it will not mount says why rather than staying quiet.
static NOTED_DISK: AtomicU64 = AtomicU64::new(0);

fn note_disk(text: &str) {
    if NOTED_DISK.fetch_add(1, Ordering::Relaxed) < 3 {
        note(text);
    }
}

/// Mount the bootloader's log disk where this session's lines can be written
/// to it, unless it is mounted already. It is a small FAT partition this
/// recovery only ever adds a folder to; failing to mount it costs nothing.
fn mount_log_disk() -> Option<PathBuf> {
    let at = Path::new(LOG_DISK_AT);
    if !Path::new(LOG_DISK).exists() {
        note_disk("the log disk is not there yet");
        return None;
    }
    if std::fs::create_dir_all(at).is_err() {
        note_disk("the log disk has nowhere to be mounted");
        return None;
    }
    let mounted = std::fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some(LOG_DISK_AT)))
        .unwrap_or(false);
    if mounted {
        return Some(at.to_path_buf());
    }
    let (source, target) = (std::ffi::CString::new(LOG_DISK).ok()?, std::ffi::CString::new(LOG_DISK_AT).ok()?);
    let kind = std::ffi::CString::new("vfat").ok()?;
    // The data mount(2) takes is for the filesystem's own driver, not the
    // options mount(8) reads: "rw" and "noatime" belong in the flags, and
    // handing them to vfat is what EINVAL means here. Read-write is what not
    // asking for MS_RDONLY already gives.
    let mounted = unsafe {
        libc::mount(source.as_ptr(), target.as_ptr(), kind.as_ptr(), libc::MS_NOATIME as libc::c_ulong, std::ptr::null())
    };
    if mounted != 0 {
        note_disk(&format!("the log disk would not mount: {}", std::io::Error::last_os_error()));
        return None;
    }
    note_disk("the log disk is mounted for this session's lines");
    Some(at.to_path_buf())
}

/// Append a line to the kernel log, where it survives until the next boot
/// and shows in `dmesg`, to the file [`keep_log`] opened, and to the lasting
/// copy once the data partition is writable, stamped with the seconds since
/// the phone started as the kernel log stamps it.
pub fn kmsg(line: &str) -> Result<()> {
    let mut f = OpenOptions::new().write(true).open("/dev/kmsg")?;
    f.write_all(format!("<6>skyward: {line}\n").as_bytes())?;
    // The stamp is made once, so both files say the same thing. A clock that
    // cannot be read leaves the kernel log alone rather than failing the
    // call: what was written there is already written.
    let Ok(secs) = since_boot().map(|d| d.as_secs_f64()) else { return Ok(()) };
    let stamped = format!("[{secs:>12.6}] {line}\n");
    if let Some(kept) = KEPT.get() {
        let mut kept = kept.lock().expect("nothing panics while writing the log");
        write!(kept, "{stamped}").with_context(|| format!("write {LOG}"))?;
    }
    if let Some((mut lasting, at)) = lasting_log() {
        write!(lasting, "{stamped}").with_context(|| format!("write {}", at.display()))?;
    }
    Ok(())
}

/// How long the phone has been running, on the clock the kernel stamps its
/// log with.
fn since_boot() -> Result<Duration> {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) } != 0 {
        return Err(std::io::Error::last_os_error()).context("read the time since the phone started");
    }
    Ok(Duration::new(t.tv_sec as u64, t.tv_nsec as u32))
}

/// The kernel log as `dmesg` prints it: as much of it as the kernel still
/// holds, each line without the level it was written at.
#[cfg(target_os = "android")]
pub fn kernel_log() -> Result<String> {
    const READ_ALL: libc::c_int = 3;
    const SIZE_BUFFER: libc::c_int = 10;
    let size = unsafe { libc::klogctl(SIZE_BUFFER, std::ptr::null_mut(), 0) };
    if size < 0 {
        return Err(std::io::Error::last_os_error()).context("ask the kernel how much log it holds");
    }
    let mut held = vec![0u8; size as usize];
    let n = unsafe { libc::klogctl(READ_ALL, held.as_mut_ptr().cast(), size) };
    if n < 0 {
        return Err(std::io::Error::last_os_error()).context("read the kernel log");
    }
    held.truncate(n as usize);
    Ok(without_levels(&String::from_utf8_lossy(&held)))
}

/// Kernel log lines without the `<6>` and the like that say the level each
/// was written at.
#[cfg(any(target_os = "android", test))]
fn without_levels(log: &str) -> String {
    log.lines().map(|line| line.strip_prefix('<').and_then(|l| l.split_once('>')).map_or(line, |(_, rest)| rest)).fold(
        String::with_capacity(log.len()),
        |mut out, line| {
            out.push_str(line);
            out.push('\n');
            out
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_log_lines_lose_their_levels() {
        let held = "<6>[    5.499346] skyward: ro.build.version.release is 16\n<3>[    6.1] dwc3 a600000.dwc3: a > b\nno level\n";
        assert_eq!(without_levels(held), "[    5.499346] skyward: ro.build.version.release is 16\n[    6.1] dwc3 a600000.dwc3: a > b\nno level\n");
    }

    #[test]
    fn power_names_round_trip() {
        for (name, cmd) in [
            ("system", "reboot,"),
            ("recovery", "reboot,recovery"),
            ("fastboot", "reboot,fastboot"),
            ("bootloader", "reboot,bootloader"),
            ("edl", "reboot,edl"),
            ("off", "shutdown,"),
        ] {
            assert_eq!(Power::parse(name).unwrap().command(), cmd);
        }
        assert_eq!(Power::parse("download"), None);
    }

    #[test]
    fn a_property_says_yes_or_no_in_the_words_android_takes() {
        for yes in ["1", "y", "yes", "on", "true"] {
            assert_eq!(yes_or_no(Some(yes)), Some(true), "{yes}");
        }
        for no in ["0", "n", "no", "off", "false"] {
            assert_eq!(yes_or_no(Some(no)), Some(false), "{no}");
        }
        assert_eq!(yes_or_no(Some("True")), None, "the words are read as written");
        assert_eq!(yes_or_no(Some("")), None);
        assert_eq!(yes_or_no(None), None);
    }
}
