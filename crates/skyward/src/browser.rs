//! Reading a web page, and saving a file it offers, on a thread of their
//! own that tells the screen how far they have come.

use crate::cdp::Cdp;
use anyhow::{Context, Result};
use skyward_net::http::{self, Servers};
use skyward_net::web::{self, Opened, Saving};
use std::fs::OpenOptions;
use std::io::Write;
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
    /// Run the plugin's `run.sh`, which writes the page to `out`.
    Full { dir: PathBuf, url: String, out: PathBuf },
    /// Start the persistent Chromium session used by the full browser.
    FullStart { dir: PathBuf, url: String },
    FullTap { x: u16, y: u16 },
    FullPinch { scale: f64 },
    FullText { text: String },
    FullReload,
    FullStop,
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

fn full_slot() -> &'static Mutex<Option<FullSession>> {
    FULL.get_or_init(|| Mutex::new(None))
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
                    "umount '{d}/dev/shm' 2>/dev/null || true; umount '{d}/sys' 2>/dev/null || true; umount '{d}/proc' 2>/dev/null || true; umount '{d}/dev' 2>/dev/null || true; rm -rf '{d}/tmp/chromium-profile'"
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
         mkdir -p \"$D/dev\" \"$D/proc\" \"$D/sys\" \"$D/dev/shm\"\n\
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
         /system/bin/mount -t tmpfs tmpfs \"$D/dev/shm\" 2>/dev/null || true\n\
         exec /system/bin/chroot \"$D\" /bin/sh /run.sh \"$SKYWARD_URL\" serve"
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
    let mut cmd = Command::new("/system/bin/sh");
    cmd.args(["-c", &script])
        .env("SKYWARD_URL", url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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
    Cdp::connect()
}

fn full_page_shot(cdp: &mut Cdp) -> Result<Vec<u8>> {
    cdp.screenshot()
}

fn full_result(cdp: &mut Cdp) -> (Result<Vec<u8>, String>, bool) {
    let focus = cdp.focus_is_text();
    (full_page_shot(cdp).map_err(|e| format!("{e:#}")), focus)
}

pub struct Worker {
    events: Receiver<Event>,
    /// Readable while events wait.
    wake: OwnedFd,
    stop: Arc<AtomicBool>,
    /// The connection in use, which stopping cuts at once.
    agent: Arc<Mutex<Option<ureq::Agent>>>,
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
        let stop = Arc::new(AtomicBool::new(false));
        let agent: Arc<Mutex<Option<ureq::Agent>>> = Arc::new(Mutex::new(None));
        let (stopping, using) = (stop.clone(), agent.clone());
        let thread = thread::Builder::new()
            .name("browser".into())
            .spawn(move || {
                let tell = |event: Event| {
                    let _ = tx.send(event);
                    unsafe { libc::write(signal.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
                };
                let a = http::agent(servers);
                *using.lock().expect("the agent is not held by a thread that panicked") = Some(a.clone());
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
                    Task::Full { dir, url, out } => {
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
                        let script = format!(
                            "D='{d}'\n\
                             /system/bin/umount \"$D/dev/shm\" 2>/dev/null || true\n\
                             /system/bin/umount \"$D/sys\" 2>/dev/null || true\n\
                             /system/bin/umount \"$D/proc\" 2>/dev/null || true\n\
                             /system/bin/umount \"$D/dev\" 2>/dev/null || true\n\
                             mkdir -p \"$D/dev\" \"$D/proc\" \"$D/sys\" \"$D/dev/shm\"\n\
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
                             /system/bin/mount -t tmpfs tmpfs \"$D/dev/shm\" 2>/dev/null || true\n\
                             /system/bin/chroot \"$D\" /bin/sh /run.sh \"$SKYWARD_URL\"\n\
                             s=$?\n\
                             /system/bin/umount \"$D/dev/shm\" 2>/dev/null || true\n\
                             /system/bin/umount \"$D/sys\" 2>/dev/null || true\n\
                             /system/bin/umount \"$D/proc\" 2>/dev/null || true\n\
                             /system/bin/umount \"$D/dev\" 2>/dev/null || true\n\
                             exit $s"
                        );
                        let ended = run_env_timeout("/system/bin/sh", &["-c", &script], "SKYWARD_URL", &url, Duration::from_secs(60))
                            .and_then(|_| std::fs::read(&out).map_err(|e| format!("read {}: {e}", out.display())));
                        tell(Event::Shot(ended));
                    }
                    Task::FullStart { dir, url } => {
                        let result = (|| -> Result<(Vec<u8>, bool)> {
                            let mut cdp = full_started(&dir, &url)?;
                            cdp.navigate(&url)?;
                            let focus = cdp.focus_is_text();
                            let png = cdp.screenshot()?;
                            Ok((png, focus))
                        })();
                        match result {
                            Ok((png, focus)) => {
                                tell(Event::Shot(Ok(png)));
                                tell(Event::Focus(focus));
                            }
                            Err(e) => {
                                stop_full();
                                tell(Event::Shot(Err(format!("{e:#}"))));
                            }
                        }
                    }
                    Task::FullTap { x, y } => {
                        let result = Cdp::connect().and_then(|mut cdp| {
                            let cx = (x as u32 * crate::cdp::WIDTH / 1000).min(crate::cdp::WIDTH - 1);
                            let cy = (y as u32 * crate::cdp::HEIGHT / 1000).min(crate::cdp::HEIGHT - 1);
                            cdp.touch(cx, cy)?;
                            Ok(full_result(&mut cdp))
                        });
                        match result {
                            Ok((png, focus)) => {
                                tell(Event::Shot(png));
                                tell(Event::Focus(focus));
                            }
                            Err(e) => {
                                stop_full();
                                tell(Event::Shot(Err(format!("{e:#}"))));
                            }
                        }
                    }
                    Task::FullPinch { scale } => {
                        let result = Cdp::connect().and_then(|mut cdp| {
                            cdp.pinch(crate::cdp::WIDTH / 2, crate::cdp::HEIGHT / 2, scale)?;
                            Ok(full_result(&mut cdp))
                        });
                        match result {
                            Ok((png, focus)) => {
                                tell(Event::Shot(png));
                                tell(Event::Focus(focus));
                            }
                            Err(e) => {
                                stop_full();
                                tell(Event::Shot(Err(format!("{e:#}"))));
                            }
                        }
                    }
                    Task::FullText { text } => {
                        let result = Cdp::connect().and_then(|mut cdp| {
                            cdp.insert_text(&text)?;
                            Ok(full_result(&mut cdp))
                        });
                        match result {
                            Ok((png, _focus)) => {
                                tell(Event::Shot(png));
                                tell(Event::Focus(false));
                            }
                            Err(e) => {
                                stop_full();
                                tell(Event::Shot(Err(format!("{e:#}"))));
                            }
                        }
                    }
                    Task::FullReload => {
                        let result = Cdp::connect().and_then(|mut cdp| {
                            cdp.reload()?;
                            Ok(full_result(&mut cdp))
                        });
                        match result {
                            Ok((png, focus)) => {
                                tell(Event::Shot(png));
                                tell(Event::Focus(focus));
                            }
                            Err(e) => {
                                stop_full();
                                tell(Event::Shot(Err(format!("{e:#}"))));
                            }
                        }
                    }
                    Task::FullStop => stop_full(),
                }
            })
            .map_err(|e| anyhow::anyhow!("start the browser's worker: {e}"))?;
        Ok(Worker { events, wake, stop, agent, thread: Some(thread) })
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

    /// Stop at once, cutting the connection in use.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(a) = self.agent.lock().expect("the agent is not held by a thread that panicked").take() {
            a.shutdown();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
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

fn run(command: &str, args: &[&str]) -> Result<(), String> {    let output = std::process::Command::new(command)
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

fn run_env_timeout(command: &str, args: &[&str], key: &str, value: &str, timeout: Duration) -> Result<(), String> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .env(key, value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A group of its own, so a hung Chromium is killed without touching the
    // recovery that started it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("run {command}: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("{command} exited {status}")),
            Ok(None) => {}
            Err(e) => return Err(format!("wait for {command}: {e}")),
        }
        if Instant::now() >= deadline {
            let pid = child.id() as i32;
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{command} timed out after {} seconds", timeout.as_secs()));
        }
        thread::sleep(Duration::from_millis(100));
    }
}
