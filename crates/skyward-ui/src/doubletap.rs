//! The double tap that lights a dark panel.
//!
//! A tap is a press let go quickly without wandering; two of them within
//! Android's window and near one another are a double tap. The state is
//! pure so the gesture can be tested without a panel.

/// How long a finger may rest before a tap is no longer a tap.
pub const TAP_MS: u64 = 320;
/// Android's double-tap window, the same guard the UI keeps after waking.
pub const DOUBLE_TAP_MS: u64 = 400;
/// How far apart two taps may land and still be a double tap, in dp.
pub const DOUBLE_TAP_SLOP_DP: f32 = 110.0;
/// How far a finger may wander before a tap is no longer a tap, in dp.
pub const TAP_SLOP_DP: f32 = 24.0;

#[derive(Clone, Debug, PartialEq)]
pub struct DoubleTap {
    down: Option<(u64, f32, f32)>,
    stray: bool,
    last: Option<(u64, f32, f32)>,
    /// How far apart two taps may land, in pixels; set from the panel's dp.
    slop: f32,
}

impl Default for DoubleTap {
    fn default() -> Self {
        Self { down: None, stray: false, last: None, slop: DOUBLE_TAP_SLOP_DP * 3.0 }
    }
}

impl DoubleTap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the distance the two taps may be apart, in pixels.
    pub fn set_slop(&mut self, px: f32) {
        self.slop = px;
    }

    pub fn down(&mut self, x: f32, y: f32, now: u64) {
        self.expire(now);
        self.down = Some((now, x, y));
        self.stray = false;
    }

    pub fn move_to(&mut self, x: f32, y: f32, slop: f32) {
        if let Some((_, x0, y0)) = self.down
            && (x - x0).hypot(y - y0) > slop {
                self.stray = true;
        }
    }

    /// A finger came up: true when this was the second tap of a double tap.
    /// A tap that strayed or was held too long neither counts nor is kept.
    pub fn up(&mut self, now: u64) -> bool {
        let Some((at, x, y)) = self.down.take() else { return false };
        if self.stray || now.saturating_sub(at) > TAP_MS {
            self.last = None;
            return false;
        }
        let twin = self.last.is_some_and(|(t, px, py)| {
            now.saturating_sub(t) <= DOUBLE_TAP_MS && (x - px).hypot(y - py) <= self.slop
        });
        self.last = if twin { None } else { Some((now, x, y)) };
        twin
    }

    /// Forget a first tap that can no longer be paired.
    pub fn expire(&mut self, now: u64) {
        if self.last.is_some_and(|(t, _, _)| now.saturating_sub(t) > DOUBLE_TAP_MS) {
            self.last = None;
        }
    }

    /// Drop the gesture in progress, keeping the panel's distance.
    pub fn reset(&mut self) {
        self.down = None;
        self.stray = false;
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taps() -> DoubleTap {
        let mut d = DoubleTap::new();
        d.set_slop(300.0);
        d
    }

    #[test]
    fn two_quick_neighbouring_taps_wake_the_panel() {
        let mut d = taps();
        d.down(100.0, 100.0, 0);
        assert!(!d.up(100));
        d.down(110.0, 105.0, 200);
        assert!(d.up(300));
    }

    #[test]
    fn a_second_tap_outside_the_window_does_not_wake() {
        let mut d = taps();
        d.down(100.0, 100.0, 0);
        assert!(!d.up(100));
        d.down(110.0, 105.0, 900);
        assert!(!d.up(1_000));
    }

    #[test]
    fn taps_far_apart_do_not_pair() {
        let mut d = DoubleTap::new();
        d.set_slop(100.0);
        d.down(100.0, 100.0, 0);
        assert!(!d.up(100));
        d.down(600.0, 100.0, 200);
        assert!(!d.up(300));
    }

    #[test]
    fn a_drag_or_a_long_press_is_not_a_tap() {
        let mut d = taps();
        d.down(100.0, 100.0, 0);
        d.move_to(400.0, 100.0, 24.0);
        assert!(!d.up(100));
        d.down(100.0, 100.0, 200);
        assert!(!d.up(900), "a held finger is not a tap");
    }

    #[test]
    fn a_first_tap_expires_and_a_third_starts_a_new_pair() {
        let mut d = taps();
        d.down(100.0, 100.0, 0);
        assert!(!d.up(100));
        d.down(100.0, 100.0, 1_000);
        assert!(!d.up(1_100), "the first tap is too old");
        d.down(100.0, 100.0, 1_300);
        assert!(d.up(1_400));
    }

    #[test]
    fn reset_drops_the_gesture_in_progress() {
        let mut d = taps();
        d.down(100.0, 100.0, 0);
        assert!(!d.up(100));
        d.reset();
        d.down(100.0, 100.0, 200);
        assert!(!d.up(300));
    }
}
