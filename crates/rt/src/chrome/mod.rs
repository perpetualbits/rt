//! Native chrome: backend-agnostic draw + hit-test for the context menu, search
//! bar, manual, preferences, colour picker, and instruments — used on BOTH
//! backends (GL and XRender). Each unit reads rt's existing overlay state (no
//! parallel state) and paints via `Backend` primitives.

pub mod colour_picker;
pub mod instruments;
pub mod manual;
pub mod menu;
pub mod prefs;
pub mod search;

/// An axis-aligned layout rectangle in window pixels.
#[derive(Clone, Copy, Debug)]
pub struct Recti {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Recti {
    /// Whether point `p` (window px) is inside this rect.
    pub fn contains(&self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 < self.x + self.w && p.1 >= self.y && p.1 < self.y + self.h
    }
}

/// Index of the first rect containing `p`, if any (menu/row hit-testing).
pub fn hit(rects: &[Recti], p: (f32, f32)) -> Option<usize> {
    rects.iter().position(|r| r.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_picks_the_containing_row() {
        let rows = vec![
            Recti { x: 0.0, y: 0.0, w: 100.0, h: 20.0 },
            Recti { x: 0.0, y: 20.0, w: 100.0, h: 20.0 },
            Recti { x: 0.0, y: 40.0, w: 100.0, h: 20.0 },
        ];
        assert_eq!(hit(&rows, (10.0, 25.0)), Some(1));
        assert_eq!(hit(&rows, (10.0, 5.0)), Some(0));
        assert_eq!(hit(&rows, (10.0, 200.0)), None);
    }
}
