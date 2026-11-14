//! Reading a web page, and saving a file it offers, on a thread of their
//! own that tells the screen how far they have come.

use crate::cdp::Cdp;
use anyhow::{Context, Result};
use skyward_net::http::{self, Servers};
use skyward_net::web::{self, Opened, Saving};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::ToSocketAddrs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The basic browser's own log, which the log page exports beside the
/// recovery and kernel logs.
pub const LOG: &str = "/skyward/run/browser.log";

/// The most often how far a save has come is told.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

pub enum Task {
    /// Read the page at this address, with the cookies kept for it.
    Open { url: String, cookie: Option<String> },
    /// Send a form the page carries.
    Submit { url: String, fields: Vec<(String, String)>, post: bool, cookie: Option<String> },
    /// Save the file at this address to this path.
    Save { url: String, path: PathBuf },
    /// Unpack the plugin package at `archive` into `dir`.
    Install { archive: PathBuf, dir: PathBuf },
    /// Start the persistent Chromium session used by the full browser, drawn
    /// for a box `view` device pixels wide and tall.
    FullStart { dir: PathBuf, url: String, view: (u32, u32) },
    /// Touch the page where the frame was touched, in thousandths of it.
    FullTap { x: u16, y: u16 },
    /// Scroll the page under the frame: `dy` thousandths of the frame's
    /// height, and `speed` in CSS pixels a second, which is what a flick
    /// keeps gliding at.
    FullScroll { dy: i32, speed: i32 },
    FullPinch { scale: f64 },
    FullText { text: String },
    FullReload,
    FullStop,
    /// End the worker's thread: nothing more is asked of it.
    Halt,
}

pub enum Event {
    /// The address turned out to hold a page or a file, or why it did not.
    Opened(Result<Opened, String>),
    /// How far the file has been saved.
    Saved(Saving),
    /// The file is whole, or why it is not.
    Ended(Result<u64, String>),
    /// The plugin was unpacked, or why it was not.
    Installed(Result<(), String>),
    /// The plugin rendered a page, or why it did not.
    Shot(Result<Vec<u8>, String>),
    /// A text field on the plugin page took focus.
    Focus(bool),
}

struct FullSession {
    child: Child,
    dir: PathBuf,
}

static FULL: OnceLock<Mutex<Option<FullSession>>> = OnceLock::new();

/// The box the recovery draws the full mode's frame in, in its own pixels.
/// Every frame is drawn for it, so that what Chromium paints is what the
/// screen shows and nothing has to be scaled afterwards - which is most of
/// what showing a frame costs.
static VIEW: Mutex<(u32, u32)> = Mutex::new((0, 0));

fn set_view(view: (u32, u32)) {
    *VIEW.lock().expect("the frame's box is not held by a thread that panicked") = view;
}

fn full_slot() -> &'static Mutex<Option<FullSession>> {
    FULL.get_or_init(|| Mutex::new(None))
}

/// Where the screen is told what the page did, from whichever thread is
/// drawing it: the worker that takes the gestures, or the one that keeps
/// drawing the page while it moves.
struct Alerts {
    events: Mutex<mpsc::Sender<Event>>,
    wake: OwnedFd,
}

impl Alerts {
    fn tell(&self, event: Event) {
        let _ = self.events.lock().expect("the browser's events are not held by a thread that panicked").send(event);
        unsafe { libc::write(self.wake.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }
}

fn stop_full() {
    let mut slot = full_slot().lock().expect("the full browser slot is not poisoned");
    if let Some(mut session) = slot.take() {
        let pid = session.child.id() as i32;
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = session.child.kill();
        let _ = session.child.wait();
        let d = session.dir.display().to_string();
        let _ = Command::new("/system/bin/sh")
            .args([
                "-c",
                &format!(
                    "umount '{d}/dev/shm' 2>/dev/null || true; umount '{d}/tmp' 2>/dev/null || true; umount '{d}/sys' 2>/dev/null || true; umount '{d}/proc' 2>/dev/null || true; umount '{d}/dev' 2>/dev/null || true; rm -rf '{d}/tmp/chromium-profile'"
                ),
            ])
            .status();
    }
}

fn full_launch_script(dir: &Path, url: &str) -> String {
    let d = dir.display().to_string();
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    let mut hosts = String::new();
    if let Some(ip) = resolved(host) {
        hosts.push_str(&format!("printf '%s %s\\n' '{ip}' '{host}' >> \"$D/etc/hosts\"\n"));
        if host.ends_with("bing.com") {
            for name in ["www.bing.com", "cn.bing.com", "th.bing.com", "r.bing.com", "c.bing.com"] {
                if let Some(ip) = resolved(name) {
                    hosts.push_str(&format!("printf '%s %s\\n' '{ip}' '{name}' >> \"$D/etc/hosts\"\n"));
                }
            }
        }
    }
    format!(
        "D='{d}'\n\
         /system/bin/umount \"$D/dev/shm\" 2>/dev/null || true\n\
         /system/bin/umount \"$D/sys\" 2>/dev/null || true\n\
         /system/bin/umount \"$D/proc\" 2>/dev/null || true\n\
         /system/bin/umount \"$D/dev\" 2>/dev/null || true\n\
         mkdir -p \"$D/dev\" \"$D/proc\" \"$D/sys\"\n\
         GW=\n\
         while read IF DEST GATEWAY REST; do if [ \"$DEST\" = \"00000000\" ]; then GW=$GATEWAY; break; fi; done < /proc/net/route 2>/dev/null || true\n\
         GLEN=$(printf '%s' \"$GW\" | wc -c)\n\
         if [ \"$GLEN\" = 8 ]; then GW=$(printf '%d.%d.%d.%d' 0x$(printf '%s' \"$GW\" | cut -c7-8) 0x$(printf '%s' \"$GW\" | cut -c5-6) 0x$(printf '%s' \"$GW\" | cut -c3-4) 0x$(printf '%s' \"$GW\" | cut -c1-2)); else GW=; fi\n\
         DNS=$GW\n\
         [ -n \"$DNS\" ] || DNS=$(getprop dhcp.wlan0.gateway 2>/dev/null || true)\n\
         [ -n \"$DNS\" ] || DNS=$(getprop net.dns1 2>/dev/null || true)\n\
         if [ -n \"$DNS\" ]; then printf 'nameserver %s\\n' \"$DNS\" > \"$D/etc/resolv.conf\"; fi\n\
         [ -s \"$D/etc/resolv.conf\" ] || cp /etc/resolv.conf \"$D/etc/resolv.conf\" 2>/dev/null || true\n\
         {hosts}\
         /system/bin/mount -o bind /dev \"$D/dev\" || exit 1\n\
         /system/bin/mount -t proc proc \"$D/proc\" || exit 1\n\
         /system/bin/mount -t sysfs sysfs \"$D/sys\" || exit 1\n\
         # /dev/shm has to be made inside the /dev that is now bound over it,\n\
         # or the tmpfs below is asked for a folder that is not there.\n\
         mkdir -p \"$D/dev/shm\" && /system/bin/mount -t tmpfs tmpfs \"$D/dev/shm\" 2>/dev/null || true\n\
         # The profile Chromium keeps is full of sockets, and the card this root\n\
         # sits on is not a filesystem that has them: memory is. The log is kept\n\
         # beside the plugin, so a tmpfs here does not hide it.\n\
         mkdir -p \"$D/tmp\" && /system/bin/mount -t tmpfs tmpfs \"$D/tmp\" 2>/dev/null || true\n\
         # The recovery's own PATH and temp folder are not in here, and a plugin\n\
         # installed before this image still looks for both of them. The root is\n\
         # a Debian one, where Chromium's own libraries sit beside it in\n\
         # /usr/lib/chromium and the rest in /usr/lib/aarch64-linux-gnu, and no\n\
         # ldconfig has run here to say so: the second is told without it.\n\
         exec /system/bin/env PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin TMPDIR=/tmp TMP=/tmp HOME=/root LD_LIBRARY_PATH=/usr/lib/chromium:/usr/lib/aarch64-linux-gnu /system/bin/chroot \"$D\" /bin/sh /run.sh \"$SKYWARD_URL\" serve"
    )
}

fn start_full(dir: &Path, url: &str) -> Result<()> {
    let mut slot = full_slot().lock().expect("the full browser slot is not poisoned");
    if let Some(session) = slot.as_mut() {
        if session.child.try_wait().ok().flatten().is_none() {
            return Ok(());
        }
        slot.take();
    }
    let script = full_launch_script(dir, url);
    // Chromium says why it would not start on its own output, and the page
    // that offers to install it asks for that when it goes wrong: what it
    // prints is kept beside the plugin, so the log outlives this session.
    let log = dir.join("chromium.log");
    let sink = || match std::fs::OpenOptions::new().create(true).append(true).open(&log) {
        Ok(file) => Stdio::from(file),
        Err(_) => Stdio::null(),
    };
    let mut cmd = Command::new("/system/bin/sh");
    cmd.args(["-c", &script])
        .env("SKYWARD_URL", url)
        .stdin(Stdio::null())
        .stdout(sink())
        .stderr(sink());
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let child = cmd.spawn().context("start the full browser")?;
    *slot = Some(FullSession { child, dir: dir.to_path_buf() });
    Ok(())
}

fn full_started(dir: &Path, url: &str) -> Result<Cdp> {
    start_full(dir, url)?;
    attach_page()
}

/// Whether the plugin's locale folder is there and empty of packs, in which
/// case Chromium has no word to draw and stops in a way that says nothing.
/// A root that keeps its packs elsewhere is not this, and says nothing.
fn locales_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir.join("usr/lib/chromium/locales"))
        .map(|entries| !entries.flatten().any(|entry| entry.path().extension().is_some_and(|ext| ext == "pak")))
        .unwrap_or(false)
}

/// The page the full mode is showing. Attaching to it costs more than drawing
/// one more frame of it, and a finger moves many times, so the connection is
/// kept between gestures and put back whenever it breaks.
static PAGE: Mutex<Option<Cdp>> = Mutex::new(None);

fn attach_page() -> Result<Cdp> {
    let view = *VIEW.lock().expect("the frame's box is not held by a thread that panicked");
    if view.0 == 0 || view.1 == 0 {
        Cdp::connect_default()
    } else {
        Cdp::connect(view)
    }
}

/// Use the page, attaching to it first if it is not attached. A page that
/// fails is let go, so the next gesture attaches to it afresh rather than
/// speaking to a socket that no longer answers.
fn with_page<T>(act: impl FnOnce(&mut Cdp) -> Result<T>) -> Result<T> {
    let mut slot = PAGE.lock().expect("the page is not held by a thread that panicked");
    if slot.is_none() {
        *slot = Some(attach_page()?);
    }
    let page = slot.as_mut().expect("the page was attached just above");
    match act(page) {
        Ok(value) => Ok(value),
        Err(e) => {
            *slot = None;
            Err(e)
        }
    }
}

/// What the page drew before, and whether it was the same as the frame before
/// that: a page whose picture keeps changing is one to go on drawing.
static LAST_DREW: Mutex<Option<Vec<u8>>> = Mutex::new(None);
static MOVING: AtomicBool = AtomicBool::new(false);

/// The thread that goes on drawing the page while it moves, so that a drag, an
/// animation or a video is seen moving with the finger still on the glass.
/// A gesture only sends what the finger did and never waits for a frame, so it
/// costs what the page takes to answer rather than what it takes to draw.
static PUMP: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static PUMPING: AtomicBool = AtomicBool::new(false);
/// Set by a gesture that changed the page, so that the frame it left is drawn
/// at once rather than at the next look when nothing is happening.
static ASK: AtomicBool = AtomicBool::new(false);

/// How far apart the frames are while the page moves, and how often it is
/// looked at when nothing is happening.
const MOVING_GAP_MS: u64 = 90;
const IDLE_GAP_MS: u64 = 400;

fn poke() {
    ASK.store(true, Ordering::Relaxed);
}

/// Whether the frame differs from the one before it: one that stands still is
/// drawn again rarely, one that moves is drawn as fast as it comes.
fn drew_again(png: &[u8]) -> bool {
    let mut last = LAST_DREW.lock().expect("the last frame is not held by a thread that panicked");
    let changed = last.as_deref() != Some(png);
    if changed {
        *last = Some(png.to_vec());
    }
    MOVING.store(changed, Ordering::Relaxed);
    changed
}

/// Start drawing the page, or start over on a page that was started again.
fn start_pump(alerts: Arc<Alerts>) {
    stop_pump();
    PUMPING.store(true, Ordering::Relaxed);
    poke();
    let spawned = thread::Builder::new().name("browser-frame".into()).spawn(move || {
        let mut cdp = match attach_page() {
            Ok(cdp) => cdp,
            // The page has not woken yet: the next gesture starts it again.
            Err(_) => return,
        };
        let mut drew = Instant::now() - Duration::from_secs(1);
        while PUMPING.load(Ordering::Relaxed) {
            let asked = ASK.swap(false, Ordering::Relaxed);
            // A frame that differs from the one before it is a page that is
            // still drawing, and is drawn as fast as it comes. Nothing is
            // asked of the page itself here: a page that is still loading
            // answers a script of its own in seconds, and a frame that waited
            // for that answer would be the picture standing still.
            let busy = MOVING.load(Ordering::Relaxed);
            let gap = Duration::from_millis(if busy || asked { MOVING_GAP_MS } else { IDLE_GAP_MS });
            if !asked && drew.elapsed() < gap {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            let png = match cdp.screenshot() {
                Ok(png) => png,
                // A page that stops answering is let go rather than drawn as it
                // was: the next gesture attaches to it afresh.
                Err(_) => break,
            };
            drew = Instant::now();
            if drew_again(&png) {
                alerts.tell(Event::Shot(Ok(png)));
            }
        }
    });
    if let Ok(handle) = spawned {
        *PUMP.lock().expect("the frame thread is not held by a thread that panicked") = Some(handle);
    }
}

fn stop_pump() {
    PUMPING.store(false, Ordering::Relaxed);
    if let Some(handle) = PUMP.lock().expect("the frame thread is not held by a thread that panicked").take() {
        let _ = handle.join();
    }
}

/// Tell the thread drawing the page to stop, without waiting for a frame of a
/// page that may not answer any more: it lets go of its own account when the
/// frame it is waiting on comes back or fails.
fn halt_pump() {
    PUMPING.store(false, Ordering::Relaxed);
    *PUMP.lock().expect("the frame thread is not held by a thread that panicked") = None;
}

/// Do one thing to the page and ask for the frame it left, or say why it could
/// not: the frame itself comes from the pump, which is drawing already.
fn full_after(tell: &dyn Fn(Event), act: impl FnOnce(&mut Cdp) -> Result<()>) {
    match with_page(|cdp| {
        act(cdp)?;
        Ok(cdp.focus_is_text())
    }) {
        Ok(focus) => {
            poke();
            tell(Event::Focus(focus));
        }
        Err(e) => {
            let why = format!("{e:#}");
            note(&format!("the page went away: {}", why.replace('\n', " ⏎ ")));
            stop_full();
            tell(Event::Shot(Err(why)));
        }
    }
}

/// Do one thing to the page that cannot have changed which field is being
/// typed into, and ask for the frame it left. Asking a page what it has
/// focused is a script run in the page, which a page in the middle of loading
/// can hold up for seconds: a drag that waited for that answer would feel
/// stuck, so only the gestures that can move the focus ask at all.
fn full_do(tell: &dyn Fn(Event), act: impl FnOnce(&mut Cdp) -> Result<()>) {
    match with_page(act) {
        Ok(()) => poke(),
        Err(e) => {
            let why = format!("{e:#}");
            note(&format!("the page went away: {}", why.replace('\n', " ⏎ ")));
            stop_full();
            tell(Event::Shot(Err(why)));
        }
    }
}

/// What Chromium itself said last, for a page that has to say why the full
/// mode would not start: its own last words rather than ours, so a plugin
/// that is missing something names it. The bus it cannot reach is the
/// recovery's ordinary noise and is left out, and so is everything before the
/// end: a plugin that has run for a while says a great deal that is not the
/// reason it stopped.
fn chromium_said(dir: &Path) -> Option<String> {
    /// The end of the log that is read, and the lines of it that are kept.
    const TAIL: u64 = 64 * 1024;
    const LINES: usize = 6;
    let mut file = std::fs::File::open(dir.join("chromium.log")).ok()?;
    let len = file.metadata().ok()?.len();
    if len > TAIL {
        file.seek(SeekFrom::End(-(TAIL as i64))).ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut kept: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.contains("dbus/") && !line.contains("object_proxy.cc") && !line.contains("Failed to call method"))
        .collect();
    // A read from the middle of the log starts in the middle of a line.
    if len > TAIL && !kept.is_empty() {
        kept.remove(0);
    }
    if kept.len() > LINES {
        kept.drain(..kept.len() - LINES);
    }
    if kept.is_empty() {
        return None;
    }
    Some(kept.join("\n"))
}

pub struct Worker {
    events: Receiver<Event>,
    /// Readable while events wait.
    wake: OwnedFd,
    stop: Arc<AtomicBool>,
    /// The connection in use, which stopping cuts at once.
    agent: Arc<Mutex<Option<ureq::Agent>>>,
    /// What is asked of the thread, which keeps working through the list.
    asking: mpsc::Sender<Task>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(task: Task, servers: Servers) -> Result<Worker> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(anyhow::anyhow!("make the browser's wake pipe: {}", std::io::Error::last_os_error()));
        }
        let (wake, signal) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let (tx, events) = mpsc::channel();
        let alerts = Arc::new(Alerts { events: Mutex::new(tx), wake: signal });
        let stop = Arc::new(AtomicBool::new(false));
        let agent: Arc<Mutex<Option<ureq::Agent>>> = Arc::new(Mutex::new(None));
        let (stopping, using) = (stop.clone(), agent.clone());
        let (asking, queue) = mpsc::channel::<Task>();
        let told = alerts.clone();
        let thread = thread::Builder::new()
            .name("browser".into())
            .spawn(move || {
                let a = http::agent(servers);
                *using.lock().expect("the agent is not held by a thread that panicked") = Some(a.clone());
                // One worker for as long as the screen keeps it: a gesture is a
                // task handed to it, not a worker of its own, so that a drag
                // never waits for a connection to be made first.
                let run = |task: Task| {
                    let tell = |event: Event| told.tell(event);
                    match task {
                    Task::Open { url, cookie } => {
                        note(&format!("open {url}"));
                        let opened = web::open_with(&a, &url, cookie.as_deref()).map_err(|e| format!("{e:#}"));
                        match &opened {
                            Ok(Opened::Page(page)) => note(&format!("opened {} ({})", page.url, page.blocks.len())),
                            Ok(Opened::File(file)) => note(&format!("file {} ({})", file.name, file.kind)),
                            Err(why) => note(&format!("open {url} failed: {why}")),
                        }
                        tell(Event::Opened(opened));
                    }
                    Task::Submit { url, fields, post, cookie } => {
                        note(&format!("submit {} {url} with {} fields", if post { "POST" } else { "GET" }, fields.len()));
                        let opened = web::submit(&a, &url, &fields, post, cookie.as_deref()).map_err(|e| format!("{e:#}"));
                        match &opened {
                            Ok(Opened::Page(page)) => note(&format!("submitted to {} ({})", page.url, page.blocks.len())),
                            Ok(Opened::File(file)) => note(&format!("submitted to file {}", file.name)),
                            Err(why) => note(&format!("submit {url} failed: {why}")),
                        }
                        tell(Event::Opened(opened));
                    }
                    Task::Save { url, path } => {
                        note(&format!("save {url} to {}", path.display()));
                        let mut told = Instant::now() - PROGRESS_EVERY;
                        let mut progress = |saving: Saving| {
                            if told.elapsed() >= PROGRESS_EVERY || saving.total.is_some_and(|t| saving.done >= t) {
                                told = Instant::now();
                                tell(Event::Saved(saving));
                            }
                        };
                        let ended = web::save(&a, &url, &path, &stopping, &mut progress).map_err(|e| {
                            if stopping.load(Ordering::Relaxed) {
                                String::from("stopped as asked")
                            } else {
                                format!("{e:#}")
                            }
                        });
                        note(&match &ended {
                            Ok(size) => format!("saved {} ({size} bytes)", path.display()),
                            Err(why) => format!("save {} failed: {why}", path.display()),
                        });
                        tell(Event::Ended(ended));
                    }
                    Task::Install { archive, dir } => {
                        let archive_name = archive.display().to_string();
                        let ended = (if dir.exists() {
                            std::fs::remove_dir_all(&dir).map_err(|e| format!("clear {}: {e}", dir.display()))
                        } else {
                            Ok(())
                        })
                        .and_then(|()| std::fs::create_dir_all(&dir).map_err(|e| format!("make {}: {e}", dir.display())))
                        .and_then(|()| {
                            if archive_name.ends_with(".tar.gz") || archive_name.ends_with(".tgz") {
                                run(
                                    "/system/bin/tar",
                                    &["-xzf", &archive_name, "-C", &dir.display().to_string()],
                                )
                            } else {
                                run(
                                    "/system/bin/unzip",
                                    &["-o", &archive_name, "-d", &dir.display().to_string()],
                                )
                            }
                        })
                        .and_then(|()| {
                            run(
                                "/system/bin/chmod",
                                &["-R", "755", &dir.display().to_string()],
                            )
                        });
                        tell(Event::Installed(ended));
                    }
                    Task::FullStart { dir, url, view } => {
                        set_view(view);
                        // The page is attached, then drawn while it loads:
                        // asking it to load first and drawing afterwards is a
                        // screen that shows nothing until the whole page is
                        // in, which on a phone can be ten seconds.
                        let opened = full_started(&dir, &url).map(|cdp| {
                            *PAGE.lock().expect("the page is not held by a thread that panicked") = Some(cdp);
                        });
                        match opened {
                            // The page is drawn by the thread that goes on
                            // drawing it, from a connection of its own, so that
                            // a gesture never waits for a frame.
                            Ok(()) => {
                                start_pump(told.clone());
                                // Loading is asked for after the drawing has
                                // begun; what went wrong is said on the page's
                                // own box, as a gesture's failure is.
                                full_do(&tell, |cdp| cdp.navigate(&url));
                            }
                            Err(e) => {
                                stop_full();
                                *LAST_DREW.lock().expect("the last frame is not held by a thread that panicked") = None;
                                let mut why = format!("{e:#}");
                                // What went wrong is said on the screen, which
                                // is no use once the phone is in someone
                                // else's hands: it is kept here too, on one
                                // line, so the log says what the page said.
                                note(&format!("the full mode stopped: {}", why.replace('\n', " ⏎ ")));
                                if let Some(said) = chromium_said(&dir) {
                                    note(&format!("the full browser said: {}", said.replace('\n', " ⏎ ")));
                                    why.push_str("\n\n");
                                    why.push_str(&said);
                                }
                                if locales_empty(&dir) {
                                    note("the plugin has no locale pack under usr/lib/chromium/locales, which the full mode stops without");
                                    why.push_str("\n\nusr/lib/chromium/locales has no .pak, and Chromium stops as soon as it wants a word: it needs the one that matches it");
                                }
                                tell(Event::Shot(Err(why)));
                            }
                        }
                    }
                    Task::FullTap { x, y } => full_after(&tell, |cdp| cdp.tap(x as u32, y as u32)),
                    Task::FullScroll { dy, speed } => full_do(&tell, |cdp| {
                        if dy != 0 {
                            cdp.scroll(dy)?;
                        }
                        // The finger lifted - or stopped moving, which lifts it
                        // too: the page is let go of where it is, going the way
                        // it was going.
                        if dy == 0 || speed != 0 {
                            cdp.drag_end(speed)?;
                        }
                        Ok(())
                    }),
                    Task::FullPinch { scale } => full_do(&tell, |cdp| cdp.pinch(scale)),
                    Task::FullText { text } => full_after(&tell, |cdp| cdp.insert_text(&text)),
                    Task::FullReload => full_do(&tell, |cdp| cdp.reload()),
                    Task::FullStop => {
                        // A finger still down from a drag is lifted, so the
                        // next page does not open with one held on it.
                        let _ = with_page(|cdp| cdp.drag_end(0));
                        // The browser is stopped before the thread drawing it,
                        // so that a frame waiting on it comes back at once.
                        stop_full();
                        stop_pump();
                        *PAGE.lock().expect("the page is not held by a thread that panicked") = None;
                        *LAST_DREW.lock().expect("the last frame is not held by a thread that panicked") = None;
                        MOVING.store(false, Ordering::Relaxed);
                    }
                    // The loop below ends on this rather than doing anything
                    // with it, so this arm is only here to make the match whole.
                    Task::Halt => {}
                    }
                };
                while let Ok(mut task) = queue.recv() {
                    if stopping.load(Ordering::Relaxed) || matches!(&task, Task::Halt) {
                        break;
                    }
                    // A drag asks once per move while a page is still catching
                    // up with the last one: what the finger did a moment ago is
                    // already out of date, so only where it is now is sent, and
                    // the moves it has passed are added to that one.
                    while let Ok(newer) = queue.try_recv() {
                        let merged = match (&task, &newer) {
                            (Task::FullScroll { dy: passed, speed: 0 }, Task::FullScroll { dy: now, speed }) => {
                                Some(Task::FullScroll { dy: passed.saturating_add(*now), speed: *speed })
                            }
                            _ => None,
                        };
                        if let Some(merged) = merged {
                            task = merged;
                            continue;
                        }
                        run(task);
                        task = newer;
                        if matches!(&task, Task::Halt) {
                            break;
                        }
                    }
                    if stopping.load(Ordering::Relaxed) || matches!(&task, Task::Halt) {
                        break;
                    }
                    run(task);
                }
            })
            .map_err(|e| anyhow::anyhow!("start the browser's worker: {e}"))?;
        let _ = asking.send(task);
        Ok(Worker { events, wake, stop, agent, asking, thread: Some(thread) })
    }

    pub fn fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// The events since the last call.
    pub fn events(&self) -> Vec<Event> {
        let mut drain = [0u8; 64];
        while unsafe { libc::read(self.wake.as_raw_fd(), drain.as_mut_ptr().cast(), drain.len()) } > 0 {}
        self.events.try_iter().collect()
    }

    /// Hand the thread one more thing to do, without waiting for it.
    pub fn tell(&self, task: Task) {
        let _ = self.asking.send(task);
    }

    /// Stop at once, cutting the connection in use.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // The thread drawing the page is let go of here rather than waited for
        // in the drop: a frame it is waiting on would hold the screen up.
        halt_pump();
        let _ = self.asking.send(Task::Halt);
        if let Some(a) = self.agent.lock().expect("the agent is not held by a thread that panicked").take() {
            a.shutdown();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // The thread ends on this, so that a worker dropped while its list is
        // empty still lets its thread go.
        let _ = self.asking.send(Task::Halt);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the browser's worker does not panic");
        }
    }
}

fn resolved(host: &str) -> Option<String> {
    if host.is_empty() {
        return None;
    }
    (host, 443)
        .to_socket_addrs()
        .ok()?
        .find(|addr| addr.is_ipv4())
        .map(|addr| addr.ip().to_string())
}

fn note(line: &str) {
    if let Some(dir) = Path::new(LOG).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(LOG) {
        let _ = writeln!(file, "{line}");
    }
}

fn run(command: &str, args: &[&str]) -> Result<(), String> {
    let output = std::process::Command::new(command)
        .args(args)
        .output()
        .map_err(|e| format!("run {command}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if err.is_empty() { format!("{command} exited {}", output.status) } else { err })
    }
}

