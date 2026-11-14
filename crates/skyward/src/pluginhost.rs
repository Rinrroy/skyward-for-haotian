//! Handing the phone to a plugin.
//!
//! A plugin is a program the recovery cannot carry inside itself: a browser
//! engine, a viewer, anything with a display of its own. What a plugin needs is
//! not a display server of its own but the recovery's own panel, so the shape of
//! the bargain is: the recovery keeps the display and the touch screen, the
//! plugin draws into a file in memory, and this module puts those pixels on the
//! panel and hands it the touches.
//!
//! The plugin is a program of the recovery's own kind — an aarch64 Android
//! binary, or a shell script that starts one — because it runs in this process's
//! own root with bionic under it. There is no chroot and no foreign libc here:
//! the plugin is a Linux program only in the sense that the recovery is one.
//! That is the whole difference from a payload that brings its own rootfs; a
//! plugin that wants one starts it itself, and this module never learns of it.
//!
//! * the image is mounted read-only under `/skyward/run/plugin/<name>/img`, on
//!   rootfs, which is certainly writable and certainly not `noexec`;
//! * the folder next to it is the plugin's own: its frame file, its input pipe
//!   and its log, none of which may be on a read-only mount;
//! * the frame file is a plain file this module creates and both sides map, one
//!   header of four kilobytes and then the pixels, so a plugin that is not
//!   running costs nothing and a plugin that has died leaves a frame that simply
//!   stops changing;
//! * the input pipe is a FIFO read without blocking, so a plugin that never
//!   reads it cannot make the recovery's own input loop wait.
//!
//! Nothing here is on the path that ends `run()`: every failure is reported to
//! the page that asked for the plugin, and a failed start leaves the recovery
//! exactly as it was, mounts and all. The frame protocol is written down in
//! `docs/plugins/webview.md` and in the plugins repository's `webview/PROTOCOL.md`.

use anyhow::{bail, Context, Result};
use skyward_ui::app::{Key, Touch};
use skyward_ui::canvas::Canvas;
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

/// Where plugins are laid out while they run. On rootfs, which the recovery
/// owns, so the mounts go away with the process that made them.
const WORK: &str = "/skyward/run/plugin";

/// The four kilobytes before the pixels: magic, version, shape, the sequence
/// number that says a frame is complete, the flags, and the rectangles that
/// changed. Fixed width because both sides read it without asking anyone.
const HEADER: usize = 4096;

/// What a frame file starts with, so that a file that is not one is noticed
/// instead of drawn.
const MAGIC: &[u8; 4] = b"SWFR";

/// The protocol this side speaks. A plugin that speaks another one draws in a
/// way this cannot read, and the page says so instead of showing noise.
const VERSION: u32 = 1;

/// How many changed rectangles a frame may name. Sixteen covers a cursor, a
/// typed character, a scrolling strip and an address bar at once; past that a
/// plugin should send the whole frame, which is one number cheaper to write.
const DIRTY_MOST: usize = 16;

/// Frame flags: the plugin has drawn something worth showing, it wants to be
/// let go, and it is asking for a way to type.
const READY: u32 = 1;
const QUIT: u32 = 2;
const KEYBOARD: u32 = 4;

/// How long a plugin has to draw its first frame before it is called failed. A
/// program that has not drawn in ten seconds is not slow, it is stuck.
const READY_WITHIN: Duration = Duration::from_secs(10);

/// How long a plugin has to end after it is asked to, before its group is
/// killed. It has been told; anything still running is not listening.
const STOP_WITHIN: Duration = Duration::from_secs(2);

/// How long a plugin may keep the phone before the recovery takes it back.
/// A phone whose screen is dark for two hours with no way out is worse than a
/// plugin that is closed under someone.
const LONGEST: Duration = Duration::from_secs(2 * 60 * 60);

/// How often the frame file's sequence number is looked at. The panel is a
/// 66 ms device at best; asking more often than this buys nothing.
const FRAME: Duration = Duration::from_millis(33);

/// Where the plugin's own output is kept, and what a phone that came back up
/// can read afterwards.
const KEPT: &str = "Skyward";

/// How a plugin ended, for the page that started it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    /// The program's exit code, or none when a signal ended it.
    pub code: Option<i32>,
    /// How long it had the phone.
    pub ran: Duration,
}

/// What one frame said about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    w: u32,
    h: u32,
    stride: u32,
}

/// A rectangle of pixels that changed, in the plugin's own pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// One small endian number out of a frame header.
fn word(bytes: &[u8], at: usize) -> u32 {
    let Some(four) = bytes.get(at..at + 4) else { return 0 };
    u32::from_le_bytes([four[0], four[1], four[2], four[3]])
}

fn put_word(bytes: &mut [u8], at: usize, value: u32) {
    if let Some(four) = bytes.get_mut(at..at + 4) {
        four.copy_from_slice(&value.to_le_bytes());
    }
}

/// What a frame header says, or nothing when it does not say it is one.
fn shape_of(head: &[u8]) -> Option<Shape> {
    if head.get(0..4) != Some(&MAGIC[..]) || word(head, 4) != VERSION {
        return None;
    }
    let (w, h, stride) = (word(head, 8), word(head, 12), word(head, 16));
    if w == 0 || h == 0 || stride < w * 4 {
        return None;
    }
    Some(Shape { w, h, stride })
}

/// The rectangles a frame names, cut down to what the panel can hold.
fn areas_of(head: &[u8], shape: Shape) -> Vec<Area> {
    let count = (word(head, 28) as usize).min(DIRTY_MOST);
    let mut areas = Vec::with_capacity(count);
    for at in 0..count {
        let base = 32 + at * 16;
        let (x, y, w, h) = (word(head, base), word(head, base + 4), word(head, base + 8), word(head, base + 12));
        // A rectangle that names no pixels is not an error; a frame that names
        // one past its own edge is waste, and copying past the end of a buffer
        // is worse than dropping it.
        let (x, y) = (x.min(shape.w), y.min(shape.h));
        let w = w.min(shape.w - x);
        let h = h.min(shape.h - y);
        if w == 0 || h == 0 {
            continue;
        }
        areas.push(Area { x, y, w, h });
    }
    areas
}

/// The frame file, on this side, and what has already been put on the panel.
struct Frame {
    file: fs::File,
    canvas: Canvas,
    seen: u32,
    ready: bool,
    /// What the plugin said its shape was, once it said anything.
    shape: Option<Shape>,
    /// Said only once per shape, because a plugin that disagrees with the panel
    /// about its size would otherwise write a line every frame.
    said: bool,
}

impl Frame {
    /// Make the frame file the plugin will map: a header of zeros and pixels
    /// the panel can hold. The file is in memory as far as the page is
    /// concerned — it lives on rootfs, which is a tmpfs — so a whole frame is a
    /// copy and not a write to the phone's storage.
    fn make(at: &Path, panel: (u32, u32)) -> Result<Frame> {
        let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).mode(0o600).open(at).with_context(|| format!("make the frame file at {}", at.display()))?;
        let pixels = u64::from(panel.0) * u64::from(panel.1) * 4;
        file.set_len(HEADER as u64 + pixels).with_context(|| format!("size the frame file at {}", at.display()))?;
        let mut head = [0u8; 64];
        head[0..4].copy_from_slice(MAGIC);
        put_word(&mut head, 4, VERSION);
        put_word(&mut head, 8, panel.0);
        put_word(&mut head, 12, panel.1);
        put_word(&mut head, 16, panel.0 * 4);
        file.write_all(&head).with_context(|| format!("write the frame header at {}", at.display()))?;
        Ok(Frame { file, canvas: Canvas::new(panel.0, panel.1), seen: 0, ready: false, shape: None, said: false })
    }

    /// Read the four kilobytes of header, as they are right now.
    fn head(&self) -> Result<[u8; 32]> {
        let mut head = [0u8; 32];
        self.file.read_exact_at(&mut head, 0).context("read the frame header")?;
        Ok(head)
    }

    /// Whether the plugin has drawn anything yet.
    fn started(&self) -> Result<bool> {
        let head = self.head()?;
        Ok(word(&head, 24) & READY != 0)
    }

    /// Whether the plugin has asked to be let go.
    fn leaving(&self) -> Result<bool> {
        let head = self.head()?;
        Ok(word(&head, 24) & QUIT != 0)
    }

    /// Whether the plugin is asking for a way to type.
    fn typing(&self) -> Result<bool> {
        let head = self.head()?;
        Ok(word(&head, 24) & KEYBOARD != 0)
    }

    /// Put the newest complete frame on the panel, and say whether anything was
    /// shown.
    ///
    /// The sequence number is written last by the plugin and is odd while it is
    /// writing, so a frame is read only when it is even and past the last one
    /// read: half a frame never reaches the panel.
    fn show(&mut self, surface: &mut dyn Surface) -> Result<bool> {
        let head = self.head()?;
        let Some(shape) = shape_of(&head) else { return Ok(false) };
        if !self.ready {
            if word(&head, 24) & READY == 0 {
                return Ok(false);
            }
            self.ready = true;
        }
        let seq = word(&head, 20);
        if seq == self.seen || seq % 2 == 1 {
            return Ok(false);
        }
        self.seen = seq;
        let panel = surface.size();
        if self.shape != Some(shape) {
            self.shape = Some(shape);
            if (shape.w, shape.h) != panel && !self.said {
                self.said = true;
                let _ = skyward_sys::kmsg(&format!(
                    "a plugin draws {}x{} into a panel of {}x{}; the rest of the panel is left as it was",
                    shape.w, shape.h, panel.0, panel.1
                ));
            }
        }
        let w = shape.w.min(panel.0);
        let h = shape.h.min(panel.1);
        let areas = areas_of(&head, shape);
        // No rectangles named, with the sequence number moved on, means the
        // whole frame changed: cheaper to copy than to ask why.
        if areas.is_empty() {
            self.whole(&shape, w, h)?;
        } else {
            for area in areas {
                self.some(&shape, area, w, h)?;
            }
        }
        surface.present(&self.canvas).context("put a plugin's frame on the panel")?;
        Ok(true)
    }

    /// Copy every pixel the panel can take.
    fn whole(&mut self, shape: &Shape, w: u32, h: u32) -> Result<()> {
        let row = (w * 4) as usize;
        if shape.stride == shape.w * 4 && w == shape.w && h == shape.h {
            // The common case: one read, and the panel is the plugin's size.
            let bytes = (w * h * 4) as usize;
            self.file.read_exact_at(&mut self.canvas.data[..bytes], HEADER as u64).context("read a whole frame")?;
            return Ok(());
        }
        for y in 0..h {
            let from = HEADER as u64 + u64::from(y) * u64::from(shape.stride);
            let to = (y * w * 4) as usize;
            self.file.read_exact_at(&mut self.canvas.data[to..to + row], from).context("read a row of a frame")?;
        }
        Ok(())
    }

    /// Copy one rectangle, clipped to the panel.
    fn some(&mut self, shape: &Shape, area: Area, w: u32, h: u32) -> Result<()> {
        let right = area.x + area.w;
        let bottom = area.y + area.h;
        if area.x >= w || area.y >= h {
            return Ok(());
        }
        let width = (right.min(w) - area.x) as usize;
        if width == 0 {
            return Ok(());
        }
        for y in area.y..bottom.min(h) {
            let from = HEADER as u64 + u64::from(y) * u64::from(shape.stride) + u64::from(area.x) * 4;
            let to = ((y * w + area.x) * 4) as usize;
            self.file.read_exact_at(&mut self.canvas.data[to..to + width * 4], from).with_context(|| format!("read the frame at {}x{}", area.x, y))?;
        }
        Ok(())
    }
}

/// The pipe one line per touch goes down, for the plugin to turn into a pointer
/// event. Opened read and write so that it neither blocks on a plugin that has
/// not opened it and does not report the plugin's absence as an error; written
/// without ever waiting, because a finger on the panel is this thread's work.
struct Pipe {
    file: fs::File,
}

impl Pipe {
    fn make(at: &Path) -> Result<Pipe> {
        let path = cstr(at)?;
        unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        // Read-write and non-blocking: a FIFO opened for writing alone waits for
        // a reader, and this may be opened before the plugin is there.
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(at)
            .with_context(|| format!("open the input pipe at {}", at.display()))?;
        Ok(Pipe { file })
    }

    fn say(&mut self, line: &str) {
        // A pipe that is full or gone is a plugin that is not reading; the
        // command is dropped and the panel stays responsive.
        let _ = self.file.write_all(line.as_bytes());
        let _ = self.file.write_all(b"\n");
    }

    fn touch(&mut self, what: &str, x: f32, y: f32, panel: (u32, u32)) {
        let x = x.round().clamp(0.0, panel.0 as f32) as u32;
        let y = y.round().clamp(0.0, panel.1 as f32) as u32;
        self.say(&format!("{what} {x} {y}"));
    }

    fn key(&mut self, name: &str) {
        self.say(&format!("key {name}"));
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

/// The loop device the image is attached to. `losetup` is the phone's own, and
/// the device it names is what gets detached when the plugin is done.
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

/// End everything the plugin started, not only the process that ended.
///
/// The plugin is started as the leader of its own process group for this
/// reason: a browser engine starts renderers, and a renderer left in the
/// background keeps the image's loop device busy. One negative signal reaches
/// the whole group.
fn end_group(id: u32) {
    let group = -i32::try_from(id).unwrap_or(i32::MAX);
    for (signal, named) in [(libc::SIGTERM, "term"), (libc::SIGKILL, "kill")] {
        if unsafe { libc::kill(group, signal) } != 0 {
            let wrong = os_error();
            // Nothing left to signal is the state being asked for, not a fault.
            if wrong.raw_os_error() != Some(libc::ESRCH) {
                let _ = skyward_sys::kmsg(&format!("the plugin's group would not take a {named}: {wrong}"));
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// What filesystem an image holds, read from the image itself.
///
/// The mount of an image needs its filesystem named: `mount(2)` with no type is
/// only allowed for a bind mount, and answers `EINVAL` for anything else. The
/// name cannot be trusted either (a payload copied in by hand carries whatever
/// name its downloader gave it), so the magic is what decides.
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
                let _ = skyward_sys::kmsg(&format!("a mount left by an earlier plugin session ({at}) came off"));
            } else {
                let _ = skyward_sys::kmsg(&format!("a mount left by an earlier plugin session ({at}) would not come off: {}", os_error()));
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
        if !(file.contains("/skyward/plugins/") || file.contains("/Skyward/")) {
            continue;
        }
        detach(&format!("/dev/block/{name}"));
        let _ = skyward_sys::kmsg(&format!("the loop device {name}, left on {} by an earlier plugin session, is detached", file.trim()));
    }
}

/// A plugin's folder, laid out and taken apart again.
///
/// Every step that mounts something records it first, so a failure half way
/// leaves nothing behind.
struct Lay {
    /// The plugin's own folder: writable, and where its frame, pipe and log are.
    root: PathBuf,
    /// The image, mounted read-only, as the plugin's files.
    img: PathBuf,
    device: Option<String>,
    mounted: Vec<PathBuf>,
}

impl Lay {
    fn of(name: &str, image: &Path) -> Result<Lay> {
        if !image.is_file() {
            bail!("{} is not a file", image.display());
        }
        clear_leftovers();
        let root = Path::new(WORK).join(name);
        let img = root.join("img");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&img).with_context(|| format!("make {}", img.display()))?;
        let mut lay = Lay { root, img, device: None, mounted: Vec::new() };
        let device = attach(image)?;
        lay.device = Some(device.clone());
        // Named from the image's own magic: a mount with no filesystem named
        // is answered with EINVAL, and the name is not a thing to guess at.
        let kind = kind_of(image).with_context(|| format!("{} is not an image this can mount", image.display()))?;
        let img = lay.img.clone();
        lay.onto(Some(Path::new(&device)), &img, Some(kind), libc::MS_RDONLY, None)?;
        Ok(lay)
    }

    /// Mount onto a folder, recording it so it comes off.
    ///
    /// `kind` is the filesystem's name, which the kernel insists on for
    /// anything that is not a bind mount or a remount.
    fn onto(&mut self, source: Option<&Path>, at: &Path, kind: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> Result<()> {
        mount(source, at, kind, flags, data)?;
        self.mounted.push(at.to_path_buf());
        Ok(())
    }

    /// The program's own path: the image's absolute path for it, understood as
    /// inside the image.
    fn program(&self, program: &str) -> Result<PathBuf> {
        let inner = Path::new(program.trim_start_matches('/'));
        let path = self.img.join(inner);
        let found = fs::metadata(&path).with_context(|| format!("{} names no file in the plugin's image", program))?;
        if !found.is_file() {
            bail!("{} names no file in the plugin's image", program);
        }
        if found.permissions().mode() & 0o111 == 0 {
            let _ = skyward_sys::kmsg(&format!("the plugin's program {program} is not marked runnable; trying it anyway"));
        }
        Ok(path)
    }

    /// Run the plugin's program, putting its frames on the panel until it ends.
    fn run(&self, name: &str, program: &str, inputs: &mut crate::input::Inputs, surface: &mut dyn Surface) -> Result<Ended> {
        let panel = surface.size();
        let frame_at = self.root.join("frame");
        let pipe_at = self.root.join("input");
        let log = self.root.join("log");
        let home = self.root.join("home");
        let _ = fs::create_dir_all(&home);
        let mut frame = Frame::make(&frame_at, panel)?;
        let mut pipe = Pipe::make(&pipe_at)?;
        let path = self.program(program)?;
        // The plugin's own output is what says why it would not start, so it is
        // kept in a file rather than thrown away. Nothing about this may stop
        // the plugin from running: a log that cannot be written is a line in the
        // kernel log and nothing more.
        let mut cmd = Command::new(&path);
        cmd.arg(start_url())
            .current_dir(&self.root)
            .env("PATH", "/system/bin:/system/xbin:/sbin:/bin")
            .env("SKYWARD_PLUGIN", "1")
            .env("SKYWARD_PLUGIN_ROOT", &self.img)
            .env("SKYWARD_PLUGIN_FRAME", &frame_at)
            .env("SKYWARD_PLUGIN_INPUT", &pipe_at)
            .env("SKYWARD_PANEL_W", panel.0.to_string())
            .env("SKYWARD_PANEL_H", panel.1.to_string())
            .env("SKYWARD_URL", start_url())
            .env("SKYWARD_USER_AGENT", USER_AGENT)
            .env("SKYWARD_DATA", storage())
            .env("SKYWARD_STORE", STORE)
            .env("SKYWARD_HOME", &home)
            .env("SKYWARD_LOG", &log)
            .stdin(Stdio::null());
        match fs::File::create(&log) {
            Ok(file) => match file.try_clone() {
                Ok(errors) => {
                    cmd.stdout(Stdio::from(file)).stderr(Stdio::from(errors));
                }
                Err(e) => {
                    let _ = skyward_sys::kmsg(&format!("the plugin's log is one stream only: {e}"));
                    cmd.stdout(Stdio::from(file));
                }
            },
            Err(e) => {
                let _ = skyward_sys::kmsg(&format!("the plugin's log cannot be written at {}: {e}", log.display()));
            }
        }
        let started = Instant::now();
        // Its own process group, so that what the plugin leaves running in the
        // background can be ended with it.
        cmd.process_group(0);
        let mut child = cmd.spawn().with_context(|| format!("start {name}'s program at {}", path.display()))?;
        // The first frame is the difference between a plugin that works and a
        // plugin that is a black panel, so it is waited for with a deadline.
        let mut ready = false;
        while started.elapsed() < READY_WITHIN {
            if let Some(status) = child.try_wait().context("ask after the plugin")? {
                let _ = skyward_sys::kmsg(&format!("{} ended before it drew anything: {status:?}; {} ends: {}", name, log.display(), tail(&log, 600)));
                bail!("the plugin ended before it drew anything ({status})");
            }
            if frame.started().unwrap_or(false) {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !ready {
            end_group(child.id());
            let _ = child.wait();
            let _ = skyward_sys::kmsg(&format!("{name} drew nothing in {READY_WITHIN:?}; {} ends: {}", log.display(), tail(&log, 600)));
            bail!("the plugin drew nothing in time");
        }
        // While it runs, the phone belongs to it twice over: the plugin's pixels
        // are put on the panel by this thread, and a finger on the panel is put
        // in the plugin's hand.
        let mut closed = false;
        let mut typed = false;
        let code = loop {
            if let Some(status) = child.try_wait().context("ask after the plugin")? {
                break status.code();
            }
            if started.elapsed() > LONGEST {
                let _ = child.kill();
                let _ = child.wait();
                let _ = skyward_sys::kmsg("the plugin was closed after two hours; the recovery has the phone back");
                break None;
            }
            if closed || frame.leaving().unwrap_or(false) {
                break None;
            }
            // A touch that waited for a frame to be built would be felt as a
            // touch that did not happen, so a finger goes first, every pass.
            if let Ok(events) = inputs.read() {
                for event in events {
                    match event {
                        crate::input::Event::Touch(Touch::Down(x, y), _) => pipe.touch("down", x, y, panel),
                        crate::input::Event::Touch(Touch::Move(x, y), _) => pipe.touch("move", x, y, panel),
                        crate::input::Event::Touch(Touch::Up, _) => pipe.touch("up", 0.0, 0.0, panel),
                        // The phone's own buttons are what a plugin has: back
                        // is its own back, and the power button ends it.
                        crate::input::Event::Key(Key::Back, true) => pipe.key("Back"),
                        crate::input::Event::Key(Key::Power, true) => closed = true,
                        crate::input::Event::Key(_, _) => {}
                    }
                }
            }
            // A plugin that wants to be typed into cannot get the recovery's
            // keyboard yet; it is told once, and draws its own until it can.
            if !typed && frame.typing().unwrap_or(false) {
                typed = true;
                let _ = skyward_sys::kmsg(&format!("{name} asked for a keyboard; the recovery has none to give it yet"));
            }
            if let Err(e) = frame.show(surface) {
                // A frame that will not go to the panel is worth a line and no
                // more: the plugin is still running and may yet be usable.
                let _ = skyward_sys::kmsg(&format!("a frame from the plugin was not shown: {e:#}"));
            }
            std::thread::sleep(FRAME);
        };
        // The process that ended is not always the last one holding something,
        // so it is asked to leave before it is made to.
        pipe.say("stop");
        let deadline = Instant::now() + STOP_WITHIN;
        while Instant::now() < deadline {
            match child.try_wait().context("ask after the plugin")? {
                Some(_) => break,
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        end_group(child.id());
        let _ = child.wait();
        // A plugin that ended at once is a plugin that failed; what it said
        // before it did is the only way to know why from the kernel log.
        if !matches!(code, Some(0) | None) {
            let _ = skyward_sys::kmsg(&format!("{} ended with {code:?}; {} ends: {}", name, log.display(), tail(&log, 600)));
        }
        keep_logs(name, &log);
        Ok(Ended { code, ran: started.elapsed() })
    }
}

impl Drop for Lay {
    fn drop(&mut self) {
        // The other way round from how they went on, and never fatally: a
        // mount that will not come off is one the next start replaces anyway.
        while let Some(at) = self.mounted.pop() {
            let Ok(target) = cstr(&at) else { continue };
            if unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) } != 0 {
                let _ = skyward_sys::kmsg(&format!("the plugin's mount at {} would not come off: {}", at.display(), os_error()));
            }
        }
        if let Some(device) = &self.device {
            // Everything written through the image and the bound partitions has
            // to be on the medium before the device goes. A phone rebooted with
            // dirty pages over a loop device is a phone whose kernel can hang in
            // its own shutdown, which is exactly what a plugin must not cause.
            unsafe { libc::sync() };
            detach(device);
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Where the plugins are installed. The page that installs them writes here,
/// and a plugin reads its own neighbours through this.
const STORE: &str = skyward_ui::app::PLUGIN_STORE;

/// What a plugin is told it is, so that a site can tell a recovery from a
/// desktop browser. The version is this build's own, so the string cannot
/// drift away from what a site is being told.
const USER_AGENT: &str = concat!("Skyward/", env!("CARGO_PKG_VERSION"), " (recovery plugin)");

/// Where the plugin opens. The recovery's own note of what a browser should
/// open with, and a name that answers from the mainland otherwise.
fn start_url() -> String {
    skyward_sys::prop("skyward.browser.url").unwrap_or_else(|| "https://cn.bing.com".to_string())
}

/// Where a plugin's files belong when the data partition is open, or nothing
/// when it is not.
fn storage() -> String {
    if skyward_block::mount::is_mount_point(Path::new("/data")).unwrap_or(false) {
        "/data/media/0/Skyward".to_string()
    } else {
        String::new()
    }
}

/// Keep what the plugin said where a phone that came back up can read it.
///
/// The log this module writes lives under the plugin's own folder on rootfs,
/// which is gone at reboot; internal storage is readable afterwards, and a copy
/// costs little and is the difference between "it exits" and why. Never fatal:
/// this is a diagnostic.
fn keep_logs(name: &str, log: &Path) {
    let storage = Path::new(skyward_crypto::data::SDCARD).join(KEPT);
    if let Err(e) = fs::create_dir_all(&storage) {
        let _ = skyward_sys::kmsg(&format!("the plugin's log has nowhere to be kept at {}: {e}", storage.display()));
        return;
    }
    if !log.is_file() {
        return;
    }
    let into = storage.join(format!("plugin-{name}.log"));
    match fs::copy(log, &into) {
        Ok(bytes) => {
            let _ = skyward_sys::kmsg(&format!("the plugin's log is kept at {} ({bytes} bytes)", into.display()));
        }
        Err(e) => {
            let _ = skyward_sys::kmsg(&format!("the plugin's log would not be kept at {}: {e}", into.display()));
        }
    }
}

/// The end of what the plugin said, on one line, for the kernel log.
fn tail(log: &Path, most: usize) -> String {
    let text = fs::read_to_string(log).unwrap_or_default();
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

/// Run the plugin `name` from `image`, whose program is `program` inside it.
///
/// The recovery keeps the display: the plugin draws into its own memory, and
/// this puts those pixels on the panel and hands it the touches, so `surface`
/// must be the one the recovery itself draws with and must stay open the whole
/// time the plugin runs.
pub fn run(name: &str, image: &Path, program: &str, inputs: &mut crate::input::Inputs, surface: &mut dyn Surface) -> Result<Ended> {
    let lay = Lay::of(name, image).with_context(|| format!("lay out {} for {name}", image.display()))?;
    lay.run(name, program, inputs, surface)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_says_what_shape_it_is_and_nothing_else_does() {
        let mut head = [0u8; 64];
        assert!(shape_of(&head).is_none(), "zeros are not a frame");
        head[0..4].copy_from_slice(MAGIC);
        assert!(shape_of(&head).is_none(), "a frame of no version is not one");
        put_word(&mut head, 4, VERSION);
        assert!(shape_of(&head).is_none(), "a frame of no shape is not one");
        put_word(&mut head, 8, 1440);
        put_word(&mut head, 12, 3200);
        assert!(shape_of(&head).is_none(), "a frame with no stride is not one");
        put_word(&mut head, 16, 1440 * 4);
        assert_eq!(shape_of(&head), Some(Shape { w: 1440, h: 3200, stride: 5760 }));
        put_word(&mut head, 4, VERSION + 1);
        assert!(shape_of(&head).is_none(), "another protocol is another frame");
    }

    #[test]
    fn a_frame_names_the_rectangles_that_changed_and_none_past_its_edge() {
        let mut head = [0u8; 288];
        head[0..4].copy_from_slice(MAGIC);
        put_word(&mut head, 4, VERSION);
        put_word(&mut head, 8, 1440);
        put_word(&mut head, 12, 3200);
        put_word(&mut head, 16, 5760);
        put_word(&mut head, 28, 2);
        // One inside the frame, one that runs off the right and the bottom.
        put_word(&mut head, 32, 10);
        put_word(&mut head, 36, 20);
        put_word(&mut head, 40, 30);
        put_word(&mut head, 44, 40);
        put_word(&mut head, 48, 1430);
        put_word(&mut head, 52, 3190);
        put_word(&mut head, 56, 100);
        put_word(&mut head, 60, 100);
        let shape = shape_of(&head).expect("a frame");
        let areas = areas_of(&head, shape);
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0], Area { x: 10, y: 20, w: 30, h: 40 });
        assert_eq!(areas[1], Area { x: 1430, y: 3190, w: 10, h: 10 }, "the frame's own edge is the edge");
        // A rectangle that names nothing is dropped rather than copied.
        put_word(&mut head, 48, 1440);
        put_word(&mut head, 52, 3200);
        put_word(&mut head, 56, 10);
        put_word(&mut head, 60, 10);
        assert_eq!(areas_of(&head, shape).len(), 1);
        // And more rectangles than the header holds are not read past it.
        put_word(&mut head, 28, 9999);
        assert!(areas_of(&head, shape).len() <= DIRTY_MOST);
    }

    #[test]
    fn a_touch_is_a_line_the_plugin_can_read() {
        let dir = std::env::temp_dir().join(format!("skyward-plugin-pipe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a folder");
        let at = dir.join("input");
        let mut pipe = Pipe::make(&at).expect("a pipe");
        pipe.touch("down", 10.4, 20.6, (1440, 3200));
        pipe.touch("move", -5.0, 9000.0, (1440, 3200));
        pipe.key("Back");
        let mut text = String::new();
        std::io::Read::read_to_string(&mut pipe.file, &mut text).expect("read the pipe");
        assert_eq!(text, "down 10 21\nmove 0 3200\nkey Back\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_image_is_told_apart_by_what_it_holds_and_not_by_its_name() {
        let dir = std::env::temp_dir().join(format!("skyward-plugin-kinds-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a folder");
        let cases: [(&str, &[u8], Option<&str>); 4] = [
            ("a.erofs", &[0xE2, 0xE1, 0xF5, 0xE0], Some("erofs")),
            ("b.squashfs", b"hsqs", Some("squashfs")),
            ("c.img", &[0x00, 0x01], None),
            ("d.ext4", &[0x00, 0x00], None),
        ];
        for (name, magic, want) in cases {
            let at = dir.join(name);
            let mut bytes = vec![0u8; 2048];
            let offset = match name {
                "a.erofs" => 1024,
                "d.ext4" => 1080,
                _ => 0,
            };
            bytes[offset..offset + magic.len()].copy_from_slice(magic);
            fs::write(&at, &bytes).expect("write an image");
            assert_eq!(kind_of(&at), want, "{name}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_shorter_than_the_line_asked_for_is_the_line() {
        let dir = std::env::temp_dir().join(format!("skyward-plugin-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a folder");
        let at = dir.join("log");
        fs::write(&at, "first line\nsecond line\n").expect("write a log");
        assert_eq!(tail(&at, 600), "first line second line");
        assert_eq!(tail(&at, 11), "second line");
        assert_eq!(tail(&dir.join("missing"), 600), "");
        let _ = fs::remove_dir_all(&dir);
    }
}
