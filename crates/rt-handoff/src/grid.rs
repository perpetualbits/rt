//! Cell data: runs of identically-styled cells, grouped into lines.
//!
//! `cell_span` is measured in COLUMNS. A wide-character run covers two columns
//! per character, so its cell count is `cell_span / 2`. A run never mixes wide
//! and narrow cells — it breaks instead. Every invariant here is checked on
//! decode, because a malformed grid from a peer must be an error, never a
//! panic in the renderer three frames later.

use crate::buf::{Reader, Writer};
use crate::error::{Result, WireError};

/// Per-line flags. Frozen at v1.
pub mod line_flags {
    /// This line soft-wraps into the next one.
    pub const WRAPPED: u32 = 1;
    /// DECDWL: double-width line.
    pub const DECDWL: u32 = 2;
    /// DECDHL: top half of a double-height line.
    pub const DECDHL_TOP: u32 = 4;
    /// DECDHL: bottom half of a double-height line.
    pub const DECDHL_BOTTOM: u32 = 8;
}

/// Per-run flags. Frozen at v1.
pub mod run_flags {
    /// A per-cell character-count array follows the text (combining marks).
    pub const CHAR_COUNTS: u8 = 1;
    /// Every cell in this run occupies two columns.
    pub const WIDE: u8 = 2;
    /// Every bit this version implements. Unlike `attrs` and `line_flags`, an
    /// unknown bit here is REJECTED rather than ignored: bit 0 already changes
    /// the GRAMMAR by appending a `char_counts` array, so a future bit that did
    /// the same would make a v1 receiver misparse every remaining run and line
    /// in the blob — silently, because a GridBlob is positional and carries no
    /// per-run length to skip by.
    pub const KNOWN: u8 = CHAR_COUNTS | WIDE;
}

/// A stretch of cells sharing one style.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Run {
    pub flags: u8,
    pub style_id: u32,
    /// Width in COLUMNS, not cells. See `cells()`.
    pub cell_span: u32,
    /// The characters of the run's cells, concatenated. Empty means blanks.
    pub text: String,
    /// One entry per cell when `CHAR_COUNTS` is set; empty otherwise.
    pub char_counts: Vec<u32>,
}

impl Run {
    /// `cell_span` blank cells in `style_id` — the common case, and the reason
    /// a mostly-empty 200-column line costs a handful of bytes.
    pub fn blank(style_id: u32, cell_span: u32) -> Run {
        Run { flags: 0, style_id, cell_span, text: String::new(), char_counts: Vec::new() }
    }

    /// A run of single-width, single-character cells.
    pub fn text(style_id: u32, s: &str) -> Run {
        Run {
            flags: 0,
            style_id,
            cell_span: s.chars().count() as u32,
            text: s.to_string(),
            char_counts: Vec::new(),
        }
    }

    /// A run of double-width cells: two columns per character.
    pub fn wide(style_id: u32, s: &str) -> Run {
        Run {
            flags: run_flags::WIDE,
            style_id,
            cell_span: (s.chars().count() as u32) * 2,
            text: s.to_string(),
            char_counts: Vec::new(),
        }
    }

    /// The number of CELLS this run covers, as opposed to columns.
    pub fn cells(&self) -> u32 {
        if self.flags & run_flags::WIDE != 0 {
            self.cell_span / 2
        } else {
            self.cell_span
        }
    }

    fn write(&self, w: &mut Writer) {
        w.u8(self.flags);
        w.varint(self.style_id as u64);
        w.varint(self.cell_span as u64);
        w.str(&self.text);
        if self.flags & run_flags::CHAR_COUNTS != 0 {
            for c in &self.char_counts {
                w.varint(*c as u64);
            }
        }
    }

    fn read(r: &mut Reader<'_>, tag: u64) -> Result<Run> {
        let flags = r.u8()?;
        if flags & !run_flags::KNOWN != 0 {
            return Err(WireError::BadValue { tag, why: "unknown run flag" });
        }
        let style_id = r.varint_u32()?;
        let cell_span = r.varint_u32()?;
        let text = r.str()?;

        if flags & run_flags::WIDE != 0 && cell_span % 2 != 0 {
            return Err(WireError::BadValue { tag, why: "wide run must span an even number of columns" });
        }
        let cells = if flags & run_flags::WIDE != 0 { cell_span / 2 } else { cell_span };

        let mut char_counts = Vec::new();
        if flags & run_flags::CHAR_COUNTS != 0 {
            for _ in 0..cells {
                char_counts.push(r.varint_u32()?);
            }
            let want: u64 = char_counts.iter().map(|c| *c as u64).sum();
            if want != text.chars().count() as u64 {
                return Err(WireError::BadValue { tag, why: "char_counts must sum to the character count of text" });
            }
        } else if !text.is_empty() && text.chars().count() as u32 != cells {
            return Err(WireError::BadValue { tag, why: "text character count must equal the run's cell count" });
        }

        Ok(Run { flags, style_id, cell_span, text, char_counts })
    }
}

/// One row: flags plus its runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Line {
    pub flags: u32,
    pub runs: Vec<Run>,
}

impl Line {
    pub fn write(&self, w: &mut Writer) {
        w.varint(self.flags as u64);
        w.varint(self.runs.len() as u64);
        for run in &self.runs {
            run.write(w);
        }
    }

    pub fn read(r: &mut Reader<'_>, tag: u64) -> Result<Line> {
        // Deliberately NOT `varint_u32`, and deliberately not rejecting unknown
        // bits: the spec says `varint line_flags`, and line flags are pure
        // booleans over a fixed grammar. A future bit changes nothing about how
        // the rest of the blob parses, so ignoring one is safe — while erroring
        // would reject an entire pane over a line decoration we cannot draw.
        let flags = r.varint()? as u32;
        let n = r.varint()? as usize;
        let mut runs = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            runs.push(Run::read(r, tag)?);
        }
        Ok(Line { flags, runs })
    }
}

/// A screen or a scrollback region: a count and that many lines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grid {
    pub lines: Vec<Line>,
}

impl Grid {
    pub fn write(&self) -> Vec<u8> {
        debug_assert!(
            self.lines.iter().flat_map(|l| &l.runs).all(|r| {
                let cells = r.cells();
                if r.flags & run_flags::CHAR_COUNTS != 0 {
                    r.char_counts.len() as u32 == cells
                } else {
                    r.text.is_empty() || r.text.chars().count() as u32 == cells
                }
            }),
            "a run's text and cell count disagree; build runs with Run::blank/text/wide"
        );
        self.write_unchecked()
    }

    /// `write` without the debug assertion, so tests can build a deliberately
    /// malformed grid and prove the DECODER rejects it.
    pub fn write_unchecked(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(self.lines.len() as u64);
        for line in &self.lines {
            line.write(&mut w);
        }
        w.into_vec()
    }

    pub fn read(body: &[u8], tag: u64) -> Result<Grid> {
        let mut r = Reader::new(body);
        let n = r.varint()? as usize;
        let mut lines = Vec::with_capacity(n.min(65536));
        for _ in 0..n {
            lines.push(Line::read(&mut r, tag)?);
        }
        r.finish()?;
        Ok(Grid { lines })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;

    fn round_trip(g: &Grid) -> Grid {
        Grid::read(&g.write(), 0x40).unwrap()
    }

    #[test]
    fn empty_grid_round_trips() {
        let g = Grid { lines: vec![] };
        assert_eq!(round_trip(&g), g);
    }

    #[test]
    fn a_blank_line_round_trips() {
        let g = Grid { lines: vec![Line { flags: 0, runs: vec![Run::blank(0, 80)] }] };
        assert_eq!(round_trip(&g), g);
    }

    #[test]
    fn plain_ascii_round_trips() {
        let g = Grid {
            lines: vec![Line { flags: 0, runs: vec![Run::text(0, "$ cargo test"), Run::blank(0, 68)] }],
        };
        assert_eq!(round_trip(&g), g);
    }

    #[test]
    fn wrapped_flag_survives() {
        let g = Grid {
            lines: vec![
                Line { flags: line_flags::WRAPPED, runs: vec![Run::text(1, "abc")] },
                Line { flags: 0, runs: vec![Run::text(1, "def")] },
            ],
        };
        let back = round_trip(&g);
        assert_eq!(back, g);
        assert_eq!(back.lines[0].flags & line_flags::WRAPPED, line_flags::WRAPPED);
        assert_eq!(back.lines[1].flags & line_flags::WRAPPED, 0);
    }

    #[test]
    fn wide_run_spans_two_columns_per_character() {
        let r = Run::wide(0, "日本語");
        assert_eq!(r.cell_span, 6, "three wide chars occupy six columns");
        assert_eq!(r.cells(), 3);
        let g = Grid { lines: vec![Line { flags: 0, runs: vec![r] }] };
        assert_eq!(round_trip(&g), g);
    }

    #[test]
    fn combining_marks_use_per_cell_char_counts() {
        // Two cells: "e" plus a combining acute, then a plain "x".
        let r = Run {
            flags: run_flags::CHAR_COUNTS,
            style_id: 0,
            cell_span: 2,
            text: "e\u{0301}x".to_string(),
            char_counts: vec![2, 1],
        };
        let g = Grid { lines: vec![Line { flags: 0, runs: vec![r] }] };
        assert_eq!(round_trip(&g), g);
    }

    #[test]
    fn wide_run_with_odd_span_is_rejected() {
        let bad = Grid {
            lines: vec![Line {
                flags: 0,
                runs: vec![Run { flags: run_flags::WIDE, style_id: 0, cell_span: 3, text: "日".into(), char_counts: vec![] }],
            }],
        };
        let err = Grid::read(&bad.write_unchecked(), 0x40).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: 0x40, why: "wide run must span an even number of columns" });
    }

    #[test]
    fn char_count_that_disagrees_with_the_text_is_rejected() {
        let bad = Grid {
            lines: vec![Line {
                flags: 0,
                runs: vec![Run { flags: run_flags::CHAR_COUNTS, style_id: 0, cell_span: 2, text: "ab".into(), char_counts: vec![1, 5] }],
            }],
        };
        let err = Grid::read(&bad.write_unchecked(), 0x40).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: 0x40, why: "char_counts must sum to the character count of text" });
    }

    #[test]
    fn text_length_that_disagrees_with_the_span_is_rejected() {
        let bad = Grid {
            lines: vec![Line {
                flags: 0,
                runs: vec![Run { flags: 0, style_id: 0, cell_span: 9, text: "abc".into(), char_counts: vec![] }],
            }],
        };
        let err = Grid::read(&bad.write_unchecked(), 0x40).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: 0x40, why: "text character count must equal the run's cell count" });
    }

    #[test]
    fn an_unknown_run_flag_is_rejected() {
        // Bit 2 is not in run_flags::KNOWN. A future version that gave it a
        // grammar — as bit 0 already has — would desynchronise this decoder for
        // the rest of the blob, so it must be an error and not an ignored bit.
        let bad = Grid {
            lines: vec![Line {
                flags: 0,
                runs: vec![Run { flags: 0b100, style_id: 0, cell_span: 1, text: String::new(), char_counts: vec![] }],
            }],
        };
        let err = Grid::read(&bad.write_unchecked(), 0x40).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: 0x40, why: "unknown run flag" });

        // ...and the high bit too, so the check is over the whole byte.
        let bad = Grid {
            lines: vec![Line {
                flags: 0,
                runs: vec![Run { flags: 0x80, style_id: 0, cell_span: 1, text: String::new(), char_counts: vec![] }],
            }],
        };
        assert_eq!(
            Grid::read(&bad.write_unchecked(), 0x40).unwrap_err(),
            WireError::BadValue { tag: 0x40, why: "unknown run flag" }
        );
    }

    #[test]
    fn every_known_run_flag_combination_is_accepted() {
        assert_eq!(run_flags::KNOWN, 0b11);
        for r in [
            Run::blank(0, 4),
            Run::text(0, "abcd"),
            Run::wide(0, "日本"),
            Run { flags: run_flags::CHAR_COUNTS | run_flags::WIDE, style_id: 0, cell_span: 4, text: "日本\u{0301}".into(), char_counts: vec![1, 2] },
        ] {
            let g = Grid { lines: vec![Line { flags: 0, runs: vec![r] }] };
            assert_eq!(round_trip(&g), g);
        }
    }

    #[test]
    fn an_unknown_line_flag_is_ignored_not_rejected() {
        // The opposite decision from run_flags, and deliberately so: line flags
        // are booleans over a fixed grammar, so a bit we cannot draw must not
        // cost the user the whole pane.
        let mut w = Writer::new();
        w.varint(1); // one line
        w.varint(1 << 20); // a line flag from a future version
        w.varint(0); // no runs
        let g = Grid::read(&w.into_vec(), 0x40).unwrap();
        assert_eq!(g.lines[0].flags, 1 << 20);
    }

    #[test]
    fn a_run_dimension_past_u32_is_rejected_not_truncated() {
        let mut w = Writer::new();
        w.varint(1); // one line
        w.varint(0); // line flags
        w.varint(1); // one run
        w.u8(0); // run flags
        w.varint(0); // style_id
        w.varint((1u64 << 32) + 80); // cell_span, one past u32
        w.str("");
        assert_eq!(Grid::read(&w.into_vec(), 0x40).unwrap_err(), WireError::VarintOverflow);
    }

    #[test]
    fn a_short_line_is_not_padded_by_the_decoder() {
        // Trailing blanks are omitted by the producer. Padding to the pane's
        // width is the adopter's job in phase 2, not the decoder's.
        let g = Grid { lines: vec![Line { flags: 0, runs: vec![Run::text(0, "hi")] }] };
        let back = round_trip(&g);
        assert_eq!(back.lines[0].runs.len(), 1);
        assert_eq!(back.lines[0].runs[0].cell_span, 2);
    }

    #[test]
    fn many_lines_round_trip() {
        let lines = (0..500)
            .map(|i| Line { flags: 0, runs: vec![Run::text(0, &format!("line {i}")), Run::blank(0, 40)] })
            .collect();
        let g = Grid { lines };
        assert_eq!(round_trip(&g), g);
    }
}
