//! The pane message: everything about one pane except its file descriptors.
//!
//! Field numbers come from the spec's PaneState table and are frozen. Adding a
//! field means adding a tag, never changing one.

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
    pub const STYLE_TABLE: u64 = 0x3F;
    pub const SCREEN_PRIMARY: u64 = 0x40;
    pub const SCREEN_ALT: u64 = 0x41;
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
        tags::STYLE_TABLE => "style_table",
        tags::SCREEN_PRIMARY => "screen_primary",
        tags::SCREEN_ALT => "screen_alt",
        _ => return None,
    })
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
    pub style_table: Vec<Style>,
    pub screen_primary: Grid,
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
        /// Encode one field's value with a fresh writer.
        fn enc(f: impl FnOnce(&mut crate::buf::Writer)) -> Vec<u8> {
            let mut w = crate::buf::Writer::new();
            f(&mut w);
            w.into_vec()
        }

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
            let mut r = crate::buf::Reader::new(val);
            match tag {
                tags::PANE_UID => p.pane_uid = r.varint()?,
                tags::TITLE => p.title = r.str()?,
                tags::CWD => p.cwd = Some(r.str()?),
                tags::COLS => p.cols = r.varint()? as u32,
                tags::ROWS => p.rows = r.varint()? as u32,
                tags::SCROLLBACK_LIMIT => p.scrollback_limit = Some(r.varint()? as u32),
                tags::COLUMNS_COUNT => p.columns_count = Some(r.varint()? as u32),
                tags::GROUP => p.group = Some(r.varint()? as u32),
                tags::BROADCAST => p.broadcast = Some(r.u8()? != 0),
                tags::CHILD_PID => p.child_pid = r.varint()? as u32,
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
    use crate::style::Style;

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
}
