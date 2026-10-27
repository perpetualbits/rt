//! Pure logic for held-arrow acceleration and arrow cruise (see
//! docs/superpowers/plans/2026-10-26-arrow-cruise.md). No I/O, no winit — the
//! event loop feeds it presses, releases, stop events and timer ticks, and it
//! answers how many moves to send.
//!
//! Holding an arrow ramps the moves-per-repeat linearly with the time held
//! (measured from the first auto-repeat, so the OS repeat delay doesn't count).
//! With cruise on, releasing a ramped arrow keeps moving at the reached speed
//! until any arrow press (swallowed), a [`ArrowCruise::stop`] event, or the
//! [`CRUISE_MAX`] limit.

use std::time::{Duration, Instant};

/// Time held per extra move per repeat.
pub const RAMP_INTERVAL: Duration = Duration::from_millis(250);
/// A cruise ends on its own after this long, so a forgotten one can't run forever.
pub const CRUISE_MAX: Duration = Duration::from_secs(10);
/// Cruise tick when no repeat interval was observed (~30/s, a typical OS repeat rate).
/// Defensive: reaching speed > 1 takes at least two repeats, which set the interval.
pub const FALLBACK_INTERVAL: Duration = Duration::from_millis(33);
/// A press more than this after the last one starts a fresh hold even without a
/// release in between — guards against a release lost to a focus change. Must
/// exceed the OS repeat delay (commonly 500-600ms), or the first auto-repeat
/// looks like a fresh press and the hold restarts one repeat late.
pub const HOLD_GAP: Duration = Duration::from_millis(1000);

/// Moves per repeat after holding for `held` (since the first repeat), capped at
/// `max`. `max <= 1` disables acceleration.
pub fn ramp(held: Duration, max: u32) -> usize {
    let max = max.max(1) as usize;
    let steps = (held.as_nanos() / RAMP_INTERVAL.as_nanos()) as usize;
    (1 + steps).min(max)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Holding {
        dir: u8,
        /// When the first auto-repeat arrived; `None` until then.
        ramp_start: Option<Instant>,
        last: Instant,
        /// Learned from consecutive repeats; `None` until two repeats arrived.
        interval: Option<Duration>,
        speed: usize,
    },
    Cruising {
        dir: u8,
        speed: usize,
        interval: Duration,
        next: Instant,
        until: Instant,
    },
}

/// Held-arrow / cruise state for one window.
#[derive(Clone, Copy, Debug)]
pub struct ArrowCruise {
    phase: Phase,
}

impl Default for ArrowCruise {
    fn default() -> Self {
        Self { phase: Phase::Idle }
    }
}

impl ArrowCruise {
    /// An arrow press or auto-repeat in direction `dir`. Returns the moves to
    /// send now; 0 means the press braked a cruise and must be swallowed.
    pub fn press(&mut self, dir: u8, now: Instant, max: u32) -> usize {
        match self.phase {
            Phase::Cruising { .. } => {
                self.phase = Phase::Idle;
                0
            }
            Phase::Holding { dir: d, ramp_start, last, interval, .. }
                if d == dir && now.duration_since(last) < HOLD_GAP =>
            {
                // An auto-repeat of the held arrow.
                let (start, interval) = match ramp_start {
                    None => (now, interval), // first repeat: the gap was the OS delay
                    Some(s) => (s, Some(now.duration_since(last))),
                };
                let speed = ramp(now.duration_since(start), max);
                self.phase = Phase::Holding { dir, ramp_start: Some(start), last: now, interval, speed };
                speed
            }
            _ => {
                // Fresh press: from idle, a different arrow, or after a gap.
                self.phase = Phase::Holding { dir, ramp_start: None, last: now, interval: None, speed: 1 };
                1
            }
        }
    }

    /// The arrow `dir` was released. Enters cruise iff `cruise_on` and the hold
    /// had ramped past one move; otherwise goes idle. A release of an arrow
    /// that isn't the held one is ignored.
    pub fn release(&mut self, dir: u8, now: Instant, cruise_on: bool) {
        if let Phase::Holding { dir: d, interval, speed, .. } = self.phase {
            if d != dir {
                return;
            }
            self.phase = if cruise_on && speed > 1 {
                let interval = interval.unwrap_or(FALLBACK_INTERVAL);
                Phase::Cruising { dir, speed, interval, next: now + interval, until: now + CRUISE_MAX }
            } else {
                Phase::Idle
            };
        }
    }

    /// A stop event: another key, focus loss, pane/tab switch, mouse press.
    pub fn stop(&mut self) {
        self.phase = Phase::Idle;
    }

    pub fn is_cruising(&self) -> bool {
        matches!(self.phase, Phase::Cruising { .. })
    }

    /// Timer tick. `Some((dir, moves))` when a cruise step is due. One step per
    /// call — a late tick doesn't burst-catch-up.
    pub fn tick(&mut self, now: Instant) -> Option<(u8, usize)> {
        let Phase::Cruising { dir, speed, interval, next, until } = self.phase else { return None };
        if now >= until {
            self.phase = Phase::Idle;
            return None;
        }
        if now < next {
            return None;
        }
        let mut next = next + interval;
        if next <= now {
            next = now + interval;
        }
        self.phase = Phase::Cruising { dir, speed, interval, next, until };
        Some((dir, speed))
    }

    /// When the event loop must wake for the next cruise tick (or its expiry).
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Cruising { next, until, .. } => Some(next.min(until)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: u32 = 10;
    const RATE: Duration = Duration::from_millis(30);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Press at `t0`, OS delay 500ms, then repeats every `RATE` until `hold`
    /// after the first repeat. Returns (state, time of last repeat, last speed).
    fn hold(c: &mut ArrowCruise, dir: u8, t0: Instant, hold: Duration) -> (Instant, usize) {
        assert_eq!(c.press(dir, t0, MAX), 1);
        let first = t0 + ms(500);
        let mut t = first;
        let mut speed = c.press(dir, t, MAX);
        while t + RATE <= first + hold {
            t += RATE;
            speed = c.press(dir, t, MAX);
        }
        (t, speed)
    }

    #[test]
    fn ramp_is_linear_in_time_and_capped() {
        assert_eq!(ramp(ms(0), MAX), 1);
        assert_eq!(ramp(ms(249), MAX), 1);
        assert_eq!(ramp(ms(250), MAX), 2);
        assert_eq!(ramp(ms(1000), MAX), 5);
        assert_eq!(ramp(ms(60_000), MAX), 10);
        assert_eq!(ramp(ms(60_000), 1), 1, "max 1 disables acceleration");
        assert_eq!(ramp(ms(60_000), 0), 1);
    }

    #[test]
    fn tap_is_one_move_and_never_cruises() {
        let mut c = ArrowCruise::default();
        let t = Instant::now();
        assert_eq!(c.press(1, t, MAX), 1);
        c.release(1, t + ms(80), true);
        assert!(!c.is_cruising());
        assert_eq!(c.tick(t + ms(500)), None);
    }

    #[test]
    fn os_delay_does_not_count_toward_the_ramp() {
        let mut c = ArrowCruise::default();
        let t = Instant::now();
        c.press(0, t, MAX);
        assert_eq!(c.press(0, t + ms(500), MAX), 1, "first repeat after a 500ms OS delay is still one move");
        // The ramp clock started AT that first repeat: one step 250ms later.
        assert_eq!(c.press(0, t + ms(530), MAX), 1);
        assert_eq!(c.press(0, t + ms(750), MAX), 2);
    }

    #[test]
    fn holding_ramps_with_time() {
        let mut c = ArrowCruise::default();
        let (_, speed) = hold(&mut c, 0, Instant::now(), ms(1000));
        assert_eq!(speed, 4, "~990ms past the first repeat → 1 + 3");
    }

    #[test]
    fn short_hold_below_ramp_does_not_cruise() {
        let mut c = ArrowCruise::default();
        let (t, speed) = hold(&mut c, 0, Instant::now(), ms(200));
        assert_eq!(speed, 1);
        c.release(0, t + ms(10), true);
        assert!(!c.is_cruising());
    }

    #[test]
    fn release_cruises_at_reached_speed_and_learned_interval() {
        let mut c = ArrowCruise::default();
        let (t, speed) = hold(&mut c, 3, Instant::now(), ms(1000));
        let r = t + ms(10);
        c.release(3, r, true);
        assert!(c.is_cruising());
        assert_eq!(c.next_deadline(), Some(r + RATE));
        assert_eq!(c.tick(r + ms(10)), None, "not due yet");
        assert_eq!(c.tick(r + RATE), Some((3, speed)));
        assert_eq!(c.tick(r + RATE * 2), Some((3, speed)));
    }

    #[test]
    fn cruise_off_goes_idle_on_release() {
        let mut c = ArrowCruise::default();
        let (t, _) = hold(&mut c, 0, Instant::now(), ms(1000));
        c.release(0, t, false);
        assert!(!c.is_cruising());
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn any_arrow_press_brakes_and_is_swallowed() {
        for brake_dir in 0..4 {
            let mut c = ArrowCruise::default();
            let (t, _) = hold(&mut c, 1, Instant::now(), ms(1000));
            c.release(1, t, true);
            assert_eq!(c.press(brake_dir, t + ms(300), MAX), 0);
            assert!(!c.is_cruising());
            // The braking tap's own release does nothing.
            c.release(brake_dir, t + ms(350), true);
            assert!(!c.is_cruising());
            // The next press is a normal fresh press.
            assert_eq!(c.press(brake_dir, t + ms(900), MAX), 1);
        }
    }

    #[test]
    fn stop_ends_a_cruise() {
        let mut c = ArrowCruise::default();
        let (t, _) = hold(&mut c, 2, Instant::now(), ms(1000));
        c.release(2, t, true);
        c.stop();
        assert!(!c.is_cruising());
        assert_eq!(c.tick(t + ms(100)), None);
    }

    #[test]
    fn cruise_expires_after_the_limit() {
        let mut c = ArrowCruise::default();
        let (t, _) = hold(&mut c, 2, Instant::now(), ms(1000));
        c.release(2, t, true);
        assert!(c.tick(t + CRUISE_MAX - ms(1)).is_some());
        assert_eq!(c.tick(t + CRUISE_MAX), None);
        assert!(!c.is_cruising());
    }

    #[test]
    fn late_tick_does_not_burst() {
        let mut c = ArrowCruise::default();
        let (t, speed) = hold(&mut c, 0, Instant::now(), ms(1000));
        c.release(0, t, true);
        let late = t + ms(500);
        assert_eq!(c.tick(late), Some((0, speed)));
        assert_eq!(c.tick(late), None, "one step per tick, then wait an interval");
        assert_eq!(c.next_deadline(), Some(late + RATE));
    }

    #[test]
    fn direction_change_restarts_the_ramp() {
        let mut c = ArrowCruise::default();
        let (t, speed) = hold(&mut c, 0, Instant::now(), ms(1000));
        assert!(speed > 1);
        let (t, speed) = hold(&mut c, 1, t + RATE, ms(1000));
        assert_eq!(speed, 4, "the new arrow ramped from scratch");
        // A stale release of the old arrow is ignored; the new one is still held.
        c.release(0, t + ms(5), true);
        assert!(c.press(1, t + RATE, MAX) >= 4, "still ramping the held arrow");
    }

    #[test]
    fn press_after_a_gap_is_fresh_even_without_release() {
        let mut c = ArrowCruise::default();
        let (t, _) = hold(&mut c, 0, Instant::now(), ms(1000));
        // Release lost (focus change); the next press comes much later.
        assert_eq!(c.press(0, t + HOLD_GAP + ms(1), MAX), 1);
        assert_eq!(c.press(0, t + HOLD_GAP + ms(501), MAX), 1, "first repeat of the new hold");
    }
}
