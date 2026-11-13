use anyhow::{anyhow, ensure, Context, Result};
use evdev::raw_stream::RawDevice;
use skyward_config::Strength;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// linux/input-event-codes.h
const EV_FF: u16 = 0x15;
const FF_PERIODIC: u16 = 0x51;
const FF_CUSTOM: u16 = 0x5d;
const FF_SINE: u16 = 0x5a;

/// What a motor swings at when its driver says the frequency nowhere: the
/// one the 17 Ultra's motor has, and the usual one of the linear motors
/// these phones carry.
const NOMINAL_HZ: f32 = 172.4;

/// How long a click runs. The driver plays a sine for the effect's replay
/// length, and a waveform it keeps for as long as that one is; either way
/// the click is taken away once this has passed.
///
/// Twelve milliseconds of a swing at about two hundred hertz is three swings,
/// which is what a linear motor's click is made of. Held for longer the same
/// sine stops being a click and becomes a buzz: the finger is given enough
/// swings in a row to feel the vibration rather than the tap.
const CLICK_LENGTH: Duration = Duration::from_millis(12);

/// How long the click takes to come up, and to go away again, in whole
/// milliseconds of its twelve. A swing that starts at its full strength
/// cracks; one that starts over a millisecond and fades over six is the tick
/// of a motor that is being told to stop, which is what these phones' own
/// system plays.
const ATTACK_MS: u16 = 1;
const FADE_MS: u16 = 6;

/// How strongly the phone's own system plays the two ticks of the screen
/// going off, whatever the slider says.
const TICKS_STRENGTH: f32 = 0.415;

/// How long between the two ticks.
const TICK_GAP: Duration = Duration::from_millis(55);

/// How much lighter the second tick is than the first.
const TICK_SECOND: f32 = 0.79;

/// The vibration motor, played through the kernel's force-feedback events.
/// Each click is loaded as it is played, at the strength the slider gives,
/// and taken away once it has run.
pub struct Haptics {
    dev: File,
    click: Click,
    /// What the two ticks of the screen going off are made of: swings at the
    /// motor's own frequency, whatever the click is.
    ticks: Click,
    strength: Strength,
    /// The click playing, by its slot and its count, until it is taken
    /// away; a click taken away early by the next is not taken away again.
    playing: Arc<Mutex<Option<(i16, u64)>>>,
    clicks: u64,
}

/// What a click is.
#[derive(Clone, Copy)]
enum Click {
    /// A waveform the driver keeps, by its number: a bank and a number are
    /// handed over, and the driver plays that waveform.
    Kept(i16),
    /// Swings of the motor at `hz`, which the driver makes up as a sine and
    /// drives at the strength it is given.
    Made(f32),
}

impl Haptics {
    /// The force-feedback device and the configured click. Loading a click
    /// fills the driver's queue with what it plays, so the click is loaded
    /// only as it is played.
    pub fn open(cfg: &skyward_config::Haptics) -> Result<Haptics> {
        let (path, dev) = motor()?;
        // A driver that says its motor's frequency nowhere, as Cirrus Logic's
        // CS40L26 does not, still takes a click made for the frequency these
        // motors swing at; the reason goes to the log and the motor is not
        // left out over it.
        let hz = match &cfg.resonance {
            Some(file) => match resonance(file) {
                Ok(hz) => hz,
                Err(e) => {
                    let _ = skyward_sys::kmsg(&format!("the motor's resonant frequency is not read, so {} Hz is used: {e:#}", NOMINAL_HZ));
                    NOMINAL_HZ
                }
            },
            None => NOMINAL_HZ,
        };
        let click = match cfg.click {
            Some(id) => {
                eprintln!("haptics {}: clicks with waveform {id}, ticks made for a motor swinging at {hz} Hz", path.display());
                Click::Kept(id)
            }
            None => {
                eprintln!("haptics {}: clicks and ticks made for a motor swinging at {hz} Hz", path.display());
                Click::Made(hz)
            }
        };
        Ok(Haptics { dev, click, ticks: Click::Made(hz), strength: cfg.strength, playing: Arc::new(Mutex::new(None)), clicks: 0 })
    }

    /// One short click at the strength the slider's `position` gives, after
    /// taking away one still playing. Gives how long it plays.
    pub fn click(&mut self, position: f32) -> Result<Duration> {
        self.start(self.click, share(&self.strength, position))
    }

    /// The two ticks of the screen going off, as strongly as the phone's own
    /// system plays them whatever the slider says, the second lighter. Gives
    /// how long they take.
    pub fn ticks(&mut self) -> Result<Duration> {
        let first = self.start(self.ticks, TICKS_STRENGTH)?;
        std::thread::sleep(TICK_GAP);
        let second = self.start(self.ticks, TICKS_STRENGTH * TICK_SECOND)?;
        Ok(first + TICK_GAP + second)
    }

    /// Play `click` at `share` of its full strength, after taking away what
    /// still plays, and take it away again once it has run out.
    fn start(&mut self, click: Click, share: f32) -> Result<Duration> {
        let mut playing = self.playing.lock().expect("no thread panics while it holds the click playing");
        if let Some((slot, _)) = playing.take() {
            erase(&self.dev, slot).context("take away the click still playing")?;
        }
        let loaded = upload(&self.dev, &click, share).context("load the click")?;
        play(&mut self.dev, loaded.slot).context("play the click")?;
        self.clicks += 1;
        let this = (loaded.slot, self.clicks);
        *playing = Some(this);
        drop(playing);
        let dev = self.dev.try_clone().context("share the motor")?;
        let (playing, length) = (self.playing.clone(), loaded.length);
        // Timed from now: a thread may wait a while before it first runs.
        let until = Instant::now() + length;
        std::thread::spawn(move || {
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
            let mut playing = playing.lock().expect("no thread panics while it holds the click playing");
            if *playing == Some(this) {
                *playing = None;
                if let Err(e) = erase(&dev, this.0) {
                    eprintln!("take away the click: {e}");
                }
            }
        });
        Ok(length)
    }
}

/// Play the configured click once at the slider's `position`, or else the
/// ticks of the screen going off, saying how strongly and for how long, for
/// trying out a motor.
pub fn probe(cfg: &skyward_config::Haptics, position: f32, ticks: bool) -> Result<()> {
    let mut haptics = Haptics::open(cfg)?;
    let (what, strength, length) = if ticks {
        ("ticks", TICKS_STRENGTH, haptics.ticks()?)
    } else {
        ("click", share(&cfg.strength, position), haptics.click(position)?)
    };
    // Once it has played out, what played goes with the device as the
    // command ends.
    std::thread::sleep(length);
    println!("played the {what} at {strength:.3} of the full strength, {} ms long", length.as_millis());
    Ok(())
}

/// Bring the motor's driver up: put the firmware it asks the kernel's loader
/// for where the loader looks, then load the modules it is in. A driver the
/// phone's own init loaded already has spent its probe on the empty folder
/// its recovery leaves; putting the firmware there does not try it again, so
/// the device is unbound from its driver and bound back to it, which probes
/// it afresh with the file now in place.
pub fn bring_up(cfg: &skyward_config::Haptics, sources: &[String]) -> Result<()> {
    for file in &cfg.firmware {
        crate::wlan::place_firmware(file)?;
    }
    if !cfg.modules.is_empty() {
        let dirs = sources.iter().map(|s| skyward_modules::source_dir(s)).collect::<Result<Vec<_>>>()?;
        for path in skyward_modules::load(&dirs, &cfg.modules)? {
            skyward_sys::kmsg(&format!("loaded {} for the motor", path.display()))?;
        }
    }
    // Only a driver that asked for firmware and went without it has to be
    // prodded; one that probed cleanly made its device already.
    if !cfg.firmware.is_empty() && !any_motor() {
        reprobe(cfg)?;
    }
    Ok(())
}

/// Unbind each of the configured drivers from its device and bind it back,
/// so the driver probes the motor again now its firmware is where the loader
/// looks. The drivers are found by the names the kernel gives them under
/// /sys/bus/i2c/drivers, which drop the suffix a module's file name carries:
/// cs40l26-i2c is the driver cs40l26.
fn reprobe(cfg: &skyward_config::Haptics) -> Result<()> {
    for module in &cfg.modules {
        let driver = driver_name(module);
        let at = Path::new("/sys/bus/i2c/drivers").join(&driver);
        let Ok(listing) = fs::read_dir(&at) else { continue };
        let bound: Vec<_> = listing.flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().contains('-')).collect();
        for name in &bound {
            let name = name.to_string_lossy();
            let _ = fs::write(at.join("unbind"), name.as_ref());
            let _ = fs::write(at.join("bind"), name.as_ref());
            skyward_sys::kmsg(&format!("bound {driver} to {name} again so it probes the motor with its firmware"))?;
        }
    }
    // The device node and its force-feedback device appear as the probe
    // runs, which is not synchronous with the write above.
    let until = std::time::Instant::now() + PROBE_WAIT;
    while std::time::Instant::now() < until {
        if any_motor() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

/// The kernel's name for the driver a module is in: the module's own name
/// without the bus it sits on, as cs40l26-i2c is the driver cs40l26 and
/// cs40l26-core is none.
fn driver_name(module: &str) -> String {
    module.trim_end_matches("-core").trim_end_matches("-i2c").replace('_', "-")
}

/// Whether any input device takes periodic force-feedback effects yet.
fn any_motor() -> bool {
    evdev::raw_stream::enumerate().any(|(_, dev)| takes_periodic(&dev))
}

/// How long a driver may take to make its force-feedback device once bound
/// to the motor again.
const PROBE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// The first input device under /dev/input that takes periodic
/// force-feedback effects, opened for writing.
fn motor() -> Result<(PathBuf, File)> {
    for (path, dev) in evdev::raw_stream::enumerate() {
        if takes_periodic(&dev) {
            let file = OpenOptions::new().read(true).write(true).open(&path).with_context(|| format!("open {}", path.display()))?;
            return Ok((path, file));
        }
    }
    Err(anyhow!("no input device takes force-feedback effects"))
}

fn takes_periodic(dev: &RawDevice) -> bool {
    dev.supported_ff().is_some_and(|ff| ff.contains(evdev::FFEffectCode::FF_PERIODIC))
}

/// The share of the click's full strength at the slider's `position`.
///
/// The slider moves the strength in equal steps of the swing itself: from a
/// quarter to three quarters of it, every tenth of the slider is a twentieth
/// more of the swing. A factor per step, which is what this used to be, is
/// the same number of decibels between every pair of neighbours, and a finger
/// does not read a click's strength in decibels: it reads how far the motor
/// throws the glass. Equal steps of the swing are what makes the slider's
/// halves tell apart.
fn share(strength: &Strength, position: f32) -> f32 {
    let at = position.clamp(0.0, 1.0);
    strength.lightest + (strength.strongest - strength.lightest) * at
}

/// What these motors swing at, from the slowest to the fastest a linear
/// motor of this kind has. A frequency read as a number outside it was not
/// in hertz in the first place.
const SWINGS_BETWEEN: (f32, f32) = (20.0, 400.0);

/// The motor's resonant frequency in hertz, from the file the driver says
/// it in. A driver that says it nowhere, as Cirrus Logic's CS40L26 does not,
/// still takes a click made for a frequency near the one these motors swing
/// at.
///
/// The files differ in what they count in. Qualcomm's says tenths of a
/// hertz in plain decimal. Cirrus Logic's `f0_stored` says it in ten
/// thousandths of a hertz, as hexadecimal: `00202CAB` is 210.859 Hz. What a
/// file counts in is not something the recovery can ask it, so both readings
/// are tried and the one that comes out a frequency these motors have is
/// taken; neither is, and the click is made at the nominal one rather than at
/// a frequency no motor swings at.
fn resonance(file: &str) -> Result<f32> {
    let text = fs::read_to_string(file).with_context(|| format!("read {file}"))?;
    let raw = text.split_whitespace().next().with_context(|| format!("{file} is empty"))?;
    // A leading zero is how the driver stamps the hexadecimal it writes, and
    // plain decimal never needs one.
    let hex = raw.strip_prefix("0x").or_else(|| raw.starts_with('0').then_some(raw));
    let counted = match hex {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => raw.parse(),
    }
    .with_context(|| format!("{file} holds {text:?}"))?;
    ensure!(counted > 0, "{file} says the motor swings at 0 Hz");
    // The two counts the drivers use, ten thousandths and tenths of a hertz,
    // and whichever of them lands in range.
    let swept = [counted as f32 / 10_000.0, counted as f32 / 10.0]
        .into_iter()
        .find(|hz| (SWINGS_BETWEEN.0..=SWINGS_BETWEEN.1).contains(hz));
    swept.with_context(|| format!("{file} says {counted}, which is no frequency these motors swing at"))
}

/// The period, in whole microseconds, of a swing at `hz`: what a periodic
/// effect gives the driver to make the swing from, and never zero.
fn period_of(hz: f32) -> u16 {
    (1_000_000.0 / hz.max(1.0)).round().clamp(1.0, f32::from(u16::MAX)) as u16
}

/// A click as the driver loaded it: the slot the kernel keeps it in, and
/// how long it plays.
struct Loaded {
    slot: i16,
    length: Duration,
}

/// Load `click` at `share` of its full strength as a periodic effect in a
/// slot of its own.
///
/// The driver tells what it is given by the effect's waveform: a waveform it
/// keeps comes as two values, a bank and a number, and a sine comes from the
/// period and the strength. Samples handed over as FIFO data are read as a
/// wavetable the driver makes up itself, which this motor's firmware refuses
/// ("Invalid OWT index: -1" in the kernel log), so none are handed over.
///
/// The envelope is what makes the sine a click rather than a buzz: the swing
/// comes up over a millisecond and fades over six of its twelve, where a sine
/// held at one strength for the whole length is a vibration the finger reads
/// as a rumble and not as a tap. A waveform the driver keeps has a shape of
/// its own and takes no envelope from here.
fn upload(dev: &File, click: &Click, share: f32) -> Result<Loaded> {
    let mut kept: [i16; 2] = [0; 2];
    let (waveform, period, len, data) = match click {
        Click::Kept(id) => {
            // The RAM waveforms are the bank before the number: 0 is the one
            // the phone's own HAL plays when it starts.
            kept[0] = 0;
            kept[1] = *id;
            (FF_CUSTOM, 0u16, std::mem::size_of_val(&kept), kept.as_mut_ptr())
        }
        Click::Made(hz) => (FF_SINE, period_of(*hz), 0, std::ptr::null_mut()),
    };
    let periodic = libc::ff_periodic_effect {
        waveform,
        period,
        magnitude: (share.clamp(0.0, 1.0) * f32::from(i16::MAX)).round() as i16,
        offset: 0,
        phase: 0,
        // From nothing up to the strength over the attack, and down to
        // nothing over the fade: a level at either end other than nothing
        // would leave the swing turning on and off at its full throw.
        envelope: libc::ff_envelope { attack_length: ATTACK_MS, attack_level: 0, fade_length: FADE_MS, fade_level: 0 },
        custom_len: len as u32,
        custom_data: data,
    };
    let mut effect = libc::ff_effect {
        type_: FF_PERIODIC,
        id: -1,
        direction: 0,
        trigger: libc::ff_trigger { button: 0, interval: 0 },
        replay: libc::ff_replay { length: CLICK_LENGTH.as_millis() as u16, delay: 0 },
        u: [0; 4],
    };
    // The union holds the periodic effect; it is exactly as large.
    const _: () = assert!(std::mem::size_of::<libc::ff_periodic_effect>() == std::mem::size_of::<[u64; 4]>());
    effect.u = unsafe { std::mem::transmute::<libc::ff_periodic_effect, [u64; 4]>(periodic) };
    // EVIOCSFF: _IOW('E', 0x80, struct ff_effect)
    let size = std::mem::size_of::<libc::ff_effect>() as u32;
    let request = (1u32 << 30) | (size << 16) | (u32::from(b'E') << 8) | 0x80;
    if unsafe { libc::ioctl(dev.as_raw_fd(), request as _, &mut effect) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(Loaded { slot: effect.id, length: CLICK_LENGTH })
}

/// Start the loaded effect in slot `id`.
fn play(dev: &mut File, id: i16) -> io::Result<()> {
    let event = libc::input_event { time: libc::timeval { tv_sec: 0, tv_usec: 0 }, type_: EV_FF, code: id as u16, value: 1 };
    let bytes = unsafe { std::slice::from_raw_parts((&event as *const libc::input_event).cast::<u8>(), std::mem::size_of::<libc::input_event>()) };
    dev.write_all(bytes)
}

/// Take the effect in slot `id` away, which stops it for good.
fn erase(dev: &File, id: i16) -> io::Result<()> {
    // EVIOCRMFF: _IOW('E', 0x81, int)
    let request = (1u32 << 30) | ((std::mem::size_of::<libc::c_int>() as u32) << 16) | (u32::from(b'E') << 8) | 0x81;
    if unsafe { libc::ioctl(dev.as_raw_fd(), request as _, libc::c_int::from(id)) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sine_is_asked_for_at_the_motors_own_period() {
        assert_eq!(period_of(200.0), 5_000);
        assert_eq!(period_of(1_000.0), 1_000);
        assert_eq!(period_of(1_000_000.0), 1, "never zero microseconds");
        assert_eq!(period_of(0.0), 1, "a frequency of nothing is still a period");
    }

    #[test]
    fn the_slider_moves_the_strength_in_equal_steps() {
        let s = Strength { lightest: 0.25, strongest: 1.0 };
        assert_eq!((share(&s, 0.0), share(&s, 1.0)), (0.25, 1.0));
        assert!((share(&s, 0.5) - 0.625).abs() < 1e-6, "halfway is halfway in between");
        for step in 0..10 {
            let (here, next) = (share(&s, step as f32 / 10.0), share(&s, (step + 1) as f32 / 10.0));
            assert!((next - here - 0.075).abs() < 1e-6, "every tenth of the slider is the same step: {here} to {next}");
        }
        assert_eq!((share(&s, -1.0), share(&s, 2.0)), (0.25, 1.0), "no further than its ends");
    }

    #[test]
    fn a_click_comes_up_and_goes_away_within_the_time_it_plays() {
        assert!(CLICK_LENGTH.as_millis() as u16 >= ATTACK_MS + FADE_MS, "the envelope fits in the click");
        assert!(CLICK_LENGTH.as_millis() < 30, "a click that long is a buzz");
    }
}
