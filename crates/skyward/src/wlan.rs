//! The phone's WLAN inside the recovery, on a thread of its own: the driver
//! brought up, the phone's own wpa_supplicant joining networks, and an
//! address asked for over DHCP and kept. The screen is told how things
//! stand each time that changes.

use anyhow::{bail, ensure, Context, Result};
use skyward_config::{Firmware, Wlan as Config};
use skyward_net::dhcp::{self, Kept, Lease};
use skyward_net::http::Servers;
use skyward_net::iface;
use skyward_net::reach::{self, Reach};
use skyward_net::supplicant::{self, Bss, Control, Event as Told, Saved, SavedNetwork, Security};
use skyward_net::wlan::{self, Joined, Link, Next, Power, Stage, Traffic, Wlan};
use std::fs;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The first folder the firmware loader looks in that a recovery can
/// write.
const FIRMWARE: &str = "/etc/firmware";

/// Where a partition a firmware file is read from is mounted meanwhile.
const PARTITIONS: &str = "/skyward/wlan";

/// The supplicant's settings, and the folder of its control socket, in the
/// recovery service's own folder.
///
/// This is RAM, deliberately: the socket has to be made by the supplicant's
/// own user while `/metadata` is read-only, and neither the socket nor a log
/// is anything a later start wants. What has to outlive a start is the
/// record of the networks joined, [`RECORD`], and the configuration below
/// is generated from it at every start.
const HOME: &str = "/skyward/run/supplicant";

/// Where the networks whose passwords are kept are written: on `/metadata`,
/// which is a real filesystem and is there before the data partition opens.
///
/// `/metadata` is kept read-only by the recovery and made writable for the
/// length of a write alone, as the settings and the kept credential are; see
/// [`save_record`]. It is small, so what is written there is capped: see
/// [`supplicant::SAVED_MOST`].
const RECORD: &str = "/metadata/skyward/wlan";

/// What a write of the record goes into first, to be moved over the file
/// whole.
const RECORD_NEW: &str = "/metadata/skyward/wlan.new";

/// The user the supplicant takes on as it starts, who has to be able to
/// make its socket.
const WIFI: u32 = 1010;

/// How long the driver may take to make its interface, the chip's firmware
/// loading meanwhile.
const INTERFACE_WAIT: Duration = Duration::from_secs(20);

/// How long the supplicant may take to open its control socket.
const SOCKET_WAIT: Duration = Duration::from_secs(10);

/// How long the supplicant gets to end once asked to.
const END_WAIT: Duration = Duration::from_secs(3);

/// How often a network joined is read for its signal.
const POLL: Duration = Duration::from_secs(5);

/// How often what went over the interface is counted, for how fast it
/// goes.
const TRAFFIC: Duration = Duration::from_secs(1);

/// How long after the network gave no address it is asked again.
const RETRY: Duration = Duration::from_secs(10);

/// What the screen asks of the WLAN.
pub enum Command {
    On,
    Off,
    Scan,
    /// Join this network, keeping its password with the others when
    /// `remember` says the phone is to join it by itself at the next start.
    Join { ssid: Vec<u8>, name: String, security: Security, password: String, remember: bool },
    Leave,
    /// Forget the network named `name`, with whatever was kept of it; an
    /// empty name forgets every network kept.
    Forget { name: String },
}

/// What comes to the thread: a command, how asking for an address went,
/// how far the network joined reaches, or that the screen has let the
/// thread go.
enum Inbox {
    Command(Command),
    Address(u64, Result<Asked>),
    Reach(u64, Reach),
    End,
}

enum Asked {
    Got(Lease),
    Kept(Kept),
}

/// What the thread tells the screen: how the WLAN stands, how fast data went
/// over it in the last second, which networks the phone keeps, or how
/// forgetting one went.
enum Report {
    Wlan(Wlan),
    Traffic(Traffic),
    Saved(Vec<SavedNetwork>),
    Forgotten(Result<String, String>),
}

/// What was told about the kept networks since the screen last asked. It is
/// stashed out of the one channel [`Worker::told`] drains, so that asking for
/// it cannot take away what that call was going to return.
#[derive(Default)]
struct Stashed {
    saved: Option<Vec<SavedNetwork>>,
    forgotten: Option<Result<String, String>>,
}

pub struct Worker {
    inbox: Sender<Inbox>,
    /// Written to wake the thread when something is in its inbox.
    poke: Arc<OwnedFd>,
    told: Receiver<Report>,
    /// What the last drain of `told` took out of it besides the WLAN's own
    /// state.
    stashed: Mutex<Stashed>,
    /// Readable while the screen has something to be told.
    wake: OwnedFd,
    /// Where the DNS servers the joined network gave are kept. Held here too,
    /// so that whoever looks a name up can reach them while the thread owns
    /// its own copy.
    servers: Servers,
    thread: Option<JoinHandle<()>>,
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(std::io::Error::last_os_error()).context("make a pipe for the WLAN thread");
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn signal(fd: &OwnedFd) {
    unsafe { libc::write(fd.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
}

fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 64];
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

impl Worker {
    /// Start the thread, with the WLAN off. `modules` are the folders the
    /// driver's modules are found in, `libraries` the bundle of the vendor
    /// interface's libraries, and `servers` where the DNS servers the
    /// network gives are kept for whoever looks names up.
    pub fn start(cfg: Config, modules: Vec<PathBuf>, libraries: Option<PathBuf>, servers: Servers) -> Result<Worker> {
        let (inbox_tx, inbox) = mpsc::channel();
        let (poked, poke) = pipe()?;
        let (told_tx, told) = mpsc::channel();
        let (wake, rung) = pipe()?;
        let poke = Arc::new(poke);
        let back = Back { inbox: inbox_tx.clone(), poke: poke.clone() };
        let kept = servers.clone();
        let thread = thread::Builder::new()
            .name("wlan".into())
            .spawn(move || {
                let tell = move |r: Report| {
                    // The screen only stops listening once it has let the
                    // worker go.
                    let _ = told_tx.send(r);
                    signal(&rung);
                };
                let mut s = Station {
                    cfg,
                    modules,
                    libraries,
                    servers,
                    state: Wlan::default(),
                    running: None,
                    back,
                    tell: Box::new(tell),
                    saved: read_record(),
                    keep: None,
                };
                s.run(&inbox, &poked);
            })
            .context("start the WLAN thread")?;
        Ok(Worker { inbox: inbox_tx, poke, told, stashed: Mutex::new(Stashed::default()), wake, servers: kept, thread: Some(thread) })
    }

    /// The DNS servers the joined network gave, as they stand now. Empty
    /// while nothing is joined: a name cannot be looked up then, which is
    /// what the caller says rather than something to fail on.
    pub fn servers(&self) -> Servers {
        self.servers.clone()
    }

    pub fn send(&self, c: Command) {
        let _ = self.inbox.send(Inbox::Command(c));
        signal(&self.poke);
    }

    /// Forget the network named `name`, with whatever the phone kept of it:
    /// its password and how it is protected. An empty name forgets every
    /// network kept, which is what the switch that turns remembering off
    /// asks for.
    pub fn forget(&self, name: String) {
        self.send(Command::Forget { name });
    }

    /// The networks the phone keeps, if they changed since the last call.
    ///
    /// This reads what [`Worker::told`] stashed rather than draining the
    /// channel itself: two drains of one channel would each take part of what
    /// the other was going to return, and which part would depend on the
    /// order the screen happened to ask in.
    pub fn saved(&self) -> Option<Vec<SavedNetwork>> {
        self.stashed().saved.take()
    }

    /// How forgetting a network went, if it ended since the last call.
    pub fn forgotten(&self) -> Option<Result<String, String>> {
        self.stashed().forgotten.take()
    }

    /// What was told besides the WLAN's state. A lock a panicking thread left
    /// held says nothing was stashed, which is what the screen reads as no
    /// news — the page is redrawn from the state it already has, so a lost
    /// notice costs a line said out loud and never the WLAN.
    fn stashed(&self) -> std::sync::MutexGuard<'_, Stashed> {
        self.stashed.lock().unwrap_or_else(|held| held.into_inner())
    }

    pub fn fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// How the WLAN stands now, and how fast data went over it in the last
    /// second, each if told since the last call.
    pub fn told(&self) -> (Option<Wlan>, Option<Traffic>) {
        drain(&self.wake);
        self.drain_told()
    }

    fn drain_told(&self) -> (Option<Wlan>, Option<Traffic>) {
        let (mut wlan, mut traffic) = (None, None);
        let mut stashed = self.stashed();
        for report in self.told.try_iter() {
            match report {
                Report::Wlan(w) => wlan = Some(w),
                Report::Traffic(t) => traffic = Some(t),
                Report::Saved(s) => stashed.saved = Some(s),
                Report::Forgotten(f) => stashed.forgotten = Some(f),
            }
        }
        (wlan, traffic)
    }
}

impl Drop for Worker {
    /// The WLAN goes off with the thread.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.inbox.send(Inbox::End);
            signal(&self.poke);
            thread.join().expect("the WLAN thread does not panic");
        }
    }
}

/// What a thread asking for an address answers to.
#[derive(Clone)]
struct Back {
    inbox: Sender<Inbox>,
    poke: Arc<OwnedFd>,
}

/// The supplicant running, and the address the network gave.
struct Running {
    child: Child,
    control: Control,
    monitor: Control,
    mac: [u8; 6],
    found: Vec<Bss>,
    lease: Option<Held>,
    /// The thread asking for an address or to keep one, and its number;
    /// what an earlier one answers is let go.
    asking: Option<(u64, Arc<AtomicBool>)>,
    asked: u64,
    /// The number of the last look at how far the network reaches; what an
    /// earlier one finds is let go.
    probed: u64,
    /// When to ask for an address again after the network gave none.
    retry_at: Option<Instant>,
    polled_at: Instant,
    /// What had gone over the interface when last counted, while it has an
    /// address.
    counted: Option<Counted>,
}

/// The interface's totals in and out, in bytes, and when they were read.
#[derive(Clone, Copy)]
struct Counted {
    at: Instant,
    rx: u64,
    tx: u64,
}

/// A lease, when it was given, and how far keeping it has come: 0 asks its
/// server at the renewal time, 1 any server at the rebinding time, 2 lets
/// it go when it runs out.
struct Held {
    lease: Lease,
    at: Instant,
    phase: u8,
}

impl Held {
    fn due(&self) -> Instant {
        let after = match self.phase {
            0 => self.lease.renew,
            1 => self.lease.rebind,
            _ => self.lease.seconds,
        };
        self.at + Duration::from_secs(after.into())
    }
}

struct Station {
    cfg: Config,
    modules: Vec<PathBuf>,
    libraries: Option<PathBuf>,
    servers: Servers,
    state: Wlan,
    running: Option<Running>,
    back: Back,
    tell: Box<dyn Fn(Report) + Send>,
    /// The networks whose passwords are kept, as [`RECORD`] last said. This
    /// is the copy every change is made against, so that what is offered to
    /// forget is what the next start would join, and never the supplicant's
    /// own running set.
    saved: Vec<Saved>,
    /// The network being joined whose password is to be kept once it is
    /// actually connected. A join that fails — a wrong password, a network
    /// out of reach — never keeps anything, which is the whole point of
    /// waiting for the address rather than writing it as it is typed.
    keep: Option<Saved>,
}

impl Station {
    fn run(&mut self, inbox: &Receiver<Inbox>, poked: &OwnedFd) {
        let mut told = self.state.clone();
        loop {
            let mut fds = vec![libc::pollfd { fd: poked.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            if let Some(r) = &self.running {
                fds.push(libc::pollfd { fd: r.monitor.fd(), events: libc::POLLIN, revents: 0 });
            }
            let timeout = self.next().map_or(-1, |at| at.saturating_duration_since(Instant::now()).as_millis().min(60_000) as i32);
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } < 0 {
                let e = std::io::Error::last_os_error();
                assert_eq!(e.kind(), std::io::ErrorKind::Interrupted, "poll the WLAN thread's inputs: {e}");
            }
            drain(poked);
            for message in inbox.try_iter() {
                match message {
                    Inbox::Command(c) => self.command(c),
                    Inbox::Address(n, answer) => self.addressed(n, answer),
                    Inbox::Reach(n, reach) => self.reached(n, reach),
                    Inbox::End => return self.off(),
                }
            }
            self.listen();
            self.timers();
            if self.state != told {
                told = self.state.clone();
                (self.tell)(Report::Wlan(told.clone()));
            }
        }
    }

    /// The soonest a timer is due.
    fn next(&self) -> Option<Instant> {
        let r = self.running.as_ref()?;
        let poll = matches!(self.state.joined, Some(Joined { stage: Stage::Connected(_), .. })).then(|| r.polled_at + POLL);
        let count = r.lease.as_ref().map(|_| r.counted.map_or_else(Instant::now, |c| c.at + TRAFFIC));
        [r.lease.as_ref().filter(|_| r.asking.is_none()).map(Held::due), r.retry_at, poll, count].into_iter().flatten().min()
    }

    fn command(&mut self, c: Command) {
        match c {
            Command::On => self.on(),
            Command::Off => self.off(),
            Command::Scan => {
                if let Some(r) = &self.running {
                    // A scan under way answers FAIL-BUSY, and its results
                    // come all the same.
                    if let Err(e) = r.control.ask("SCAN") {
                        self.lost(e);
                    }
                }
            }
            Command::Join { ssid, name, security, password, remember } => {
                self.unaddress();
                // Kept only once the network gives an address: a password
                // that turns out wrong is one no later start should try.
                self.keep = remember.then(|| Saved { ssid: ssid.clone(), name: name.clone(), security, password: password.clone() });
                let mut joined = Joined { ssid, name, security, stage: Stage::Joining };
                if let Err(e) = self.join(&joined, &password) {
                    let _ = skyward_sys::kmsg(&format!("joining {} failed: {e:#}", joined.name));
                    joined.stage = Stage::Failed;
                    self.keep = None;
                }
                self.state.joined = Some(joined);
            }
            Command::Leave => {
                self.unaddress();
                self.keep = None;
                if let Some(r) = &self.running {
                    if let Err(e) = r.control.order("REMOVE_NETWORK all") {
                        self.lost(e);
                    }
                }
                self.state.joined = None;
            }
            // Forgetting is not leaving: leaving drops the network this
            // start joined, and forgetting takes away what the phone would
            // join by itself at the next one. What was kept is written where
            // the supplicant reads it, so it goes from there and then the
            // supplicant is told to save what is left.
            Command::Forget { name } => {
                let outcome = self.forget(&name);
                if let Err(e) = &outcome {
                    let _ = skyward_sys::kmsg(&format!("forgetting a network: {e:#}"));
                }
                (self.tell)(Report::Forgotten(outcome.map(|()| name.clone()).map_err(|e| format!("{e:#}"))));
            }
        }
    }

    /// Take the network named `name` out of what the phone keeps; an empty
    /// name takes every network kept, which is what turning the switch that
    /// remembers passwords off asks for.
    ///
    /// The record this station holds is the authority, not the supplicant's
    /// own list: a network this start joined by hand is only kept once it has
    /// an address, and one kept from an earlier start is already joined by
    /// the generated configuration before the page is ever opened. The record
    /// is written first and the copy in memory replaced only once the write
    /// worked, so that what the page offers to forget stays the same thing
    /// the next start will read.
    ///
    /// Matching is by the name the page showed, which is the network's own
    /// name with what is not UTF-8 put right: two networks whose names differ
    /// only in bytes that are not UTF-8 read alike, and are then forgotten
    /// together. That costs the password of a network the page could not tell
    /// apart from the one asked for, and nothing else — the network stays
    /// listed and joining it again keeps it again.
    fn forget(&mut self, name: &str) -> Result<()> {
        let kept: Vec<Saved> = if name.is_empty() { Vec::new() } else { self.saved.iter().filter(|k| k.name != name).cloned().collect() };
        if kept.len() == self.saved.len() && !name.is_empty() {
            // Nothing kept went by that name: say so rather than write the
            // same record again and tell the page something was forgotten.
            bail!("no network named {name} is kept");
        }
        save_record(&kept)?;
        self.saved = kept;
        (self.tell)(Report::Saved(self.saved.iter().map(Saved::shown).collect()));
        // A network whose password is gone must not stay up on it: the
        // supplicant is told to drop what it holds, and the address it gave
        // goes with it.
        let dropped = name.is_empty() || self.state.joined.as_ref().is_some_and(|j| j.name == name);
        if dropped {
            self.keep = None;
            self.unaddress();
            if let Some(r) = &self.running {
                r.control.order("REMOVE_NETWORK all")?;
            }
            self.state.joined = None;
        }
        Ok(())
    }

    fn on(&mut self) {
        if self.running.is_some() {
            return;
        }
        self.state.power = Power::Starting;
        (self.tell)(Report::Wlan(self.state.clone()));
        match self.start() {
            Ok(r) => {
                self.running = Some(r);
                self.state.power = Power::On;
                self.command(Command::Scan);
            }
            Err(e) => {
                let _ = skyward_sys::kmsg(&format!("the WLAN did not come up: {e:#}"));
                self.state.power = Power::Failed(format!("{e:#}"));
            }
        }
    }

    fn start(&self) -> Result<Running> {
        bring_up(&self.cfg, &self.modules)?;
        let mut child = start_supplicant(&self.cfg, self.libraries.as_deref())?;
        let socket = Path::new(HOME).join("sockets").join(&self.cfg.interface);
        let connected = (|| -> Result<(Control, Control)> {
            let control = Control::open(&socket)?;
            ensure!(control.ask("PING")?.trim_end() == "PONG", "wpa_supplicant does not answer PING");
            let monitor = Control::open(&socket)?;
            monitor.attach()?;
            Ok((control, monitor))
        })();
        let (control, monitor) = match connected {
            Ok(both) => both,
            Err(e) => {
                end(&mut child, None);
                return Err(e);
            }
        };
        let mac = iface::hardware_address(&self.cfg.interface)?;
        Ok(Running {
            child,
            control,
            monitor,
            mac,
            found: Vec::new(),
            lease: None,
            asking: None,
            asked: 0,
            probed: 0,
            retry_at: None,
            polled_at: Instant::now(),
            counted: None,
        })
    }

    fn off(&mut self) {
        self.unaddress();
        self.keep = None;
        if let Some(mut r) = self.running.take() {
            end(&mut r.child, Some(&r.control));
        }
        if let Err(e) = iface::set_up(&self.cfg.interface, false) {
            let _ = skyward_sys::kmsg(&format!("{e:#}"));
        }
        self.state = Wlan::default();
    }

    /// The supplicant no longer answers: the WLAN is off, and why is said.
    fn lost(&mut self, e: anyhow::Error) {
        let _ = skyward_sys::kmsg(&format!("the WLAN went away: {e:#}"));
        self.off();
        self.state.power = Power::Failed(format!("{e:#}"));
    }

    fn join(&self, joined: &Joined, password: &str) -> Result<()> {
        let r = self.running.as_ref().context("the WLAN is off")?;
        r.control.order("REMOVE_NETWORK all")?;
        let id: u32 = r.control.ask("ADD_NETWORK")?.trim().parse().context("wpa_supplicant numbered the network with no number")?;
        // One the last scan did not see may hide its name, as one added by
        // hand may.
        let hidden = !r.found.iter().any(|b| b.ssid == joined.ssid);
        for c in supplicant::network_commands(id, &joined.ssid, joined.security, password, hidden)? {
            r.control.order(&c)?;
        }
        r.control.order(&format!("SELECT_NETWORK {id}"))
    }

    /// Read what the supplicant has told.
    fn listen(&mut self) {
        let Some(r) = &self.running else { return };
        let messages = match r.monitor.messages() {
            Ok(m) => m,
            Err(e) => return self.lost(e),
        };
        for m in messages {
            let Some(told) = supplicant::event(&m) else { continue };
            match told {
                Told::ScanResults => {
                    if let Err(e) = self.read_scan() {
                        return self.lost(e);
                    }
                }
                Told::ScanFailed => {}
                Told::Terminating => return self.lost(anyhow::anyhow!("wpa_supplicant ended")),
                other => self.joining(&other),
            }
        }
    }

    fn read_scan(&mut self) -> Result<()> {
        let r = self.running.as_mut().expect("scan results come from a supplicant running");
        let mut found = Vec::new();
        let mut first = 0;
        loop {
            let page = supplicant::bss_list(&r.control.ask(&supplicant::bss_command(first))?)?;
            let Some(last) = page.last() else { break };
            first = last.id + 1;
            found.extend(page);
        }
        self.state.networks = wlan::networks(&found);
        r.found = found;
        Ok(())
    }

    fn joining(&mut self, told: &Told) {
        let Some(j) = &mut self.state.joined else { return };
        match j.on(told) {
            Next::Show => {}
            Next::Address => self.ask_address(),
            Next::Unaddress => self.unaddress(),
            Next::Forget => {
                // The network turned the password down: there is nothing
                // worth keeping.
                self.keep = None;
                if let Some(r) = &self.running {
                    if let Err(e) = r.control.order("REMOVE_NETWORK all") {
                        self.lost(e);
                    }
                }
            }
        }
    }

    /// Ask the network for an address, on a thread of its own.
    fn ask_address(&mut self) {
        let interface = self.cfg.interface.clone();
        let back = self.back.clone();
        let Some(r) = &mut self.running else { return };
        r.retry_at = None;
        if let Some((_, stop)) = r.asking.take() {
            stop.store(true, Ordering::Relaxed);
        }
        r.asked += 1;
        let n = r.asked;
        let stop = Arc::new(AtomicBool::new(false));
        r.asking = Some((n, stop.clone()));
        let mac = r.mac;
        let keeping = r.lease.as_ref().map(|h| (h.lease.clone(), h.phase));
        thread::spawn(move || {
            let answer = match keeping {
                None => dhcp::obtain(&interface, mac, &stop).map(Asked::Got),
                Some((lease, phase)) => dhcp::keep(&interface, mac, &lease, phase > 0, &stop).map(Asked::Kept),
            };
            let _ = back.inbox.send(Inbox::Address(n, answer));
            signal(&back.poke);
        });
    }

    fn addressed(&mut self, n: u64, answer: Result<Asked>) {
        let Some(r) = &mut self.running else { return };
        if r.asking.as_ref().map(|(m, _)| *m) != Some(n) {
            return;
        }
        r.asking = None;
        let lease = match answer {
            Ok(Asked::Got(lease)) | Ok(Asked::Kept(Kept::Kept(lease))) => lease,
            Ok(Asked::Kept(Kept::Unanswered)) => {
                if let Some(h) = &mut r.lease {
                    h.phase += 1;
                }
                return;
            }
            Ok(Asked::Kept(Kept::Refused)) => {
                self.unaddress();
                return self.ask_address();
            }
            Err(e) if e.is::<skyward_net::Stopped>() => return,
            Err(e) => {
                let _ = skyward_sys::kmsg(&format!("no address: {e:#}"));
                r.retry_at = Some(Instant::now() + RETRY);
                if let Some(j) = &mut self.state.joined {
                    j.stage = Stage::NoAddress(format!("{e:#}"));
                }
                return;
            }
        };
        let same = r.lease.as_ref().is_some_and(|h| (h.lease.address, h.lease.mask, h.lease.router) == (lease.address, lease.mask, lease.router));
        if !same {
            if let Err(e) = iface::configure(&self.cfg.interface, lease.address, lease.mask, lease.router) {
                let _ = skyward_sys::kmsg(&format!("{e:#}"));
                r.retry_at = Some(Instant::now() + RETRY);
                if let Some(j) = &mut self.state.joined {
                    j.stage = Stage::NoAddress(format!("{e:#}"));
                }
                return;
            }
        }
        let _ = skyward_sys::kmsg(&format!(
            "{} has {} ({}) through {:?} for {} s, kept again after {} s",
            self.cfg.interface, lease.address, lease.mask, lease.router, lease.seconds, lease.renew
        ));
        *self.servers.lock().expect("the DNS servers are not held by a thread that panicked") = lease.dns.clone();
        let mut link = Link::new(&lease, r.mac);
        // An address kept keeps how far its network was found to reach; a
        // new one has it looked at.
        link.reach = match &self.state.joined {
            Some(Joined { stage: Stage::Connected(had), .. }) if same => had.reach.clone(),
            _ => None,
        };
        let look = link.reach.is_none().then(|| lease.dns.clone());
        r.lease = Some(Held { lease, at: Instant::now(), phase: 0 });
        if let Some(j) = &mut self.state.joined {
            j.stage = Stage::Connected(link);
        }
        if let Some(joined) = self.keep.take() {
            self.keep_saved(joined);
        }
        if let Some(dns) = look {
            self.probe(dns);
        }
        self.poll_link();
    }

    /// Keep the password of a network that has just answered, so that the
    /// next start joins it by itself.
    ///
    /// This is called only once the network gave an address: a password typed
    /// wrong, or a network that would not have us, is never written down.
    /// A record that cannot be written is said and nothing more — a phone
    /// that cannot remember a network still has one joined now, and the page
    /// goes on offering what is actually kept rather than what this tried to
    /// keep.
    fn keep_saved(&mut self, network: Saved) {
        let name = network.name.clone();
        let mut next = self.saved.clone();
        match next.iter().position(|k| k.same(&network)) {
            Some(at) => next[at] = network,
            // A record at its cap keeps what it has: the network is joined
            // now and joined by hand again later, which is a smaller loss
            // than a password dropped for one that may never be seen again.
            None if next.len() < supplicant::SAVED_MOST => next.push(network),
            None => {
                let _ = skyward_sys::kmsg(&format!("{name} is not kept: {} networks are already kept", supplicant::SAVED_MOST));
                return;
            }
        }
        match save_record(&next) {
            Ok(()) => {
                self.saved = next;
                (self.tell)(Report::Saved(self.saved.iter().map(Saved::shown).collect()));
            }
            Err(e) => {
                let _ = skyward_sys::kmsg(&format!("{name} is not kept: {e:#}"));
            }
        }
    }

    /// Look how far the network joined reaches, on a thread of its own, the
    /// page's name looked up in the network's own DNS servers, `dns`.
    fn probe(&mut self, dns: Vec<Ipv4Addr>) {
        let (page, back) = (self.cfg.probe.clone(), self.back.clone());
        let Some(r) = &mut self.running else { return };
        r.probed += 1;
        let n = r.probed;
        thread::spawn(move || {
            let reach = reach::probe(&page, &dns);
            let _ = back.inbox.send(Inbox::Reach(n, reach));
            signal(&back.poke);
        });
    }

    fn reached(&mut self, n: u64, reach: Reach) {
        if self.running.as_ref().is_none_or(|r| r.probed != n) {
            return;
        }
        let Some(Joined { stage: Stage::Connected(link), .. }) = &mut self.state.joined else { return };
        let _ = skyward_sys::kmsg(&format!("the network on {} {reach}", self.cfg.interface));
        link.reach = Some(reach);
    }

    /// Give the address back, and stop asking for one or keeping it.
    fn unaddress(&mut self) {
        let Some(r) = &mut self.running else { return };
        r.retry_at = None;
        if let Some((_, stop)) = r.asking.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if r.lease.take().is_some() {
            if let Err(e) = iface::unconfigure(&self.cfg.interface) {
                let _ = skyward_sys::kmsg(&format!("{e:#}"));
            }
        }
        // Nothing goes over a network left, and counting starts afresh on
        // the next one.
        if r.counted.take().is_some() {
            (self.tell)(Report::Traffic(Traffic::default()));
        }
        self.servers.lock().expect("the DNS servers are not held by a thread that panicked").clear();
    }

    fn timers(&mut self) {
        let now = Instant::now();
        let Some(r) = &mut self.running else { return };
        if r.retry_at.is_some_and(|at| now >= at) {
            r.retry_at = None;
            return self.ask_address();
        }
        if r.asking.is_none() {
            if let Some(h) = &r.lease {
                if now >= h.due() {
                    if h.phase >= 2 {
                        // The lease ran out with no server to keep it.
                        self.unaddress();
                        if let Some(j) = &mut self.state.joined {
                            j.stage = Stage::Addressing;
                        }
                    }
                    return self.ask_address();
                }
            }
        }
        if now >= r.polled_at + POLL {
            self.poll_link();
        }
        if let Some(r) = &self.running {
            if r.lease.is_some() && r.counted.is_none_or(|c| now >= c.at + TRAFFIC) {
                self.count(now);
            }
        }
    }

    /// Read what has gone over the interface, and tell how fast it went
    /// since the last reading.
    fn count(&mut self, now: Instant) {
        let stats = Path::new("/sys/class/net").join(&self.cfg.interface).join("statistics");
        let read = |name: &str| -> Result<u64> {
            let path = stats.join(name);
            let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            text.trim().parse().with_context(|| format!("{} holds no count", path.display()))
        };
        let (rx, tx) = match (read("rx_bytes"), read("tx_bytes")) {
            (Ok(rx), Ok(tx)) => (rx, tx),
            (Err(e), _) | (_, Err(e)) => return self.lost(e),
        };
        let Some(r) = &mut self.running else { return };
        if let Some(c) = r.counted {
            let secs = now.duration_since(c.at).as_secs_f64();
            let rate = |total: u64, before: u64| (total.saturating_sub(before) as f64 / secs).round() as u64;
            (self.tell)(Report::Traffic(Traffic { rx: rate(rx, c.rx), tx: rate(tx, c.tx) }));
        }
        r.counted = Some(Counted { at: now, rx, tx });
    }

    /// Read the network joined for its signal and speed.
    fn poll_link(&mut self) {
        let Some(r) = &mut self.running else { return };
        r.polled_at = Instant::now();
        let Some(Joined { stage: Stage::Connected(link), .. }) = &mut self.state.joined else { return };
        // A plain STATUS has Android's supplicant tell every listener again
        // that the network is connected; this one leaves that out.
        let (signal, status) = match (r.control.ask("SIGNAL_POLL"), r.control.ask("STATUS-NO_EVENTS")) {
            (Ok(signal), Ok(status)) => (signal, status),
            (Err(e), _) | (_, Err(e)) => return self.lost(e),
        };
        let fields = supplicant::fields(&signal);
        if let Some(rssi) = fields.get("RSSI").and_then(|v| v.parse().ok()) {
            link.signal = rssi;
        }
        if let Some(f) = fields.get("FREQUENCY").and_then(|v| v.parse().ok()) {
            link.frequency = f;
        }
        link.speed = fields.get("LINKSPEED").and_then(|v| v.parse().ok());
        link.generation = supplicant::fields(&status).get("wifi_generation").and_then(|v| v.parse().ok());
    }
}

/// Load the driver where its interface is not there yet, and bring the
/// interface up.
fn bring_up(cfg: &Config, modules: &[PathBuf]) -> Result<()> {
    let net = Path::new("/sys/class/net").join(&cfg.interface);
    if !net.exists() {
        for file in &cfg.firmware {
            place(file)?;
        }
        for path in skyward_modules::load(modules, &cfg.modules)? {
            skyward_sys::kmsg(&format!("loaded {}", path.display()))?;
        }
        for w in &cfg.writes {
            fs::write(&w.path, &w.value).with_context(|| format!("write {} to {}", w.value, w.path))?;
        }
        let until = Instant::now() + INTERFACE_WAIT;
        while !net.exists() {
            ensure!(Instant::now() < until, "the driver made no {} in {} seconds", cfg.interface, INTERFACE_WAIT.as_secs());
            thread::sleep(Duration::from_millis(100));
        }
    }
    iface::set_up(&cfg.interface, true)
}

/// Put a file the driver asks for where the firmware loader looks.
fn place(file: &Firmware) -> Result<()> {
    place_firmware(file)
}

/// Put a file a driver asks the kernel's firmware loader for where the
/// loader looks, read out of the running recovery or off the partition the
/// file is kept on. The motor's driver asks for its own the same way, and
/// ends its probe without it.
pub(crate) fn place_firmware(file: &Firmware) -> Result<()> {
    let to = Path::new(FIRMWARE).join(&file.name);
    let dir = to.parent().expect("a firmware file is in a folder");
    fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    let bytes = match &file.partition {
        None => fs::read(&file.path).with_context(|| format!("read {}", file.path))?,
        Some(partition) => {
            // The partition may already be mounted where the phone's own
            // table mounts it, as the configuration's [partitions] asks for
            // persist: a second mount of the same device is refused while
            // one is there, whatever is asked of it. Read through that one
            // first, and mount the partition only when it is nowhere.
            if let Some(bytes) = read_through_mount(partition, &file.path) {
                bytes?
            } else {
                let at = Path::new(PARTITIONS).join(partition);
                fs::create_dir_all(&at).with_context(|| format!("make {}", at.display()))?;
                skyward_block::mount::mount_ro(&Path::new(skyward_block::BY_NAME).join(partition), &at)?;
                let read = fs::read(at.join(&file.path)).with_context(|| format!("read {} on {partition}", file.path));
                skyward_block::mount::umount(&at)?;
                read?
            }
        }
    };
    fs::write(&to, bytes).with_context(|| format!("write {}", to.display()))
}

/// `path` on `partition` read through a mount the phone's own table already
/// has, when an earlier mount of that partition puts the file within reach.
/// `None` when the partition is mounted nowhere and has to be mounted.
fn read_through_mount(partition: &str, path: &str) -> Option<Result<Vec<u8>>> {
    let at = skyward_block::fstab::mount_point(partition).ok().flatten()?;
    let file = at.join(path);
    // A partition that is mounted somewhere is read through that mount even
    // when the file is not the one expected: mounting it a second time is
    // refused while the first is there, so the read's own reason is the one
    // worth having.
    Some(fs::read(&file).with_context(|| format!("read {} through {}", file.display(), at.display())))
}

/// The networks whose passwords are kept, read from where they are written.
///
/// A record that cannot be read is no record: none is joined by itself, and
/// why goes to the kernel log. This is not a failure worth stopping the WLAN
/// for — a phone whose `/metadata` will not mount still has a radio, and a
/// network joined by hand still works, it is only not remembered.
fn read_record() -> Vec<Saved> {
    match fs::read_to_string(RECORD) {
        Ok(text) => supplicant::saved_read(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            let _ = skyward_sys::kmsg(&format!("the networks kept were not read: {e:#}"));
            Vec::new()
        }
    }
}

/// Write `saved` where a later start finds it, or take the record away when
/// there is none to keep.
///
/// `/metadata` is kept read-only by the recovery, so it is made writable for
/// the length of the write alone and read-only again after it, whether the
/// write worked or not — as the settings file and the kept credential are
/// written, and for the same reason: a plain write fails against a read-only
/// filesystem and the record is silently never kept. The write goes into a
/// file beside the record, is flushed, and is moved over it whole, so a start
/// cut off in the middle leaves either the old record or the new one and never
/// half of one.
fn save_record(saved: &[Saved]) -> Result<()> {
    with_metadata(|| {
        if saved.is_empty() {
            // Nothing kept is the record not being there: a file holding no
            // networks is one an earlier version would still have to parse.
            let removed = match fs::remove_file(RECORD) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| format!("take away {RECORD}")),
            };
            return removed;
        }
        if let Some(folder) = Path::new(RECORD).parent() {
            fs::create_dir_all(folder).with_context(|| format!("make {}", folder.display()))?;
        }
        fs::write(RECORD_NEW, supplicant::saved_text(saved)).with_context(|| format!("write {RECORD_NEW}"))?;
        fs::File::open(RECORD_NEW).and_then(|f| f.sync_all()).with_context(|| format!("flush {RECORD_NEW}"))?;
        fs::rename(RECORD_NEW, RECORD).with_context(|| format!("move {RECORD_NEW} over {RECORD}"))
    })
}

/// Do `work` on `/metadata` with it made writable, and read-only again after
/// whether `work` worked or not.
///
/// The write's own failure is the one worth reading: a mount left writable is
/// said too, but only after the write's reason, so that a caller reports what
/// actually went wrong with the record rather than that `/metadata` could not
/// be closed again.
fn with_metadata<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
    skyward_vab::metadata_writable(true).context("make /metadata writable")?;
    let done = work();
    let back = skyward_vab::metadata_writable(false);
    let kept = done?;
    back.context("make /metadata read-only again")?;
    Ok(kept)
}

/// Start the supplicant on the interface, and wait for its control socket.
///
/// The configuration it is started with is generated here, from the networks
/// the phone keeps, rather than saved by the supplicant itself: a
/// `SAVE_CONFIG` would have the supplicant write `/metadata` on its own
/// account, for a length of time this program does not own and with no flush
/// and rename, and a start cut off part-way through would leave half a
/// configuration. So the record is what outlives a start, and this file is
/// made afresh in RAM from it each time.
fn start_supplicant(cfg: &Config, libraries: Option<&Path>) -> Result<Child> {
    let home = Path::new(HOME);
    let sockets = home.join("sockets");
    fs::create_dir_all(&sockets).with_context(|| format!("make {}", sockets.display()))?;
    fs::set_permissions(home, fs::Permissions::from_mode(0o755)).with_context(|| format!("open {} to the supplicant", home.display()))?;
    std::os::unix::fs::chown(&sockets, Some(WIFI), Some(WIFI)).with_context(|| format!("give {} to the supplicant", sockets.display()))?;
    fs::set_permissions(&sockets, fs::Permissions::from_mode(0o770)).with_context(|| format!("keep {} to the supplicant", sockets.display()))?;
    let settings = home.join("wpa_supplicant.conf");
    let kept = read_record();
    if !kept.is_empty() {
        let _ = skyward_sys::kmsg(&format!("{} networks are joined by themselves at this start", kept.len()));
    }
    let text = supplicant::config_text(&sockets.display().to_string(), &kept);
    fs::write(&settings, text).with_context(|| format!("write {}", settings.display()))?;
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o644)).with_context(|| format!("open {} to the supplicant", settings.display()))?;
    let socket = sockets.join(&cfg.interface);
    // One an earlier run left would answer nothing.
    if socket.exists() {
        fs::remove_file(&socket).with_context(|| format!("take away {}", socket.display()))?;
    }
    let mut path: Vec<String> = cfg.libraries.clone();
    if let Some(dir) = libraries {
        path.push(dir.to_string_lossy().into_owned());
    }
    let mut child = Process::new(&cfg.supplicant)
        .args(["-i", &cfg.interface, "-D", "nl80211", "-c"])
        .arg(&settings)
        .env("LD_LIBRARY_PATH", path.join(":"))
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start {}", cfg.supplicant))?;
    let until = Instant::now() + SOCKET_WAIT;
    while !socket.exists() {
        if let Some(status) = child.try_wait().context("look at wpa_supplicant")? {
            bail!("wpa_supplicant ended as it started: {status}");
        }
        if Instant::now() >= until {
            end(&mut child, None);
            bail!("wpa_supplicant opened no control socket in {} seconds", SOCKET_WAIT.as_secs());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(child)
}

/// End the supplicant: asked to, where it can be, and killed after a while.
fn end(child: &mut Child, control: Option<&Control>) {
    if let Some(c) = control {
        let _ = c.ask("TERMINATE");
        let until = Instant::now() + END_WAIT;
        while Instant::now() < until {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
