use anyhow::{bail, ensure, Context, Result};
use procfs::process::{FDTarget, MMapPath, Process};
use skyward_config::Service;
use skyward_sys::ueventd;
use skyward_vab::Mounted;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// A program that ends sooner than this after starting waits this long
/// before it is started again.
const RESTART_DELAY: Duration = Duration::from_secs(1);

/// How long a program gets to become what it says it will be.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(100);

/// The most lines of a program that runs once are kept: enough for a step
/// to name what was missing, short of a step that fails in a loop.
const SAID_LINES: usize = 40;

/// The same for a program that stays up, kept per start: a daemon that
/// ends at once says why on its way out, and next to nothing while it
/// runs, so a handful of its first lines is what tells the two apart.
const DAEMON_LINES: usize = 12;

/// What the services last said on their way out, kept where the page can
/// reach it: a phone whose key service will not stay up says so in the
/// recovery's own log, which its owner can read nowhere. The page is the
/// one place they can.
static SAID: std::sync::OnceLock<std::sync::Mutex<Vec<(String, String)>>> = std::sync::OnceLock::new();

/// How many of a service's last lines are kept for the page.
const SAID_KEPT: usize = 4;

/// A line with its numbers blanked, so that a service started again and
/// again is not remembered once per pid. A phone whose key service dies on
/// every start leaves three hundred lines that differ only in the number,
/// and keeping them would push out the one line saying why it will not
/// stay up.
fn shape(line: &str) -> String {
    line.chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect()
}

/// Keep one line of a service's own words, a few of the latest: a step that
/// runs once says why at its end, a daemon says it first, and four of the
/// latest hold both.
fn remember(line: String) {
    let kept = SAID.get_or_init(Default::default);
    let mut kept = kept.lock().expect("no thread panics while the words are kept");
    let key = shape(&line);
    kept.retain(|(k, _)| *k != key);
    kept.push((key, line));
    while kept.len() > SAID_KEPT {
        kept.remove(0);
    }
}

/// What the services last said that explains why one of them is not
/// running, when any of them said anything.
pub fn why() -> Option<String> {
    let kept = SAID.get()?.lock().ok()?;
    match kept.is_empty() {
        true => None,
        false => Some(kept.iter().map(|(_, line)| line.as_str()).collect::<Vec<_>>().join("; ")),
    }
}

/// The installed system's own linker, where it keeps one. A program built
/// for the phone may need a library this recovery does not carry, and one
/// that cannot be had is the end of the road: the linker refuses the
/// program outright. The phone's own linker knows the phone's own
/// libraries — its libc++, its base, its log — and they were built
/// together, where this recovery's were not.
const PHONE_LINKERS: [&str; 2] = ["/mnt/system/system/bin/linker64", "/mnt/system/bin/linker64"];

/// Where the phone's own libraries are, for a program started through the
/// phone's own linker.
const PHONE_LIBRARIES: &str = "/mnt/system/system/lib64:/mnt/system/system/lib64/bootstrap:/mnt/system/lib64:/mnt/system/system_ext/lib64:/mnt/system/product/lib64:/mnt/system/odm/lib64:/vendor/lib64:/vendor/lib64/hw:/odm/lib64";

/// What the linker says when it refuses a program for want of a library.
const REFUSED: &str = "CANNOT LINK EXECUTABLE";

/// Where the kernel says the highest capability it knows.
const CAP_LAST_CAP: &str = "/proc/sys/kernel/cap_last_cap";

/// The version of the kernel's capability sets that holds 64 of them.
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// Keep each program running, one thread per program: start it, and start
/// it again whenever it exits. They are started in the order they are
/// written, and one that says what it will be ready with holds up those
/// after it until it is. A program still running from an earlier instance
/// of the recovery service is waited on instead of started twice. A
/// program that runs once is run to its end where it stands in that order.
/// Each runs as the user, in the groups and with the capabilities the
/// phone's own init gives it.
///
/// `libraries` is a directory of shared libraries every one of them may
/// need, searched after whatever it names itself.
pub fn start(services: &[Service], libraries: Option<&Path>) -> Result<()> {
    let known = ueventd::accounts(&ueventd::PASSWD_FILES.iter().chain(&ueventd::GROUP_FILES).copied().collect::<Vec<_>>());
    for s in services {
        let mut s = s.clone();
        if !Path::new(&s.path).is_file() {
            // The same service carries another name on other builds of the
            // hardware; the installed ROM renamed its vendor binary. Take
            // the first alternate that is there, so the service keeps
            // working without a separate device file per build.
            if let Some(alts) = &s.alternates {
                if let Some(found) = alts.iter().find(|a| Path::new(a).is_file()) {
                    s.path = found.clone();
                }
            }
        }
        if !Path::new(&s.path).is_file() {
            if s.optional {
                skyward_sys::kmsg(&format!("{} is missing; this phone does without it", s.path))?;
                continue;
            }
            bail!("{} is missing; is its partition mounted?", s.path);
        }
        if let Some(dir) = libraries {
            s.libraries.push(dir.to_string_lossy().into_owned());
        }
        let who = match Credentials::of(&s, &known) {
            Ok(who) => who,
            Err(e) if s.optional => {
                skyward_sys::kmsg(&format!("{e:#}; this phone does without it"))?;
                continue;
            }
            Err(e) => return Err(e),
        };
        if s.oneshot {
            // What it said goes to the log. A step that fails names its
            // reason there and nowhere else, and one that finds nothing to
            // do says so; without this the log keeps a bare exit status and
            // the reason is lost with the pipe it went down.
            let done = command(&s, &who)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .with_context(|| format!("run {}", s.path))?;
            let said = format!("{}{}", String::from_utf8_lossy(&done.stdout), String::from_utf8_lossy(&done.stderr));
            // The last lines are kept, not the first: a step that fails says
            // why at its end, and a step that talks its way there would
            // otherwise push the reason out of the log.
            let lines: Vec<&str> = said.lines().filter(|l| !l.trim().is_empty()).collect();
            // Only a step that failed is remembered for the page: a step
            // that succeeded may still have said something, and that would
            // not explain a later failure.
            let failed = !done.status.success();
            if failed && lines.is_empty() {
                // A step that failed without a word is still named for the
                // page: which step it was is half the answer.
                remember(format!("{} {} ended: {}", s.path, s.args.join(" "), done.status));
            }
            for line in &lines[lines.len().saturating_sub(SAID_LINES)..] {
                let said = format!("{}: {line}", s.path);
                if failed {
                    remember(said.clone());
                }
                skyward_sys::kmsg(&said)?;
            }
            ensure!(done.status.success(), "{} {} ended: {}", s.path, s.args.join(" "), done.status);
            skyward_sys::kmsg(&format!("ran {} {}", s.path, s.args.join(" ")))?;
            continue;
        }
        let optional = s.optional;
        let waiting = s.clone();
        let name = Path::new(&s.path).file_name().map_or_else(|| s.path.clone(), |n| n.to_string_lossy().into_owned());
        thread::Builder::new().name(name).spawn(move || supervise(&s, &who)).context("start a supervising thread")?;
        if let Err(e) = ready(&waiting) {
            // One the phone can do without holds up nothing: it may come up
            // late, or not at all, and what needs it says so when it is
            // asked.
            if !optional {
                return Err(e);
            }
            skyward_sys::kmsg(&format!("{e:#}"))?;
        }
    }
    Ok(())
}

/// Who a program runs as and what of root's it keeps, as the phone's own
/// init has it run, worked out before it is started: between the fork and
/// the exec nothing may be looked up.
#[derive(Debug, Clone)]
struct Credentials {
    uid: libc::uid_t,
    gid: libc::gid_t,
    /// The other groups it is in.
    groups: Vec<libc::gid_t>,
    kept: Option<Kept>,
}

/// The capabilities a program keeps, as masks.
#[derive(Debug, Clone, Copy)]
struct Kept {
    mask: u64,
    /// Those and the right to drop the others for good, which goes once
    /// they are dropped.
    dropping: u64,
    /// The highest capability the kernel knows.
    last: u32,
}

impl Credentials {
    /// Who `s` runs as, by the names the phone gives its accounts: root,
    /// in no other group, keeping all of root's, where it names none.
    fn of(s: &Service, known: &HashMap<String, u32>) -> Result<Self> {
        let id = |name: &String| ueventd::account(known, name).with_context(|| format!("{}: the phone has no account named {name}", s.path));
        let uid = s.user.as_ref().map(id).transpose()?.unwrap_or(0);
        let mut groups = s.groups.iter().map(id).collect::<Result<Vec<_>>>()?;
        let gid = if groups.is_empty() { 0 } else { groups.remove(0) };
        let kept = if s.capabilities.is_empty() {
            None
        } else {
            let bit = |name: &str| skyward_config::capability(name).map(|n| 1u64 << n).with_context(|| format!("{}: {name} is not a capability", s.path));
            let mask = s.capabilities.iter().try_fold(0, |mask, c| bit(c).map(|b| mask | b))?;
            let last = fs::read_to_string(CAP_LAST_CAP).with_context(|| format!("read {CAP_LAST_CAP}"))?;
            let last = last.trim().parse().with_context(|| format!("read the number in {CAP_LAST_CAP}"))?;
            Some(Kept { mask, dropping: mask | bit("SETPCAP")?, last })
        };
        Ok(Self { uid, gid, groups, kept })
    }

    /// Become who the program runs as, in the child about to run it, as
    /// init does: take its groups and its user, keeping the capabilities
    /// through the change where it keeps some, then drop every other one
    /// for good and hand those it keeps on through the exec.
    fn take_on(&self) -> io::Result<()> {
        let done = |r: libc::c_int| if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) };
        let prctl = |option: libc::c_int, a: libc::c_ulong, b: libc::c_ulong| done(unsafe { libc::prctl(option, a, b, 0 as libc::c_ulong, 0 as libc::c_ulong) });
        if self.kept.is_some() {
            prctl(libc::PR_SET_KEEPCAPS, 1, 0)?;
        }
        done(unsafe { libc::setgroups(self.groups.len(), self.groups.as_ptr()) })?;
        done(unsafe { libc::setresgid(self.gid, self.gid, self.gid) })?;
        done(unsafe { libc::setresuid(self.uid, self.uid, self.uid) })?;
        let Some(kept) = self.kept else { return Ok(()) };
        set_capabilities(kept.dropping)?;
        for cap in (0..=kept.last).filter(|c| kept.mask & 1 << c == 0) {
            prctl(libc::PR_CAPBSET_DROP, cap.into(), 0)?;
        }
        set_capabilities(kept.mask)?;
        for cap in (0..=kept.last).filter(|c| kept.mask & 1 << c != 0) {
            prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_RAISE as libc::c_ulong, cap.into())?;
        }
        Ok(())
    }
}

/// Make `mask` the capabilities the process has, may take up and hands on.
fn set_capabilities(mask: u64) -> io::Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    struct Sets {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header { version: CAPABILITY_VERSION_3, pid: 0 };
    let half = |bits: u32| Sets { effective: bits, permitted: bits, inheritable: bits };
    let sets = [half(mask as u32), half((mask >> 32) as u32)];
    if unsafe { libc::syscall(libc::SYS_capset, &header, sets.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Wait until the program has what it says it will be ready with.
fn ready(s: &Service) -> Result<()> {
    let waited_on = match (&s.ready_property, &s.ready_service, &s.ready_hardware_service) {
        (Some(p), _, _) => format!("set no {p}"),
        (_, Some(n), _) => format!("registered no {n}"),
        (_, _, Some(n)) => format!("registered no {n}"),
        _ => return Ok(()),
    };
    let until = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(name) = &s.ready_property {
            if skyward_sys::prop(name).is_some_and(|v| !v.is_empty()) {
                return Ok(());
            }
        }
        // Either directory may itself be starting again, and until it
        // answers nothing can be looked up in it.
        if let Some(name) = &s.ready_service {
            if registered(name) {
                return Ok(());
            }
        }
        if let Some(name) = &s.ready_hardware_service {
            if registered_hardware(name) {
                return Ok(());
            }
        }
        if Instant::now() >= until {
            bail!("{} {waited_on} in {} seconds", s.path, READY_TIMEOUT.as_secs());
        }
        thread::sleep(READY_POLL);
    }
}

fn supervise(s: &Service, who: &Credentials) -> ! {
    loop {
        let started = Instant::now();
        let ended = match running(&s.path) {
            Some(pid) => wait_for(pid).map(|()| format!("pid {pid}, left by an earlier recovery service, exited")),
            None => run(s, who),
        };
        let line = match ended {
            Ok(how) => format!("{}: {how}", s.path),
            Err(e) => format!("{}: {e:#}", s.path),
        };
        // The page is told which service stopped, even when it stopped
        // without a word.
        remember(line.clone());
        let _ = skyward_sys::kmsg(&line);
        if started.elapsed() < RESTART_DELAY {
            thread::sleep(RESTART_DELAY);
        }
    }
}

/// Start the program and wait for it to end, keeping the first lines it
/// says on the way.
///
/// A daemon that ends at once — a library it cannot load, a device that is
/// not there — says so on standard error, and that one line is the whole
/// explanation. Its streams were thrown away, so a phone whose service
/// would not stay up left only "ended: exit status 1" behind.
fn run(s: &Service, who: &Credentials) -> Result<String> {
    let refused = Arc::new(AtomicBool::new(false));
    let (pid, status) = spawn(s, who, None, refused.clone())?;
    if refused.load(Ordering::Relaxed) && is_program(&s.path) {
        // The linker would not have the program. The phone's own linker
        // knows the phone's own libraries, and those were built together.
        if let Some(linker) = phone_linker() {
            let _ = skyward_sys::kmsg(&format!(
                "{}: the recovery's own libraries will not do; it is started with the phone's own",
                s.path
            ));
            let (pid, status) = spawn(s, who, Some(&linker), Arc::new(AtomicBool::new(false)))?;
            return Ok(format!("pid {pid} ended: {status}"));
        }
    }
    Ok(format!("pid {pid} ended: {status}"))
}

/// Start the program and wait for it to end. `runner` is the phone's own
/// linker, when this recovery's libraries will not have the program.
fn spawn(s: &Service, who: &Credentials, runner: Option<&Path>, refused: Arc<AtomicBool>) -> Result<(u32, ExitStatus)> {
    let tag = match runner {
        Some(_) => " through the phone's own linker",
        None => "",
    };
    let mut started = command_with(s, who, runner);
    started.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = started.spawn().with_context(|| format!("start {}", s.path))?;
    let mut keepers = Vec::new();
    if let Some(out) = child.stdout.take() {
        keepers.push(said(s.path.clone(), out, refused.clone(), tag.to_string()));
    }
    if let Some(err) = child.stderr.take() {
        keepers.push(said(s.path.clone(), err, refused.clone(), tag.to_string()));
    }
    let status = child.wait().context("wait for it")?;
    for k in keepers {
        let _ = k.join();
    }
    Ok((child.id(), status))
}

/// Whether the file at `path` is a program the linker can load, rather
/// than a script it would refuse for a different reason: a script's own
/// error is the script's to report, and the phone's linker would refuse
/// the script itself.
fn is_program(path: &str) -> bool {
    let Ok(mut file) = fs::File::open(path) else { return false };
    let mut head = [0u8; 4];
    let Ok(read) = file.read(&mut head) else { return false };
    read == 4 && &head[..2] != b"#!"
}

/// The installed system's own linker, where the phone keeps one.
fn phone_linker() -> Option<PathBuf> {
    PHONE_LINKERS.iter().map(Path::new).find(|p| p.is_file()).map(Path::to_path_buf)
}

/// Keep the first [`DAEMON_LINES`] lines a program writes to one of its
/// streams, and go on reading the rest without keeping it — a program that
/// fills the pipe must never be blocked by a log that is full.
fn said(path: String, stream: impl io::Read + Send + 'static, refused: Arc<AtomicBool>, tag: String) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        use io::BufRead;
        let mut kept = 0;
        for line in io::BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            if line.trim().is_empty() {
                continue;
            }
            if line.contains(REFUSED) {
                refused.store(true, Ordering::Relaxed);
            }
            if kept < DAEMON_LINES {
                kept += 1;
                let said = format!("{path}{tag}: {line}");
                remember(said.clone());
                let _ = skyward_sys::kmsg(&said);
            }
        }
    })
}

/// The program as it is started: with what it is given, from the top of the
/// tree, reading and writing nothing of the recovery's, finding its
/// libraries where it names them, and as `who`.
fn command(s: &Service, who: &Credentials) -> Command {
    command_with(s, who, None)
}

/// The program to run. Through the phone's own linker, the libraries are
/// the phone's own and its own linker decides where they are; this
/// recovery's search path is of no use to it, and is left out.
fn command_with(s: &Service, who: &Credentials, runner: Option<&Path>) -> Command {
    let mut cmd = match runner {
        Some(linker) => {
            // The phone's own linker takes the program as its first
            // argument and has no option for the libraries: it reads
            // LD_LIBRARY_PATH, like any other program it starts.
            let mut c = Command::new(linker);
            c.arg(&s.path).args(&s.args);
            c.env("LD_LIBRARY_PATH", PHONE_LIBRARIES);
            c
        }
        None => {
            let mut c = Command::new(&s.path);
            c.args(&s.args);
            c
        }
    };
    cmd.current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if runner.is_none() && !s.libraries.is_empty() {
        cmd.env("LD_LIBRARY_PATH", s.libraries.join(":"));
    }
    let who = who.clone();
    // Only system calls run between the fork and the exec, on what was
    // worked out before it.
    unsafe { cmd.pre_exec(move || who.take_on()) };
    cmd
}

/// Lock in memory every file the running programs have mapped or open on
/// the `mounted` partitions, so they keep working once those partitions
/// are written over, and give the mount points the files are on. The
/// locked mappings stay for the rest of the recovery's life.
pub fn pin(services: &[Service], mounted: &[Mounted]) -> Result<Vec<PathBuf>> {
    let mut files = BTreeSet::new();
    // A program run once has ended, and one between restarts has nothing in
    // memory to keep either.
    for s in services.iter().filter(|s| !s.oneshot) {
        let Some(pid) = running(&s.path) else { continue };
        let process = Process::new(pid as i32).with_context(|| format!("open process {pid}"))?;
        for map in process.maps().with_context(|| format!("read the mappings of {}", s.path))? {
            if let MMapPath::Path(p) = map.pathname {
                files.insert(p);
            }
        }
        for fd in process.fd().with_context(|| format!("read the files {} has open", s.path))? {
            if let FDTarget::Path(p) = fd.with_context(|| format!("read a file {} has open", s.path))?.target {
                files.insert(p);
            }
        }
    }
    let mut busy: Vec<PathBuf> = Vec::new();
    for file in files {
        let Some(m) = mounted.iter().find(|m| file.starts_with(&m.mount_point)) else { continue };
        if !fs::metadata(&file).with_context(|| format!("stat {}", file.display()))?.is_file() {
            continue;
        }
        lock(&file)?;
        let _ = skyward_sys::kmsg(&format!("kept {} in memory", file.display()));
        if !busy.contains(&m.mount_point) {
            busy.push(m.mount_point.clone());
        }
    }
    Ok(busy)
}

/// Map `path` whole and lock the mapping: its pages then stay in memory
/// for every process that maps the file.
fn lock(path: &Path) -> Result<()> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata().with_context(|| format!("stat {}", path.display()))?.len() as usize;
    if len == 0 {
        return Ok(());
    }
    let at = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0) };
    if at == libc::MAP_FAILED {
        return Err(io::Error::last_os_error()).with_context(|| format!("map {}", path.display()));
    }
    if unsafe { libc::mlock(at, len) } != 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("lock {} in memory", path.display()));
    }
    Ok(())
}

/// Whether a service has registered. The service directory may itself be
/// starting again, and until it answers nothing is registered as far as
/// anyone can tell.
pub fn registered(name: &str) -> bool {
    match skyward_binder::driver::Binder::open() {
        Ok(binder) => matches!(skyward_binder::service::find(&binder, name), Ok(Some(_))),
        Err(_) => false,
    }
}

/// Whether a hardware service of the older kind has registered with the
/// directory its kind uses. It is named as the interface and then which of
/// them it is.
pub fn registered_hardware(name: &str) -> bool {
    let Some((interface, instance)) = name.rsplit_once('/') else { return false };
    match skyward_binder::driver::Binder::hardware() {
        Ok(binder) => matches!(skyward_binder::hidl::find(&binder, interface, instance), Ok(Some(_))),
        Err(_) => false,
    }
}

/// Whether the service directory takes calls. What it is asked about does
/// not matter; that it answers does.
pub fn reachable() -> bool {
    match skyward_binder::driver::Binder::open() {
        Ok(binder) => skyward_binder::service::find(&binder, "android.os.IServiceManager").is_ok(),
        Err(_) => false,
    }
}

/// A process already running `path`.
pub fn running(path: &str) -> Option<u32> {
    fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid: u32 = e.file_name().to_str()?.parse().ok()?;
        (fs::read_link(e.path().join("exe")).ok()? == Path::new(path)).then_some(pid)
    })
}

/// Wait for a process that is not our child to exit.
fn wait_for(pid: u32) -> Result<()> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        let e = io::Error::last_os_error();
        // It exited between being found and being opened.
        if e.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(e).context("open a pidfd");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    loop {
        if unsafe { libc::poll(&mut pfd, 1, -1) } >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e).context("wait on the pidfd");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_program, PHONE_LIBRARIES};

    fn write(name: &str, head: &[u8]) -> std::path::PathBuf {
        let at = std::env::temp_dir().join(name);
        std::fs::write(&at, head).expect("write the file");
        at
    }

    #[test]
    fn a_script_is_not_a_program_the_linker_can_load() {
        let at = write("skyward-test-script", b"#!/system/bin/sh\necho hi\n");
        assert!(!is_program(at.to_str().expect("a path")));
        let _ = std::fs::remove_file(at);
    }

    #[test]
    fn a_program_is_one_the_linker_can_load() {
        let at = write("skyward-test-program", b"\x7fELF\x02\x01\x01\x00");
        assert!(is_program(at.to_str().expect("a path")));
        let _ = std::fs::remove_file(at);
    }

    #[test]
    fn a_file_that_is_not_there_is_not_a_program() {
        assert!(!is_program("/nowhere/at/all/skyward"));
    }

    #[test]
    fn the_phone_s_libraries_include_the_bootstrap_folder() {
        // The library of the loader lives in a folder of its own beside the
        // others, and a linker that is not given it says so.
        assert!(PHONE_LIBRARIES.contains("/bootstrap:"), "{PHONE_LIBRARIES}");
    }
}
