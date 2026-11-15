//! The panel's own doze: a little kept on the glass with the backlight out,
//! as the stock system keeps its always-on display.
//!
//! On Xiaomi's phones the panel is put into a doze by sending it command sets
//! of its own through Xiaomi's `mi_disp` driver, which lives inside
//! `msm_drm` — the same module that drives the pages. The driver lists the
//! features it offers, each under a number, in `disp_param` beside the node
//! that sends a command set. What glows is decided by the frame, not by the
//! light: an OLED lights exactly the subpixels the buffer lights, so the face
//! drawn here is all that is on, and everything else must be black.
//!
//! The whole of the decision — what to send, and whether to fall back to the
//! recovery's own blanking — is [`Doze::plan`], a function with no state and
//! no paths in it, so that a computer can test every way it can go. The code
//! that carries it out is in the recovery, and every write there is
//! best-effort: a phone whose driver does not answer is put out the way the
//! recovery has always put it out.

use crate::canvas::Canvas;
use crate::color::Rgba;
use crate::font::Fonts;
use crate::Lang;

/// The black the rest of the face is: on an OLED, the pixels that are not
/// lit, which is all but a few hundred of them.
const BLACK: Rgba = Rgba::rgb(0, 0, 0);
/// The clock's ink. White, because every channel of the glass shows white
/// whatever it makes of the others: the `haotian`'s red and blue both take
/// the buffer's blue byte (see AGENT.md), and white is the one colour that
/// comes through it unchanged either way.
const INK: Rgba = Rgba::rgb(255, 255, 255);
/// The mark's ink, dimmer, so that it is not the brightest thing left on the
/// glass through the night.
const MARK: Rgba = Rgba::rgb(96, 96, 96);

/// The clock's size in dp, and how far down the screen its middle sits as a
/// share of the screen's height.
const CLOCK_DP: f32 = 64.0;
const CLOCK_AT: f32 = 0.22;
/// The mark under the clock: how far below its middle it sits, how wide it
/// is, and how thick, all in dp.
const MARK_DROP_DP: f32 = 56.0;
const MARK_WIDE_DP: f32 = 72.0;
const MARK_THICK_DP: f32 = 2.0;

/// The line of readings under the mark: the battery, the slot the recovery
/// runs from, and whether the data partition is open. Its size in dp, and how
/// far below the clock's middle it sits.
const FACTS_DP: f32 = 15.0;
const FACTS_DROP_DP: f32 = 84.0;
/// The date above the clock, its size in dp and how far above the clock's
/// middle its baseline sits.
const DATE_DP: f32 = 17.0;
const DATE_RISE_DP: f32 = 78.0;

/// The offset face: the hour and the minute, each as large as the screen has
/// room for. Their size in dp, where the hour's middle sits down the screen
/// as a share of its height, how far either side of the middle the pair
/// stands, how far the minute sits below the hour, and where the date above
/// the hour and the readings below the minute go.
const OFFSET_DP: f32 = 150.0;
const OFFSET_AT: f32 = 0.28;
const OFFSET_SKEW_DP: f32 = 90.0;
const OFFSET_DROP_DP: f32 = 150.0;
const OFFSET_DATE_RISE_DP: f32 = 110.0;
const OFFSET_FACTS_DROP_DP: f32 = 120.0;

/// The dial: where its middle sits down the screen as a share of its height,
/// how far the ring reaches in dp, and how thick its rim is.
const DIAL_AT: f32 = 0.40;
const DIAL_RADIUS_DP: f32 = 104.0;
const DIAL_RING_DP: f32 = 3.0;
/// Its twelve marks: how far in from the rim one reaches, the quarters'
/// deeper than the rest, how far short of the rim both stop, and how wide
/// each is, halves in dp.
const DIAL_MARK_IN_DP: f32 = 10.0;
const DIAL_QUARTER_IN_DP: f32 = 14.0;
const DIAL_MARK_OUT_DP: f32 = 6.0;
const DIAL_MARK_HALF_DP: f32 = 1.25;
const DIAL_QUARTER_HALF_DP: f32 = 2.5;
/// The numerals the quarters are named with: their size in dp, and how deep
/// inside the rim they sit as a share of the ring's radius.
const DIAL_NUMERAL_DP: f32 = 26.0;
const DIAL_NUMERAL_AT: f32 = 0.72;
/// The hands: how far out each reaches as a share of the ring's radius, and
/// how wide each is at its pin and at its tip, in dp.
const DIAL_HOUR_LEN: f32 = 0.48;
const DIAL_MINUTE_LEN: f32 = 0.76;
const DIAL_HOUR_BASE_DP: f32 = 5.0;
const DIAL_HOUR_TIP_DP: f32 = 1.5;
const DIAL_MINUTE_BASE_DP: f32 = 3.5;
const DIAL_MINUTE_TIP_DP: f32 = 1.0;
/// The pin the hands turn on, and the black it is bored with.
const DIAL_PIN_DP: f32 = 6.0;
const DIAL_PIN_CORE_DP: f32 = 2.0;
/// The date above the ring, how far above its top it sits in dp, and where
/// the readings line goes down the screen as a share of its height.
const DIAL_DATE_RISE_DP: f32 = 46.0;
const DIAL_FACTS_AT: f32 = 0.72;

/// The faces the always-on display can wear, in the order the setting goes
/// round them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AodStyle {
    /// `HH:MM` large, centred: the face the recovery has always drawn.
    #[default]
    Digits,
    /// The hour and the minute set askew, each as large as the screen allows.
    Offset,
    /// A clock face: a ring, twelve marks, the quarters named, and the hands.
    Dial,
}

/// Every style, in the order the setting goes round them.
pub const STYLES: [AodStyle; 3] = [AodStyle::Digits, AodStyle::Offset, AodStyle::Dial];

impl AodStyle {
    /// The name the style is known by, as the setting shows it.
    pub fn name(self, l: Lang) -> &'static str {
        let names: [&'static str; 3] = match self {
            AodStyle::Digits => ["数字", "數字", "Digits"],
            AodStyle::Offset => ["错位", "錯位", "Offset"],
            AodStyle::Dial => ["表盘", "錶盤", "Dial"],
        };
        l.pick(names)
    }

    /// The style after this one, as the setting is tapped.
    pub fn next(self) -> Self {
        match self {
            AodStyle::Digits => AodStyle::Offset,
            AodStyle::Offset => AodStyle::Dial,
            AodStyle::Dial => AodStyle::Digits,
        }
    }
}

/// What the face says beside the time: the readings a person glancing at the
/// phone wants, and nothing that has to be read closely.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// The date, as the phone's locale writes it, such as `2026年11月7日`.
    pub date: String,
    /// The battery's level, as a share of a full charge, and whether it is
    /// charging.
    pub battery: Option<(u8, bool)>,
    /// The slot the recovery runs from, as its letter: `A` or `B`.
    pub slot: Option<String>,
    /// Whether the data partition is open, as the decrypt page names it.
    pub data: String,
}

impl Facts {
    /// The readings as one line, in the order they are drawn: the battery,
    /// the slot, and the data partition. A reading the phone does not give is
    /// left out, and a face with nothing to say draws no line at all.
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        if let Some((level, charging)) = self.battery {
            parts.push(if charging { format!("{level}% +") } else { format!("{level}%") });
        }
        if let Some(slot) = &self.slot {
            parts.push(slot.clone());
        }
        if !self.data.is_empty() {
            parts.push(self.data.clone());
        }
        parts.join("   ")
    }
}


/// One of the display driver's features and the value it is to be given, as
/// the phone's configuration names it: the number `disp_param` prints in the
/// brackets after the feature's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feature {
    /// The number in the brackets, read as the configuration writes it.
    pub id: u32,
    /// What is written to the node the driver names that feature by.
    pub value: String,
}

/// The panel's own doze as the phone's configuration names it: the feature
/// values written and the command sets sent, on the way in and on the way
/// out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doze {
    /// The values written as the panel goes into its doze.
    pub enter: Vec<Feature>,
    /// The command sets sent as it goes in, by the number the driver's own
    /// listing gives each.
    pub enter_cmds: Vec<u32>,
    /// The values written as it comes back out.
    pub leave: Vec<Feature>,
    /// The command sets sent as it comes back out.
    pub leave_cmds: Vec<u32>,
}

/// What the pages want of the panel just now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// The pages are lit and the panel is to show them.
    Lit,
    /// The pages have gone dark: nothing on the glass needs reading.
    Dark,
}

/// What the panel is doing, as the recovery last left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// Showing the pages.
    Lit,
    /// Put out the way the recovery has always put it out: the frame blacked
    /// and the backlight at 0.
    Blank,
    /// In the panel's own doze, with the face on the glass.
    Dozing,
}

/// What the display driver answered about itself when it was asked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Driver {
    /// The features `disp_param` lists, each with the number the driver
    /// prints in the brackets after its name, as it prints it, and the name
    /// it gives it.
    pub features: Vec<(String, String)>,
    /// Whether the node a command set's number is sent through could be
    /// opened for writing.
    pub sending: bool,
}

impl Driver {
    /// The name the driver gives feature `id`, where it lists one under it.
    ///
    /// The brackets are read the way the driver prints them, which is
    /// hexadecimal on the phones this is known for; a build that prints them
    /// as decimal numbers is read as well, rather than quietly skipped.
    pub fn name(&self, id: u32) -> Option<&str> {
        self.features.iter().find(|(n, _)| reads_as(n, id)).map(|(_, name)| name.as_str())
    }

    /// What is written for each of `features`, in order: the node the driver
    /// names the feature by, and the value. A feature the driver does not
    /// list is left out, and so is one whose name could not be a file of its
    /// own — neither is written blind.
    fn writes(&self, features: &[Feature]) -> Vec<(String, String)> {
        features
            .iter()
            .filter_map(|f| {
                let name = self.name(f.id)?;
                let usable = !name.is_empty() && !name.contains('/') && !name.contains(char::is_whitespace);
                usable.then(|| (name.to_string(), f.value.clone()))
            })
            .collect()
    }
}

/// Whether the number a driver printed in the brackets is `id`, read either
/// way a driver of this generation has printed them.
fn reads_as(printed: &str, id: u32) -> bool {
    let printed = printed.trim();
    u32::from_str_radix(printed, 16).ok() == Some(id) || printed.parse::<u32>().ok() == Some(id)
}

/// What is to be done to the panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The recovery's own way, unchanged: the pages shown, or the frame
    /// blacked out with the backlight at 0.
    Ordinary,
    /// Into the panel's own doze: these values written first, then these
    /// command sets sent.
    Doze { writes: Vec<(String, String)>, cmds: Vec<u32> },
    /// Out of the doze and back to the pages.
    Wake { writes: Vec<(String, String)>, cmds: Vec<u32> },
}

/// Why the panel is left the recovery's own way where its doze was wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The configuration names no command set to put the panel into its doze
    /// with, so there would be nothing to send.
    NothingToSend,
    /// The display driver was not reached: the phone has none, or its folder
    /// is not where the recovery can open it.
    NoDriver,
    /// The driver offers no node that sends a command set to the panel.
    NotSending,
}

impl Refusal {
    /// The words that go to the kernel log, once, where a dark panel is put
    /// out the ordinary way instead of dozing.
    pub fn why(self) -> &'static str {
        match self {
            Refusal::NothingToSend => {
                "the configuration names no command set that puts the panel into its own doze, so the panel is put out the ordinary way"
            }
            Refusal::NoDriver => "the display driver says nothing of itself, so the panel is put out the ordinary way",
            Refusal::NotSending => "the display driver sends no command set to the panel, so it is put out the ordinary way",
        }
    }
}

/// What the panel is to be told, and what it is doing once it has been.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub plan: Plan,
    /// What the panel is doing once the plan is carried out. A step that the
    /// panel refuses leaves the recovery to set this itself.
    pub shown: Shown,
    /// Why the panel's own doze is not used, where it was wanted and not
    /// given.
    pub refusal: Option<Refusal>,
}

impl Doze {
    /// What the panel is to be told, given what it is doing now, what the
    /// pages want of it, and what the display driver answered.
    ///
    /// This is the whole of the decision, with nothing of the phone in it, so
    /// that a computer can test every way it goes: the caller only carries it
    /// out. Where the panel's doze cannot be had, the answer is the recovery
    /// as it was before any of this — the ordinary blanking — and the reason
    /// for a log line.
    pub fn plan(&self, shown: Shown, want: Want, driver: Option<&Driver>) -> Decision {
        match (shown, want) {
            // The pages came back: out of the doze, and the page over it.
            (Shown::Dozing, Want::Lit) => self.leave(driver),
            // The pages went dark: into the doze, where it can be had.
            (Shown::Lit, Want::Dark) => self.enter(driver),
            // Already as it should be, and nothing to tell the panel.
            (Shown::Lit, Want::Lit) | (Shown::Blank, Want::Dark) | (Shown::Dozing, Want::Dark) => {
                Decision { plan: Plan::Ordinary, shown, refusal: None }
            }
            (Shown::Blank, Want::Lit) => Decision { plan: Plan::Ordinary, shown: Shown::Lit, refusal: None },
        }
    }

    /// Into the panel's own doze, where there is a command set to send and the
    /// driver takes one. Anything short of that is the ordinary blanking.
    fn enter(&self, driver: Option<&Driver>) -> Decision {
        let ordinary = |refusal| Decision { plan: Plan::Ordinary, shown: Shown::Blank, refusal: Some(refusal) };
        let Some(driver) = driver else { return ordinary(Refusal::NoDriver) };
        if !driver.sending {
            return ordinary(Refusal::NotSending);
        }
        // A doze with nothing sent is no doze at all: the panel would stay
        // black with the backlight at 0, which is the ordinary blanking with
        // more ways to fail.
        if self.enter_cmds.is_empty() {
            return ordinary(Refusal::NothingToSend);
        }
        let plan = Plan::Doze { writes: driver.writes(&self.enter), cmds: self.enter_cmds.clone() };
        Decision { plan, shown: Shown::Dozing, refusal: None }
    }

    /// Out of the panel's own doze and back to the pages. A panel that cannot
    /// be brought out is given up on for the rest of the run rather than sent
    /// another command set every frame.
    fn leave(&self, driver: Option<&Driver>) -> Decision {
        let ordinary = |refusal| Decision { plan: Plan::Ordinary, shown: Shown::Lit, refusal: Some(refusal) };
        let Some(driver) = driver else { return ordinary(Refusal::NoDriver) };
        if !driver.sending {
            return ordinary(Refusal::NotSending);
        }
        if self.leave_cmds.is_empty() {
            return ordinary(Refusal::NothingToSend);
        }
        let plan = Plan::Wake { writes: driver.writes(&self.leave), cmds: self.leave_cmds.clone() };
        Decision { plan, shown: Shown::Lit, refusal: None }
    }
}

/// Draw the face the panel keeps on the glass while it dozes, in the style
/// the setting is on: the date, the time, and the readings a glance wants, on
/// black.
///
/// The frame is what glows, so everything but what is drawn here is black: a
/// page's worth of light would burn the panel and drain the battery with the
/// backlight never coming on. The face stands a little to one side or the
/// other of the middle, a different place each minute, so that the same
/// subpixels are not lit for hours on end, and whatever the style draws, it
/// is in the inks that phone shows as they were drawn (see [`INK`]).
pub fn face(out: &mut Canvas, f: &Fonts, clock: &str, facts: &Facts, style: AodStyle, px_per_dp: f32) {
    out.clear(BLACK);
    let (w, h) = (out.w as f32, out.h as f32);
    let cx = w / 2.0 + drift(clock, px_per_dp);
    match style {
        AodStyle::Digits => digits(out, f, clock, facts, cx, h, px_per_dp),
        AodStyle::Offset => offset(out, f, clock, facts, cx, h, px_per_dp),
        AodStyle::Dial => dial(out, f, clock, facts, cx, h, px_per_dp),
    }
}

/// The face the recovery has always drawn: the date above, the clock large in
/// the middle, a small mark under it, and the readings under that.
fn digits(out: &mut Canvas, f: &Fonts, clock: &str, facts: &Facts, cx: f32, h: f32, px_per_dp: f32) {
    let cy = h * CLOCK_AT;
    if !facts.date.is_empty() {
        out.text_center(&f.body, &facts.date, cx, cy - DATE_RISE_DP * px_per_dp, DATE_DP * px_per_dp, MARK);
    }
    out.text_center(&f.display, clock, cx, cy, CLOCK_DP * px_per_dp, INK);
    let (wide, thick) = (MARK_WIDE_DP * px_per_dp, (MARK_THICK_DP * px_per_dp).max(1.0));
    out.fill_rect(cx - wide / 2.0, cy + MARK_DROP_DP * px_per_dp, wide, thick, MARK);
    let line = facts.line();
    if !line.is_empty() {
        out.text_center(&f.body, &line, cx, cy + FACTS_DROP_DP * px_per_dp, FACTS_DP * px_per_dp, MARK);
    }
}

/// The hour and the minute set askew, each as large as the screen has room
/// for: the hour up and to one side, the minute down and to the other, the
/// date above the hour and the readings below the minute. The parts of a
/// clock that is no `HH:MM` are drawn as they come, and a face with no clock
/// at all draws only the date and the readings.
fn offset(out: &mut Canvas, f: &Fonts, clock: &str, facts: &Facts, cx: f32, h: f32, px_per_dp: f32) {
    let (hour, minute) = clock.split_once(':').unwrap_or((clock, ""));
    let (hour_x, hour_y) = (cx - OFFSET_SKEW_DP * px_per_dp, h * OFFSET_AT);
    let (minute_x, minute_y) = (cx + OFFSET_SKEW_DP * px_per_dp, hour_y + OFFSET_DROP_DP * px_per_dp);
    if !facts.date.is_empty() {
        out.text_center(&f.body, &facts.date, hour_x, hour_y - OFFSET_DATE_RISE_DP * px_per_dp, DATE_DP * px_per_dp, MARK);
    }
    out.text_center(&f.title, hour, hour_x, hour_y, OFFSET_DP * px_per_dp, INK);
    out.text_center(&f.title, minute, minute_x, minute_y, OFFSET_DP * px_per_dp, INK);
    let line = facts.line();
    if !line.is_empty() {
        out.text_center(&f.body, &line, minute_x, minute_y + OFFSET_FACTS_DROP_DP * px_per_dp, FACTS_DP * px_per_dp, MARK);
    }
}

/// A clock face: a ring with twelve marks about it, the quarters named, and
/// the hands where the time says, the date above the ring and the readings
/// below it. A clock that reads as no time draws no ring and no hands — the
/// date and the readings the phone gives are still drawn.
fn dial(out: &mut Canvas, f: &Fonts, clock: &str, facts: &Facts, cx: f32, h: f32, px_per_dp: f32) {
    let (cy, r) = (h * DIAL_AT, DIAL_RADIUS_DP * px_per_dp);
    if let Some((hour, minute)) = hands(clock) {
        // The rim is a disc of ink with the black taken back out of it, so
        // that only the ring is left lit.
        out.fill_circle(cx, cy, r, MARK);
        out.fill_circle(cx, cy, (r - DIAL_RING_DP * px_per_dp).max(0.0), BLACK);
        for i in 0..12u32 {
            let a = i as f32 * std::f32::consts::FRAC_PI_6;
            let (sx, sy) = (a.sin(), -a.cos());
            let (qx, qy) = (-sy, sx);
            let quarter = i % 3 == 0;
            let (reach, half) = if quarter { (DIAL_QUARTER_IN_DP, DIAL_QUARTER_HALF_DP) } else { (DIAL_MARK_IN_DP, DIAL_MARK_HALF_DP) };
            let (r0, r1, half) = (r - reach * px_per_dp, r - DIAL_MARK_OUT_DP * px_per_dp, half * px_per_dp);
            out.fill_convex(
                &[
                    (cx + sx * r0 + qx * half, cy + sy * r0 + qy * half),
                    (cx + sx * r1 + qx * half, cy + sy * r1 + qy * half),
                    (cx + sx * r1 - qx * half, cy + sy * r1 - qy * half),
                    (cx + sx * r0 - qx * half, cy + sy * r0 - qy * half),
                ],
                INK,
            );
            if quarter {
                let numeral = match i {
                    0 => "12",
                    3 => "3",
                    6 => "6",
                    _ => "9",
                };
                out.text_center(&f.heading, numeral, cx + sx * r * DIAL_NUMERAL_AT, cy + sy * r * DIAL_NUMERAL_AT, DIAL_NUMERAL_DP * px_per_dp, INK);
            }
        }
        let tau = std::f32::consts::TAU;
        let mut hand = |angle: f32, len: f32, base: f32, tip: f32| {
            let (sx, sy) = (angle.sin(), -angle.cos());
            let (qx, qy) = (-sy, sx);
            out.fill_convex(
                &[
                    (cx - qx * base, cy - qy * base),
                    (cx + sx * len + qx * tip, cy + sy * len + qy * tip),
                    (cx + sx * len - qx * tip, cy + sy * len - qy * tip),
                    (cx + qx * base, cy + qy * base),
                ],
                INK,
            );
        };
        hand(hour.rem_euclid(12.0) / 12.0 * tau + minute / 60.0 * (tau / 12.0), r * DIAL_HOUR_LEN, DIAL_HOUR_BASE_DP * px_per_dp, DIAL_HOUR_TIP_DP * px_per_dp);
        hand(minute / 60.0 * tau, r * DIAL_MINUTE_LEN, DIAL_MINUTE_BASE_DP * px_per_dp, DIAL_MINUTE_TIP_DP * px_per_dp);
        out.fill_circle(cx, cy, DIAL_PIN_DP * px_per_dp, INK);
        out.fill_circle(cx, cy, DIAL_PIN_CORE_DP * px_per_dp, BLACK);
    }
    if !facts.date.is_empty() {
        out.text_center(&f.body, &facts.date, cx, cy - r - DIAL_DATE_RISE_DP * px_per_dp, DATE_DP * px_per_dp, MARK);
    }
    let line = facts.line();
    if !line.is_empty() {
        out.text_center(&f.body, &line, cx, h * DIAL_FACTS_AT, FACTS_DP * px_per_dp, MARK);
    }
}

/// The hour and the minute `clock` reads as, where it reads as `HH:MM`: what
/// the dial's hands point at. A clock that is no time — the empty one a phone
/// whose RTC never set gives, or words — has no hands, and the dial draws
/// neither them nor its ring.
fn hands(clock: &str) -> Option<(f32, f32)> {
    let (hour, minute) = clock.split_once(':')?;
    // A field that is no number at all, or no number of an hour or a minute,
    // has no hand to point with; `99:99` is read, and the hands go where its
    // arithmetic puts them rather than failing.
    let hour: f32 = hour.trim().parse().ok()?;
    let minute: f32 = minute.trim().parse().ok()?;
    (hour.is_finite() && minute.is_finite()).then_some((hour, minute))
}

/// How far, in pixels, the face stands from the middle of the screen for the
/// minute it shows: five places, a step apart, so that no subpixel of it is
/// lit for more than a few minutes together.
fn drift(clock: &str, px_per_dp: f32) -> f32 {
    const STEPS: [f32; 5] = [-16.0, -8.0, 0.0, 8.0, 16.0];
    let minute = clock.rsplit(':').next().and_then(|m| m.trim().parse::<usize>().ok());
    minute.map_or(0.0, |m| STEPS[m % STEPS.len()] * px_per_dp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver as `haotian`'s says itself to be: the features it lists, and
    /// a node that takes a command set.
    fn haotian() -> Driver {
        let features = [
            ("00", "dimming"),
            ("01", "hbm"),
            ("03", "doze_brightness"),
            ("06", "flat_mode"),
            ("36", "apl_dimming"),
        ];
        Driver { features: features.iter().map(|(n, name)| (n.to_string(), name.to_string())).collect(), sending: true }
    }

    /// The doze `haotian`'s configuration names: the low-brightness doze in,
    /// and the switch out of the low-power mode.
    fn doze() -> Doze {
        Doze { enter: Vec::new(), enter_cmds: vec![45], leave: Vec::new(), leave_cmds: vec![18] }
    }

    #[test]
    fn a_dark_panel_goes_into_its_own_doze_when_the_phone_names_one() {
        let d = doze().plan(Shown::Lit, Want::Dark, Some(&haotian()));
        assert_eq!(d, Decision { plan: Plan::Doze { writes: vec![], cmds: vec![45] }, shown: Shown::Dozing, refusal: None });
        // And coming back out is the command set the way out names.
        let d = doze().plan(Shown::Dozing, Want::Lit, Some(&haotian()));
        assert_eq!(d, Decision { plan: Plan::Wake { writes: vec![], cmds: vec![18] }, shown: Shown::Lit, refusal: None });
    }

    #[test]
    fn a_panel_with_no_driver_of_its_own_is_put_out_the_ordinary_way() {
        // The folder is not there, or nothing could be opened: the panel goes
        // out as it always did, and the reason is one line of the log.
        let d = doze().plan(Shown::Lit, Want::Dark, None);
        assert_eq!(d.plan, Plan::Ordinary, "no command set is sent to nothing");
        assert_eq!(d.shown, Shown::Blank, "the frame is blacked out and the backlight put to 0");
        assert_eq!(d.refusal, Some(Refusal::NoDriver));
        assert!(!Refusal::NoDriver.why().is_empty());
    }

    #[test]
    fn a_driver_with_no_command_set_node_is_put_out_the_ordinary_way() {
        let mut driver = haotian();
        driver.sending = false;
        let d = doze().plan(Shown::Lit, Want::Dark, Some(&driver));
        assert_eq!((d.plan, d.shown, d.refusal), (Plan::Ordinary, Shown::Blank, Some(Refusal::NotSending)));
    }

    #[test]
    fn a_configuration_naming_no_command_set_sends_nothing_and_falls_back() {
        // The nodes are all there and the driver answers, but there is no
        // set to send: a doze that sends nothing would leave the panel black
        // with the backlight at 0, which is the blanking with more ways to
        // fail.
        let empty = Doze { leave_cmds: vec![18], ..Doze::default() };
        let d = empty.plan(Shown::Lit, Want::Dark, Some(&haotian()));
        assert_eq!((d.plan, d.shown, d.refusal), (Plan::Ordinary, Shown::Blank, Some(Refusal::NothingToSend)));
        let empty = Doze { enter_cmds: vec![45], ..Doze::default() };
        let d = empty.plan(Shown::Dozing, Want::Lit, Some(&haotian()));
        assert_eq!((d.plan, d.shown, d.refusal), (Plan::Ordinary, Shown::Lit, Some(Refusal::NothingToSend)));
    }

    #[test]
    fn a_panel_already_as_the_pages_want_is_told_nothing() {
        // Nothing changes on a page left lit, and nothing changes on a panel
        // already dark: no write is made on every frame.
        for (shown, want) in [(Shown::Lit, Want::Lit), (Shown::Blank, Want::Dark), (Shown::Dozing, Want::Dark)] {
            let d = doze().plan(shown, want, Some(&haotian()));
            assert_eq!(d, Decision { plan: Plan::Ordinary, shown, refusal: None }, "{shown:?} {want:?}");
        }
        // A blanked panel lit again is only the page over it.
        let d = doze().plan(Shown::Blank, Want::Lit, Some(&haotian()));
        assert_eq!(d, Decision { plan: Plan::Ordinary, shown: Shown::Lit, refusal: None });
    }

    #[test]
    fn a_feature_is_written_only_under_the_name_the_driver_gives_it() {
        let doze = Doze {
            enter: vec![Feature { id: 3, value: "1".into() }, Feature { id: 41, value: "1".into() }],
            enter_cmds: vec![45],
            ..Doze::default()
        };
        let d = doze.plan(Shown::Lit, Want::Dark, Some(&haotian()));
        let Plan::Doze { writes, cmds } = d.plan else { panic!("the doze is wanted") };
        assert_eq!(cmds, vec![45]);
        assert_eq!(writes, vec![("doze_brightness".to_string(), "1".to_string())], "the feature the driver does not list is left alone");
    }

    #[test]
    fn the_numbers_the_driver_prints_are_read_as_it_prints_them() {
        let driver = haotian();
        // `disp_param` prints `doze_brightness[03]` and `apl_dimming[36]`.
        assert_eq!(driver.name(3), Some("doze_brightness"));
        assert_eq!(driver.name(0x36), Some("apl_dimming"), "the brackets are hexadecimal, as the driver prints them");
        assert_eq!(driver.name(41), None, "a feature this driver does not offer");
        // A build that prints decimal numbers is read too, rather than
        // silently skipped: the same node answers either way.
        let decimal = Driver { features: vec![("36".into(), "apl_dimming".into())], sending: true };
        assert_eq!(decimal.name(36), Some("apl_dimming"), "read as decimal where it is not a hexadecimal number of the driver's");
    }

    #[test]
    fn a_feature_named_as_no_file_of_its_own_is_not_written() {
        let driver = Driver { features: vec![("03".into(), "doze/brightness".into())], sending: true };
        let doze = Doze { enter: vec![Feature { id: 3, value: "1".into() }], enter_cmds: vec![45], ..Doze::default() };
        let Plan::Doze { writes, .. } = doze.plan(Shown::Lit, Want::Dark, Some(&driver)).plan else { panic!("the doze is wanted") };
        assert!(writes.is_empty(), "a name that is no file of the driver's folder is not written blind");
    }

    /// A panel the size of `haotian`'s, at its density.
    fn screen() -> (Canvas, Fonts) {
        (Canvas::filled(1440, 3200, Rgba::rgb(9, 9, 9)), crate::font::bundled())
    }

    /// The readings a face on `haotian` is given: a date, a battery at 48%
    /// that is charging, slot A, and the data partition still closed.
    fn facts() -> Facts {
        Facts { date: "2026年11月7日".into(), battery: Some((48, true)), slot: Some("A".into()), data: "未解密".into() }
    }

    /// How many pixels of `out` are lit at all, as a face with the backlight
    /// out is the only light on the glass.
    fn lit(out: &Canvas) -> usize {
        out.data.as_chunks::<4>().0.iter().filter(|p| p[0] > 0).count()
    }

    #[test]
    fn the_face_lights_a_clock_and_its_readings_on_black_and_nothing_else() {
        let (mut out, f) = screen();
        face(&mut out, &f, "03:27", &facts(), AodStyle::Digits, 3.75);
        let on = lit(&out);
        assert!(on > 0, "the clock is on the glass");
        // A page's worth of light would burn the panel: a few thousand
        // pixels out of four and a half million is all the face lights.
        assert!(on < 90_000, "{on} pixels lit of {}", out.data.len() / 4);
        // Everything that is not the face is black; its edges are
        // anti-aliased, so a little more than its ink is not.
        let black = out.data.as_chunks::<4>().0.iter().filter(|p| p[..3] == [0, 0, 0]).count();
        assert!(black > out.data.len() / 4 * 97 / 100, "{} of {} pixels are black", black, out.data.len() / 4);
    }

    #[test]
    fn the_face_shows_the_readings_the_phone_gives_and_leaves_out_the_rest() {
        // A whole line: the battery, its charging, the slot and the data
        // partition, each where a glance finds it.
        assert_eq!(facts().line(), "48% +   A   未解密");
        assert_eq!(Facts { battery: Some((48, false)), ..facts() }.line(), "48%   A   未解密");
        // A phone that gives none of them draws no line at all, and the face
        // is still the clock over its mark.
        let bare = Facts::default();
        assert_eq!(bare.line(), "");
        for style in STYLES {
            let (mut out, f) = screen();
            face(&mut out, &f, "03:27", &bare, style, 3.75);
            assert!(lit(&out) > 0, "{style:?} still has the clock on the glass");
        }
    }

    #[test]
    fn the_face_lights_no_channel_the_glass_would_mangle() {
        // `haotian`'s glass shows the buffer's blue byte as both red and blue
        // (see AGENT.md), so a face whose lit pixels all have red equal to
        // blue is a face that phone shows as it was drawn. Everything on it is
        // grey or white for that reason, whichever face the setting is on.
        let f = crate::font::bundled();
        for style in STYLES {
            let mut out = Canvas::filled(1440, 3200, Rgba::rgb(9, 9, 9));
            face(&mut out, &f, "03:27", &facts(), style, 3.75);
            let wrong: Vec<[u8; 4]> = out.data.as_chunks::<4>().0.iter().copied().filter(|p| p[0] != p[2]).collect();
            assert!(wrong.is_empty(), "{style:?}: {} pixels depend on a channel the glass mangles: {:?}", wrong.len(), wrong.first());
        }
    }

    #[test]
    fn the_face_moves_with_the_minute_so_no_pixel_is_lit_for_long() {
        let (mut one, f) = screen();
        let mut two = one.clone();
        face(&mut one, &f, "03:20", &facts(), AodStyle::Digits, 3.75);
        face(&mut two, &f, "03:21", &facts(), AodStyle::Digits, 3.75);
        assert_ne!(one.data, two.data, "the next minute's face stands somewhere else");
        // And it stays on the glass, whatever minute it is.
        for minute in 0..60 {
            let mut out = one.clone();
            face(&mut out, &f, &format!("03:{minute:02}"), &facts(), AodStyle::Digits, 3.75);
            assert!(!out.data.as_chunks::<4>().0.iter().all(|p| p[..3] == [0, 0, 0]), "minute {minute} is on the glass");
            assert_eq!(out.w, 1440, "the face does not resize the frame");
        }
        // The other faces stand somewhere else with the minute too, so no
        // subpixel of them is lit for hours on end either.
        for style in [AodStyle::Offset, AodStyle::Dial] {
            let mut one = Canvas::filled(1440, 3200, Rgba::rgb(9, 9, 9));
            let mut two = Canvas::filled(1440, 3200, Rgba::rgb(9, 9, 9));
            face(&mut one, &f, "03:20", &facts(), style, 3.75);
            face(&mut two, &f, "03:21", &facts(), style, 3.75);
            assert_ne!(one.data, two.data, "{style:?} stands somewhere else the next minute");
        }
        // The odd clock a phone whose RTC never set gives, without a minute,
        // stands in the middle rather than panicking.
        let mut out = one.clone();
        face(&mut out, &f, "", &facts(), AodStyle::Digits, 3.75);
        assert_eq!(drift("", 3.75), 3.75 * 0.0);
    }

    #[test]
    fn the_faces_go_round_in_the_order_the_setting_names() {
        assert_eq!(STYLES, [AodStyle::Digits, AodStyle::Offset, AodStyle::Dial]);
        // A phone that never opens the setting keeps the face it always had.
        assert_eq!(AodStyle::default(), AodStyle::Digits);
        let mut style = AodStyle::default();
        let mut went = Vec::new();
        for _ in 0..STYLES.len() {
            went.push(style);
            style = style.next();
        }
        assert_eq!(went, STYLES.to_vec(), "each tap is the next face round");
        assert_eq!(style, AodStyle::Digits, "and round again to the one it started on");
    }

    #[test]
    fn every_language_names_the_three_faces_apart() {
        for lang in crate::Lang::ALL {
            let names: Vec<&str> = STYLES.iter().map(|s| s.name(lang)).collect();
            assert!(names.iter().all(|n| !n.is_empty()), "{lang:?} leaves no face unnamed: {names:?}");
            for (i, name) in names.iter().enumerate() {
                assert!(!name.contains(char::is_whitespace), "{name:?} is one word, as the setting shows it");
                for other in &names[i + 1..] {
                    assert_ne!(name, other, "{lang:?} tells the faces apart");
                }
            }
        }
        assert_eq!(STYLES.map(|s| s.name(crate::Lang::Hans)), ["数字", "错位", "表盘"]);
        assert_eq!(STYLES.map(|s| s.name(crate::Lang::Hant)), ["數字", "錯位", "錶盤"]);
        assert_eq!(STYLES.map(|s| s.name(crate::Lang::En)), ["Digits", "Offset", "Dial"]);
    }

    #[test]
    fn the_three_faces_are_three_different_faces_of_the_same_clock() {
        let f = crate::font::bundled();
        let mut drawn: Vec<Canvas> = Vec::new();
        for style in STYLES {
            let mut out = Canvas::filled(1440, 3200, Rgba::rgb(9, 9, 9));
            face(&mut out, &f, "10:08", &facts(), style, 3.75);
            drawn.push(out);
        }
        for (i, one) in drawn.iter().enumerate() {
            for (j, other) in drawn.iter().enumerate().skip(i + 1) {
                assert_ne!(one.data, other.data, "{:?} and {:?} are drawn differently", STYLES[i], STYLES[j]);
            }
        }
    }

    #[test]
    fn a_dial_with_no_time_on_it_still_draws_what_the_phone_gives() {
        // The clock a phone whose RTC never set gives, and the odd ones a
        // clock that is no `HH:MM` gives: the dial draws no ring and no hands
        // to point, but the date and the readings the phone does have are
        // still on the glass, and nothing panics.
        let f = crate::font::bundled();
        for clock in ["", "nope", "99:99", ":", "12:", ":45", "-1:-1", "03:27:11"] {
            let mut out = Canvas::filled(1440, 3200, Rgba::rgb(9, 9, 9));
            face(&mut out, &f, clock, &facts(), AodStyle::Dial, 3.75);
            assert!(lit(&out) > 0, "{clock:?} still shows the date and the readings");
        }
        // A phone that gives neither a time nor a reading lights nothing at
        // all rather than a ring about a clock it does not know.
        let mut bare = Canvas::filled(1440, 3200, Rgba::rgb(9, 9, 9));
        face(&mut bare, &f, "", &Facts::default(), AodStyle::Dial, 3.75);
        assert_eq!(lit(&bare), 0, "no time, no date and no readings light nothing");
    }

    #[test]
    fn every_face_keeps_the_glass_to_a_small_part_of_its_light() {
        // The face is what glows with the backlight out, so it must stay a
        // small part of the glass: a page's worth of light would burn the
        // panel and drain the battery through the night. The digits and the
        // dial light a few tens of thousands of pixels of the four and a half
        // million; the offset face lights the most, its two fields being the
        // largest thing any page of the recovery draws, and is kept to one
        // part in twenty of the glass.
        let f = crate::font::bundled();
        let glass = 1440 * 3200;
        for (style, ceiling) in [(AodStyle::Digits, 90_000), (AodStyle::Dial, 90_000), (AodStyle::Offset, glass / 20)] {
            let mut out = Canvas::filled(1440, 3200, Rgba::rgb(9, 9, 9));
            face(&mut out, &f, "03:27", &facts(), style, 3.75);
            let on = lit(&out);
            assert!(on < ceiling, "{style:?} lights {on} pixels, more than the {ceiling} that face is kept to");
        }
    }
}
