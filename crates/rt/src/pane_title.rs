//! What a pane's titlebar says, and how it is cut down to fit.
//!
//! Two questions, both of them pure, both of them wrong in rt before this module
//! existed:
//!
//! 1. **What does an UNTITLED pane say?** It said the literal `"Terminal"`. On
//!    Linux that is nearly invisible, because a Debian/Ubuntu `bash` sets the
//!    title from `PROMPT_COMMAND` on every prompt. On macOS nothing sets it at
//!    all: Apple's `/etc/zshrc` only emits the title escape for terminals it
//!    recognises, keyed off `TERM_PROGRAM=Apple_Terminal` — and rt exports `TERM`
//!    and `COLORTERM` into a pane and nothing else (`rt_config::term_name`, and
//!    docs/MACOS.md "Terminal type (`TERM`)"), so that test never matches and
//!    must not be made to. So every Mac pane, in every window, said "Terminal" —
//!    a label that is true of all of them and therefore names none of them.
//!    [`derived`] builds the fallback instead: Terminal.app's own shape,
//!    `<dir> — <program> — <cols>x<rows>`.
//!
//! 2. **What gets dropped when the title does not fit?** [`fit`]. The old site
//!    did `chars().take(avail)`, which keeps the HEAD — so
//!    `roland@dop561: ~/git/rt/crates/rt/src/chrome` in a narrow pane became
//!    `roland@dop561: ~/git`, i.e. the half that is identical in every pane on
//!    the machine, with the half that says *which* pane thrown away.
//!
//! # Why a module, rather than a few lines at the draw site
//!
//! Same reason as `cpu_heat`, `scale_policy` and `textdrop`: the decision is
//! plain data in and a string out — a cwd, a program name and a grid size — so it
//! is written once, tested on Linux CI, and used unchanged on both platforms.
//! Only the leaf queries that FETCH a cwd and a program name are per-platform,
//! and those live in `proc_info`. [`describing_pid`] is the same split again: the
//! walk is graph logic with the kernel query injected, exactly like
//! `cpu_heat::subtree_sum`.
//!
//! # Untrusted input
//!
//! A title arrives from OSC 0/2 — arbitrary bytes a program printed — and a
//! directory name is barely better (anyone can `mkdir` one). [`sanitize`] is the
//! one door: it strips control characters (which would otherwise reach
//! `draw_char` as glyph garbage) and the bidi overrides (which can visually
//! reverse a titlebar and make one path read as another), and bounds the length
//! before any of it reaches layout.

use std::borrow::Cow;

/// Hard cap on a title, in characters, applied by [`sanitize`] before the string
/// reaches layout.
///
/// Not a display limit — [`fit`] does that, against the room the pane actually
/// has, which is never more than a couple of hundred cells. This is the bound on
/// what a *program* can hand us: a pane that prints a 4 MB OSC 2 must not make
/// the render loop walk 4 MB per frame. 256 is far past any real title.
pub const MAX_CHARS: usize = 256;

/// The separator Terminal.app uses between the parts of its derived title: a
/// spaced em dash. Kept as a constant because [`derived`] both joins with it and
/// budgets for it.
const SEP: &str = " — ";

/// Which end of an over-long string survives [`fit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// Keep the start, ellipsis at the end: `"◉ selecting · 12…"`. For labels
    /// that are read left to right and whose first words carry the meaning.
    Head,
    /// Keep the end, ellipsis at the start: `"…rt/src/chrome"`. For paths, where
    /// every pane on a machine shares the leading `user@host: ~/` and differs
    /// only in the tail.
    Tail,
}

/// What the OS was able to say about the process a pane's title describes.
///
/// Both fields are already [`sanitize`]d (a directory name is untrusted too), and
/// both are optional because every query here can legitimately fail: the process
/// exits between one call and the next, or the kernel declines to answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcFacts {
    /// Absolute working directory, e.g. `/Users/roland/git/rt`.
    pub cwd: Option<String>,
    /// Program name, e.g. `zsh`, `vim`, `cargo`.
    pub name: Option<String>,
}

/// Strip what must never reach the draw path, and bound the length.
///
/// Removed:
/// * **control characters** (`char::is_control`, i.e. C0, DEL and C1) — a raw
///   `\n` or `\x07` in a title is not a glyph, and feeding one to `draw_char`
///   draws whatever the font's notdef is, or nothing, at a cell that then no
///   longer lines up with the count used to lay the string out;
/// * **bidi controls and isolates** (U+200E..U+200F, U+202A..U+202E,
///   U+2066..U+2069) — these reorder what follows them, so a title can be made to
///   *display* as a path it is not. Cheap to drop, and nothing legitimate puts
///   them in a terminal title.
///
/// Returns [`Cow::Borrowed`] when there is nothing to do, which is every ordinary
/// title — this runs per pane per titlebar repaint, so the clean case allocates
/// nothing.
pub fn sanitize(raw: &str) -> Cow<'_, str> {
    let bad = |c: char| {
        c.is_control()
            || matches!(c, '\u{200E}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
    };
    // Two cheap scans beat one allocation: `chars().count()` and `any` are both
    // O(len) with no heap traffic, and the overwhelmingly common answer is "this
    // string is fine, hand it back".
    if !raw.chars().any(bad) && raw.chars().count() <= MAX_CHARS {
        return Cow::Borrowed(raw);
    }
    Cow::Owned(raw.chars().filter(|&c| !bad(c)).take(MAX_CHARS).collect())
}

/// Cut `text` to at most `avail` cells, keeping the end [`Keep`] names and
/// marking the cut with `…`.
///
/// The one truncation rule in the titlebar: the pane title, the selection status
/// that replaces it, and the tab labels all come through here, so they cannot
/// drift apart.
///
/// Degenerate budgets are part of the contract, because `avail` is derived from a
/// pane width that the user can drag to nothing:
/// * `avail == 0` → `""` (nothing is drawn at all);
/// * `avail == 1` → `"…"` — a bare ellipsis says "there is a title, it does not
///   fit", which is more than any single character of it would say.
///
/// Counts `char`s, not grapheme clusters or display columns, because that is what
/// the caller's layout does: the draw loop advances exactly one cell per `char`.
pub fn fit(text: &str, avail: usize, keep: Keep) -> String {
    if avail == 0 {
        return String::new();
    }
    let n = text.chars().count();
    if n <= avail {
        return text.to_string();
    }
    if avail == 1 {
        return "…".to_string();
    }
    let keep_n = avail - 1; // one cell for the ellipsis
    match keep {
        Keep::Head => {
            let mut s: String = text.chars().take(keep_n).collect();
            s.push('…');
            s
        }
        Keep::Tail => {
            let mut s = String::from("…");
            s.extend(text.chars().skip(n - keep_n));
            s
        }
    }
}

/// The label for a working directory: its last path component.
///
/// Terminal.app's choice, and it is the right one — `/Users/roland` shows as
/// `roland`, `~/git/rt` as `rt`. The full path is what the tail-keeping [`fit`]
/// is for when a *program* puts one in the title; the derived title is a name,
/// not a path.
///
/// `/` has no last component and labels itself. A trailing slash is ignored
/// (`/a/b/` is `b`), and a path that is somehow empty yields `None` so the caller
/// drops the segment rather than drawing a blank one.
pub fn dir_label(cwd: &str) -> Option<String> {
    let trimmed = cwd.trim_end_matches('/');
    if trimmed.is_empty() {
        // Either "" (no cwd) or "/" (the root, whose only name is itself).
        return if cwd.starts_with('/') { Some("/".to_string()) } else { None };
    }
    trimmed.rsplit('/').next().filter(|s| !s.is_empty()).map(str::to_string)
}

/// The fallback title for a pane no program has titled: Terminal.app's shape,
/// `<dir> — <program> — <cols>x<rows>`, cut to `avail` cells.
///
/// # What gets dropped first
///
/// Unlike a program's title — which is usually a path, and is therefore cut from
/// the LEFT — this string is composed here, so when it does not fit we know
/// exactly which part is worth least and drop whole segments from the right:
///
/// 1. the grid size, which rt already prints at the other end of the same
///    titlebar (it is in the string at all only because Terminal.app puts it
///    there, and it is the first thing to go);
/// 2. the program name, which is `zsh` in every pane at a prompt;
/// 3. nothing — the directory is the whole point, and if even that does not fit
///    it is cut keeping its HEAD, because a single name reads left to right.
///
/// Character-eliding the joined string instead would keep `…— zsh — 120x30` and
/// throw away the directory: the exact inversion of what the pane is asking to be
/// told.
///
/// With neither a directory nor a program name — the process vanished, or the
/// kernel refused — this returns `None` and the caller keeps its own last-resort
/// label. Inventing a directory would be worse than saying nothing.
pub fn derived(facts: &ProcFacts, cols: usize, rows: usize, avail: usize) -> Option<String> {
    let dir = facts.cwd.as_deref().and_then(dir_label);
    let name = facts.name.as_deref().filter(|n| !n.is_empty());
    // Nothing known at all: let the caller fall back. (A pane with a program name
    // but no cwd — a sandboxed or exited process — still gets "zsh — 120x30".)
    if dir.is_none() && name.is_none() {
        return None;
    }
    let size = format!("{cols}x{rows}");
    let mut parts: Vec<&str> = Vec::with_capacity(3);
    if let Some(d) = dir.as_deref() {
        parts.push(d);
    }
    if let Some(n) = name {
        parts.push(n);
    }
    parts.push(&size);
    // Drop from the right while the join is too wide; never drop the first part.
    while parts.len() > 1 && join_len(&parts) > avail {
        parts.pop();
    }
    let joined = parts.join(SEP);
    // One part left and still too wide: keep its head (see the doc above).
    Some(if joined.chars().count() > avail { fit(&joined, avail, Keep::Head) } else { joined })
}

/// Width in cells of `parts` joined by [`SEP`], without building the string.
fn join_len(parts: &[&str]) -> usize {
    let text: usize = parts.iter().map(|p| p.chars().count()).sum();
    text + SEP.chars().count() * parts.len().saturating_sub(1)
}

/// How deep [`describing_pid`] will follow a chain of single children.
///
/// A guard, not a real limit: a shell running a program running a program is two
/// levels, and nothing a pane title cares about is eight deep. It bounds the
/// kernel calls one refresh can make even if the process tree is pathological.
const MAX_DEPTH: usize = 8;

/// Which process in a pane's subtree the derived title should describe, given
/// `root` (the pane's login shell) and a way to ask for a process's children.
///
/// # The rule, and why it is this one
///
/// **Follow the chain while it is unambiguous; stop where it forks.** A shell at
/// a prompt has no children, so the title describes the shell. Run `vim` and the
/// shell has exactly one child, so the title follows it and reports vim's cwd —
/// which is the interesting one, and the reason this does not simply read the
/// pane's own pid. Run `cargo build`, and the walk descends into `cargo` and stops
/// there, because `cargo` has many children and no way to say which one matters.
///
/// The obvious alternative — descend into the newest (highest) pid — guesses at
/// the foreground job and guesses wrong every time pids wrap (macOS recycles them
/// past 99999). Stopping at a fork is never *wrong*, only less specific: it names
/// a real ancestor of whatever is running.
///
/// The genuinely correct answer is the tty's foreground process group
/// (`tcgetpgrp` on the PTY master), which is what Terminal.app uses. rt does not
/// expose the master fd outside `rt-engine`, and plumbing it out to two crates for
/// a titlebar label is a worse trade than this walk, which reuses
/// `cpu_heat::child_pids` — already written, already tested, already per-platform.
/// The one case it misreads is a single BACKGROUND job (`sleep 100 &`) at an
/// otherwise idle prompt, where it names the job instead of the shell.
///
/// `seen` guards the cycle a confused kernel interface can hand back, the same
/// failure `cpu_heat::subtree_sum` guards.
pub fn describing_pid(root: u32, mut children: impl FnMut(u32) -> Vec<u32>) -> u32 {
    let mut pid = root;
    let mut seen = vec![root];
    for _ in 0..MAX_DEPTH {
        let kids = children(pid);
        // 0 children: a leaf, this is the process. 2+: a fork, and no basis to
        // choose — stop at the parent, which is a true statement about both.
        let [only] = kids[..] else { return pid };
        if seen.contains(&only) {
            return pid; // a cycle; stay where we are
        }
        seen.push(only);
        pid = only;
    }
    pid
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- sanitize -------------------------------------------------------------

    /// The common case must not allocate: an ordinary title comes back borrowed.
    #[test]
    fn a_clean_title_is_passed_through_without_allocating() {
        let t = "roland@dop561: ~/git/rt";
        assert!(matches!(sanitize(t), Cow::Borrowed(_)));
        assert_eq!(sanitize(t), t);
    }

    /// Control characters never reach `draw_char`: they are not glyphs, and they
    /// desynchronise the cell count the layout was computed from.
    #[test]
    fn control_characters_are_stripped() {
        assert_eq!(sanitize("a\nb\tc\x07d\x1be"), "abcde");
        assert_eq!(sanitize("build\x07 done"), "build done");
        assert_eq!(sanitize("a\u{7f}b"), "ab", "DEL is a control character too");
    }

    /// A title can be made to *read* as a different path with a right-to-left
    /// override. Dropping the bidi controls is the cheapest defence and costs
    /// nothing real.
    #[test]
    fn bidi_overrides_are_stripped() {
        assert_eq!(sanitize("~/git/\u{202E}gnp.exe"), "~/git/gnp.exe");
        assert_eq!(sanitize("\u{2066}a\u{2069}b"), "ab");
    }

    /// A program that prints a megabyte of OSC 2 must not make the render loop
    /// walk a megabyte per frame.
    #[test]
    fn an_absurd_title_is_bounded() {
        let huge = "x".repeat(100_000);
        assert_eq!(sanitize(&huge).chars().count(), MAX_CHARS);
    }

    /// Multi-byte characters survive: the cap counts characters, and slicing by
    /// bytes would panic or corrupt them.
    #[test]
    fn multibyte_titles_survive_sanitising() {
        assert_eq!(sanitize("→ κόσμε ✓"), "→ κόσμε ✓");
        let wide = "漢".repeat(MAX_CHARS + 50);
        assert_eq!(sanitize(&wide).chars().count(), MAX_CHARS);
    }

    // --- fit ------------------------------------------------------------------

    /// The bug this module was written for: the tail of a path is what
    /// distinguishes one pane from another, so the tail is what survives.
    #[test]
    fn a_long_path_keeps_its_tail() {
        let t = "roland@dop561: ~/git/rt/crates/rt/src/chrome";
        assert_eq!(fit(t, 14, Keep::Tail), "…rt/src/chrome");
        assert_eq!(fit(t, 14, Keep::Tail).chars().count(), 14);
        // The old behaviour, for contrast: everything that identifies the pane is
        // gone.
        assert_eq!(fit(t, 20, Keep::Head), "roland@dop561: ~/gi…");
    }

    /// A string that already fits is returned untouched — no stray ellipsis, and
    /// no silent loss of the last character.
    #[test]
    fn a_short_enough_string_is_untouched() {
        assert_eq!(fit("rt", 2, Keep::Tail), "rt");
        assert_eq!(fit("rt", 80, Keep::Tail), "rt");
        assert_eq!(fit("rt", 2, Keep::Head), "rt");
    }

    /// Budgets a user can produce by dragging a divider: zero cells draws
    /// nothing, one cell says "there is something here" and nothing more.
    #[test]
    fn degenerate_budgets_are_safe() {
        assert_eq!(fit("anything", 0, Keep::Tail), "");
        assert_eq!(fit("anything", 1, Keep::Tail), "…");
        assert_eq!(fit("anything", 1, Keep::Head), "…");
        assert_eq!(fit("anything", 2, Keep::Tail), "…g");
        assert_eq!(fit("anything", 2, Keep::Head), "a…");
    }

    /// Never wider than the budget, whatever the input — the whole point of the
    /// call, since the cells to the right belong to the scrollback meter.
    #[test]
    fn the_result_never_exceeds_the_budget() {
        let samples = ["", "x", "漢字のタイトル", "roland@host: ~/a/b/c/d/e", "→→→→→→→→→→"];
        for s in samples {
            for avail in 0..12 {
                for keep in [Keep::Head, Keep::Tail] {
                    assert!(
                        fit(s, avail, keep).chars().count() <= avail,
                        "fit({s:?}, {avail}, {keep:?}) overran its budget"
                    );
                }
            }
        }
    }

    // --- dir_label ------------------------------------------------------------

    /// The label Terminal.app shows: the last component, so a home directory
    /// reads as the user's name and a repo as the repo.
    #[test]
    fn a_directory_labels_itself_by_its_last_component() {
        assert_eq!(dir_label("/Users/roland"), Some("roland".into()));
        assert_eq!(dir_label("/Users/roland/git/rt"), Some("rt".into()));
        assert_eq!(dir_label("/Users/roland/Library/"), Some("Library".into()), "a trailing slash is not a component");
        assert_eq!(dir_label("/"), Some("/".into()), "the root's only name is itself");
        assert_eq!(dir_label(""), None, "no cwd at all: say nothing");
    }

    // --- derived --------------------------------------------------------------

    /// Terminal.app's shape, which is what was asked for: directory, program,
    /// grid size.
    #[test]
    fn a_roomy_pane_gets_the_whole_terminal_app_shape() {
        let f = ProcFacts { cwd: Some("/Users/roland".into()), name: Some("zsh".into()) };
        assert_eq!(derived(&f, 120, 30, 80).unwrap(), "roland — zsh — 120x30");
        let f = ProcFacts { cwd: Some("/Users/roland/Library".into()), name: Some("bash".into()) };
        assert_eq!(derived(&f, 120, 30, 80).unwrap(), "Library — bash — 120x30");
    }

    /// The segment-dropping order, which is the part that makes this useful in a
    /// narrow pane: the size goes first (rt prints it at the other end of the same
    /// bar anyway), then the program, and the directory is the last thing standing.
    #[test]
    fn a_narrow_pane_drops_the_least_useful_segment_first() {
        let f = ProcFacts { cwd: Some("/Users/roland/git/rt".into()), name: Some("zsh".into()) };
        assert_eq!(derived(&f, 120, 30, 80).unwrap(), "rt — zsh — 120x30");
        assert_eq!(derived(&f, 120, 30, 17).unwrap(), "rt — zsh — 120x30", "exactly enough room");
        assert_eq!(derived(&f, 120, 30, 16).unwrap(), "rt — zsh", "no room for the size");
        assert_eq!(derived(&f, 120, 30, 7).unwrap(), "rt", "no room for the program either");
        assert_eq!(derived(&f, 120, 30, 2).unwrap(), "rt");
        // Even the directory has to be cut: keep its head, it is a name.
        let f = ProcFacts { cwd: Some("/Users/roland/some-very-long-directory".into()), name: Some("zsh".into()) };
        assert_eq!(derived(&f, 120, 30, 8).unwrap(), "some-ve…");
    }

    /// Never wider than the budget, for any combination of facts and any width —
    /// the size segment is variable-length, so this is not obvious by inspection.
    #[test]
    fn a_derived_title_never_exceeds_the_budget() {
        let facts = [
            ProcFacts { cwd: Some("/Users/roland/git/rt".into()), name: Some("zsh".into()) },
            ProcFacts { cwd: Some("/".into()), name: None },
            ProcFacts { cwd: None, name: Some("cargo".into()) },
            ProcFacts { cwd: Some("/漢字/ディレクトリ".into()), name: Some("vim".into()) },
        ];
        for f in &facts {
            for avail in 0..40 {
                if let Some(t) = derived(f, 1920, 1080, avail) {
                    assert!(t.chars().count() <= avail, "derived({f:?}, {avail}) = {t:?} overran");
                }
            }
        }
    }

    /// Half-known facts still produce something better than "Terminal"; knowing
    /// nothing produces nothing, rather than an invented directory.
    #[test]
    fn partial_facts_degrade_one_segment_at_a_time() {
        let f = ProcFacts { cwd: None, name: Some("vim".into()) };
        assert_eq!(derived(&f, 80, 24, 40).unwrap(), "vim — 80x24");
        let f = ProcFacts { cwd: Some("/tmp".into()), name: None };
        assert_eq!(derived(&f, 80, 24, 40).unwrap(), "tmp — 80x24");
        let f = ProcFacts::default();
        assert_eq!(derived(&f, 80, 24, 40), None, "nothing known: the caller keeps its own label");
    }

    // --- describing_pid -------------------------------------------------------

    /// The two shapes that matter: a shell sitting at a prompt describes itself,
    /// and a shell running one program describes the program (whose cwd is the
    /// one worth showing).
    #[test]
    fn an_unambiguous_chain_is_followed_to_its_end() {
        assert_eq!(describing_pid(1, |_| vec![]), 1, "a shell at a prompt is the shell");
        // 1 (zsh) ─ 2 (vim)
        assert_eq!(describing_pid(1, |p| if p == 1 { vec![2] } else { vec![] }), 2);
        // 1 ─ 2 ─ 3: follow all the way down.
        let kids = |p: u32| match p {
            1 => vec![2],
            2 => vec![3],
            _ => vec![],
        };
        assert_eq!(describing_pid(1, kids), 3);
    }

    /// A fork has no single answer, so the walk stops at the parent — which is a
    /// true statement about everything below it.
    #[test]
    fn a_fork_stops_the_walk_at_the_parent() {
        // 1 ─ 2 (cargo) ─ {3, 4, 5}
        let kids = |p: u32| match p {
            1 => vec![2],
            2 => vec![3, 4, 5],
            _ => vec![],
        };
        assert_eq!(describing_pid(1, kids), 2, "cargo, not one of its rustc children");
    }

    /// The guards: a cycle must terminate, and a long chain must not walk forever
    /// inside the render loop.
    #[test]
    fn cycles_and_deep_chains_terminate() {
        // 1 ─ 2 ─ 1 …
        let kids = |p: u32| match p {
            1 => vec![2],
            2 => vec![1],
            _ => vec![],
        };
        assert_eq!(describing_pid(1, kids), 2);
        assert_eq!(describing_pid(7, |_| vec![7]), 7, "a self-parenting pid");
        let mut calls = 0usize;
        describing_pid(0, |p| {
            calls += 1;
            vec![p + 1]
        });
        assert!(calls <= MAX_DEPTH, "walked {calls} levels, cap is {MAX_DEPTH}");
    }
}
