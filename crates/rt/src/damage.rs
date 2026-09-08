//! Pure, GL-free damage accumulation. Collects the pixel regions that changed
//! this frame (from engine cell damage, the cursor, animated chrome, etc.) and
//! coalesces them into a small set of rectangles the renderer can scissor to.
//! No GL, no winit — just integer rectangle math, so it is fully unit-tested.

use rt_engine::Damage;

/// A rectangle in **physical pixels, top-left origin** (winit convention).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PxRect {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    /// Do the two rectangles overlap or touch (shared edge counts, so touching
    /// rects merge into one scissor region rather than two adjacent passes)?
    pub fn intersects(&self, other: &PxRect) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }
    pub fn area(&self) -> i64 {
        if self.is_empty() { 0 } else { self.w as i64 * self.h as i64 }
    }
    /// Area shared by the two rectangles (0 when they only touch or are apart).
    pub fn intersection_area(&self, other: &PxRect) -> i64 {
        let w = (self.right().min(other.right()) - self.x.max(other.x)).max(0) as i64;
        let h = (self.bottom().min(other.bottom()) - self.y.max(other.y)).max(0) as i64;
        w * h
    }
    /// Should these two be merged into their union? Only if they overlap or
    /// touch AND the union adds at most half as many pixels again as the two
    /// rects cover (waste <= 50% of covered).
    /// Aligned neighbours (adjacent cells in a row, stacked full-width lines),
    /// containment and heavy overlap merge; two thin bands meeting at a corner,
    /// or a cell next to a tall band, stay separate. A merged rect is repainted
    /// in full, so wasted union area is wasted fragment work.
    pub fn merges_tightly(&self, other: &PxRect) -> bool {
        if !self.intersects(other) {
            return false;
        }
        let covered = self.area() + other.area() - self.intersection_area(other);
        let waste = self.union(other).area() - covered;
        waste * 2 <= covered
    }
    /// Smallest rectangle covering both.
    pub fn union(&self, other: &PxRect) -> PxRect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        PxRect { x, y, w: right - x, h: bottom - y }
    }
}

/// The coalesced damage for one frame.
pub enum FrameDamage {
    Full,
    Rects(Vec<PxRect>),
}

impl FrameDamage {
    /// Bounding box of all damage rects, or `None` for `Full`/empty. Phase 1's
    /// scissored redraw uses this single box (see the renderer); multi-rect
    /// scissoring is a later refinement.
    pub fn bbox(&self) -> Option<PxRect> {
        match self {
            FrameDamage::Full => None,
            FrameDamage::Rects(rs) => {
                let mut it = rs.iter().filter(|r| !r.is_empty());
                let first = *it.next()?;
                Some(it.fold(first, |acc, r| acc.union(r)))
            }
        }
    }
}

/// Accumulates this frame's damage. Reused across frames via `begin_frame()`.
pub struct DamageAccumulator {
    full: bool,
    rects: Vec<PxRect>,
}

impl DamageAccumulator {
    pub fn new() -> Self {
        Self { full: false, rects: Vec::new() }
    }

    /// Start a fresh frame's accumulation.
    pub fn begin_frame(&mut self) {
        self.full = false;
        self.rects.clear();
    }

    pub fn mark_full(&mut self) {
        self.full = true;
    }

    pub fn is_full(&self) -> bool {
        self.full
    }

    /// Add a pixel rectangle. Empty rects and additions after `mark_full()` are
    /// ignored (once full, individual rects are moot).
    pub fn add_rect(&mut self, r: PxRect) {
        if self.full || r.is_empty() {
            return;
        }
        self.rects.push(r);
    }

    /// Map one cell span (`left..=right` inclusive on row `line`) to a pixel
    /// rect at a pane's content origin and add it.
    pub fn add_cell_span(
        &mut self,
        line: usize,
        left: usize,
        right: usize,
        origin_x: i32,
        origin_y: i32,
        cell_w: i32,
        cell_h: i32,
    ) {
        if right < left {
            return; // undamaged span
        }
        let cols = (right - left + 1) as i32;
        self.add_rect(PxRect {
            x: origin_x + left as i32 * cell_w,
            y: origin_y + line as i32 * cell_h,
            w: cols * cell_w,
            h: cell_h,
        });
    }

    /// Fold a pane's engine damage in. `Full` marks the whole frame full.
    pub fn add_cells(
        &mut self,
        damage: &Damage,
        origin_x: i32,
        origin_y: i32,
        cell_w: i32,
        cell_h: i32,
    ) {
        match damage {
            Damage::Full => self.mark_full(),
            Damage::Lines(lines) => {
                for d in lines {
                    self.add_cell_span(d.line, d.left, d.right, origin_x, origin_y, cell_w, cell_h);
                }
            }
            // A scroll-blit is handled earlier (backend `CopyArea` + span repaint); if it
            // reaches the generic pixel-damage path it means no backend consumed it, so the
            // safe, correct answer is a full repaint of the (already scroll-shifted) grid.
            Damage::Scroll { .. } => self.mark_full(),
        }
    }

    /// Coalesce and return this frame's damage. Repeatedly merges any two rects
    /// that overlap or touch — but only when their union wastes little area
    /// (see [`PxRect::merges_tightly`]) — until no more merges are possible, so
    /// the renderer scissors a handful of regions instead of hundreds of tiny
    /// ones, and a thin L-shape (a pane's border bands) does NOT balloon into
    /// its bounding box. That distinction is the whole partial-frame budget on
    /// a software rasteriser: the four 6px bands of a pane touch at the corners,
    /// and merging them by bounding box made every "partial" frame the whole
    /// pane, which then swallowed the keystroke's cell too.
    pub fn finish(&self) -> FrameDamage {
        if self.full {
            return FrameDamage::Full;
        }
        let mut merged: Vec<PxRect> = Vec::new();
        for r in self.rects.iter().filter(|r| !r.is_empty()) {
            let mut cur = *r;
            let mut i = 0;
            while i < merged.len() {
                if merged[i].merges_tightly(&cur) {
                    cur = merged[i].union(&cur);
                    merged.swap_remove(i); // re-test cur against the rest
                    i = 0; // reset to re-test grown cur against all remaining elements
                } else {
                    i += 1;
                }
            }
            merged.push(cur);
        }
        FrameDamage::Rects(merged)
    }
}

impl Default for DamageAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_engine::{CellDamage, Damage};

    /// A pane's four border bands touch at the corners; merging them by bounding
    /// box would make the "partial" region the whole pane. They must stay four.
    #[test]
    fn border_bands_do_not_merge_into_the_pane_bbox() {
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        let (x, y, w, h, t) = (8, 8, 1615, 898, 6);
        acc.add_rect(PxRect { x, y, w, h: 24 }); // titlebar strip
        acc.add_rect(PxRect { x, y: y + h - t, w, h: t }); // bottom
        acc.add_rect(PxRect { x, y, w: t, h }); // left
        acc.add_rect(PxRect { x: x + w - t, y, w: t, h }); // right
        acc.add_rect(PxRect { x: 400, y: 300, w: 10, h: 19 }); // a keystroke cell in the middle
        match acc.finish() {
            FrameDamage::Rects(rs) => {
                assert_eq!(rs.len(), 5, "{rs:?}");
                let total: i64 = rs.iter().map(|r| r.area()).sum();
                assert!(total < 100_000, "damage area ballooned: {total}");
            }
            FrameDamage::Full => panic!("expected Rects"),
        }
    }

    /// Aligned neighbours still merge: adjacent cells on a row become one span,
    /// stacked full-width rows become one block, and containment collapses.
    #[test]
    fn aligned_neighbours_and_containment_still_merge() {
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        acc.add_rect(PxRect { x: 0, y: 0, w: 8, h: 16 });
        acc.add_rect(PxRect { x: 8, y: 0, w: 8, h: 16 }); // right neighbour
        acc.add_rect(PxRect { x: 0, y: 16, w: 16, h: 16 }); // row below, same width
        acc.add_rect(PxRect { x: 2, y: 2, w: 4, h: 4 }); // inside the first
        match acc.finish() {
            FrameDamage::Rects(rs) => assert_eq!(rs, vec![PxRect { x: 0, y: 0, w: 16, h: 32 }]),
            FrameDamage::Full => panic!("expected Rects"),
        }
    }

    #[test]
    fn cell_span_maps_to_pixels() {
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        // Row 2, cols 3..=5, pane at (10,20), 8x16 cells.
        acc.add_cell_span(2, 3, 5, 10, 20, 8, 16);
        match acc.finish() {
            FrameDamage::Rects(rs) => {
                assert_eq!(rs.len(), 1);
                let r = rs[0];
                assert_eq!(r.x, 10 + 3 * 8); // 34
                assert_eq!(r.y, 20 + 2 * 16); // 52
                assert_eq!(r.w, (5 - 3 + 1) * 8); // 3 cols → 24
                assert_eq!(r.h, 16);
            }
            FrameDamage::Full => panic!("expected Rects, got Full"),
        }
    }

    #[test]
    fn engine_full_propagates() {
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        acc.add_cells(&Damage::Full, 0, 0, 8, 16);
        assert!(acc.is_full());
        assert!(matches!(acc.finish(), FrameDamage::Full));
    }

    #[test]
    fn engine_lines_map_each_span() {
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        let d = Damage::Lines(vec![
            CellDamage { line: 0, left: 0, right: 0 },
            CellDamage { line: 9, left: 2, right: 4 },
        ]);
        acc.add_cells(&d, 0, 0, 8, 16);
        match acc.finish() {
            FrameDamage::Rects(rs) => assert_eq!(rs.len(), 2),
            FrameDamage::Full => panic!("expected Rects"),
        }
    }

    #[test]
    fn overlapping_rects_coalesce() {
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        acc.add_rect(PxRect { x: 0, y: 0, w: 10, h: 10 });
        acc.add_rect(PxRect { x: 5, y: 5, w: 10, h: 10 }); // overlaps the first
        match acc.finish() {
            FrameDamage::Rects(rs) => {
                assert_eq!(rs.len(), 1, "overlapping rects should merge");
                assert_eq!(rs[0], PxRect { x: 0, y: 0, w: 15, h: 15 });
            }
            FrameDamage::Full => panic!("expected Rects"),
        }
    }

    #[test]
    fn disjoint_rects_stay_separate() {
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        acc.add_rect(PxRect { x: 0, y: 0, w: 5, h: 5 });
        acc.add_rect(PxRect { x: 100, y: 100, w: 5, h: 5 });
        match acc.finish() {
            FrameDamage::Rects(rs) => assert_eq!(rs.len(), 2),
            FrameDamage::Full => panic!("expected Rects"),
        }
    }

    #[test]
    fn empty_and_zero_size_rects_dropped() {
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        acc.add_rect(PxRect { x: 0, y: 0, w: 0, h: 10 }); // zero width → dropped
        assert!(matches!(acc.finish(), FrameDamage::Rects(rs) if rs.is_empty()));
    }

    #[test]
    fn bbox_of_rects() {
        let fd = FrameDamage::Rects(vec![
            PxRect { x: 10, y: 10, w: 5, h: 5 },
            PxRect { x: 100, y: 50, w: 20, h: 20 },
        ]);
        let b = fd.bbox().unwrap();
        assert_eq!(b, PxRect { x: 10, y: 10, w: 110, h: 60 }); // (10,10)..(120,70)
        assert!(FrameDamage::Full.bbox().is_none());
    }

    #[test]
    fn transitive_chain_coalesces_to_one() {
        // X and Y are disjoint from each other, but C bridges them (aligned on
        // one row, so every merge is tight). Inserting in the order X, Y, C used
        // to leave Y un-merged (scan index not reset after C absorbed X). All
        // three must collapse into one row span.
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        acc.add_rect(PxRect { x: 0, y: 0, w: 10, h: 10 });  // X: x[0,10]
        acc.add_rect(PxRect { x: 20, y: 0, w: 10, h: 10 }); // Y: x[20,30]
        acc.add_rect(PxRect { x: 10, y: 0, w: 10, h: 10 }); // C: x[10,20], touches both
        match acc.finish() {
            FrameDamage::Rects(rs) => {
                assert_eq!(rs.len(), 1, "transitive chain must merge to one rect, got {}", rs.len());
                assert_eq!(rs[0], PxRect { x: 0, y: 0, w: 30, h: 10 });
            }
            FrameDamage::Full => panic!("expected Rects"),
        }
    }
}
