//! The commands install scripts written for other recoveries run that a
//! stock recovery does not carry: 7za, which takes a partition image out of
//! the zip the script came in, and zstd, which decompresses it on its way to
//! the partition; avbctl, which turns off verified boot's checks before a
//! script writes a system that would not pass them, and resetprop, which
//! reads the phone's properties and sets them; and unzip, as the stock
//! recovery's own but pouring out what `-p` takes as it reads it. The
//! ramdisk links each to the recovery service binary, which runs the one it
//! is started as. Unlike a copy of a command a script brings along, built
//! against libraries each phone's recovery carries in a version of its
//! own, these need none of them.

use skyward_sys::kmsg;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::time::Instant;

pub mod avbctl;
mod resetprop;
mod sevenzip;
mod unzip;
mod zstandard;

/// How much is read at a time: a whole partition image may go through a
/// command, and a frame of a compressed one comes out in pieces of a
/// megabyte.
const CHUNK: usize = 1 << 20;

/// How much is written at a time: what a command writes may be a whole
/// partition, going to a flash that takes megabytes far better than it takes
/// the megabyte pieces a compressed frame comes out in, so the pieces are
/// gathered into this much before the write that hands them over.
const WRITE_CHUNK: usize = 4 << 20;

/// What `ioctl` asks a block device for how big it is, in bytes:
/// `BLKGETSIZE64`, which the libc crate does not carry for every target.
const BLKGETSIZE64: libc::Ioctl = 0x8008_1272u32 as libc::Ioctl;

/// What a command reads and writes besides the files it names.
struct Io<'a> {
    input: &'a mut dyn Read,
    output: &'a mut dyn Write,
    errors: &'a mut dyn Write,
}

/// A command: its arguments, without its name, to the status it ends with.
type Command = fn(&[OsString], &mut Io) -> i32;

/// The command a program started as `name` is, if it is one of them.
fn command(name: &OsStr) -> Option<Command> {
    match name.to_str()? {
        "7za" => Some(sevenzip::run),
        "avbctl" => Some(avbctl::run),
        "resetprop" => Some(resetprop::run),
        "unzip" => Some(unzip::run),
        "zstd" => Some(zstandard::run),
        _ => None,
    }
}

/// Run the command this program was started as, `name`, with `args`, and
/// give the status it ends with; none when `name` is not one of them.
pub fn run(name: &OsStr, args: &[OsString]) -> Option<i32> {
    let command = command(name)?;
    // As with the programs these stand in for, writing to a pipe whose
    // reader has gone ends the command.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let (stdin, stdout, stderr) = (io::stdin(), io::stdout(), io::stderr());
    let (mut input, mut output, mut errors) = (stdin.lock(), stdout.lock(), stderr.lock());
    // An install script flashes a partition by giving the image to a command
    // and sending what it writes to the partition, so what goes out of
    // standard output may be a whole one: it is counted as it goes, told of
    // in the kernel log a gigabyte at a time, and flushed once the command
    // has written all of it, before the script that ran it ends and the phone
    // reboots, which the pages the kernel still holds would not survive.
    let mut written = Written::new(&mut output, "stdout", libc::STDOUT_FILENO);
    let status = command(args, &mut Io { input: &mut input, output: &mut written, errors: &mut errors });
    written.finish();
    Some(status)
}

/// What a command writes, as it goes out: how much there is of it and how
/// fast it goes, which the kernel log is told of a gigabyte at a time, and
/// the flush that has all of it in the flash by the time the command ends.
/// A command whose output is a pipe, a file or a terminal is counted the
/// same and told of nothing: the rate of a partition is the one worth
/// knowing, and it is the one that is slow.
struct Written<'a> {
    to: &'a mut dyn Write,
    /// Where the bytes go, as the lines telling the rate name it.
    name: &'a str,
    /// The file or device they go to, which the flush at the end is on; a
    /// pipe has nothing to flush and answers with an error that is no more
    /// the command's to answer for than a slow flash is.
    fd: RawFd,
    /// Whether they go to a block device, which is what the lines are for,
    /// and how big it is, where the kernel says.
    device: bool,
    size: Option<u64>,
    bytes: u64,
    /// How many gigabytes of `bytes` have been told of.
    told: u64,
    started: Instant,
}

impl<'a> Written<'a> {
    /// `to` as it goes to the file or device `fd`, named `name` in the lines
    /// telling the rate.
    fn new(to: &'a mut dyn Write, name: &'a str, fd: RawFd) -> Written<'a> {
        let device = device_of(fd);
        Written { to, name, fd, device: device.is_some(), size: device.flatten(), bytes: 0, told: 0, started: Instant::now() }
    }

    /// Count what went out, and tell of each whole gigabyte of it as it is
    /// passed: often enough to see a partition written at half the speed it
    /// is read at, and rarely enough not to fill the log of a script that
    /// writes a dozen of them.
    fn counted(&mut self, bytes: usize) {
        self.bytes += bytes as u64;
        let told = self.bytes >> 30;
        if told > self.told {
            self.told = told;
            self.tell();
        }
    }

    /// Tell the kernel log how far the write has come and how fast it is
    /// going; nothing of one that does not go to a device.
    fn tell(&self) {
        if !self.device {
            return;
        }
        let (done, rate) = (self.bytes >> 20, self.rate());
        let line = match self.size {
            Some(size) => format!("{}: {done} of {} MiB, {rate} MB/s", self.name, size >> 20),
            None => format!("{}: {done} MiB, {rate} MB/s", self.name),
        };
        // The log is a diagnostic: it may never fail what it tells of.
        let _ = kmsg(&line);
    }

    /// How many megabytes a second have gone out since this began; nought
    /// where the clock has not moved, which a cast rounds and stops at the
    /// ends rather than panicking over.
    fn rate(&self) -> u64 {
        let seconds = self.started.elapsed().as_secs_f64();
        match seconds > 0.0 {
            true => (self.bytes as f64 / seconds / 1_000_000.0) as u64,
            false => 0,
        }
    }

    /// Flush the last of what went out, have it reach the flash, and tell of
    /// what a whole gigabyte did not cover, so that the log ends with the
    /// whole of what was written rather than the last gigabyte of it.
    fn finish(&mut self) {
        let _ = self.to.flush();
        // A pipe, a terminal, or a file nothing is keeping before the flash
        // has nothing to sync and answers with an error, which is no fault of
        // the command's to fail on: what a script wrote and got no error for,
        // it may keep.
        let _ = unsafe { libc::fsync(self.fd) };
        if self.device && self.bytes > self.told << 30 {
            self.tell();
        }
    }
}

impl Write for Written<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let went = self.to.write(data)?;
        self.counted(went);
        Ok(went)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.to.flush()
    }
}

/// The size of the block device `fd` is, where it is one: `None` where it is
/// not, and no size where the kernel will not say how big it is.
fn device_of(fd: RawFd) -> Option<Option<u64>> {
    // Padding a structure the kernel fills in whole is what zeroing it is
    // for, and a file it does not fill in is one it did not say anything of.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 || stat.st_mode & libc::S_IFMT != libc::S_IFBLK {
        return None;
    }
    let mut bytes: u64 = 0;
    // What the kernel answers is a status, not the size: the size is written
    // into `bytes`.
    let answer = unsafe { libc::ioctl(fd, BLKGETSIZE64, &mut bytes as *mut u64) };
    Some((answer == 0).then_some(bytes))
}

/// Where copying stopped.
#[derive(Debug)]
enum Failed {
    Reading(io::Error),
    Writing(io::Error),
}

/// Copy what is left of `from` to `to`, giving how much there was.
fn copy(from: &mut dyn Read, to: &mut dyn Write) -> Result<u64, Failed> {
    let mut chunk = vec![0; WRITE_CHUNK];
    let mut filled = 0;
    let mut copied = 0;
    loop {
        // A frame, or a file in a zip, comes out in pieces far smaller than
        // this, so the chunk is filled by reading again and again before it
        // is written: what goes to a partition then goes in whole
        // [`WRITE_CHUNK`]s, which is what the flash writes fastest.
        while filled < chunk.len() {
            match from.read(&mut chunk[filled..]) {
                Ok(0) => {
                    // The last of it, short of a [`WRITE_CHUNK`] and worth
                    // writing all the same: a reader gets the whole of what
                    // was read, however early the input stopped.
                    if filled > 0 {
                        to.write_all(&chunk[..filled]).map_err(Failed::Writing)?;
                        copied += filled as u64;
                    }
                    return Ok(copied);
                }
                Ok(read) => filled += read,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Failed::Reading(e)),
            }
        }
        to.write_all(&chunk).map_err(Failed::Writing)?;
        copied += chunk.len() as u64;
        filled = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn every_command_the_ramdisk_links_is_one_this_binary_runs() {
        for link in skyward_config::ramdisk::COMMANDS {
            let name = Path::new(link).file_name().expect("a link has a name");
            assert!(command(name).is_some(), "{link}");
        }
        assert!(command(OsStr::new("recovery")).is_none());
        assert!(command(OsStr::new("zipinfo")).is_none(), "the stock recovery's own answers to that name");
    }
}
