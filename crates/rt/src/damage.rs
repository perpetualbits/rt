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

    /// `self` with `cut`'s area removed, as up to 4 axis-aligned fragments (a
    /// "picture frame" split: top/bottom strips spanning the full width, then
    /// left/right strips confined to the middle band). Empty fragments are
    /// omitted. Used to make the final damage list pairwise disjoint: a wide
    /// band and a tall band from two unrelated instruments (a pane's own
    /// border band and the window's latency frame, say) can overlap in just
    /// the corner square where both reach — too small a shared area for
    /// `merges_tightly` to fold them into one rect (that threshold exists
    /// precisely so thin bands don't balloon into a bounding box), so both
    /// survive as separate scissor rects. `end_frame` draws the frame's whole
    /// vertex batch once per rect, so that corner square got any translucent
    /// fill over it (the titlebar's `lift_fill` tint) alpha-blended onto
    /// itself twice — a visibly different colour baked into the corner. This
    /// makes overlap structurally impossible rather than requiring every pair
    /// of instrument bands to be hand-checked against each other.
    fn subtract(&self, cut: &PxRect) -> [PxRect; 4] {
        // The hole: self ∩ cut, always within self's bounds (each edge is a
        // max/min against self's own edge).
        let (ix0, ix1) = (self.x.max(cut.x), self.right().min(cut.right()));
        let (iy0, iy1) = (self.y.max(cut.y), self.bottom().min(cut.bottom()));
        [
            PxRect { x: self.x, y: self.y, w: self.w, h: iy0 - self.y },              // above the hole (full width)
            PxRect { x: self.x, y: iy1, w: self.w, h: self.bottom() - iy1 },          // below the hole (full width)
            PxRect { x: self.x, y: iy0, w: ix0 - self.x, h: iy1 - iy0 },              // left of the hole
            PxRect { x: ix1, y: iy0, w: self.right() - ix1, h: iy1 - iy0 },           // right of the hole
        ]
    }
}

/// The coalesced damage for one frame.
#[derive(Clone, Debug)]
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

    /// Diagnostic only (see `rt::cursor_damage` in main.rs): is `target` fully
    /// covered by this damage? `Rects` is assumed pairwise disjoint (true of
    /// every `FrameDamage` this module hands out, via `finish()`'s
    /// `make_disjoint`), so summing each rect's overlap with `target` can't
    /// double-count and equals the real covered area.
    pub fn covers(&self, target: PxRect) -> bool {
        match self {
            FrameDamage::Full => true,
            FrameDamage::Rects(rs) => {
                let covered: i64 = rs.iter().map(|r| target.intersection_area(r)).sum();
                covered >= target.area()
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
    ///
    /// `cell_w`/`cell_h` are the renderer's exact (fractional) per-cell size —
    /// the same values `draw_panes` multiplies by column/row to position glyph
    /// and cursor geometry. Truncating that to a whole-pixel cell size here
    /// used to build a DIFFERENT (smaller) mapping from cell index to pixel
    /// than the one geometry is actually drawn at; at high column/row indices
    /// the two diverged by several pixels, and the GL scissor test — exact,
    /// no margin — clipped a sliver of the true glyph away without ever
    /// clearing the pixels the previous frame's glyph occupied there: a
    /// stale residue (worst on the cursor cell, since it redraws every frame
    /// for the blink and so keeps re-exposing the gap). Flooring the left/top
    /// and ceiling the right/bottom, instead of truncating a per-cell size and
    /// multiplying, guarantees the rect fully contains the true float span
    /// regardless of how cell_w/cell_h round — same fix shape as `scissor_box`
    /// rounding the other direction would risk shrinking it.
    pub fn add_cell_span(
        &mut self,
        line: usize,
        left: usize,
        right: usize,
        origin_x: i32,
        origin_y: i32,
        cell_w: f32,
        cell_h: f32,
    ) {
        if right < left {
            return; // undamaged span
        }
        self.add_rect(Self::cell_span_rect(line, left, right, origin_x, origin_y, cell_w, cell_h));
    }

    /// The pixel rect `add_cell_span` would add, without adding it — lets a
    /// caller track a cell span's rect (e.g. the cursor's, frame to frame)
    /// using the exact same float-precise mapping the renderer draws with.
    /// `right < left` (undamaged span) returns an empty rect.
    pub fn cell_span_rect(
        line: usize,
        left: usize,
        right: usize,
        origin_x: i32,
        origin_y: i32,
        cell_w: f32,
        cell_h: f32,
    ) -> PxRect {
        if right < left {
            return PxRect { x: 0, y: 0, w: 0, h: 0 };
        }
        let x0 = origin_x as f32 + left as f32 * cell_w;
        let x1 = origin_x as f32 + (right + 1) as f32 * cell_w;
        let y0 = origin_y as f32 + line as f32 * cell_h;
        let y1 = y0 + cell_h;
        let x = x0.floor() as i32;
        let y = y0.floor() as i32;
        PxRect { x, y, w: x1.ceil() as i32 - x, h: y1.ceil() as i32 - y }
    }

    /// Fold a pane's engine damage in. `Full` marks the whole frame full.
    pub fn add_cells(
        &mut self,
        damage: &Damage,
        origin_x: i32,
        origin_y: i32,
        cell_w: f32,
        cell_h: f32,
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
        FrameDamage::Rects(make_disjoint(merged))
    }
}

/// Split every pairwise overlap out of a rect list, without merging any of
/// them into a bounding box. Two rects from UNRELATED sources — a pane's own
/// border band and the window's latency frame, say — can each be individually
/// too "thin-band-ish" for `merges_tightly` to fold them together (that
/// threshold exists so a wide band and a tall band don't balloon into their
/// shared bounding box), yet still share a small corner square where both
/// reach. `end_frame` scissors to and redraws the frame's whole vertex batch
/// once per rect, so a pixel in that shared square got any translucent fill
/// over it (the titlebar's `lift_fill` tint) alpha-blended onto itself twice —
/// a visibly different colour baked into the corner. Clipping each rect
/// against every rect already placed (via `subtract`) guarantees the result
/// is pairwise disjoint — same total coverage, just tiled instead of
/// overlapping — so this holds for any future instrument's bands too, not
/// just the specific pair that first exposed it.
fn make_disjoint(rects: Vec<PxRect>) -> Vec<PxRect> {
    let mut out: Vec<PxRect> = Vec::new();
    for r in rects {
        let mut pieces = vec![r];
        for placed in &out {
            pieces = pieces
                .into_iter()
                .flat_map(|p| {
                    if p.intersection_area(placed) > 0 {
                        p.subtract(placed).into_iter().filter(|f| !f.is_empty()).collect()
                    } else {
                        vec![p]
                    }
                })
                .collect();
        }
        out.extend(pieces);
    }
    out
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
        let top = 24;
        acc.add_rect(PxRect { x, y, w, h: top }); // titlebar strip
        acc.add_rect(PxRect { x, y: y + h - t, w, h: t }); // bottom
        acc.add_rect(PxRect { x, y: y + top, w: t, h: h - top - t }); // left, between top & bottom
        acc.add_rect(PxRect { x: x + w - t, y: y + top, w: t, h: h - top - t }); // right, between top & bottom
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

    /// The bug this module exists to prevent: two bands from UNRELATED sources
    /// (a pane's own top border band, full window width; the latency frame's
    /// left band, full window height) that are each too "thin" relative to
    /// their union for `merges_tightly` to fold together, yet share a small
    /// corner square. Before `make_disjoint`, both survived as separate
    /// scissor rects and `end_frame` redrew the frame's whole vertex batch
    /// once per rect — double-blending any translucent fill in that shared
    /// square (the titlebar's ghost-cursor corner artefact). The final list
    /// must be pairwise disjoint and cover exactly the same total area as the
    /// two rects' union.
    #[test]
    fn unrelated_bands_sharing_only_a_corner_end_up_disjoint() {
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        let top_band = PxRect { x: 0, y: 0, w: 1000, h: 30 }; // a pane's top border band
        let left_band = PxRect { x: 0, y: 0, w: 8, h: 600 }; // the window's latency-frame left band
        acc.add_rect(top_band);
        acc.add_rect(left_band);
        match acc.finish() {
            FrameDamage::Rects(rs) => {
                for i in 0..rs.len() {
                    for j in (i + 1)..rs.len() {
                        let a = rs[i].intersection_area(&rs[j]);
                        assert_eq!(a, 0, "rects {i} and {j} overlap by {a}px²: {rs:?}");
                    }
                }
                let total: i64 = rs.iter().map(|r| r.area()).sum();
                let union_area = top_band.area() + left_band.area() - top_band.intersection_area(&left_band);
                assert_eq!(total, union_area, "coverage changed: {rs:?}");
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
        acc.add_cell_span(2, 3, 5, 10, 20, 8.0, 16.0);
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
        acc.add_cells(&Damage::Full, 0, 0, 8.0, 16.0);
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
        acc.add_cells(&d, 0, 0, 8.0, 16.0);
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

    /// `covers` backs the `rt::cursor_damage` diagnostic: it must say yes only
    /// when the target is ENTIRELY inside the damage, including the split case
    /// `make_disjoint` produces (no single rect contains the target, but their
    /// union does).
    #[test]
    fn covers_reports_full_and_partial_and_split_coverage() {
        assert!(FrameDamage::Full.covers(PxRect { x: 5, y: 5, w: 8, h: 16 }));

        let one_rect = FrameDamage::Rects(vec![PxRect { x: 0, y: 0, w: 100, h: 100 }]);
        assert!(one_rect.covers(PxRect { x: 10, y: 10, w: 8, h: 16 }));
        assert!(!one_rect.covers(PxRect { x: 90, y: 90, w: 20, h: 20 })); // hangs off the edge

        // Target is split across two disjoint rects that together cover it,
        // as `make_disjoint` would produce when something else overlapped it.
        let split = FrameDamage::Rects(vec![
            PxRect { x: 0, y: 0, w: 10, h: 16 },
            PxRect { x: 10, y: 0, w: 10, h: 16 },
        ]);
        assert!(split.covers(PxRect { x: 5, y: 0, w: 10, h: 16 })); // straddles both
    }

    /// The cursor blinks in place: every frame while focused, `add_cell_span`
    /// re-damages the SAME cell (6d158ce, "the focused cursor cell joins the
    /// damage each frame so its blink pulse ... still repaints"), and a move
    /// additionally damages the vacated cell (fix for the cursor-trail bug).
    /// `cell_span_rect` floors the start and ceils the end independently on
    /// each axis, so a single cell's rect can be up to 1px larger than its
    /// true (fractional-cell-size) bounds. Check that a cursor blinking in
    /// place, alone (nothing else damaged — the common idle-blink frame),
    /// never produces a rect taller or wider than one cell: an oversized rect
    /// would scissor 1px into a genuinely undamaged neighbour cell.
    #[test]
    fn lone_blinking_cursor_rect_does_not_exceed_one_cell() {
        // Realistic fractional cell size (font_px / line-height rarely lands
        // on an integer pixel boundary) — matches dop561's reported gl_boxes.
        let (cell_w, cell_h) = (9.6_f32, 19.34_f32);
        let (origin_x, origin_y) = (12_i32, 40_i32);
        for line in 0..30 {
            for col in 0..30 {
                let mut acc = DamageAccumulator::new();
                acc.begin_frame();
                acc.add_cell_span(line, col, col, origin_x, origin_y, cell_w, cell_h);
                match acc.finish() {
                    FrameDamage::Rects(rs) => {
                        assert_eq!(rs.len(), 1, "line={line} col={col}: {rs:?}");
                        let r = rs[0];
                        assert!(
                            r.w as f32 <= cell_w.ceil() + 1.0 && r.h as f32 <= cell_h.ceil() + 1.0,
                            "line={line} col={col}: rect {r:?} exceeds one cell ({cell_w}x{cell_h})"
                        );
                    }
                    FrameDamage::Full => panic!("expected Rects"),
                }
            }
        }
    }

    /// Two DIFFERENT frames' worth of lone cursor-blink damage (line 5 this
    /// frame, line 6 last frame — e.g. the cursor moved down a row between
    /// frames and each frame's damage was computed independently) can each
    /// individually be up to 1px oversized at their shared boundary, and nothing
    /// ties them together to trigger `make_disjoint`'s pairwise subtraction —
    /// each was the ONLY rect in its own frame's accumulator. Confirms whether
    /// a 1px sliver at the shared row boundary is a real, reachable case.
    #[test]
    fn adjacent_row_lone_cursor_rects_can_share_a_pixel_row() {
        let (cell_w, cell_h) = (9.6_f32, 19.34_f32);
        let (origin_x, origin_y) = (12_i32, 40_i32);
        let rect_for_line = |line: usize| -> PxRect {
            DamageAccumulator::cell_span_rect(line, 10, 10, origin_x, origin_y, cell_w, cell_h)
        };
        let mut touching = 0;
        for line in 0..30 {
            let a = rect_for_line(line);
            let b = rect_for_line(line + 1);
            if a.intersection_area(&b) > 0 {
                touching += 1;
            }
        }
        assert!(touching > 0, "expected at least one adjacent-row pair to overlap by 1px given fractional cell_h={cell_h}");
    }

    /// `plan_frame` (main.rs) unions each of the last `age` frames' ALREADY
    /// -disjoint `FrameDamage::Rects` into a fresh accumulator and calls
    /// `finish()` again, to catch up an EGL back buffer that is `age` swaps
    /// stale. `covers()` assumes the result is still pairwise disjoint. This
    /// reproduces a real typing sequence — each frame the cursor advances one
    /// column, damaging the just-typed (now-vacated) cell plus the new cursor
    /// cell, both re-finished per frame exactly as `main.rs`'s per-pane loop
    /// does — then re-unions 3 such frames (buffer age 3) the way `plan_frame`
    /// does, and checks the final rect list is still pairwise disjoint. A
    /// human-confirmed regression test: this cursor-trail ghost bug was
    /// eventually root-caused to issuing several separate small scissored GL
    /// overwrite passes within one frame, which corrupts content on at least
    /// one NVIDIA GL/Wayland driver (see main.rs's border-band damage merge
    /// comment) — independent of whether the rects were disjoint. So if this
    /// test ever finds an overlap, that overlap is `end_frame`'s double-blend
    /// hazard (see `subtract`'s doc comment) reintroduced by history-union,
    /// not just a same-frame one `finish()` already guards against.
    #[test]
    fn history_union_across_frames_stays_disjoint() {
        let (cell_w, cell_h) = (9.6_f32, 19.34_f32);
        let (origin_x, origin_y) = (12_i32, 40_i32);
        let line = 5;
        let mut history: Vec<FrameDamage> = Vec::new();
        for col in 0..8 {
            let mut acc = DamageAccumulator::new();
            acc.begin_frame();
            // The just-typed (now-vacated) cell: engine damage + cursor_track's
            // vacated-rect both land here, identically, each frame col>0.
            if col > 0 {
                acc.add_cell_span(line, col - 1, col - 1, origin_x, origin_y, cell_w, cell_h);
            }
            // The cursor's new cell, re-damaged every frame for the blink pulse.
            acc.add_cell_span(line, col, col, origin_x, origin_y, cell_w, cell_h);
            history.insert(0, acc.finish()); // push_front, like `damage_history`
        }
        let age = 3;
        let mut union_acc = DamageAccumulator::new();
        union_acc.begin_frame();
        for fd in history.iter().take(age) {
            match fd {
                FrameDamage::Full => panic!("expected Rects"),
                FrameDamage::Rects(rs) => {
                    for r in rs {
                        union_acc.add_rect(*r);
                    }
                }
            }
        }
        match union_acc.finish() {
            FrameDamage::Full => panic!("expected Rects"),
            FrameDamage::Rects(rs) => {
                for i in 0..rs.len() {
                    for j in (i + 1)..rs.len() {
                        assert_eq!(
                            rs[i].intersection_area(&rs[j]),
                            0,
                            "rects {} and {} overlap after history union: {:?} vs {:?} (full list: {rs:?})",
                            i, j, rs[i], rs[j]
                        );
                    }
                }
            }
        }
    }
}
