//! Reading a web page, and saving a file it offers, on a thread of their
//! own that tells the screen how far they have come.

use anyhow::Result;
use skyward_net::http::{self, Servers};
use skyward_net::web::{self, Opened, Saving};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
                        let opened = web::open_with(&a, &url, cookie.as_deref()).map_err(|e| format!("{e:#}"));
                        tell(Event::Opened(opened));
                    }
                    Task::Submit { url, fields, post, cookie } => {
                        let opened = web::submit(&a, &url, &fields, post, cookie.as_deref()).map_err(|e| format!("{e:#}"));
                        tell(Event::Opened(opened));
                    }
                    Task::Save { url, path } => {
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
                        let script = format!(
                            "D='{d}'\n\
                             /system/bin/umount \"$D/dev/shm\" 2>/dev/null || true\n\
                             /system/bin/umount \"$D/sys\" 2>/dev/null || true\n\
                             /system/bin/umount \"$D/proc\" 2>/dev/null || true\n\
                             /system/bin/umount \"$D/dev\" 2>/dev/null || true\n\
                             mkdir -p \"$D/dev\" \"$D/proc\" \"$D/sys\" \"$D/dev/shm\"\n\
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
                        let ended = run_env("/system/bin/sh", &["-c", &script], "SKYWARD_URL", &url)
                            .and_then(|_| std::fs::read(&out).map_err(|e| format!("read {}: {e}", out.display())));
                        tell(Event::Shot(ended));
                    }
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

fn run_env(command: &str, args: &[&str], key: &str, value: &str) -> Result<(), String> {
    let output = std::process::Command::new(command)
        .args(args)
        .env(key, value)
        .output()
        .map_err(|e| format!("run {command}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if err.is_empty() { format!("{command} exited {}", output.status) } else { err })
    }
}
