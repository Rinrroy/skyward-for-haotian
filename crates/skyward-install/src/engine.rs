//! How the OTA engine is waited on, and how its output is read.
//!
//! update_engine starts helpers of its own — snapuserd for the snapshots of a
//! slot it writes — and those helpers inherit the pipes it was given. Waiting
//! for an output pipe to close is therefore not the same as waiting for the
//! engine to finish: a helper that stays alive for as long as the phone runs
//! keeps the pipe open after the engine has already exited, and one that exits
//! with the engine can leave the last of its output unread. Both are waited on
//! here, separately.

use anyhow::{Context, Result};
use std::io::{self, ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// The most often the loop wakes on its own, to look at whether the engine
/// has exited.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// How long the output is still read after the engine has exited, so that a
/// line it wrote just before going is not lost.
const LINGER: Duration = Duration::from_secs(1);

/// Wait for `child` to end, handing everything `rx` carries to `receive` in
/// the meantime, and give its status.
///
/// The engine's exit is looked at separately from the pipes closing: a
/// descendant holding an output descriptor keeps the pipes open after the
/// engine is gone, and that is when the wait has to end anyway — with the
/// engine's own status — without ending the descendant, which may be serving
/// live snapshots.
pub(crate) fn wait_engine<T>(child: &mut Child, rx: &Receiver<T>, mut receive: impl FnMut(T)) -> Result<ExitStatus> {
    let mut ended = None;
    loop {
        if ended.is_none() {
            if let Some(status) = child.try_wait().context("check whether update_engine has exited")? {
                ended = Some((status, Instant::now()));
            }
        }
        match rx.recv_timeout(PROGRESS_EVERY) {
            Ok(line) => receive(line),
            Err(RecvTimeoutError::Timeout) => {}
            // Both output pipes closed. That is not proof that the engine has
            // ended: it may have handed them on and kept working.
            Err(RecvTimeoutError::Disconnected) => return child.wait().context("wait for update_engine"),
        }
        if let Some((status, at)) = ended {
            if at.elapsed() >= LINGER {
                return Ok(status);
            }
        }
    }
}

/// Hand each line of `from` to `each` until `stop` is raised, or the writer
/// closes it, including a last line that has no line ending after it.
///
/// Reading waits on the descriptor with a deadline rather than in the kernel,
/// so a reader can always be let go: a pipe kept open by a helper that
/// outlives the engine would otherwise strand the thread for as long as the
/// phone runs, and the flash it belongs to would never conclude.
pub(crate) fn engine_lines(mut from: impl Read + AsRawFd, stop: &AtomicBool, mut each: impl FnMut(&str) -> io::Result<()>) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut buf = [0; 4096];
    while !stop.load(Ordering::Relaxed) {
        let mut waiting = libc::pollfd { fd: from.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut waiting, 1, 100) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        let read = match from.read(&mut buf) {
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..read]);
        let mut start = 0;
        for end in 0..pending.len() {
            if pending[end] == b'\n' {
                each(String::from_utf8_lossy(&pending[start..end]).trim_end_matches('\r'))?;
                start = end + 1;
            }
        }
        pending.drain(..start);
    }
    if !pending.is_empty() {
        each(String::from_utf8_lossy(&pending).trim_end_matches('\r'))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::Stdio;
    use std::sync::{mpsc, Arc};
    use std::thread;

    fn pipe() -> Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
    }

    #[test]
    fn an_output_its_helper_keeps_open_does_not_hide_the_engine_exiting() {
        let (read, write) = pipe().unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30 & echo $!; exit 7"])
            .stdout(Stdio::from(write))
            .spawn()
            .unwrap();
        // The first line is the helper's process id: it is holding the pipe
        // open, and the engine has already exited by the time the wait ends.
        let mut reader = BufReader::new(File::from(read));
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let helper: libc::pid_t = line.trim().parse().unwrap();
        let (tx, rx) = mpsc::channel::<()>();
        let started = Instant::now();
        let ended = wait_engine(&mut child, &rx, |()| {}).unwrap();
        let alive = unsafe { libc::kill(helper, 0) } == 0;
        unsafe {
            libc::kill(helper, libc::SIGKILL);
        }
        drop(tx);
        assert_eq!(ended.code(), Some(7));
        assert!(started.elapsed() < Duration::from_secs(5), "the wait ended with the engine, not with the pipe");
        assert!(alive, "waiting must not end a helper that is serving snapshots");
    }

    #[test]
    fn output_that_closed_is_not_the_engine_finishing() {
        let mut child = std::process::Command::new("/bin/sh").args(["-c", "sleep 0.2; exit 9"]).spawn().unwrap();
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx);
        assert_eq!(wait_engine(&mut child, &rx, |()| {}).unwrap().code(), Some(9));
    }

    #[test]
    fn a_reader_that_is_told_to_stop_hands_over_the_line_it_holds() {
        let (read, write) = pipe().unwrap();
        let mut write = File::from(write);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let reading = thread::spawn(move || {
            let mut lines = Vec::new();
            engine_lines(File::from(read), &stopping, |line| {
                lines.push(line.to_string());
                Ok(())
            })
            .unwrap();
            lines
        });
        write.write_all(b"a line\r\nhalf a line").unwrap();
        thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::Relaxed);
        assert_eq!(reading.join().unwrap(), ["a line", "half a line"]);
        drop(write);
    }
}
