//! Regression test for the stale-interior-colour bug: changing the background
//! colour in preferences updated only the window margin — pane interiors kept
//! showing the OLD colour until the next resize.
//!
//! Root cause (confirmed from a `[colourdbg]` capture): `VtPane::render_snapshot`
//! refreshes `last_render` — the persistent resolved grid — by re-resolving
//! ONLY the cells vt-term reports as damaged (`apply_damage`). A palette change
//! damages no cells, so `set_palette` swapping `self.palette` left every cached
//! cell's RGB pointing at the OLD palette until something else (a resize, which
//! reflows and damages everything) forced a full re-resolve.
//!
//! This must be exercised against vt-term SPECIFICALLY — `TermPane::spawn`
//! picks the engine from the ambient `RT_ENGINE`, which would silently test the
//! vendored alacritty backend (immune to this bug, since it resolves colours
//! fresh on every `capture_locked`) in an environment exporting
//! `RT_ENGINE=alacritty`. So this spawns via `TermPane::spawn_vt_env`.

use rt_engine::{Palette, TermPane, DEFAULT_SCROLLBACK};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn test_budget() -> Arc<rt_engine::budget::Budget> {
    Arc::new(rt_engine::budget::Budget::default())
}

/// A palette whose default background is unmistakably `bg`, so any cell still
/// resolving to the old colour is easy to spot.
fn palette_with_bg(bg: rt_engine::Rgb) -> Palette {
    Palette::new(rt_engine::DEFAULT_FG, bg, [bg; 16])
}

#[test]
fn set_palette_repaints_the_cached_grid_immediately() {
    // A plain shell with no output: every visible cell stays "default background"
    // and resolves straight from the palette, so this isolates the palette-swap
    // path from any content-driven damage.
    let shell = Some(("/bin/sh".to_string(), vec![]));
    let mut pane = TermPane::spawn_vt_env(shell, None, 80, 24, &[], DEFAULT_SCROLLBACK, &test_budget())
        .expect("vt-term pane spawns");

    // Let the shell settle, then take the first render_snapshot so `last_render`
    // is populated (this is the persistent cache the bug leaves stale).
    std::thread::sleep(Duration::from_millis(200));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut before = pane.render_snapshot();
    while Instant::now() < deadline && before.rows.is_empty() {
        before = pane.render_snapshot();
        std::thread::sleep(Duration::from_millis(20));
    }
    let old_bg = before.rows[0][0].bg;

    // Change the background to something unmistakably different (midnight-purple
    // -> green, mirroring the captured repro) and take another snapshot WITHOUT
    // any resize or content change in between.
    let new_bg: rt_engine::Rgb = if old_bg == [0, 31, 0] { [2, 0, 31] } else { [0, 31, 0] };
    assert_ne!(old_bg, new_bg, "test palettes must actually differ");
    pane.set_palette(palette_with_bg(new_bg));

    let after = pane.render_snapshot();

    // Every cell in the grid must now resolve against the NEW palette. Before the
    // fix, `render_snapshot` only re-resolves vt-term's reported damage — a
    // palette change damages nothing, so the cached cells kept the OLD colour.
    for (r, row) in after.rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            assert_eq!(
                cell.bg, new_bg,
                "cell ({r},{c}) still shows the stale background {:?} (expected {:?}) after set_palette",
                cell.bg, new_bg
            );
        }
    }

    // The returned damage must also say "repaint everything" so consumers on the
    // partial/scissored path (not just a full-redraw path) actually pick this up.
    assert!(after.damage.is_full(), "set_palette's first snapshot must report Damage::Full, got {:?}", after.damage);
}
