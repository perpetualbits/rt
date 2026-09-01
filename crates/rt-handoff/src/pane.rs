//! The pane message: everything about one pane except its file descriptors.
//!
//! Field numbers come from the spec's PaneState table and are frozen. Adding a
//! field means adding a tag, never changing one.

use crate::buf::enc;
use crate::error::{Result, WireError};
use crate::grid::Grid;
use crate::style::{self, Style};
use crate::tlv::{self, FieldWriter};

/// Field tags. Frozen: see rule R1. Task 7 adds 0x20–0x2A, 0x50 and 0x51.
pub mod tags {
    pub const PANE_UID: u64 = 0x01;
    pub const TITLE: u64 = 0x02;
    pub const CWD: u64 = 0x03;
    pub const COLS: u64 = 0x04;
    pub const ROWS: u64 = 0x05;
    pub const SCROLLBACK_LIMIT: u64 = 0x06;
    pub const COLUMNS_COUNT: u64 = 0x07;
    pub const GROUP: u64 = 0x08;
    pub const BROADCAST: u64 = 0x09;
    pub const CHILD_PID: u64 = 0x0A;
    pub const SHELL_ARGV: u64 = 0x0B;
    pub const ENV_EXTRAS: u64 = 0x0C;
    pub const SHOW_TITLEBAR: u64 = 0x0D;
    pub const PALETTE: u64 = 0x0E;
    pub const TAG_NAMES: u64 = 0x0F;
    pub const MODES: u64 = 0x20;
    pub const CURSOR: u64 = 0x21;
    pub const SAVED_CURSOR: u64 = 0x22;
    pub const CHARSETS: u64 = 0x23;
    pub const TAB_STOPS: u64 = 0x24;
    pub const MARGINS: u64 = 0x25;
    pub const TITLE_STACK: u64 = 0x26;
    pub const PEN: u64 = 0x27;
    pub const ACTIVE_SCREEN: u64 = 0x28;
    pub const KITTY_KBD: u64 = 0x29;
    pub const PENDING_RAW: u64 = 0x2A;
    pub const STYLE_TABLE: u64 = 0x3F;
    pub const SCREEN_PRIMARY: u64 = 0x40;
    pub const SCREEN_ALT: u64 = 0x41;
    pub const URI_TABLE: u64 = 0x50;
    pub const IMAGE_TABLE: u64 = 0x51;
}

/// The short name a receiver prints when it skips a field it does not know
/// (rule R6). Donors ship this table so an old receiver can still name a new
/// field; a tag with no name is reported by number.
pub fn name_of_tag(tag: u64) -> Option<&'static str> {
    Some(match tag {
        tags::PANE_UID => "pane_uid",
        tags::TITLE => "title",
        tags::CWD => "cwd",
        tags::COLS => "cols",
        tags::ROWS => "rows",
        tags::SCROLLBACK_LIMIT => "scrollback_limit",
        tags::COLUMNS_COUNT => "columns",
        tags::GROUP => "group",
        tags::BROADCAST => "broadcast",
        tags::CHILD_PID => "child_pid",
        tags::SHELL_ARGV => "shell_argv",
        tags::ENV_EXTRAS => "env_extras",
        tags::SHOW_TITLEBAR => "show_titlebar",
        tags::PALETTE => "palette",
        tags::TAG_NAMES => "tag_names",
        tags::MODES => "modes",
        tags::CURSOR => "cursor",
        tags::SAVED_CURSOR => "saved_cursor",
        tags::CHARSETS => "charsets",
        tags::TAB_STOPS => "tab_stops",
        tags::MARGINS => "margins",
        tags::TITLE_STACK => "title_stack",
        tags::PEN => "pen",
        tags::ACTIVE_SCREEN => "active_screen",
        tags::KITTY_KBD => "kitty_kbd",
        tags::PENDING_RAW => "pending_raw",
        tags::STYLE_TABLE => "style_table",
        tags::SCREEN_PRIMARY => "screen_primary",
        tags::SCREEN_ALT => "screen_alt",
        tags::URI_TABLE => "uri_table",
        tags::IMAGE_TABLE => "image_table",
        _ => return None,
    })
}

/// One terminal mode, identified the way the VT spec identifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeEntry {
    /// 0 = ANSI mode, 1 = DEC private mode. Two distinct namespaces.
    pub kind: u8,
    /// The mode number itself: 1 DECCKM, 7 DECAWM, 25 DECTCEM, 2004 bracketed
    /// paste, 1049 alt screen, 2026 synchronised update, and so on.
    pub number: u32,
    pub value: u8,
}

/// Where the cursor is and how it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorState {
    pub col: u32,
    pub row: u32,
    /// 0 block, 1 underline, 2 bar.
    pub shape: u8,
    pub visible: bool,
    pub blink: bool,
    /// The deferred-wrap flag: the cursor sits past the last column and the
    /// next printable character wraps first. Dropping it misplaces output.
    pub pending_wrap: bool,
}

/// DECSC state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SavedCursor {
    pub col: u32,
    pub row: u32,
    pub pen: Style,
    pub charsets: Charsets,
    pub origin: bool,
}

/// G0..G3 designators plus the GL/GR locking shifts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charsets {
    /// The final character of each designation sequence: b'B' ASCII, b'0' DEC
    /// graphics, and so on.
    pub g: [u8; 4],
    pub gl: u8,
    pub gr: u8,
}

impl Default for Charsets {
    fn default() -> Self {
        Charsets { g: [b'B'; 4], gl: 0, gr: 0 }
    }
}

/// Scrolling and horizontal margins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margins {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

/// Kitty keyboard protocol flag stack plus the xterm modifyOtherKeys level.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KittyKbd {
    pub stack: Vec<u32>,
    pub modify_other_keys: u8,
}

/// One inline image. `format` is 1 for PNG; other values are reserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageEntry {
    pub id: u32,
    pub format: u8,
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

/// One pane, ready to be handed to another rt process.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneWire {
    pub pane_uid: u64,
    pub title: String,
    pub cwd: Option<String>,
    pub cols: u32,
    pub rows: u32,
    pub scrollback_limit: Option<u32>,
    pub columns_count: Option<u32>,
    pub group: Option<u32>,
    pub broadcast: Option<bool>,
    pub child_pid: u32,
    pub shell_argv: Vec<String>,
    pub env_extras: Vec<(String, String)>,
    pub show_titlebar: Option<bool>,
    /// Exactly 256 entries when present.
    pub palette: Option<Vec<(u8, u8, u8)>>,
    pub modes: Vec<ModeEntry>,
    pub cursor: CursorState,
    pub saved_cursor: Option<SavedCursor>,
    pub charsets: Option<Charsets>,
    /// One entry per column.
    pub tab_stops: Option<Vec<bool>>,
    pub margins: Option<Margins>,
    pub title_stack: Vec<String>,
    /// The current SGR state, so output after the move continues in the same
    /// attributes.
    pub pen: Style,
    /// 0 primary, 1 alt.
    pub active_screen: u8,
    pub kitty_kbd: Option<KittyKbd>,
    /// Bytes of an escape sequence the donor's parser had not finished, to be
    /// replayed into the receiver's parser before anything else.
    pub pending_raw: Vec<u8>,
    pub uri_table: Vec<(u32, String)>,
    pub image_table: Vec<ImageEntry>,
    pub style_table: Vec<Style>,
    /// The VISIBLE screen — **not** the ANSI primary screen.
    ///
    /// The name is frozen and it misleads: when [`active_screen`](Self::active_screen)
    /// is 1 this holds the ALT screen's content, and `screen_alt` holds the
    /// primary's, held aside. Read it as "the screen that is showing", with
    /// `active_screen` saying which one that is. A receiver that renders
    /// `screen_primary` and honours `active_screen` is correct; one that assumes
    /// this is the primary screen paints the alt screen's content into the
    /// primary and then swaps it away.
    pub screen_primary: Grid,
    /// The screen held aside, if either screen is. Its own cursor, pen, charsets
    /// and pending-wrap do NOT travel — see "What does not survive a move".
    pub screen_alt: Option<Grid>,
    /// Names for every tag the donor emitted (0x0F). Filled on decode.
    pub tag_names: Vec<(u64, String)>,
    /// Tags this build skipped. Always empty when encoding. Rule R6.
    pub unknown_tags: Vec<u64>,
}

impl PaneWire {
    /// Every field this pane emits, as `(tag, bytes)` in ascending tag order,
    /// `tag_names` included. `encode` is just this list written out, so a test
    /// can drop or corrupt one field and still produce an otherwise-valid body.
    pub fn fields(&self) -> Vec<(u64, Vec<u8>)> {
        let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
        out.push((tags::PANE_UID, enc(|w| w.varint(self.pane_uid))));
        out.push((tags::TITLE, enc(|w| w.str(&self.title))));
        if let Some(cwd) = &self.cwd {
            out.push((tags::CWD, enc(|w| w.str(cwd))));
        }
        out.push((tags::COLS, enc(|w| w.varint(self.cols as u64))));
        out.push((tags::ROWS, enc(|w| w.varint(self.rows as u64))));
        if let Some(v) = self.scrollback_limit {
            out.push((tags::SCROLLBACK_LIMIT, enc(|w| w.varint(v as u64))));
        }
        if let Some(v) = self.columns_count {
            out.push((tags::COLUMNS_COUNT, enc(|w| w.varint(v as u64))));
        }
        if let Some(v) = self.group {
            out.push((tags::GROUP, enc(|w| w.varint(v as u64))));
        }
        if let Some(v) = self.broadcast {
            out.push((tags::BROADCAST, enc(|w| w.u8(v as u8))));
        }
        out.push((tags::CHILD_PID, enc(|w| w.varint(self.child_pid as u64))));
        if !self.shell_argv.is_empty() {
            out.push((tags::SHELL_ARGV, enc(|w| {
                w.varint(self.shell_argv.len() as u64);
                for a in &self.shell_argv {
                    w.str(a);
                }
            })));
        }
        if !self.env_extras.is_empty() {
            out.push((tags::ENV_EXTRAS, enc(|w| {
                w.varint(self.env_extras.len() as u64);
                for (k, v) in &self.env_extras {
                    w.str(k);
                    w.str(v);
                }
            })));
        }
        if let Some(v) = self.show_titlebar {
            out.push((tags::SHOW_TITLEBAR, enc(|w| w.u8(v as u8))));
        }
        if let Some(pal) = &self.palette {
            out.push((tags::PALETTE, enc(|w| {
                for (r, g, b) in pal {
                    w.u8(*r);
                    w.u8(*g);
                    w.u8(*b);
                }
            })));
        }
        out.push((tags::MODES, enc(|w| {
            w.varint(self.modes.len() as u64);
            for m in &self.modes {
                w.u8(m.kind);
                w.varint(m.number as u64);
                w.u8(m.value);
            }
        })));
        out.push((tags::CURSOR, enc(|w| {
            w.varint(self.cursor.col as u64);
            w.varint(self.cursor.row as u64);
            w.u8(self.cursor.shape);
            w.u8(self.cursor.visible as u8);
            w.u8(self.cursor.blink as u8);
            w.u8(self.cursor.pending_wrap as u8);
        })));
        if let Some(sc) = &self.saved_cursor {
            out.push((tags::SAVED_CURSOR, enc(|w| {
                w.varint(sc.col as u64);
                w.varint(sc.row as u64);
                sc.pen.write(w);
                w.raw(&sc.charsets.g);
                w.u8(sc.charsets.gl);
                w.u8(sc.charsets.gr);
                w.u8(sc.origin as u8);
            })));
        }
        if let Some(cs) = &self.charsets {
            out.push((tags::CHARSETS, enc(|w| {
                w.raw(&cs.g);
                w.u8(cs.gl);
                w.u8(cs.gr);
            })));
        }
        if let Some(stops) = &self.tab_stops {
            out.push((tags::TAB_STOPS, enc(|w| {
                w.varint(stops.len() as u64);
                for chunk in stops.chunks(8) {
                    let mut byte = 0u8;
                    for (i, on) in chunk.iter().enumerate() {
                        if *on {
                            byte |= 1 << i;
                        }
                    }
                    w.u8(byte);
                }
            })));
        }
        if let Some(m) = &self.margins {
            out.push((tags::MARGINS, enc(|w| {
                w.varint(m.top as u64);
                w.varint(m.bottom as u64);
                w.varint(m.left as u64);
                w.varint(m.right as u64);
            })));
        }
        if !self.title_stack.is_empty() {
            out.push((tags::TITLE_STACK, enc(|w| {
                w.varint(self.title_stack.len() as u64);
                for t in &self.title_stack {
                    w.str(t);
                }
            })));
        }
        out.push((tags::PEN, enc(|w| self.pen.write(w))));
        out.push((tags::ACTIVE_SCREEN, enc(|w| w.u8(self.active_screen))));
        if let Some(k) = &self.kitty_kbd {
            out.push((tags::KITTY_KBD, enc(|w| {
                w.varint(k.stack.len() as u64);
                for f in &k.stack {
                    w.varint(*f as u64);
                }
                w.u8(k.modify_other_keys);
            })));
        }
        if !self.pending_raw.is_empty() {
            out.push((tags::PENDING_RAW, enc(|w| w.bytes(&self.pending_raw))));
        }
        if !self.uri_table.is_empty() {
            out.push((tags::URI_TABLE, enc(|w| {
                w.varint(self.uri_table.len() as u64);
                for (id, uri) in &self.uri_table {
                    w.varint(*id as u64);
                    w.str(uri);
                }
            })));
        }
        if !self.image_table.is_empty() {
            out.push((tags::IMAGE_TABLE, enc(|w| {
                w.varint(self.image_table.len() as u64);
                for img in &self.image_table {
                    w.varint(img.id as u64);
                    w.u8(img.format);
                    w.varint(img.w as u64);
                    w.varint(img.h as u64);
                    w.bytes(&img.data);
                }
            })));
        }
        out.push((tags::STYLE_TABLE, style::write_table(&self.style_table)));
        out.push((tags::SCREEN_PRIMARY, self.screen_primary.write()));
        if let Some(alt) = &self.screen_alt {
            out.push((tags::SCREEN_ALT, alt.write()));
        }

        // tag_names names every tag in the message, itself included, so an old
        // receiver can print a name for a field it does not understand (R6).
        let mut names: Vec<(u64, &'static str)> = out
            .iter()
            .filter_map(|(t, _)| name_of_tag(*t).map(|n| (*t, n)))
            .collect();
        names.push((tags::TAG_NAMES, "tag_names"));
        names.sort_by_key(|(t, _)| *t);
        out.push((tags::TAG_NAMES, enc(|w| {
            w.varint(names.len() as u64);
            for (t, n) in &names {
                w.varint(*t);
                w.str(n);
            }
        })));

        out.sort_by_key(|(tag, _)| *tag);
        out
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut fw = FieldWriter::new();
        for (tag, val) in &self.fields() {
            fw.field(*tag, val);
        }
        fw.into_vec()
    }
}

impl PaneWire {
    pub fn decode(body: &[u8]) -> Result<PaneWire> {
        let mut p = PaneWire::default();
        let mut seen: Vec<u64> = Vec::new();

        let unknown = tlv::walk(body, |tag, val| {
            // A repeated known tag is malformed. `shell_argv` would ACCUMULATE
            // and `title` would OVERWRITE, so `decode -> encode` would not be
            // idempotent for such a body — the exact property the golden corpus
            // rests on. Unknown tags are exempt: R2 says they are never an error.
            if seen.contains(&tag) {
                return Err(WireError::BadValue { tag, why: "duplicate tag" });
            }
            let mut r = crate::buf::Reader::new(val);
            match tag {
                tags::PANE_UID => p.pane_uid = r.varint()?,
                tags::TITLE => p.title = r.str()?,
                tags::CWD => p.cwd = Some(r.str()?),
                tags::COLS => p.cols = r.varint_u32()?,
                tags::ROWS => p.rows = r.varint_u32()?,
                tags::SCROLLBACK_LIMIT => p.scrollback_limit = Some(r.varint_u32()?),
                tags::COLUMNS_COUNT => p.columns_count = Some(r.varint_u32()?),
                tags::GROUP => p.group = Some(r.varint_u32()?),
                tags::BROADCAST => p.broadcast = Some(r.u8()? != 0),
                tags::CHILD_PID => p.child_pid = r.varint_u32()?,
                tags::SHELL_ARGV => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        p.shell_argv.push(r.str()?);
                    }
                }
                tags::ENV_EXTRAS => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        let k = r.str()?;
                        let v = r.str()?;
                        p.env_extras.push((k, v));
                    }
                }
                tags::SHOW_TITLEBAR => p.show_titlebar = Some(r.u8()? != 0),
                tags::PALETTE => {
                    if val.len() != 256 * 3 {
                        return Err(WireError::BadValue { tag, why: "palette must hold exactly 256 entries" });
                    }
                    let mut pal = Vec::with_capacity(256);
                    for _ in 0..256 {
                        pal.push((r.u8()?, r.u8()?, r.u8()?));
                    }
                    p.palette = Some(pal);
                }
                tags::TAG_NAMES => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        let t = r.varint()?;
                        let name = r.str()?;
                        p.tag_names.push((t, name));
                    }
                }
                tags::STYLE_TABLE => {
                    p.style_table = style::read_table(val)?;
                    seen.push(tag);
                    return Ok(true); // read_table consumed the whole value
                }
                tags::SCREEN_PRIMARY => {
                    p.screen_primary = Grid::read(val, tag)?;
                    seen.push(tag);
                    return Ok(true);
                }
                tags::SCREEN_ALT => {
                    p.screen_alt = Some(Grid::read(val, tag)?);
                    seen.push(tag);
                    return Ok(true);
                }
                tags::MODES => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        let kind = r.u8()?;
                        let number = r.varint_u32()?;
                        let value = r.u8()?;
                        p.modes.push(ModeEntry { kind, number, value });
                    }
                }
                tags::CURSOR => {
                    p.cursor = CursorState {
                        col: r.varint_u32()?,
                        row: r.varint_u32()?,
                        shape: r.u8()?,
                        visible: r.u8()? != 0,
                        blink: r.u8()? != 0,
                        pending_wrap: r.u8()? != 0,
                    };
                }
                tags::SAVED_CURSOR => {
                    let col = r.varint_u32()?;
                    let row = r.varint_u32()?;
                    let pen = Style::read(&mut r, tag)?;
                    let gb = r.take(4)?;
                    let charsets = Charsets { g: [gb[0], gb[1], gb[2], gb[3]], gl: r.u8()?, gr: r.u8()? };
                    p.saved_cursor = Some(SavedCursor { col, row, pen, charsets, origin: r.u8()? != 0 });
                }
                tags::CHARSETS => {
                    let gb = r.take(4)?;
                    p.charsets = Some(Charsets { g: [gb[0], gb[1], gb[2], gb[3]], gl: r.u8()?, gr: r.u8()? });
                }
                tags::TAB_STOPS => {
                    let n = r.varint()? as usize;
                    let bytes = r.take(n.div_ceil(8))?; // one byte per eight columns, LSB first
                    let mut stops = Vec::with_capacity(n.min(65536));
                    for i in 0..n {
                        stops.push(bytes[i / 8] & (1 << (i % 8)) != 0);
                    }
                    p.tab_stops = Some(stops);
                }
                tags::MARGINS => {
                    p.margins = Some(Margins {
                        top: r.varint_u32()?,
                        bottom: r.varint_u32()?,
                        left: r.varint_u32()?,
                        right: r.varint_u32()?,
                    });
                }
                tags::TITLE_STACK => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        p.title_stack.push(r.str()?);
                    }
                }
                tags::PEN => p.pen = Style::read(&mut r, tag)?,
                tags::ACTIVE_SCREEN => {
                    let v = r.u8()?;
                    if v > 1 {
                        return Err(WireError::BadValue { tag, why: "active_screen must be 0 or 1" });
                    }
                    p.active_screen = v;
                }
                tags::KITTY_KBD => {
                    let n = r.varint()? as usize;
                    let mut stack = Vec::with_capacity(n.min(256));
                    for _ in 0..n {
                        stack.push(r.varint_u32()?);
                    }
                    p.kitty_kbd = Some(KittyKbd { stack, modify_other_keys: r.u8()? });
                }
                tags::PENDING_RAW => p.pending_raw = r.bytes()?.to_vec(),
                tags::URI_TABLE => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        let id = r.varint_u32()?;
                        let uri = r.str()?;
                        p.uri_table.push((id, uri));
                    }
                }
                tags::IMAGE_TABLE => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        let id = r.varint_u32()?;
                        let format = r.u8()?;
                        let w = r.varint_u32()?;
                        let h = r.varint_u32()?;
                        let data = r.bytes()?.to_vec();
                        p.image_table.push(ImageEntry { id, format, w, h, data });
                    }
                }
                _ => return Ok(false),
            }
            r.finish()?;
            seen.push(tag);
            Ok(true)
        })?;

        for required in [
            tags::PANE_UID,
            tags::TITLE,
            tags::COLS,
            tags::ROWS,
            tags::CHILD_PID,
            tags::MODES,
            tags::CURSOR,
            tags::PEN,
            tags::TAG_NAMES,
            tags::STYLE_TABLE,
            tags::SCREEN_PRIMARY,
        ] {
            if !seen.contains(&required) {
                return Err(WireError::MissingField { tag: required });
            }
        }

        p.unknown_tags = unknown;
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;
    use crate::grid::{Grid, Line, Run};
    use crate::style::{Colour, Style};

    /// Encode, decode, compare — normalising `tag_names`, which the ENCODER
    /// produces and the DECODER reads back, so a freshly-built pane never has
    /// it. Everything else must survive the trip untouched.
    fn assert_round_trips(p: &PaneWire) {
        let back = PaneWire::decode(&p.encode()).unwrap();
        let mut expected = p.clone();
        expected.tag_names = back.tag_names.clone();
        assert_eq!(back, expected);
    }

    /// The smallest pane that satisfies every required field.
    fn minimal() -> PaneWire {
        PaneWire {
            pane_uid: 42,
            title: "zsh".to_string(),
            cols: 80,
            rows: 24,
            child_pid: 1234,
            modes: vec![ModeEntry { kind: 1, number: 7, value: 1 }],
            cursor: CursorState { col: 0, row: 0, shape: 0, visible: true, blink: true, pending_wrap: false },
            pen: Style::default(),
            style_table: vec![Style::default()],
            screen_primary: Grid { lines: vec![Line { flags: 0, runs: vec![Run::blank(0, 80)] }] },
            ..PaneWire::default()
        }
    }

    #[test]
    fn minimal_pane_round_trips() {
        assert_round_trips(&minimal());
    }

    #[test]
    fn every_optional_field_round_trips() {
        let p = PaneWire {
            cwd: Some("/home/roland/git/rt".to_string()),
            scrollback_limit: Some(50_000),
            columns_count: Some(3),
            group: Some(2),
            broadcast: Some(true),
            shell_argv: vec!["/bin/zsh".to_string(), "-l".to_string()],
            env_extras: vec![("RT_OUT".to_string(), "/run/user/1000/rt/jacks/a/out".to_string())],
            show_titlebar: Some(false),
            palette: Some((0..256).map(|i| (i as u8, 0, 0)).collect()),
            screen_alt: Some(Grid { lines: vec![Line { flags: 0, runs: vec![Run::text(0, "alt")] }] }),
            ..minimal()
        };
        assert_round_trips(&p);
    }

    #[test]
    fn absent_optionals_stay_absent() {
        let back = PaneWire::decode(&minimal().encode()).unwrap();
        assert_eq!(back.cwd, None);
        assert_eq!(back.columns_count, None);
        assert_eq!(back.screen_alt, None);
        assert!(back.shell_argv.is_empty());
    }

    #[test]
    fn tag_names_covers_every_emitted_tag() {
        let bytes = minimal().encode();
        let emitted = crate::tlv::tags_of(&bytes).unwrap();
        let back = PaneWire::decode(&bytes).unwrap();
        for tag in emitted {
            assert!(back.tag_names.iter().any(|(t, _)| *t == tag), "tag 0x{tag:02x} has no name");
        }
        // 0x0F names itself, so a receiver can report it too.
        assert!(back.tag_names.iter().any(|(t, n)| *t == tags::TAG_NAMES && n == "tag_names"));
    }

    #[test]
    fn fields_are_emitted_in_ascending_order() {
        let tags = crate::tlv::tags_of(&minimal().encode()).unwrap();
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        assert_eq!(tags, sorted, "R7 violated");
    }

    #[test]
    fn unknown_tags_are_skipped_and_recorded() {
        // Splice a field from an imagined future version between 0x40 and the end.
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in minimal().fields() {
            fw.field(tag, &val);
        }
        fw.field(0x7000, b"something from rt 0.9");
        let body = fw.into_vec();

        let back = PaneWire::decode(&body).unwrap();
        assert_eq!(back.unknown_tags, vec![0x7000]);
        assert_eq!(back.title, "zsh", "known fields must be unaffected");
    }

    #[test]
    fn each_required_field_is_enforced() {
        for missing in [tags::PANE_UID, tags::TITLE, tags::COLS, tags::ROWS, tags::CHILD_PID, tags::TAG_NAMES, tags::STYLE_TABLE, tags::SCREEN_PRIMARY] {
            let mut fw = crate::tlv::FieldWriter::new();
            for (tag, val) in minimal().fields() {
                if tag != missing {
                    fw.field(tag, &val);
                }
            }
            let err = PaneWire::decode(&fw.into_vec()).unwrap_err();
            assert_eq!(err, WireError::MissingField { tag: missing }, "tag 0x{missing:02x}");
        }
    }

    #[test]
    fn a_palette_of_the_wrong_length_is_rejected() {
        let mut fields = minimal().fields();
        fields.push((tags::PALETTE, vec![1, 2, 3])); // three bytes, not 256 * 3
        fields.sort_by_key(|(t, _)| *t);
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in &fields {
            fw.field(*tag, val);
        }
        let err = PaneWire::decode(&fw.into_vec()).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: tags::PALETTE, why: "palette must hold exactly 256 entries" });
    }

    #[test]
    fn a_large_pane_round_trips_and_stays_compact() {
        let lines = (0..5_000)
            .map(|i| Line { flags: 0, runs: vec![Run::text(0, &format!("[{i:05}] make[2]: Entering directory")), Run::blank(0, 140)] })
            .collect();
        let p = PaneWire { screen_primary: Grid { lines }, ..minimal() };
        let bytes = p.encode();
        assert_round_trips(&p);
        // Run-length encoding is why the spec specifies no compression: a
        // typical line must cost tens of bytes, not hundreds.
        assert!(bytes.len() < 5_000 * 60, "5k lines took {} bytes", bytes.len());
    }

    #[test]
    fn modes_travel_by_their_dec_number() {
        let p = PaneWire {
            modes: vec![
                ModeEntry { kind: 1, number: 1, value: 1 },    // DECCKM on
                ModeEntry { kind: 1, number: 2004, value: 1 }, // bracketed paste on
                ModeEntry { kind: 1, number: 1049, value: 0 }, // alt screen off
                ModeEntry { kind: 0, number: 20, value: 1 },   // LNM on (ANSI, not DEC)
            ],
            ..minimal()
        };
        let back = PaneWire::decode(&p.encode()).unwrap();
        assert_eq!(back.modes, p.modes);
        // The ANSI/DEC namespaces are distinct: mode 20 in each is a different mode.
        assert_eq!(back.modes[3].kind, 0);
    }

    #[test]
    fn a_mode_number_this_build_never_heard_of_survives_the_trip() {
        // The decoder does not filter modes: it is not the transport's job to
        // decide which modes an engine supports.
        let p = PaneWire { modes: vec![ModeEntry { kind: 1, number: 65000, value: 1 }], ..minimal() };
        assert_eq!(PaneWire::decode(&p.encode()).unwrap().modes, p.modes);
    }

    #[test]
    fn cursor_state_round_trips_including_pending_wrap() {
        let p = PaneWire {
            cursor: CursorState { col: 79, row: 3, shape: 2, visible: true, blink: false, pending_wrap: true },
            ..minimal()
        };
        let back = PaneWire::decode(&p.encode()).unwrap();
        assert_eq!(back.cursor, p.cursor);
        assert!(back.cursor.pending_wrap, "deferred wrap is part of the cursor, not a detail");
    }

    #[test]
    fn the_whole_terminal_state_round_trips() {
        let p = PaneWire {
            modes: vec![ModeEntry { kind: 1, number: 7, value: 1 }],
            cursor: CursorState { col: 1, row: 2, shape: 1, visible: true, blink: true, pending_wrap: false },
            saved_cursor: Some(SavedCursor {
                col: 5,
                row: 6,
                pen: Style { fg: Colour::Indexed(3), ..Style::default() },
                charsets: Charsets { g: [b'B', b'0', b'B', b'B'], gl: 0, gr: 2 },
                origin: true,
            }),
            charsets: Some(Charsets { g: [b'B', b'B', b'B', b'B'], gl: 0, gr: 0 }),
            tab_stops: Some((0..80).map(|i| i % 8 == 0).collect()),
            margins: Some(Margins { top: 1, bottom: 22, left: 0, right: 79 }),
            title_stack: vec!["one".into(), "two".into()],
            pen: Style { attrs: crate::style::attrs::BOLD, ..Style::default() },
            active_screen: 1,
            kitty_kbd: Some(KittyKbd { stack: vec![1, 5], modify_other_keys: 2 }),
            pending_raw: vec![0x1b, b'[', b'3'],
            uri_table: vec![(1, "https://example.invalid/a".into())],
            image_table: vec![ImageEntry { id: 9, format: 1, w: 4, h: 2, data: vec![1, 2, 3, 4] }],
            ..minimal()
        };
        assert_round_trips(&p);
    }

    #[test]
    fn pending_raw_carries_an_incomplete_sequence_verbatim() {
        // The freeze captures a half-parsed escape; the receiver replays the
        // bytes into its own parser. Raw bytes mean the same thing in every
        // build, which is why the format ships them rather than parser state.
        let p = PaneWire { pending_raw: vec![0x1b, b'[', b'3', b'8', b';', b'5'], ..minimal() };
        assert_eq!(PaneWire::decode(&p.encode()).unwrap().pending_raw, p.pending_raw);
    }

    #[test]
    fn the_new_required_fields_are_enforced() {
        for missing in [tags::MODES, tags::CURSOR, tags::PEN] {
            let mut fw = crate::tlv::FieldWriter::new();
            for (tag, val) in minimal().fields() {
                if tag != missing {
                    fw.field(tag, &val);
                }
            }
            let err = PaneWire::decode(&fw.into_vec()).unwrap_err();
            assert_eq!(err, WireError::MissingField { tag: missing }, "tag 0x{missing:02x}");
        }
    }

    #[test]
    fn a_repeated_known_tag_is_rejected() {
        // Two cases with different symptoms, both fatal to idempotence:
        // shell_argv ACCUMULATES, title OVERWRITES. Either way `decode ->
        // encode` stops reproducing the bytes, which is the property the golden
        // corpus rests on.
        for (tag, second) in [
            (tags::SHELL_ARGV, crate::buf::enc(|w| {
                w.varint(1);
                w.str("-l");
            })),
            (tags::TITLE, crate::buf::enc(|w| w.str("a different title"))),
        ] {
            let mut p = minimal();
            p.shell_argv = vec!["/bin/zsh".into()];
            // FieldWriter enforces ascending order, so the repeat is spliced by hand.
            let mut body = p.encode();
            body.extend_from_slice(&crate::buf::enc(|w| {
                w.varint(tag);
                w.bytes(&second);
            }));
            assert_eq!(
                PaneWire::decode(&body).unwrap_err(),
                WireError::BadValue { tag, why: "duplicate tag" },
                "tag 0x{tag:02x}"
            );
        }
    }

    #[test]
    fn a_repeated_unknown_tag_is_still_skipped() {
        // R2 is not weakened by the duplicate check: it binds known tags only.
        let mut body = minimal().encode();
        for _ in 0..2 {
            body.extend_from_slice(&crate::buf::enc(|w| {
                w.varint(0x7000);
                w.bytes(b"from rt 0.9");
            }));
        }
        assert_eq!(PaneWire::decode(&body).unwrap().unknown_tags, vec![0x7000, 0x7000]);
    }

    #[test]
    fn a_dimension_past_u32_is_rejected_rather_than_truncated() {
        // The silent case this closes: 2^32 + 80 must not decode to cols 80.
        let mut fields = minimal().fields();
        fields.retain(|(t, _)| *t != tags::COLS);
        fields.push((tags::COLS, crate::buf::enc(|w| w.varint((1u64 << 32) + 80))));
        fields.sort_by_key(|(t, _)| *t);
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in &fields {
            fw.field(*tag, val);
        }
        assert_eq!(PaneWire::decode(&fw.into_vec()).unwrap_err(), WireError::VarintOverflow);
    }

    #[test]
    fn active_screen_must_be_zero_or_one() {
        let mut fields = minimal().fields();
        fields.retain(|(t, _)| *t != tags::ACTIVE_SCREEN);
        fields.push((tags::ACTIVE_SCREEN, vec![7]));
        fields.sort_by_key(|(t, _)| *t);
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in &fields {
            fw.field(*tag, val);
        }
        let err = PaneWire::decode(&fw.into_vec()).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: tags::ACTIVE_SCREEN, why: "active_screen must be 0 or 1" });
    }
}
