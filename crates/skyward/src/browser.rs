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
use skyward_ui::app::{Key, Touch};
use skyward_ui::canvas::Canvas;
use skyward_ui::color::Rgba;
use skyward_ui::surface::Surface;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
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

/// The framebuffer the payload's display server keeps, as a file this can read:
/// `Xvfb :0 -screen 0 1440x3200x24 -fbdir /data/skyward/browser/fb` writes every
/// pixel it holds there, and the recovery is what those pixels reach the panel
/// through.
///
/// This is the third answer to "who draws on this phone", and the first two were
/// measured rather than guessed: a Wayland compositor cannot take a seat because
/// the kernel is built without `CONFIG_VT`, and Xorg's modesetting driver
/// segfaults inside itself while describing the card's outputs to RandR, with
/// one output or four, glamor or shadow, a cursor or none. So the payload has
/// been taken off the card entirely: it draws into its own memory, and this
/// module copies that memory onto the panel with the card the recovery already
/// holds.
const MIRROR: &str = "/data/skyward/browser/fb/Xvfb_screen0";

/// What the payload's display server was told to be. A framebuffer file carries
/// no shape of its own, so the shape has to be agreed on both sides.
const MIRROR_SHAPE: (u32, u32) = (1440, 3200);

/// Where one line per finger goes, for the payload to turn into a pointer
/// event: the recovery owns the touch screen, and the page is on the other side
/// of a display server that never sees it.
const TOUCH: &str = "/data/skyward/browser/touch.fifo";

/// How often the panel is offered a new frame. Thirty a second is more than a
/// page being read needs and less than a finger can feel the lack of.
const FRAME: Duration = Duration::from_millis(33);

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

/// What the payload's display server puts in its framebuffer file.
///
/// The file is not raw pixels. `Xvfb -fbdir` writes an XWD image: a header of
/// `header_size` bytes, then a colour table of `ncolors` entries of twelve
/// bytes, and only then the picture, row by row with `bytes_per_line` in a row.
/// Read from the first byte instead, that header lands on the panel as if it
/// were the top of the page and every row below it is shifted — which is what a
/// page wearing a seam, drawn twice, is made of.
struct Dump {
    width: u32,
    height: u32,
    /// Bytes from the start of one row to the start of the next.
    stride: usize,
    /// Where the first pixel is.
    offset: usize,
    /// 0 when the pixels are least significant byte first, 1 when they are not.
    order: u32,
}

/// The first hundred bytes are enough: twenty-five big-endian words describe the
/// image, and the colour table that follows is counted rather than read.
const XWD_HEAD: usize = 100;

/// The version of the dump format this understands.
const XWD_VERSION: u32 = 7;

fn word(head: &[u8; XWD_HEAD], at: usize) -> u32 {
    u32::from_be_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]])
}

impl Dump {
    /// What a header says, or nothing when it is not a header this understands.
    fn parse(head: &[u8; XWD_HEAD]) -> Option<Self> {
        if word(head, 4) != XWD_VERSION {
            return None;
        }
        let header = word(head, 0) as usize;
        let colours = word(head, 76) as usize;
        let width = word(head, 16);
        let height = word(head, 20);
        let order = word(head, 28);
        let bits = word(head, 44);
        let stride = word(head, 48) as usize;
        if !(XWD_HEAD..=8192).contains(&header) || colours > 65_536 {
            return None;
        }
        if width == 0 || height == 0 || width > 16_384 || height > 16_384 || order > 1 {
            return None;
        }
        // Four-byte pixels are the only shape a canvas can take, and a row
        // shorter than its pixels would read past each row into the next.
        if bits != 32 || stride < (width as usize) * 4 {
            return None;
        }
        Some(Self { width, height, stride, offset: header + colours * 12, order })
    }

    fn of(file: &fs::File) -> Option<Self> {
        let mut head = [0u8; XWD_HEAD];
        file.read_exact_at(&mut head, 0).ok()?;
        Self::parse(&head)
    }

    /// How much of the file holds the picture.
    fn len(&self) -> usize {
        self.offset + self.stride * self.height as usize
    }
}

/// Which of the two discs a finger landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disc {
    /// Leave the browser: the page has no way out of its own.
    Back,
    /// Ask the payload for its keyboard, or put it away again.
    Keyboard,
}

/// The payload's framebuffer, on its way to the panel.
///
/// Each pass reads the file the display server writes and puts on the panel only
/// the rows that changed since the pass before, so a page being read costs
/// almost nothing and a page being drawn costs one blit.
struct Mirror {
    /// What the panel is given: the same shape as the panel, in the byte order
    /// the drawing canvas uses.
    canvas: Canvas,
    /// The payload's pixels as they were read this time, and as they were the
    /// time before, to know what to send.
    now: Vec<u8>,
    was: Vec<u8>,
    /// What the last frame read said about itself, and what it is assumed to be
    /// while the file has not been read yet.
    dump: Dump,
    /// Whether anything has ever been sent, so the first frame always goes.
    shown: bool,
    /// Whether the payload has been asked for its keyboard, and whether the
    /// discs on the panel were drawn in the state they are now in.
    keyboard: bool,
    drawn: bool,
    /// Reads that found nothing yet, counted so the log says it once.
    missed: u32,
    /// Said once: the header was not one this understands, so the frame is
    /// guessed at, which is worse but still shows something.
    guessed: bool,
}

impl Mirror {
    fn new(size: (u32, u32)) -> Self {
        let (w, h) = size;
        let (sw, sh) = MIRROR_SHAPE;
        let assumed = Dump { width: sw, height: sh, stride: (sw as usize) * 4, offset: 0, order: 0 };
        let _ = (w, h);
        Self {
            canvas: Canvas::unfilled(w, h, 0..h),
            now: Vec::new(),
            was: Vec::new(),
            dump: assumed,
            shown: false,
            keyboard: false,
            drawn: false,
            missed: 0,
            guessed: false,
        }
    }

    /// The shape the payload's page is drawn in, which is what a finger on the
    /// panel has to be said in.
    fn shape(&self) -> (u32, u32) {
        (self.dump.width, self.dump.height)
    }

    /// The two discs, in panel pixels, at the bottom corners where a page keeps
    /// nothing worth reading.
    fn discs(&self) -> [(Disc, f32, f32); 2] {
        let (w, h) = (self.canvas.w as f32, self.canvas.h as f32);
        let edge = (w / 36.0).round();
        let at = (h - edge, edge);
        [(Disc::Back, at.1, at.0), (Disc::Keyboard, w - at.1, at.0)]
    }

    /// Which disc, if any, a point of the panel is on.
    fn disc_at(&self, x: f32, y: f32) -> Option<Disc> {
        let (w, _) = (self.canvas.w as f32, self.canvas.h as f32);
        let reach = w / 18.0;
        self.discs().into_iter().find(|(_, cx, cy)| (x - cx).powi(2) + (y - cy).powi(2) <= reach * reach).map(|(disc, _, _)| disc)
    }

    /// Draw the two discs into a finished frame.
    fn draw_discs(&mut self) {
        let reach = (self.canvas.w as f32 / 18.0).round();
        let ink = Rgba::rgb(16, 16, 18).with_alpha(150);
        let face = Rgba::rgb(255, 255, 255).with_alpha(235);
        for (disc, cx, cy) in self.discs() {
            self.canvas.fill_circle(cx, cy, reach, ink);
            self.canvas.stroke_circle(cx, cy, reach, (reach / 18.0).max(2.0), face);
            match disc {
                Disc::Back => {
                    self.canvas.line(cx + reach * 0.22, cy - reach * 0.34, cx - reach * 0.22, cy, (reach / 9.0).max(3.0), face);
                    self.canvas.line(cx - reach * 0.22, cy, cx + reach * 0.22, cy + reach * 0.34, (reach / 9.0).max(3.0), face);
                }
                Disc::Keyboard => {
                    let wide = reach * 0.9;
                    let tall = reach * 0.62;
                    self.canvas.stroke_round_rect(cx - wide / 2.0, cy - tall / 2.0, wide, tall, reach * 0.16, (reach / 14.0).max(2.0), face);
                    for dot in [-0.28, 0.0, 0.28] {
                        self.canvas.fill_circle(cx + wide * dot, cy - tall * 0.16, (reach / 14.0).max(2.0), face);
                    }
                    self.canvas.line(cx - wide * 0.3, cy + tall * 0.2, cx + wide * 0.3, cy + tall * 0.2, (reach / 14.0).max(2.0), face);
                }
            }
        }
    }

    /// One frame: read what the payload holds, and send it if it is new.
    fn show(&mut self, surface: &mut dyn Surface) -> Result<()> {
        let (w, h) = (self.canvas.w, self.canvas.h);
        let file = match fs::File::open(MIRROR) {
            Ok(file) => file,
            Err(e) => {
                // Not a failure: the payload is still unpacking itself and the
                // first frame is not due yet.
                self.missed = self.missed.saturating_add(1);
                if self.missed == 1 || self.missed.is_multiple_of(50) {
                    let _ = skyward_sys::kmsg(&format!("the browser's framebuffer is not there yet ({e})"));
                }
                return Ok(());
            }
        };
        match Dump::of(&file) {
            Some(dump) => self.dump = dump,
            None => {
                if !self.guessed {
                    self.guessed = true;
                    let _ = skyward_sys::kmsg("the browser's framebuffer does not say what shape it is; it is being guessed at");
                }
            }
        }
        let Dump { width: sw, height: sh, stride, offset, order } = self.dump;
        if sw == 0 || sh == 0 {
            return Ok(());
        }
        let need = self.dump.len();
        if self.now.len() != need {
            self.now.resize(need, 0);
            self.was.resize(need, 0);
            self.shown = false;
        }
        file.read_exact_at(&mut self.now, 0).with_context(|| format!("read {MIRROR}"))?;
        let mut changed = false;
        for y in 0..h {
            let sy = if sh == h { y } else { y.saturating_mul(sh) / h.max(1) };
            let src = offset + (sy as usize) * stride;
            let dst = (y as usize) * (w as usize) * 4;
            let row = &self.now[src..src + (sw as usize) * 4];
            if self.shown && row == &self.was[src..src + (sw as usize) * 4] {
                continue;
            }
            changed = true;
            let out = &mut self.canvas.data[dst..dst + (w as usize) * 4];
            let pixel_of = |from: &[u8]| -> [u8; 4] {
                // The server keeps its pixels in one byte order and the canvas
                // holds another; the fourth byte is padding either way.
                if order == 0 {
                    [from[2], from[1], from[0], 0xFF]
                } else {
                    [from[1], from[2], from[3], 0xFF]
                }
            };
            if sw == w {
                for (pixel, from) in out.as_chunks_mut::<4>().0.iter_mut().zip(row.as_chunks::<4>().0) {
                    *pixel = pixel_of(from);
                }
            } else {
                let last = row.len() - 4;
                for (x, pixel) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let from = &row[((x * (sw as usize)) / (w as usize).max(1) * 4).min(last)..];
                    *pixel = pixel_of(from);
                }
            }
        }
        self.was.copy_from_slice(&self.now);
        // The discs are drawn after the page so a frame that changed under them
        // cannot rub them out, and a change of keyboard state is a frame of its
        // own: the page below it goes on being unchanged.
        self.draw_discs();
        if self.drawn != self.keyboard {
            self.drawn = self.keyboard;
            changed = true;
        }
        if changed {
            surface.present(&self.canvas).context("put the browser's frame on the panel")?;
            self.shown = true;
        }
        Ok(())
    }
}

/// The way a finger gets from the panel to the page.
///
/// It is a named pipe, because the two sides are two processes with only the
/// data partition in common: a line per event, `down x y`, `move x y`, `up x y`
/// or `key Name`, which the payload turns into an xdotool command.
struct Finger {
    to: Option<fs::File>,
    /// Whether the pipe has been looked for already, so its absence is said once.
    told_about: bool,
    /// The shape the page is drawn in, which is what a point of the panel has to
    /// be said in.
    shape: (u32, u32),
}

impl Finger {
    fn new() -> Self {
        Self { to: None, told_about: false, shape: MIRROR_SHAPE }
    }

    fn set_shape(&mut self, shape: (u32, u32)) {
        if shape.0 > 0 && shape.1 > 0 {
            self.shape = shape;
        }
    }

    fn open(&mut self) -> bool {
        if self.to.is_some() {
            return true;
        }
        // Opening a pipe for writing waits for a reader that may not be there;
        // a non-blocking open says whether it is.
        match fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(TOUCH) {
            Ok(file) => {
                self.to = Some(file);
                true
            }
            Err(e) => {
                if !self.told_about {
                    self.told_about = true;
                    let _ = skyward_sys::kmsg(&format!("no one is listening for touches at {TOUCH} ({e})"));
                }
                false
            }
        }
    }

    /// A finger at a point on the panel, said in the payload's own pixels.
    fn at(&mut self, what: &str, x: f32, y: f32, size: (u32, u32)) {
        let (sw, sh) = self.shape;
        let sx = (x.max(0.0) as u32).min(size.0.saturating_sub(1)).saturating_mul(sw) / size.0.max(1);
        let sy = (y.max(0.0) as u32).min(size.1.saturating_sub(1)).saturating_mul(sh) / size.1.max(1);
        self.say(&format!("{what} {sx} {sy}"));
    }

    fn key(&mut self, name: &str) {
        self.say(&format!("key {name}"));
    }

    fn say(&mut self, line: &str) {
        if !self.open() {
            return;
        }
        let Some(file) = &mut self.to else { return };
        // A pipe whose reader has gone fails rather than blocks, and the next
        // attempt opens it again.
        if file.write_all(format!("{line}\n").as_bytes()).is_err() {
            self.to = None;
        }
    }
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

/// Let the loop device go, and keep asking until it goes.
///
/// The image can still be held for a moment after the last process that had it
/// has ended, so one try is not always enough. This matters more than it looks:
/// a loop device left attached over a file on a mounted partition is the shape
/// of a phone that never finishes rebooting, because the kernel's own device
/// shutdown walks into a block device with I/O still queued behind it.
fn detach(device: &str) {
    for attempt in 1..=6 {
        let gone = Command::new("/system/bin/losetup")
            .args(["-d", device])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if gone {
            return;
        }
        let _ = skyward_sys::kmsg(&format!("the loop device {device} is still held; asking again ({attempt})"));
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = skyward_sys::kmsg(&format!("the loop device {device} would not come off"));
}

/// End everything the payload started, not only the process that ended.
///
/// The payload is started as the leader of its own process group for this
/// reason: the script that starts a browser is not the only thing it starts,
/// and a compositor left in the background keeps DRM master and keeps the
/// image's loop device busy. One negative signal reaches the whole group.
fn end_group(id: u32) {
    let group = -i32::try_from(id).unwrap_or(i32::MAX);
    for (signal, named) in [(libc::SIGTERM, "term"), (libc::SIGKILL, "kill")] {
        if unsafe { libc::kill(group, signal) } != 0 {
            let wrong = os_error();
            // Nothing left to signal is the state being asked for, not a fault.
            if wrong.raw_os_error() != Some(libc::ESRCH) {
                let _ = skyward_sys::kmsg(&format!("the browser's group would not take a {named}: {wrong}"));
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// What a browser's own children do to escape a process group.
///
/// A browser puts its renderers and its zygote in process groups of their own,
/// so one negative signal does not reach them, and a renderer that survives
/// keeps the image's folders busy. What they cannot change is where their root
/// is: every process still running with this chroot as its root is a process of
/// the payload and nothing else, so that is how they are found. Returns how many
/// were signalled.
fn kill_inside(root: &Path) -> usize {
    let Ok(entries) = fs::read_dir("/proc") else { return 0 };
    let mut ended = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_string_lossy().parse::<i32>().ok() else { continue };
        // A process of ours is one whose root is the image's folder. Our own
        // root is "/", so nothing here can name this process.
        if fs::read_link(entry.path().join("root")).ok().as_deref() != Some(root) {
            continue;
        }
        if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
            ended += 1;
        }
    }
    ended
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

/// Take apart what a session that ended without its drop left behind.
///
/// The release build aborts on a panic, and an aborted process runs no
/// destructors, so a session that died that way leaves the image mounted on its
/// loop device. That is not only untidy: a loop device still attached over a
/// file on a mounted partition is the shape of a phone whose reboot never
/// finishes, because the kernel's own device shutdown walks into a block device
/// with I/O still queued behind it. So anything under the work folder comes off,
/// and any loop device backed by a file of ours is detached, before a new
/// session is laid out — which also means the folder can be removed at all,
/// since a mount point cannot be.
fn clear_leftovers() {
    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        // Collected first: unmounting while reading the file that describes the
        // mounts reads a list that is changing underneath.
        let stale: Vec<String> = mounts
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|at| at.starts_with(WORK))
            .map(|at| at.replace("\\040", " "))
            .collect();
        for at in stale {
            let Ok(target) = cstr(Path::new(&at)) else { continue };
            if unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } == 0 {
                let _ = skyward_sys::kmsg(&format!("a mount left by an earlier browser session ({at}) came off"));
            } else {
                let _ = skyward_sys::kmsg(&format!("a mount left by an earlier browser session ({at}) would not come off: {}", os_error()));
            }
        }
    }
    let Ok(blocks) = fs::read_dir("/sys/block") else { return };
    for entry in blocks.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if !name.starts_with("loop") {
            continue;
        }
        let Ok(file) = fs::read_to_string(entry.path().join("loop/backing_file")) else { continue };
        // Only ours: the recovery attaches loops for other work too, and
        // detaching someone else's device would be a fault of its own.
        if !(file.contains("/Skyward/") || file.contains("/skyward/")) {
            continue;
        }
        detach(&format!("/dev/block/{name}"));
        let _ = skyward_sys::kmsg(&format!(
            "the loop device {name}, left on {} by an earlier browser session, is detached",
            file.trim()
        ));
    }
}

impl Lay {
    /// Lay the image out. Every step that mounts something records it first,
    /// so a failure half way leaves nothing behind.
    fn of(image: &Path) -> Result<Lay> {
        if !image.is_file() {
            bail!("{} is not a file", image.display());
        }
        clear_leftovers();
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
    fn run(&self, program: &str, inputs: &mut crate::input::Inputs, surface: &mut dyn Surface) -> Result<Ended> {
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
        // Its own process group, so that what the payload leaves running in the
        // background can be ended with it.
        cmd.process_group(0);
        let mut child = cmd.spawn().with_context(|| format!("start {program}"))?;
        // While it runs, the phone belongs to it twice over: the payload's pixels
        // are put on the panel by this thread, and a finger on the panel is put
        // in the payload's hand.
        let mut mirror = Mirror::new(surface.size());
        let mut finger = Finger::new();
        let mut closed = false;
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
            if closed {
                let _ = child.kill();
                break None;
            }
            // Drawing first: a finger answered before the frame that shows where
            // it landed is answered blind.
            if let Err(e) = mirror.show(surface) {
                // A frame that will not go to the panel is worth a line and no
                // more: the browser is still running and may yet be usable.
                let _ = skyward_sys::kmsg(&format!("a frame from the browser was not shown: {e:#}"));
            }
            if let Ok(events) = inputs.read() {
                finger.set_shape(mirror.shape());
                for event in events {
                    match event {
                        crate::input::Event::Touch(Touch::Down(x, y), _) => match mirror.disc_at(x, y) {
                            // The page has no window manager and no tab strip, so
                            // the two things it cannot offer are drawn on top of it.
                            Some(Disc::Back) => finger.key("Escape"),
                            Some(Disc::Keyboard) => {
                                mirror.keyboard = !mirror.keyboard;
                                finger.say(if mirror.keyboard { "kbd on" } else { "kbd off" });
                            }
                            None => finger.at("down", x, y, surface.size()),
                        },
                        crate::input::Event::Touch(Touch::Move(x, y), _) => finger.at("move", x, y, surface.size()),
                        crate::input::Event::Touch(Touch::Up, _) => finger.at("up", 0.0, 0.0, surface.size()),
                        // The phone's own buttons are what the page has: back is
                        // escape, and the power button ends the browser.
                        crate::input::Event::Key(Key::Back, true) => finger.key("Escape"),
                        crate::input::Event::Key(Key::Power, true) => closed = true,
                        crate::input::Event::Key(_, _) => {}
                    }
                }
            }
            std::thread::sleep(FRAME);
        };
        // The process that ended is not always the last one holding something:
        // the payload's script starts its compositor in the background, and a
        // survivor keeps the DRM card and the image's loop device. Nothing the
        // payload started outlives it.
        end_group(child.id());
        child.wait().context("reap the browser")?;
        // And a process that moved itself out of that group — a browser's own
        // children do — is found by where its root is: anything still running
        // inside this chroot is ours to end.
        let left = kill_inside(&self.root);
        if left > 0 {
            let _ = skyward_sys::kmsg(&format!("{left} process(es) the browser left inside its image were ended"));
        }
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
        keep_logs(&log);
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
            // Everything written through the image and the bound partitions has
            // to be on the medium before the device goes. A phone rebooted with
            // dirty pages over a loop device is a phone whose kernel can hang in
            // its own shutdown, which is exactly what a browser must not cause.
            unsafe { libc::sync() };
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

/// Keep what the browser said where a phone that came back up can read it.
///
/// The log this module writes lives under the data partition, which Android
/// will not let a plain shell read, so a browser that will not start is a
/// question with no answer while the phone is booted. Internal storage is
/// readable; a copy costs little and is the difference between "it exits" and
/// why. Never fatal: this is a diagnostic.
fn keep_logs(log: &Path) {
    let storage = Path::new(skyward_crypto::data::SDCARD).join("Skyward");
    if let Err(e) = fs::create_dir_all(&storage) {
        let _ = skyward_sys::kmsg(&format!("the browser's log has nowhere to be kept at {}: {e}", storage.display()));
        return;
    }
    for (from, name) in [(log.to_path_buf(), "browser.log"), (log.with_file_name("xorg.log"), "browser-xorg.log")] {
        if !from.is_file() {
            continue;
        }
        let into = storage.join(name);
        match fs::copy(&from, &into) {
            Ok(bytes) => {
                let _ = skyward_sys::kmsg(&format!("the browser's log is kept at {} ({bytes} bytes)", into.display()));
            }
            Err(e) => {
                let _ = skyward_sys::kmsg(&format!("the browser's log would not be kept at {}: {e}", into.display()));
            }
        }
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
/// The recovery keeps the display: the payload draws into its own memory, and
/// this puts those pixels on the panel and hands it the touches, so `surface`
/// must be the one the recovery itself draws with and must stay open the whole
/// time the browser runs.
pub fn open(image: &Path, program: &str, inputs: &mut crate::input::Inputs, surface: &mut dyn Surface) -> Result<Ended> {
    let program = starter(program);
    let lay = Lay::of(image).with_context(|| format!("lay out {}", image.display()))?;
    lay.run(&program, inputs, surface)
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
    fn a_frame_dump_says_where_its_pixels_begin() {
        // The header the phone's own display server wrote when payload 1.8 was
        // asked to draw a 1440x3200 page: 160 bytes of header, a colour table
        // of 256 entries, then 5760 bytes a row.
        let mut head = [0u8; XWD_HEAD];
        let words: [(usize, u32); 13] = [
            (0, 160),      // header size
            (4, 7),        // file version
            (8, 2),        // ZPixmap
            (12, 24),      // depth
            (16, 1440),    // width
            (20, 3200),    // height
            (28, 0),       // LSBFirst
            (44, 32),      // bits per pixel
            (48, 5760),    // bytes per line
            (56, 0xFF0000), // red
            (60, 0xFF00),  // green
            (64, 0xFF),    // blue
            (76, 256),     // colours in the table
        ];
        for (at, value) in words {
            head[at..at + 4].copy_from_slice(&value.to_be_bytes());
        }
        let dump = Dump::parse(&head).expect("a header the display server writes");
        assert_eq!((dump.width, dump.height), (1440, 3200));
        assert_eq!(dump.stride, 5760);
        assert_eq!(dump.offset, 160 + 256 * 12);
        assert_eq!(dump.len(), dump.offset + 5760 * 3200);
        assert_eq!(dump.order, 0);

        // A file that is not a dump is not read as one: guessing shows
        // something, and the caller falls back to what it assumed.
        assert!(Dump::parse(&[0u8; XWD_HEAD]).is_none());
        let mut wrong = head;
        wrong[4..8].copy_from_slice(&3u32.to_be_bytes());
        assert!(Dump::parse(&wrong).is_none(), "another version");
        let mut short_row = head;
        short_row[48..52].copy_from_slice(&1440u32.to_be_bytes());
        assert!(Dump::parse(&short_row).is_none(), "a row shorter than its pixels");
        let mut odd = head;
        odd[44..48].copy_from_slice(&24u32.to_be_bytes());
        assert!(Dump::parse(&odd).is_none(), "pixels that are not four bytes");
    }

    #[test]
    fn the_discs_are_on_the_panel_and_a_finger_finds_them() {
        let mirror = Mirror::new((1440, 3200));
        let [(back, bx, by), (keyboard, kx, ky)] = mirror.discs();
        assert_eq!(back, Disc::Back);
        assert_eq!(keyboard, Disc::Keyboard);
        // Bottom left and bottom right, and inside the panel.
        assert!(bx < 1440.0 / 2.0 && bx > 0.0 && by > 0.0 && by < 3200.0);
        assert!(kx > 1440.0 / 2.0 && kx < 1440.0 && ky > 0.0 && ky < 3200.0);
        assert_eq!(mirror.disc_at(bx, by), Some(Disc::Back));
        assert_eq!(mirror.disc_at(kx, ky), Some(Disc::Keyboard));
        // The middle of the page is the page's.
        assert_eq!(mirror.disc_at(720.0, 1600.0), None);
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
