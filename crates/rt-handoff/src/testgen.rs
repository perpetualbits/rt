//! Deterministic model generator for the round-trip tests.
//!
//! Compiled unconditionally so the integration tests under `tests/` can reach
//! it, and so phase 2's engine tests can reuse it.

use crate::grid::{line_flags, run_flags, Grid, Line, Run};
use crate::msg::{Adopted, Claim, DropTargetWire, Failed, Hello, Message, Offer, PaneFds, ScrollChunk};
use crate::pane::{Charsets, CursorState, ImageEntry, KittyKbd, Margins, ModeEntry, PaneWire, SavedCursor};
use crate::style::{attrs, Colour, Style};
use crate::tree::Node;

/// xorshift64. No dependencies, and a seed always replays exactly.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1) // avoid the all-zero fixed point
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    pub fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    pub fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

fn gen_colour(r: &mut Rng) -> Colour {
    match r.below(3) {
        0 => Colour::Default,
        1 => Colour::Indexed(r.below(256) as u8),
        _ => Colour::Rgb(r.below(256) as u8, r.below(256) as u8, r.below(256) as u8),
    }
}

fn gen_style(r: &mut Rng) -> Style {
    Style {
        fg: gen_colour(r),
        bg: gen_colour(r),
        underline: gen_colour(r),
        attrs: (r.next_u64() as u32) & attrs::KNOWN,
        link_id: r.below(4) as u32,
    }
}

fn gen_text(r: &mut Rng, cells: usize) -> String {
    const ALPHABET: [char; 12] = ['a', 'z', ' ', '0', '9', '/', '$', '~', 'é', 'ß', '→', '★'];
    (0..cells).map(|_| r.pick(&ALPHABET)).collect()
}

fn gen_run(r: &mut Rng, styles: u32) -> Run {
    let style_id = r.below(styles.max(1) as u64) as u32;
    match r.below(5) {
        0 => Run::blank(style_id, 1 + r.below(80) as u32),
        1 => {
            // Wide: two columns per character.
            let n = 1 + r.below(6) as usize;
            let text: String = (0..n).map(|_| r.pick(&['日', '本', '語', '中'])).collect();
            Run::wide(style_id, &text)
        }
        2 => {
            // Combining marks: some cells carry more than one char.
            let cells = 1 + r.below(6) as usize;
            let mut text = String::new();
            let mut char_counts = Vec::with_capacity(cells);
            for _ in 0..cells {
                text.push(r.pick(&['e', 'a', 'o']));
                if r.bool() {
                    text.push('\u{0301}');
                    char_counts.push(2);
                } else {
                    char_counts.push(1);
                }
            }
            Run { flags: run_flags::CHAR_COUNTS, style_id, cell_span: cells as u32, text, char_counts }
        }
        _ => {
            let cells = 1 + r.below(40) as usize;
            Run::text(style_id, &gen_text(r, cells))
        }
    }
}

fn gen_line(r: &mut Rng, styles: u32) -> Line {
    let flags = r.pick(&[0, line_flags::WRAPPED, line_flags::DECDWL, line_flags::DECDHL_TOP, line_flags::DECDHL_BOTTOM]);
    let runs = (0..r.below(5)).map(|_| gen_run(r, styles)).collect();
    Line { flags, runs }
}

fn gen_grid(r: &mut Rng, styles: u32, max_lines: u64) -> Grid {
    Grid { lines: (0..r.below(max_lines)).map(|_| gen_line(r, styles)).collect() }
}

/// A pane with every field exercised, valid by construction.
pub fn gen_pane(r: &mut Rng) -> PaneWire {
    let n_styles = 1 + r.below(8) as u32;
    PaneWire {
        pane_uid: r.next_u64(),
        title: {
            let n = 1 + r.below(20) as usize;
            gen_text(r, n)
        },
        cwd: r.bool().then(|| format!("/home/u/{}", gen_text(r, 5))),
        cols: 1 + r.below(400) as u32,
        rows: 1 + r.below(200) as u32,
        scrollback_limit: r.bool().then(|| r.below(5_000_000) as u32),
        columns_count: r.bool().then(|| 1 + r.below(8) as u32),
        group: r.bool().then(|| r.below(16) as u32),
        broadcast: r.bool().then(|| r.bool()),
        child_pid: r.below(4_000_000) as u32,
        shell_argv: (0..r.below(4)).map(|_| gen_text(r, 6)).collect(),
        env_extras: (0..r.below(3)).map(|_| (gen_text(r, 6), gen_text(r, 20))).collect(),
        show_titlebar: r.bool().then(|| r.bool()),
        palette: r.bool().then(|| (0..256).map(|_| (r.below(256) as u8, r.below(256) as u8, r.below(256) as u8)).collect()),
        modes: (0..r.below(20))
            .map(|_| ModeEntry { kind: r.below(2) as u8, number: r.below(3000) as u32, value: r.below(2) as u8 })
            .collect(),
        cursor: CursorState {
            col: r.below(400) as u32,
            row: r.below(200) as u32,
            shape: r.below(3) as u8,
            visible: r.bool(),
            blink: r.bool(),
            pending_wrap: r.bool(),
        },
        saved_cursor: r.bool().then(|| SavedCursor {
            col: r.below(400) as u32,
            row: r.below(200) as u32,
            pen: gen_style(r),
            charsets: Charsets { g: [b'B', b'0', b'A', b'B'], gl: r.below(4) as u8, gr: r.below(4) as u8 },
            origin: r.bool(),
        }),
        charsets: r.bool().then(|| Charsets { g: [b'B'; 4], gl: r.below(4) as u8, gr: r.below(4) as u8 }),
        tab_stops: r.bool().then(|| (0..1 + r.below(400)).map(|_| r.bool()).collect()),
        margins: r.bool().then(|| Margins {
            top: r.below(100) as u32,
            bottom: r.below(100) as u32,
            left: r.below(100) as u32,
            right: r.below(100) as u32,
        }),
        title_stack: (0..r.below(4)).map(|_| gen_text(r, 8)).collect(),
        pen: gen_style(r),
        active_screen: r.below(2) as u8,
        kitty_kbd: r.bool().then(|| KittyKbd {
            stack: (0..r.below(4)).map(|_| r.below(32) as u32).collect(),
            modify_other_keys: r.below(3) as u8,
        }),
        pending_raw: (0..r.below(8)).map(|_| r.below(256) as u8).collect(),
        style_table: (0..n_styles).map(|_| gen_style(r)).collect(),
        screen_primary: gen_grid(r, n_styles, 60),
        screen_alt: r.bool().then(|| gen_grid(r, n_styles, 30)),
        uri_table: (0..r.below(4)).map(|_| (r.below(1000) as u32, format!("https://h.invalid/{}", gen_text(r, 6)))).collect(),
        image_table: (0..r.below(3))
            .map(|_| ImageEntry {
                id: r.below(1000) as u32,
                format: 1,
                w: 1 + r.below(64) as u32,
                h: 1 + r.below(64) as u32,
                data: (0..r.below(64)).map(|_| r.below(256) as u8).collect(),
            })
            .collect(),
        tag_names: Vec::new(),   // filled by the decoder, never by a producer
        unknown_tags: Vec::new(), // ditto
    }
}

/// A layout tree of bounded depth.
pub fn gen_tree(r: &mut Rng, depth: u32) -> Node {
    if depth == 0 || r.below(3) == 0 {
        return Node::Leaf { pane_uid: r.next_u64() };
    }
    let ratio = (r.below(1001) as f32) / 1000.0;
    let n = 2 + r.below(2) as usize;
    match r.below(3) {
        0 => Node::HSplit { ratio, children: (0..n).map(|_| gen_tree(r, depth - 1)).collect() },
        1 => Node::VSplit { ratio, children: (0..n).map(|_| gen_tree(r, depth - 1)).collect() },
        _ => Node::Tabs {
            active: r.below(n as u64) as u32,
            children: (0..n).map(|_| (gen_text(r, 6), gen_tree(r, depth - 1))).collect(),
        },
    }
}

pub fn gen_scroll_chunk(r: &mut Rng) -> ScrollChunk {
    ScrollChunk {
        pane_uid: r.next_u64(),
        more: r.bool(),
        lines: (0..r.below(200)).map(|_| gen_line(r, 4)).collect(),
    }
}

/// Any message, so the frame dispatch is exercised too.
pub fn gen_message(r: &mut Rng) -> Message {
    match r.below(10) {
        0 => Message::Hello(Hello {
            proto_min: 1,
            proto_max: 1 + r.below(4) as u32,
            rt_version: format!("0.{}.{}", r.below(10), r.below(40)),
            engine: if r.bool() { "vtterm".into() } else { "alacritty".into() },
            boot_id: gen_text(r, 16),
            caps: (0..r.below(5)).map(|_| r.below(64)).collect(),
            max_scrollback_lines: r.below(5_000_000) as u32,
            max_payload_bytes: r.below(64 * 1024 * 1024),
            display: if r.bool() { "wayland".into() } else { "x11".into() },
            unknown_tags: vec![],
        }),
        1 => {
            let mut token = [0u8; 16];
            for b in token.iter_mut() {
                *b = r.below(256) as u8;
            }
            Message::Offer(Offer {
                token,
                pane_count: r.below(20) as u32,
                titles: (0..r.below(6)).map(|_| gen_text(r, 8)).collect(),
                byte_estimate: r.next_u64() % 1_000_000_000,
                unknown_tags: vec![],
            })
        }
        2 => {
            let mut token = [0u8; 16];
            for b in token.iter_mut() {
                *b = r.below(256) as u8;
            }
            let target = match r.below(7) {
                0 => DropTargetWire::Root,
                1 => DropTargetWire::SplitLeft(r.next_u64()),
                2 => DropTargetWire::SplitRight(r.next_u64()),
                3 => DropTargetWire::SplitAbove(r.next_u64()),
                4 => DropTargetWire::SplitBelow(r.next_u64()),
                5 => DropTargetWire::Swap(r.next_u64()),
                _ => DropTargetWire::TabInsert { first_pane: r.next_u64(), index: r.below(10) as u32 },
            };
            Message::Claim(Claim { token, target, accepted_budget: r.below(5_000_000) as u32, unknown_tags: vec![] })
        }
        3 => Message::Tree(gen_tree(r, 4)),
        4 => Message::PaneState(Box::new(gen_pane(r))),
        5 => Message::PaneFds(PaneFds { pane_uid: r.next_u64() }),
        6 => Message::ScrollChunk(gen_scroll_chunk(r)),
        7 => Message::Adopted(Adopted { pane_uids: (0..r.below(8)).map(|_| r.next_u64()).collect() }),
        8 => Message::Failed(Failed { code: 1 + r.below(8) as u32, text: gen_text(r, 20) }),
        _ => match r.below(4) {
            0 => Message::Cancel,
            1 => Message::Ping,
            2 => Message::Pong,
            _ => Message::Bye,
        },
    }
}
