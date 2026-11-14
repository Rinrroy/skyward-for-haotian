//! The power menu: four slide-to-confirm pills over a blurred page.
//!
//! It is an overlay, not a page: the page stack is never touched, so
//! fastboot, sideload and whatever the system asked for stay exactly where
//! they were when the power key was held. The four choices are the phone's
//! own power menu's, minus the silent-mode slider, and each one is a
//! `Target` the runtime already knows how to reboot into.

use crate::app::Target;
use crate::canvas::Canvas;
use crate::color::Rgba;
use crate::font::Fonts;
use crate::icons::{self, Icon};
use crate::lang::Lang;
use crate::theme::{Metrics, Theme};

pub const COUNT: usize = 4;
/// How far along its pill the knob must be let go to confirm.
pub const CONFIRM_AT: f32 = 0.72;

const RED: Rgba = Rgba::rgb(235, 59, 48);
const INK: Rgba = Rgba::rgb(38, 38, 46);
const BLUE: Rgba = Rgba::rgb(10, 122, 255);
const ORANGE: Rgba = Rgba::rgb(245, 145, 20);
const WHITE: Rgba = Rgba::rgb(255, 255, 255);
const LABEL: Rgba = Rgba { r: 255, g: 255, b: 255, a: 226 };
const HINT: Rgba = Rgba { r: 255, g: 255, b: 255, a: 150 };
const GLASS_EDGE: Rgba = Rgba { r: 255, g: 255, b: 255, a: 82 };
const SHADOW: Rgba = Rgba { r: 0, g: 0, b: 20, a: 96 };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Off,
    Reboot,
    Recovery,
    Fastboot,
}

pub const CHOICES: [Choice; COUNT] = [Choice::Off, Choice::Reboot, Choice::Recovery, Choice::Fastboot];

impl Choice {
    pub fn target(self) -> Target {
        match self {
            Choice::Off => Target::PowerOff,
            Choice::Reboot => Target::System,
            Choice::Recovery => Target::Recovery,
            Choice::Fastboot => Target::Fastboot,
        }
    }

    pub fn label(self, l: Lang) -> &'static str {
        l.pick(match self {
            Choice::Off => ["滑动来关机", "滑動來關機", "Slide to power off"],
            Choice::Reboot => ["滑动来重启", "滑動來重新啟動", "Slide to reboot"],
            Choice::Recovery => ["滑动进入 Recovery", "滑動進入 Recovery", "Slide to recovery"],
            Choice::Fastboot => ["滑动进入 Fastboot", "滑動進入 Fastboot", "Slide to fastboot"],
        })
    }

    fn ink(self) -> Rgba {
        match self {
            Choice::Off => RED,
            Choice::Reboot => INK,
            Choice::Recovery => BLUE,
            Choice::Fastboot => ORANGE,
        }
    }
}

/// One pill, in pixels. `x` and `y` are its top-left corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Row {
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }

    pub fn radius(&self) -> f32 {
        self.h / 2.0
    }

    /// Where the knob's centre rests before a finger moves it.
    pub fn rest_x(&self) -> f32 {
        self.x + self.h / 2.0
    }

    /// How far the knob's centre may travel, from rest to the far end.
    pub fn travel(&self) -> f32 {
        self.w - self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub rows: [Row; COUNT],
    pub knob_r: f32,
    pub hint_y: f32,
    pub label_px: f32,
    pub hint_px: f32,
}

/// The four pills, centred a little above the middle as the phone's own
/// menu holds them, with the hint left under them.
pub fn geometry(m: &Metrics) -> Geometry {
    let (w, h) = (m.w as f32, m.h as f32);
    let inset = m.dp(56.0);
    let row_w = w - 2.0 * inset;
    let row_h = m.dp(92.0);
    let gap = m.dp(30.0);
    let total = COUNT as f32 * row_h + (COUNT - 1) as f32 * gap;
    let top = (h - total) / 2.0 - m.dp(64.0);
    let rows = std::array::from_fn(|i| Row {
        x: inset,
        y: top + i as f32 * (row_h + gap),
        w: row_w,
        h: row_h,
    });
    Geometry {
        rows,
        knob_r: row_h * 0.365,
        hint_y: (top + total + m.dp(88.0)).min(h - m.dp(56.0)),
        label_px: m.dp(20.0),
        hint_px: m.dp(13.2),
    }
}

/// Which pill `(x, y)` lands on. Asked only for `Touch::Down`, so a press
/// on a knob stays a press even when the finger drifts.
pub fn hit(g: &Geometry, x: f32, y: f32) -> Option<usize> {
    g.rows.iter().position(|r| x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h)
}

/// How far a knob dragged to `x` has gone along pill `i`, 0 to 1.
pub fn progress(g: &Geometry, i: usize, x: f32) -> f32 {
    let r = &g.rows[i];
    ((x - r.rest_x()) / r.travel()).clamp(0.0, 1.0)
}

/// The knob a finger is holding, and how far it has carried it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drag {
    pub index: usize,
    pub progress: f32,
}

/// Tint the already-blurred page toward the deep blue the phone's own
/// power menu puts behind its glass. `fill_rect` overwrites rather than
/// blends, so this goes a pixel at a time through `shade_rows`; it is done
/// once, to the cached background, never on every frame.
pub fn tint(c: &mut Canvas) {
    let h = c.h as f32;
    c.shade_rows(0, c.h, |_, y| {
        let k = (y / h).clamp(0.0, 1.0);
        Rgba { r: 16, g: 18, b: (56.0 + 18.0 * k) as u8, a: (116.0 - 16.0 * k) as u8 }
    });
}

/// Draw the whole menu over `out`. `knob` is the pill being dragged, if any.
pub fn draw(out: &mut Canvas, t: &Theme, f: &Fonts, l: Lang, g: &Geometry, knob: Option<Drag>) {
    let face = f.face(400.0).expect("the bundled face takes the menu's weight");
    let bold = f.face(700.0).expect("the bundled face takes the menu's weight");
    let small = f.face(400.0).expect("the bundled face takes the menu's weight");
    for (i, row) in g.rows.iter().enumerate() {
        let (x, y, w, h, r) = (row.x, row.y, row.w, row.h, row.radius());
        let along = knob.filter(|d| d.index == i).map_or(0.0, |d| d.progress);
        out.shadow_round_rect(x, y + h * 0.10, w, h, r, h * 0.16, SHADOW);
        out.fill_round_rect_graded(x, y, w, h, r, &|fy| {
            let k = ((fy - y) / h).clamp(0.0, 1.0);
            Rgba { r: 234, g: 230, b: 255, a: (58.0 - 20.0 * k) as u8 }
        });
        out.stroke_round_rect(x, y, w, h, r, t.m.dp(0.6), GLASS_EDGE);
        // The label starts just past the knob and fades a little as the
        // finger carries the pill toward its end. All four start at the
        // same x, as the phone's own menu does; a long one shrinks to fit
        // rather than running under the knob.
        let label_ink = Rgba { a: (226.0 * (1.0 - 0.45 * along)) as u8, ..LABEL };
        let label = CHOICES[i].label(l);
        let text_x = row.rest_x() + g.knob_r + t.m.dp(26.0);
        let room = x + w - t.m.dp(24.0) - text_x;
        let mut px = g.label_px;
        let width = out.text_width(&face, label, px);
        if width > room {
            px = (px * room / width).max(g.label_px * 0.75);
        }
        out.text_vc(&face, label, text_x, row.cy(), px, label_ink, None);
        let kx = row.rest_x() + along * row.travel();
        out.fill_circle(kx, row.cy(), g.knob_r + t.m.dp(1.5), Rgba { r: 0, g: 0, b: 20, a: 60 });
        out.fill_circle(kx, row.cy(), g.knob_r, WHITE);
        out.stroke_circle(kx, row.cy(), g.knob_r, t.m.dp(0.5), Rgba { r: 210, g: 210, b: 225, a: 160 });
        mark(out, CHOICES[i], kx, row.cy(), g.knob_r, &bold);
    }
    let hint = l.pick([
        "滑动以确认 · 轻触空白处取消",
        "滑動以確認 · 輕觸空白處取消",
        "Slide to confirm · Tap outside to cancel",
    ]);
    out.text_center(&small, hint, t.m.w as f32 / 2.0, g.hint_y, g.hint_px, HINT);
}

/// The coloured sign inside a knob: the power glyph, a restart ring, or the
/// letter the phone's own menu puts there.
fn mark(c: &mut Canvas, choice: Choice, cx: f32, cy: f32, r: f32, bold: &crate::font::Face) {
    let ink = choice.ink();
    match choice {
        Choice::Off => icons::glyph(c, Icon::Power, cx, cy, r * 1.05, ink),
        Choice::Reboot => {
            let ring = r * 0.46;
            let start = -2.35f32;
            let sweep = 4.65f32;
            c.stroke_arc(cx, cy, ring, start, sweep, r * 0.15, ink);
            // A small head at the ring's open end, pointing along it.
            let (ex, ey) = (cx + ring * start.cos(), cy + ring * start.sin());
            let (tx, ty) = (start.sin(), -start.cos());
            let (nx, ny) = (start.cos(), start.sin());
            let a = (ex + tx * r * 0.10 + nx * r * 0.34, ey + ty * r * 0.10 + ny * r * 0.34);
            let b = (ex + tx * r * 0.10 - nx * r * 0.34, ey + ty * r * 0.10 - ny * r * 0.34);
            let tip = (ex - tx * r * 0.42, ey - ty * r * 0.42);
            c.fill_convex(&[tip, a, b], ink);
        }
        Choice::Recovery => c.text_center(bold, "R", cx, cy, r * 1.15, ink),
        Choice::Fastboot => c.text_center(bold, "F", cx, cy, r * 1.15, ink),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 1440;
    const H: u32 = 3200;
    const PPD: f32 = 3.5;

    fn metrics() -> Metrics {
        Theme::dark(W, H, PPD).m
    }

    /// Make a picture of the menu over a stand-in page, to look at the
    /// glass and the blur without a device. Ignored so an ordinary test run
    /// never needs the filesystem; run it with `--ignored` to refresh it.
    #[test]
    #[ignore]
    fn dump_preview() {
        use crate::blur;
        let t = Theme::dark(W, H, PPD);
        let f = crate::font::bundled();
        const CARDS: [Rgba; 6] = [
            Rgba::rgb(238, 242, 255),
            Rgba::rgb(84, 126, 240),
            Rgba::rgb(248, 176, 72),
            Rgba::rgb(226, 88, 96),
            Rgba::rgb(96, 208, 176),
            Rgba::rgb(196, 214, 255),
        ];
        let mut page = Canvas::filled(W, H, Rgba::rgb(22, 24, 38));
        // A grid of small bright marks, as a launcher's icons are, so the
        // blur has something to spread into soft pools of colour.
        for row in 0..14 {
            for col in 0..5 {
                let (cx, cy) = (200.0 + col as f32 * 260.0, 190.0 + row as f32 * 230.0);
                page.fill_circle(cx, cy, 82.0, CARDS[(row * 5 + col) % CARDS.len()]);
                page.fill_circle(cx, cy, 30.0, Rgba::rgb(255, 255, 255));
            }
        }
        let face = f.face(500.0).expect("the bundled face takes a weight");
        page.text_center(&face, "Skyward", W as f32 / 2.0, 300.0, 110.0, Rgba::rgb(240, 240, 255));
        let mut bg = blur::gaussian(&page);
        let pure = std::fs::File::create("/tmp/powermenu-blur.png").expect("open the blur picture");
        bg.write_png(pure).expect("write the blur picture");
        tint(&mut bg);
        let mut out = Canvas::filled(W, H, Rgba::rgb(0, 0, 0));
        out.lay(&bg, 0, 0, 1.0);
        draw(&mut out, &t, &f, Lang::Hans, &geometry(&t.m), None);
        let file = std::fs::File::create("/tmp/powermenu-preview.png").expect("open the preview");
        out.write_png(file).expect("write the preview");
    }

    #[test]
    fn the_four_pills_are_centred_and_do_not_overlap() {
        let g = geometry(&metrics());
        assert_eq!(g.rows.len(), 4);
        for pair in g.rows.windows(2) {
            assert!(pair[0].y + pair[0].h < pair[1].y, "a gap between pills");
        }
        assert!(g.rows[0].y > 0.0 && g.rows[3].y + g.rows[3].h < 3200.0);
        assert!(g.rows[0].x > 0.0 && g.rows[0].x + g.rows[0].w < 1440.0);
        assert!(g.hint_y > g.rows[3].y + g.rows[3].h);
    }

    #[test]
    fn a_press_on_a_pill_finds_it_and_one_below_them_finds_none() {
        let g = geometry(&metrics());
        let r = g.rows[2];
        assert_eq!(hit(&g, r.x + r.w * 0.5, r.cy()), Some(2));
        assert_eq!(hit(&g, r.x + r.w * 0.5, r.y - 20.0), None);
    }

    #[test]
    fn the_knob_travels_from_zero_to_one_and_no_further() {
        let g = geometry(&metrics());
        let r = g.rows[0];
        assert_eq!(progress(&g, 0, r.rest_x() - 500.0), 0.0);
        assert_eq!(progress(&g, 0, r.rest_x() + r.travel()), 1.0);
        assert_eq!(progress(&g, 0, r.rest_x() + r.travel() * 2.0), 1.0);
        let mid = progress(&g, 0, r.rest_x() + r.travel() * 0.5);
        assert!((mid - 0.5).abs() < 1e-6);
    }

    #[test]
    fn every_choice_maps_to_a_target_the_runtime_knows() {
        assert_eq!(CHOICES[0].target(), Target::PowerOff);
        assert_eq!(CHOICES[1].target(), Target::System);
        assert_eq!(CHOICES[2].target(), Target::Recovery);
        assert_eq!(CHOICES[3].target(), Target::Fastboot);
    }

    #[test]
    fn the_labels_speak_all_three_languages() {
        for choice in CHOICES {
            let a: Vec<&str> = Lang::ALL.map(|l| choice.label(l)).to_vec();
            assert!(a.iter().all(|s| !s.is_empty()));
        }
    }
}
