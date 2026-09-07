//! Multi-touch gestures, as pure state.
//!
//! Wayland hands a client raw touch points and nothing else: there is no
//! compositor-side emulation turning a finger into a pointer, and certainly
//! none turning two fingers into a scroll. winit 0.31 does the first half —
//! a finger arrives as an ordinary pointer press/move/release, which is why a
//! tap already clicks, a drag already selects, and rt needed no new code for
//! either. This module is the second half: it remembers which fingers are
//! down and where, and answers one question per touch event — does this event
//! drive the pointer, scroll the pane, or get swallowed because it belongs to
//! a gesture the pointer must not see?
//!
//! Deliberately free of winit types: a finger is the `usize` behind winit's
//! `FingerId`, so the whole gesture machine is unit-testable with no display.

/// Pixels of two-finger travel that make one scrolled line, in LOGICAL pixels.
/// Matches the pixel-delta wheel conversion in the run loop, so a touchpad flick
/// and a two-finger drag of the same distance move the scrollback equally far.
///
/// Logical, because winit reports touch and pixel-delta positions in PHYSICAL
/// pixels: the same finger travel across the same glass produces twice the
/// number on a 2x display. Multiplied by the display's backing factor via
/// [`Touch::set_scale`] (and, for the wheel, in the run loop) so a flick moves
/// the same distance whatever the display. Registered in
/// `chrome_scale::logical::PX_PER_LINE`, which pins this value; it is defined
/// here rather than there because this module is also compiled into the
/// `rt_app` library, which has no `chrome_scale`.
pub const PX_PER_LINE: f32 = 20.0;

/// What the caller should do with the touch event it just reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Treat it exactly as pointer input: one finger, so a tap is a click and
    /// a drag is a drag.
    Pointer,
    /// Part of a multi-finger gesture: the pointer must not see it.
    Swallow,
    /// A second finger just landed. Swallow this event AND undo whatever the
    /// first finger's press started — the half-drawn selection, the armed pane
    /// drag — because the gesture turned out to be a scroll, not a click.
    CancelPointer,
    /// A two-finger drag: scroll by this many lines, with the wheel's sign
    /// (positive = toward older content). Never zero.
    Scroll(isize),
}

/// The fingers currently on the glass, and the gesture they add up to.
#[derive(Debug, Default)]
pub struct Touch {
    points: Vec<(usize, (f32, f32))>, // fingers down, in the order they landed
    accum: f32,                       // sub-line remainder of two-finger travel
    multi: bool,                      // a multi-finger gesture is under way
    scale: Option<f32>,               // display backing factor; None = 1.0 (see set_scale)
}

impl Touch {
    /// Tell the gesture machine the display's backing factor, so `PX_PER_LINE`
    /// (a LOGICAL distance) is compared against the PHYSICAL positions winit
    /// reports. `None`/never-called means 1.0, which is what every 1x display
    /// gets and is bit-for-bit the old behaviour.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = Some(if scale.is_finite() && scale > 0.0 { scale } else { 1.0 });
    }

    /// Physical pixels of travel per scrolled line on the current display.
    fn px_per_line(&self) -> f32 {
        self.scale.unwrap_or(1.0) * PX_PER_LINE
    }
    /// A finger landed at `pos`.
    pub fn press(&mut self, id: usize, pos: (f32, f32)) -> Verdict {
        // winit reuses a `FingerId` once its finger is gone, so an id we still
        // hold is a NEW finger, not a duplicate report: drop the stale entry
        // rather than end up tracking one finger twice.
        self.points.retain(|(i, _)| *i != id);
        self.points.push((id, pos));
        if self.points.len() >= 2 {
            let first = !self.multi;
            self.multi = true;
            self.accum = 0.0;
            // Only the SECOND finger cancels: by the third there is nothing
            // left for the pointer to undo.
            return if first { Verdict::CancelPointer } else { Verdict::Swallow };
        }
        Verdict::Pointer
    }

    /// A finger moved to `pos`.
    pub fn motion(&mut self, id: usize, pos: (f32, f32)) -> Verdict {
        let Some(idx) = self.points.iter().position(|(i, _)| *i == id) else {
            // Motion from a finger we never saw land. Mid-gesture it is still
            // gesture; otherwise let it drive the pointer.
            return if self.multi { Verdict::Swallow } else { Verdict::Pointer };
        };
        let prev = self.points[idx].1;
        self.points[idx].1 = pos;
        if !self.multi {
            return Verdict::Pointer;
        }
        if self.points.len() < 2 {
            return Verdict::Swallow; // fingers are lifting out of the gesture
        }
        // Scroll by the MEAN vertical travel of the fingers down, so the view
        // doesn't lurch when one finger moves and the other rests. This event
        // moved one finger, so it contributes 1/n of that mean.
        self.accum += (pos.1 - prev.1) / self.points.len() as f32;
        let ppl = self.px_per_line();
        let lines = (self.accum / ppl) as isize; // truncates toward zero
        if lines != 0 {
            self.accum -= lines as f32 * ppl;
            // Dragging DOWN pulls the content down and so reveals OLDER lines —
            // the direction a wheel-up gives, which is the wheel's positive.
            return Verdict::Scroll(lines);
        }
        Verdict::Swallow
    }

    /// A finger lifted (or the system cancelled tracking it).
    pub fn release(&mut self, id: usize) -> Verdict {
        self.points.retain(|(i, _)| *i != id);
        if !self.points.is_empty() {
            return Verdict::Swallow; // still mid-gesture, or lifting out of one
        }
        // The last finger is up: the gesture is over either way, but a release
        // that ends a two-finger scroll must not reach the pointer as a click.
        let was_multi = std::mem::replace(&mut self.multi, false);
        self.accum = 0.0;
        if was_multi { Verdict::Swallow } else { Verdict::Pointer }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_finger_is_the_pointer() {
        let mut t = Touch::default();
        assert_eq!(t.press(1, (10.0, 10.0)), Verdict::Pointer);
        assert_eq!(t.motion(1, (10.0, 40.0)), Verdict::Pointer);
        assert_eq!(t.release(1), Verdict::Pointer);
    }

    #[test]
    fn second_finger_cancels_the_pointer_gesture() {
        let mut t = Touch::default();
        assert_eq!(t.press(1, (10.0, 10.0)), Verdict::Pointer);
        assert_eq!(t.press(2, (30.0, 10.0)), Verdict::CancelPointer);
        // A third adds nothing to undo.
        assert_eq!(t.press(3, (50.0, 10.0)), Verdict::Swallow);
    }

    #[test]
    fn two_finger_drag_scrolls_by_the_mean_travel() {
        let mut t = Touch::default();
        t.press(1, (10.0, 100.0));
        t.press(2, (30.0, 100.0));
        // Move BOTH fingers a full line down: each contributes half, so the
        // line lands on the second move, not the first.
        assert_eq!(t.motion(1, (10.0, 100.0 + PX_PER_LINE)), Verdict::Swallow);
        assert_eq!(t.motion(2, (30.0, 100.0 + PX_PER_LINE)), Verdict::Scroll(1));
        // Down = toward older content = the wheel's positive, and bringing both
        // fingers back up is the exact mirror of it.
        assert_eq!(t.motion(1, (10.0, 100.0)), Verdict::Swallow);
        assert_eq!(t.motion(2, (30.0, 100.0)), Verdict::Scroll(-1));
    }

    #[test]
    fn sub_line_travel_accumulates_instead_of_being_lost() {
        let mut t = Touch::default();
        t.press(1, (10.0, 0.0));
        t.press(2, (30.0, 0.0));
        // Ten nudges of a fifth of a line, on both fingers: one line, once.
        let step = PX_PER_LINE / 5.0;
        let mut scrolled = 0;
        for k in 1..=5 {
            for (id, x) in [(1usize, 10.0), (2usize, 30.0)] {
                if let Verdict::Scroll(n) = t.motion(id, (x, step * k as f32)) {
                    scrolled += n;
                }
            }
        }
        assert_eq!(scrolled, 1, "five fifths of a line is one line, not zero and not five");
    }

    #[test]
    fn lifting_one_of_two_never_hands_a_click_to_the_pointer() {
        let mut t = Touch::default();
        t.press(1, (10.0, 10.0));
        t.press(2, (30.0, 10.0));
        assert_eq!(t.release(2), Verdict::Swallow);
        assert_eq!(t.motion(1, (10.0, 90.0)), Verdict::Swallow, "the survivor must not select");
        assert_eq!(t.release(1), Verdict::Swallow);
        // The gesture ends with the last finger: the next tap is a plain click.
        assert_eq!(t.press(1, (10.0, 10.0)), Verdict::Pointer);
    }

    #[test]
    fn a_reused_finger_id_is_a_new_finger() {
        let mut t = Touch::default();
        t.press(1, (10.0, 10.0));
        t.release(1);
        // The platform hands the same id back for an unrelated finger.
        assert_eq!(t.press(1, (80.0, 80.0)), Verdict::Pointer, "not a phantom second finger");
    }
}
