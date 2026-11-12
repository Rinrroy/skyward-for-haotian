//! Handing the phone to a plugin.
//!
//! The browser is not a program the recovery can call and forget about: it
//! wants a display, an input stack, a name resolver and a dynamic linker, and
//! it wants them in a shape Android does not give a recovery. So the payload
//! is a Linux rootfs in one image, and this module lays that image out as the
//! phone's root for as long as the browser runs:
//!
//! * the image is mounted read-only at a folder on rootfs, which is the only
//!   place that is certainly writable and certainly not `noexec`;
//! * `/dev`, `/proc` and `/sys` are bound in, so DRM, input and udev see the
//!   hardware the recovery sees;
//! * `/data` is bound in when it is open, because that is where a browser's
//!   profile and cache belong — a session's cookies should outlive the
//!   session;
//! * `/etc/resolv.conf` is a file this module writes and binds over the
//!   image's own, so the name servers are the ones the Wi-Fi gave us rather
//!   than whatever a chroot cannot reach;
//! * SELinux is permissive while the browser runs and is put back afterwards,
//!   because a policy written for Android has no rules for a Debian rootfs;
//! * the caller releases the DRM card before this runs and takes it back
//!   after, so the browser's compositor is the only DRM master.
//!
//! Nothing here is on the path that ends `run()`: every failure is reported
//! to the page that asked for the browser, and a failed start leaves the
//! recovery exactly as it was, mounts and all.

use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where the image is laid out while it runs. On rootfs, which the recovery
/// owns, so the mounts go away with the process that made them.
const WORK: &str = "/skyward/run/browser";

/// A script in the browser's own folder that starts it instead of the image's
/// own program, when there is one and it can be run.
///
/// This is how a payload is put right without rebuilding it: the image is
/// read-only, but the data partition is not, and the same path means the same
/// file inside the chroot because `/data` is bound in at `/data`.
const OVERRIDE: &str = "/data/skyward/browser/start.sh";

/// The image's own folder inside the chroot, and the folders that are bound
/// or mounted over it.
const BOUND: [(&str, &str); 3] = [("dev", "/dev"), ("proc", "/proc"), ("sys", "/sys")];

/// What the browser is given when the phone's own properties say nothing.
/// AliDNS and DNSPod: both answer from inside the mainland, which is where a
/// phone using this feature is.
const DNS: [&str; 2] = ["223.5.5.5", "119.29.29.29"];

/// How long a browser may keep the phone before the recovery takes it back.
/// A phone whose screen is dark for two hours with no way out is worse than a
/// browser that is closed under someone.
const LONGEST: Duration = Duration::from_secs(2 * 60 * 60);

/// How the browser ended, for the page that started it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    /// The program's exit code, or none when a signal ended it.
    pub code: Option<i32>,
    /// How long it had the phone.
    pub ran: Duration,
}

/// A payload laid out and ready to run. Everything it mounted is unmounted
/// when it is dropped, in the reverse order, whatever went wrong.
struct Lay {
    root: PathBuf,
    device: Option<String>,
    /// Mount points, outermost first, in the order they must come off.
    mounted: Vec<PathBuf>,
    /// SELinux was permissive before, or this is what it was.
    enforce: Option<String>,
}

fn cstr(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).with_context(|| format!("{} holds a NUL", path.display()))
}

fn os_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn mount(source: Option<&Path>, target: &Path, kind: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> Result<()> {
    let src = match source {
        Some(path) => Some(cstr(path)?),
        None => None,
    };
    let dst = cstr(target)?;
    let ty = kind.map(|k| CString::new(k).expect("filesystem names hold no NUL"));
    let extra = data.map(|d| CString::new(d).expect("mount data holds no NUL"));
    let (src, ty, extra) = (
        src.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
        ty.as_ref().map_or(std::ptr::null(), |t| t.as_ptr()),
        extra.as_ref().map_or(std::ptr::null(), |e| e.as_ptr().cast()),
    );
    if unsafe { libc::mount(src, dst.as_ptr(), ty, flags, extra) } != 0 {
        return Err(os_error()).with_context(|| {
            format!(
                "mount {} at {}{}",
                source.map(|s| s.display().to_string()).unwrap_or_else(|| "none".into()),
                target.display(),
                kind.map(|k| format!(" as {k}")).unwrap_or_default()
            )
        });
    }
    Ok(())
}

/// The name servers the phone was given, or the ones this build falls back to.
/// `net.dns1` and its neighbours are what Android's own resolver reads, and a
/// Wi-Fi join is what sets them.
fn name_servers(props: &[(String, Option<String>)]) -> Vec<String> {
    let mut servers: Vec<String> = Vec::new();
    for (name, value) in props {
        let _ = name;
        let Some(value) = value else { continue };
        let value = value.trim();
        if value.is_empty() || servers.iter().any(|s| s == value) {
            continue;
        }
        servers.push(value.to_string());
    }
    if servers.is_empty() {
        servers.extend(DNS.iter().map(|s| s.to_string()));
    }
    servers.truncate(3);
    servers
}

/// What a resolver file says, one `nameserver` line per server.
fn resolv_text(servers: &[String]) -> String {
    let mut text = String::from("# written by Skyward for the browser\n");
    for server in servers {
        text.push_str(&format!("nameserver {server}\n"));
    }
    text
}

fn asked() -> Vec<(String, Option<String>)> {
    ["net.dns1", "net.dns2", "net.dns3", "net.dns4"]
        .iter()
        .map(|name| ((*name).to_string(), skyward_sys::prop(name)))
        .collect()
}

/// The loop device the image is attached to. `losetup` is the phone's own, and
/// the device it names is what gets detached when the browser is done.
fn attach(image: &Path) -> Result<String> {
    let out = Command::new("/system/bin/losetup")
        // Read-only: the image is never written, and a payload kept on a
        // read-only or nearly-full partition must still attach.
        .args(["-r", "-f", "--show"])
        .arg(image)
        .stdin(Stdio::null())
        .output()
        .context("ask losetup for a loop device")?;
    if !out.status.success() {
        bail!("losetup: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let device = String::from_utf8(out.stdout).context("losetup named a device that is not text")?;
    let device = device.trim().to_string();
    if !device.starts_with("/dev/") {
        bail!("losetup named {device}, which is not a device");
    }
    Ok(device)
}

fn detach(device: &str) {
    let _ = Command::new("/system/bin/losetup")
        .args(["-d", device])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// What filesystem an image holds, read from the image itself.
///
/// The mount of an image needs its filesystem named: `mount(2)` with no type
/// is only allowed for a bind mount, and answers `EINVAL` for anything else —
/// which is exactly what happened when a browser image was mounted with no
/// type at all. The name cannot be trusted either (a payload copied in by hand
/// carries whatever name its downloader gave it), so the magic is what decides.
fn kind_of(image: &Path) -> Option<&'static str> {
    // Far enough in for the erofs and ext4 magics, which sit past the first
    // block; a squashfs magic is at the very front.
    const HEAD: usize = 1082;
    let mut head = [0u8; HEAD];
    let mut file = fs::File::open(image).ok()?;
    std::io::Read::read_exact(&mut file, &mut head).ok()?;
    if head[0..4] == *b"hsqs" {
        return Some("squashfs");
    }
    if head[1024..1028] == [0xE2, 0xE1, 0xF5, 0xE0] {
        return Some("erofs");
    }
    if head[1080..1082] == [0x53, 0xEF] {
        return Some("ext4");
    }
    None
}

/// SELinux's current mode, as the file the kernel keeps it in says it.
fn enforce_now() -> Option<String> {
    fs::read_to_string("/sys/fs/selinux/enforce").ok().map(|s| s.trim().to_string())
}

fn set_enforce(value: &str) {
    let _ = fs::write("/sys/fs/selinux/enforce", value);
}

impl Lay {
    /// Lay the image out. Every step that mounts something records it first,
    /// so a failure half way leaves nothing behind.
    fn of(image: &Path) -> Result<Lay> {
        if !image.is_file() {
            bail!("{} is not a file", image.display());
        }
        let root = Path::new(WORK).join("root");
        let _ = fs::remove_dir_all(WORK);
        fs::create_dir_all(&root).with_context(|| format!("make {}", root.display()))?;
        let mut lay = Lay { root, device: None, mounted: Vec::new(), enforce: None };

        let device = attach(image)?;
        lay.device = Some(device.clone());
        let root = lay.root.clone();
        // Named from the image's own magic: a mount with no filesystem named
        // is answered with EINVAL, and the name is not a thing to guess at.
        let kind = kind_of(image).with_context(|| format!("{} is not an image this can mount", image.display()))?;
        lay.onto(Some(Path::new(&device)), &root, Some(kind), libc::MS_RDONLY, None)?;
        for (inside, outside) in BOUND {
            let at = lay.root.join(inside);
            fs::create_dir_all(&at).with_context(|| format!("make {}", at.display()))?;
            lay.onto(Some(Path::new(outside)), &at, None, libc::MS_BIND | libc::MS_REC, None)?;
        }
        // /tmp and /run are the image's own empty folders; a browser needs
        // both writable, and neither should be on the image.
        for (name, mode) in [("tmp", "1777"), ("run", "0755")] {
            let at = lay.root.join(name);
            fs::create_dir_all(&at).with_context(|| format!("make {}", at.display()))?;
            let data = format!("mode={mode}");
            lay.onto(None, &at, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some(&data))?;
        }
        lay.resolver()?;
        lay.data()?;
        lay.enforce = enforce_now();
        set_enforce("0");
        Ok(lay)
    }

    /// Mount onto a folder inside the image, recording it so it comes off.
    ///
    /// `kind` is the filesystem's name, which the kernel insists on for
    /// anything that is not a bind mount or a remount.
    fn onto(&mut self, source: Option<&Path>, at: &Path, kind: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> Result<()> {
        mount(source, at, kind, flags, data)?;
        self.mounted.push(at.to_path_buf());
        Ok(())
    }

    /// Give the chroot the name servers the phone has, by binding a file this
    /// process wrote over the one the image ships. The image is read-only, so
    /// a file it can be told to read instead is the only way in.
    fn resolver(&mut self) -> Result<()> {
        let servers = name_servers(&asked());
        let file = Path::new(WORK).join("resolv.conf");
        fs::write(&file, resolv_text(&servers)).with_context(|| format!("write {}", file.display()))?;
        let at = self.root.join("etc/resolv.conf");
        if let Some(parent) = at.parent() {
            let _ = fs::create_dir_all(parent);
        }
        // The image may ship no such file; binding over a missing target needs
        // one to exist, and the image is read-only, so the mount of the image
        // is what a missing file cannot be fixed in. A missing file therefore
        // leaves the image's own resolver setting alone, and the fallback in
        // it (if any) is what the browser gets.
        if at.exists() {
            self.onto(Some(&file), &at, None, libc::MS_BIND, None)?;
        } else {
            let _ = skyward_sys::kmsg("the browser image has no /etc/resolv.conf to bind the phone's name servers over");
        }
        Ok(())
    }

    /// Let the browser keep its profile on the data partition, when it is open.
    fn data(&mut self) -> Result<()> {
        let at = self.root.join("data");
        fs::create_dir_all(&at).with_context(|| format!("make {}", at.display()))?;
        if !skyward_block::mount::is_mount_point(Path::new("/data")).unwrap_or(false) {
            let _ = skyward_sys::kmsg("the browser runs without the data partition, so its profile lasts one session");
            return Ok(());
        }
        let home = Path::new("/data/skyward/browser");
        fs::create_dir_all(home).with_context(|| format!("make {}", home.display()))?;
        self.onto(Some(Path::new("/data")), &at, None, libc::MS_BIND | libc::MS_REC, None)
    }

    /// Run the payload's own program inside the chroot and wait for it.
    fn run(&self, program: &str) -> Result<Ended> {
        let root = cstr(&self.root)?;
        let mut cmd = Command::new(program);
        cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .env("HOME", "/data/skyward/browser")
            .env("XDG_RUNTIME_DIR", "/run/skyward")
            .env("SKYWARD_START_URL", start_url())
            .stdin(Stdio::null());
        // What the payload says is what says why it would not start, so it is
        // kept in a file rather than thrown away. Nothing about this may stop
        // the browser from running: a log that cannot be written is a line in
        // the kernel log and nothing more.
        let log = log_path();
        if let Some(parent) = log.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::File::create(&log) {
            Ok(file) => match file.try_clone() {
                Ok(errors) => {
                    cmd.stdout(Stdio::from(file)).stderr(Stdio::from(errors));
                }
                Err(e) => {
                    let _ = skyward_sys::kmsg(&format!("the browser's log is one stream only: {e}"));
                    cmd.stdout(Stdio::from(file));
                }
            },
            Err(e) => {
                let _ = skyward_sys::kmsg(&format!("the browser's log cannot be written at {}: {e}", log.display()));
            }
        }
        // chroot, then start in the new root: the program's own PT_INTERP is
        // what the kernel then loads, which is the whole point of a chroot
        // here — no bionic linker namespace is involved.
        unsafe {
            cmd.pre_exec(move || {
                if libc::chroot(root.as_ptr()) != 0 {
                    return Err(os_error());
                }
                if libc::chdir(c"/".as_ptr()) != 0 {
                    return Err(os_error());
                }
                Ok(())
            });
        }
        let started = Instant::now();
        let mut child = cmd.spawn().with_context(|| format!("start {program}"))?;
        let code = loop {
            if let Some(status) = child.try_wait().context("ask after the browser")? {
                break status.code();
            }
            if started.elapsed() > LONGEST {
                let _ = child.kill();
                let _ = child.wait();
                let _ = skyward_sys::kmsg("the browser was closed after two hours; the recovery has the phone back");
                break None;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        // A browser that ended at once is a browser that failed; what it said
        // before it did is the only way to know why from the kernel log.
        if !matches!(code, Some(0) | None) {
            if let Ok(text) = fs::read_to_string(&log) {
                let _ = skyward_sys::kmsg(&format!(
                    "the browser ended with {code:?}; {} ends: {}",
                    log.display(),
                    last_words(&text, 600)
                ));
            }
        }
        Ok(Ended { code, ran: started.elapsed() })
    }
}

impl Drop for Lay {
    fn drop(&mut self) {
        // The other way round from how they went on, and never fatally: a
        // mount that will not come off is one the next start replaces anyway.
        while let Some(at) = self.mounted.pop() {
            if unsafe { libc::umount2(cstr(&at).expect("a recorded mount point holds no NUL").as_ptr(), libc::MNT_DETACH) } != 0 {
                let _ = skyward_sys::kmsg(&format!("the browser's mount at {} would not come off: {}", at.display(), os_error()));
            }
        }
        if let Some(device) = &self.device {
            detach(device);
        }
        if let Some(enforce) = &self.enforce {
            set_enforce(enforce);
        }
        let _ = fs::remove_dir_all(WORK);
    }
}

/// Where the browser opens. The image's own home page is Bing in the mainland;
/// a page that starts it somewhere else says so.
fn start_url() -> String {
    skyward_sys::prop("skyward.browser.url").unwrap_or_else(|| "https://cn.bing.com".to_string())
}

/// Where the payload's own output is kept. The data partition when it is open,
/// because that is where a person can look at it afterwards from the system
/// that shares it; the rootfs otherwise, where it lasts until the next start.
fn log_path() -> PathBuf {
    let open = skyward_block::mount::is_mount_point(Path::new("/data")).unwrap_or(false);
    if open {
        Path::new("/data/skyward/browser/logs").join("start.log")
    } else {
        Path::new("/tmp").join("skyward-browser.log")
    }
}

/// The end of what the browser said, on one line, for the kernel log.
fn last_words(text: &str, most: usize) -> String {
    let trimmed = text.trim_end();
    let start = trimmed.len().saturating_sub(most);
    let tail = match trimmed.get(start..) {
        Some(tail) => tail,
        // A boundary inside a character: begin after it instead of losing the
        // whole line.
        None => trimmed.char_indices().map(|(at, _)| at).find(|at| *at >= start).map_or("", |at| &trimmed[at..]),
    };
    tail.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Which program starts the browser: the script in its own folder when there
/// is one that can be run, otherwise what the plugin says.
fn starter(program: &str) -> String {
    let at = Path::new(OVERRIDE);
    match at.metadata() {
        Ok(found) if found.is_file() && found.permissions().mode() & 0o111 != 0 => {
            let _ = skyward_sys::kmsg(&format!("the browser is started by {OVERRIDE}, not by the image's own program"));
            OVERRIDE.to_string()
        }
        Ok(_) => {
            let _ = skyward_sys::kmsg(&format!("{OVERRIDE} is not a program that can be run; the image's own program starts the browser"));
            program.to_string()
        }
        Err(_) => program.to_string(),
    }
}

/// Hand the phone to the browser at `image`, running `program` inside it.
///
/// The caller must have released the display: the browser's compositor takes
/// the DRM card as its own master, and a recovery that still holds it would
/// leave the panel black.
pub fn open(image: &Path, program: &str) -> Result<Ended> {
    let program = starter(program);
    let lay = Lay::of(image).with_context(|| format!("lay out {}", image.display()))?;
    lay.run(&program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_servers_are_the_phones_and_the_fallback_is_used_only_without_them() {
        let given = [("net.dns1".to_string(), Some("192.168.1.1".to_string())), ("net.dns2".to_string(), Some(" ".to_string()))];
        assert_eq!(name_servers(&given), vec!["192.168.1.1"]);
        let none = [("net.dns1".to_string(), None), ("net.dns2".to_string(), None)];
        assert_eq!(name_servers(&none), vec!["223.5.5.5", "119.29.29.29"]);
        let repeated = [
            ("net.dns1".to_string(), Some("10.0.0.1".to_string())),
            ("net.dns2".to_string(), Some("10.0.0.1".to_string())),
            ("net.dns3".to_string(), Some("10.0.0.2".to_string())),
        ];
        assert_eq!(name_servers(&repeated), vec!["10.0.0.1", "10.0.0.2"]);
    }

    #[test]
    fn a_resolver_file_is_one_line_per_server() {
        let text = resolv_text(&["1.1.1.1".into(), "8.8.8.8".into()]);
        assert_eq!(text.lines().filter(|l| l.starts_with("nameserver ")).count(), 2);
        assert!(text.contains("nameserver 1.1.1.1\n"));
        assert!(text.ends_with("nameserver 8.8.8.8\n"));
    }

    #[test]
    fn a_start_url_is_the_phones_own_when_it_names_one() {
        // The property is read from the phone; off it, the fallback stands.
        let url = start_url();
        assert!(url.starts_with("https://"), "{url}");
    }

    #[test]
    fn the_end_of_what_the_browser_said_fits_on_one_line() {
        assert_eq!(last_words("first\nsecond\n", 600), "first second");
        assert_eq!(last_words("a\nb\nc", 3), "b c");
        assert_eq!(last_words("", 10), "");
        // A cut that lands inside a character loses only that character.
        let chinese = "浏览器起不来";
        let cut = last_words(chinese, 13);
        assert!(chinese.ends_with(&cut), "{cut}");
        assert!(cut.chars().all(|c| chinese.contains(c)), "{cut}");
    }

    #[test]
    fn an_image_is_told_apart_by_what_it_holds_and_not_by_its_name() {
        let dir = std::env::temp_dir().join(format!("skyward-browser-kinds-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a folder to keep test images in");

        let mut erofs = vec![0u8; 1082];
        erofs[1024..1028].copy_from_slice(&[0xE2, 0xE1, 0xF5, 0xE0]);
        let mut ext4 = vec![0u8; 1082];
        ext4[1080..1082].copy_from_slice(&[0x53, 0xEF]);
        let mut squash = vec![0u8; 1082];
        squash[0..4].copy_from_slice(b"hsqs");

        for (name, bytes, kind) in [
            ("payload.erofs", erofs, Some("erofs")),
            ("an-image-named-erofs.img", ext4, Some("ext4")),
            ("whatever", squash, Some("squashfs")),
            ("empty", vec![0u8; 1082], None),
        ] {
            let at = dir.join(name);
            fs::write(&at, &bytes).expect("write a test image");
            assert_eq!(kind_of(&at), kind, "{name}");
        }
        // A file too short to hold a superblock is not an image either.
        let short = dir.join("short");
        fs::write(&short, b"hsq").expect("write a short file");
        assert_eq!(kind_of(&short), None);

        let _ = fs::remove_dir_all(&dir);
    }
}
