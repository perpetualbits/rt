//! Every committed v1 payload must still decode, and must re-encode to the
//! same bytes. This is the cross-version guarantee, checkable with one build.

use std::path::{Path, PathBuf};

use rt_handoff::frame::Frame;
use rt_handoff::grid::run_flags;
use rt_handoff::msg::Message;
use rt_handoff::pane::{Charsets, CursorState, Margins, ModeEntry, PaneWire, SavedCursor};
use rt_handoff::style::{Colour, Style};
use rt_handoff::tree::Node;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wire-v1")
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir()).expect("the fixture directory must exist") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        out.push((name, std::fs::read(&path).unwrap()));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn the_corpus_is_present_and_complete() {
    // MANIFEST.txt pins every fixture's name and length, so a fixture that is
    // deleted, truncated, or changed in LENGTH fails here rather than silently
    // shrinking the guarantee. It does not catch a rewrite that keeps the
    // length — `every_fixture_decodes_and_re_encodes_byte_for_byte` and the
    // exact-value tests below are what stand between us and that.
    let manifest = std::fs::read_to_string(dir().join("MANIFEST.txt")).expect("MANIFEST.txt must exist");
    let mut expected: Vec<(String, usize)> = manifest
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (name, len) = l.rsplit_once(' ').expect("MANIFEST line is `<name> <length>`");
            (name.to_string(), len.parse().unwrap())
        })
        .collect();
    expected.sort();

    let actual: Vec<(String, usize)> = fixtures().into_iter().map(|(n, b)| (n, b.len())).collect();
    assert_eq!(actual, expected, "the corpus does not match MANIFEST.txt");
    assert!(actual.len() >= 32, "the corpus must not shrink: {} fixtures", actual.len());
}

#[test]
fn every_fixture_decodes_and_re_encodes_byte_for_byte() {
    for (name, bytes) in fixtures() {
        let re_encoded = round_trip(&name, &bytes);
        assert_eq!(
            re_encoded,
            bytes,
            "{name}: re-encoding changed the bytes. The format is frozen — if this is \
             a deliberate protocol change, it is a BREAKING one; read the fixtures README."
        );
    }
}

/// Decode a fixture by its filename prefix and encode it straight back.
fn round_trip(name: &str, bytes: &[u8]) -> Vec<u8> {
    if name.starts_with("pane_") {
        PaneWire::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}")).encode()
    } else if name.starts_with("tree_") {
        Node::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}")).encode()
    } else if name.starts_with("msg_") {
        let (frame, used) = Frame::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(used, bytes.len(), "{name}: trailing bytes after the frame");
        let msg = Message::from_frame(&frame).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut out = Vec::new();
        msg.to_frame().unwrap().encode(&mut out).unwrap();
        out
    } else {
        panic!("{name}: fixture name must start with pane_, tree_ or msg_");
    }
}

fn pane_fixture(name: &str) -> PaneWire {
    let bytes = std::fs::read(dir().join(name)).unwrap_or_else(|e| panic!("{name} must exist: {e}"));
    PaneWire::decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn a_known_fixture_decodes_to_known_values() {
    // EXACT values, not ranges. The corpus pins the byte layout and the tag
    // numbers, and `every_fixture_decodes_and_re_encodes_byte_for_byte` pins
    // that a decode/encode pair agrees with itself — but none of that pins the
    // MEANING of a position inside a value. Swap two same-width fields in an
    // encoder and its decoder together and the bytes never move; only what they
    // mean does. `Colour`'s a/b/c, `Style`'s fg/bg/underline, `Margins`' four
    // fields and `CursorState`'s six were all open to exactly that. Ranges
    // restating the generator's own bounds cannot close it; these values,
    // observed by decoding the committed fixture, can.
    let p = pane_fixture("pane_seed00042.bin");

    assert_eq!(p.pane_uid, 12_685_210_767_805_585_150);
    assert_eq!(p.title, "ß★$ 0~★0zz→zé/aéa0★$");
    assert_eq!(p.cwd.as_deref(), Some("/home/u/ßz9/a"));
    assert_eq!(p.cols, 361);
    assert_eq!(p.rows, 14);
    assert_eq!(p.scrollback_limit, Some(2_353_478));
    assert_eq!(p.columns_count, None);
    assert_eq!(p.group, None);
    assert_eq!(p.broadcast, Some(true));
    assert_eq!(p.child_pid, 1_189_992);
    assert!(p.shell_argv.is_empty());
    assert_eq!(
        p.env_extras,
        vec![
            ("→a→ßaz".to_string(), "★a/$a$a0a9/9$$/a9/★ß".to_string()),
            ("~/ß★éé".to_string(), "a→ ßzß~$a~aa →0★~→zé".to_string()),
        ]
    );
    assert_eq!(p.show_titlebar, Some(false));
    assert_eq!(p.palette, None);
    assert_eq!(p.active_screen, 1);

    // Three booleans in three adjacent one-byte slots. Pinned individually so
    // that reading them in the wrong order is a failure, not a shrug.
    assert_eq!(p.cursor, CursorState { col: 145, row: 16, shape: 1, visible: true, blink: true, pending_wrap: true });
    assert_eq!(p.cursor.col, 145, "col and row are not interchangeable");
    assert_eq!(p.cursor.row, 16);
    assert!(p.cursor.visible);
    assert!(p.cursor.blink);
    assert!(p.cursor.pending_wrap);

    // Asymmetric colour ROLES: fg, bg and underline each carry a different
    // value, so swapping two of them cannot pass.
    assert_eq!(
        p.pen,
        Style { fg: Colour::Default, bg: Colour::Default, underline: Colour::Indexed(70), attrs: 12_956, link_id: 1 }
    );
    assert_eq!(p.style_table.len(), 4);
    assert_eq!(
        p.style_table[0],
        Style { fg: Colour::Default, bg: Colour::Rgb(150, 255, 120), underline: Colour::Default, attrs: 259, link_id: 1 }
    );
    assert_eq!(
        p.style_table[1],
        Style { fg: Colour::Rgb(15, 185, 222), bg: Colour::Default, underline: Colour::Default, attrs: 12_597, link_id: 3 }
    );
    // Three DIFFERENT indexed colours, one per role: this is the entry that
    // makes an fg/bg/underline swap impossible to hide.
    assert_eq!(
        p.style_table[2],
        Style { fg: Colour::Indexed(120), bg: Colour::Indexed(157), underline: Colour::Indexed(148), attrs: 6_159, link_id: 3 }
    );
    assert_eq!(
        p.style_table[3],
        Style { fg: Colour::Indexed(4), bg: Colour::Indexed(86), underline: Colour::Default, attrs: 1_610, link_id: 2 }
    );
    // An Rgb triple's a/b/c ordering, pinned by three distinct components.
    assert_eq!(p.style_table[0].bg, Colour::Rgb(150, 255, 120));

    // Both mode kinds — 0 ANSI and 1 DEC private — are two distinct namespaces
    // in one list, so the kind byte's meaning is pinned too.
    assert_eq!(p.modes.len(), 10);
    assert_eq!(
        p.modes[..3],
        [
            ModeEntry { kind: 0, number: 2509, value: 1 },
            ModeEntry { kind: 1, number: 461, value: 1 },
            ModeEntry { kind: 0, number: 1025, value: 1 },
        ]
    );
    assert_eq!(p.modes[3], ModeEntry { kind: 0, number: 2142, value: 0 }, "a mode that is OFF");

    assert_eq!(
        p.saved_cursor,
        Some(SavedCursor {
            col: 170,
            row: 105,
            pen: Style { fg: Colour::Rgb(0, 94, 110), bg: Colour::Default, underline: Colour::Default, attrs: 7_454, link_id: 0 },
            // gl and gr differ, so their order is pinned.
            charsets: Charsets { g: [b'B', b'0', b'A', b'B'], gl: 0, gr: 3 },
            origin: true,
        })
    );
    assert_eq!(p.charsets, None);
    assert_eq!(p.tab_stops.as_ref().map(|t| t.len()), Some(269));
    // Four distinct values in four adjacent varints: no two may be swapped.
    assert_eq!(p.margins, Some(Margins { top: 66, bottom: 5, left: 87, right: 61 }));
    assert_eq!(p.title_stack, vec!["ß$a★/é/→".to_string()]);
    assert_eq!(p.kitty_kbd, None);
    assert_eq!(p.pending_raw, vec![193, 126, 74, 132, 135, 142, 137]);
    assert_eq!(
        p.uri_table,
        vec![
            (250, "https://h.invalid/$/★~é~".to_string()),
            (461, "https://h.invalid/$9éaé$".to_string()),
            (252, "https://h.invalid/→/★a9a".to_string()),
        ]
    );
    assert!(p.image_table.is_empty());

    // The grid, down to the run level: line flags, run flags, style ids, the
    // COLUMN span, the text and the per-cell char counts.
    assert_eq!(p.screen_primary.lines.len(), 37);
    let line0 = &p.screen_primary.lines[0];
    assert_eq!(line0.flags, 8, "DECDHL bottom");
    assert_eq!(line0.runs.len(), 2);
    // A combining-mark run: two cells, three chars, counts [2, 1].
    assert_eq!(line0.runs[0].flags, run_flags::CHAR_COUNTS);
    assert_eq!(line0.runs[0].style_id, 0);
    assert_eq!(line0.runs[0].cell_span, 2);
    assert_eq!(line0.runs[0].text, "e\u{0301}e");
    assert_eq!(line0.runs[0].char_counts, vec![2, 1]);
    assert_eq!(line0.runs[0].cells(), 2);
    // A blank run: empty text, 31 columns of style 1.
    assert_eq!(line0.runs[1].flags, 0);
    assert_eq!(line0.runs[1].style_id, 1);
    assert_eq!(line0.runs[1].cell_span, 31);
    assert_eq!(line0.runs[1].text, "");
    assert!(line0.runs[1].char_counts.is_empty());

    // A WIDE run: cell_span is in COLUMNS, so three characters span six.
    let wide = &p.screen_primary.lines[4].runs[0];
    assert_eq!(wide.flags, run_flags::WIDE);
    assert_eq!(wide.style_id, 0);
    assert_eq!(wide.cell_span, 6);
    assert_eq!(wide.cells(), 3, "columns and cells are not the same number");
    assert_eq!(wide.text, "語語語");

    assert_eq!(p.screen_alt.as_ref().map(|g| g.lines.len()), Some(26));
    assert_eq!(p.tag_names.len(), 24);
    assert!(p.unknown_tags.is_empty(), "a v1 fixture must have no unknown tags in a v1 build");
}

#[test]
fn every_pane_fixtures_cursor_flags_are_pinned() {
    // `pane_seed00042` happens to have `visible` and `blink` both true, so it
    // alone cannot catch those two being read in each other's slot. These
    // fixtures can: the first two disagree.
    for (name, visible, blink, pending_wrap) in [
        ("pane_seed00001.bin", false, true, false),
        ("pane_seed00002.bin", true, false, false),
        ("pane_seed00003.bin", true, false, false),
        ("pane_seed00007.bin", true, true, true),
        ("pane_seed00011.bin", true, true, false),
        ("pane_seed00042.bin", true, true, true),
        ("pane_seed01234.bin", true, true, false),
        ("pane_seed99991.bin", true, true, false),
    ] {
        let c = pane_fixture(name).cursor;
        assert_eq!((c.visible, c.blink, c.pending_wrap), (visible, blink, pending_wrap), "{name}");
    }
}

#[test]
fn fixture_paths_are_relative_to_the_crate_not_the_cwd() {
    // Guards against a fixture loader that only works when run from the repo
    // root — the failure mode that makes a corpus silently stop running.
    assert!(Path::new(&dir()).is_absolute());
    assert!(dir().ends_with("tests/fixtures/wire-v1"));
}
