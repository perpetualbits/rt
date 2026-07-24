//! In-memory clipboard history: a bounded most-recently-used ring of the text
//! rt has copied this session. Pure (no I/O, nothing persisted) so it is
//! unit-testable without the event loop. See
//! docs/superpowers/specs/2026-07-24-clipboard-history-design.md.

/// Most clips kept. Older ones fall off the back as new ones arrive.
pub const CLIP_HISTORY_MAX: usize = 20;

/// A ring of recent clippings, newest at index 0. In-memory only.
#[derive(Default)]
pub struct ClipHistory {
    items: Vec<String>,
}

impl ClipHistory {
    pub fn new() -> Self {
        ClipHistory { items: Vec::new() }
    }

    /// Record a freshly-copied clip. Empty / whitespace-only text is ignored. A
    /// clip already present is moved to the front (MRU) rather than duplicated;
    /// otherwise it is pushed to the front and the ring is capped at
    /// `CLIP_HISTORY_MAX`, dropping the oldest.
    pub fn record(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        if let Some(pos) = self.items.iter().position(|c| *c == text) {
            self.items.remove(pos);
        }
        self.items.insert(0, text);
        self.items.truncate(CLIP_HISTORY_MAX);
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The i-th clip, newest first (`0` is the most recent), or `None`.
    pub fn get(&self, i: usize) -> Option<&str> {
        self.items.get(i).map(String::as_str)
    }

    /// Clips newest-first.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(String::as_str)
    }
}

/// A one-line preview of a clip for the overlay: newlines shown as `↵`, tabs as
/// spaces, truncated to `max_cols` characters with a trailing `…`. Never spans
/// lines; the real (multi-line) text is only ever used at paste time.
pub fn preview(text: &str, max_cols: usize) -> String {
    let flat: String = text
        .trim()
        .chars()
        .map(|c| match c {
            '\n' => '↵',
            '\r' => '↵',
            '\t' => ' ',
            c => c,
        })
        .collect();
    if flat.chars().count() <= max_cols {
        return flat;
    }
    let keep = max_cols.saturating_sub(1);
    let mut s: String = flat.chars().take(keep).collect();
    s.push('…');
    s
}

/// A compact size badge: `"<chars>c"`, prefixed with `"<lines>L·"` when the clip
/// spans more than one line.
pub fn badge(text: &str) -> String {
    let chars = text.chars().count();
    let lines = text.lines().count().max(1);
    if lines > 1 {
        format!("{lines}L·{chars}c")
    } else {
        format!("{chars}c")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_newest_first_and_skips_blank() {
        let mut h = ClipHistory::new();
        h.record("one".into());
        h.record("two".into());
        h.record("   ".into()); // whitespace-only: ignored
        h.record("".into()); // empty: ignored
        assert_eq!(h.len(), 2);
        assert_eq!(h.get(0), Some("two"));
        assert_eq!(h.get(1), Some("one"));
        assert_eq!(h.iter().collect::<Vec<_>>(), vec!["two", "one"]);
    }

    #[test]
    fn re_recording_moves_to_front_without_duplicating() {
        let mut h = ClipHistory::new();
        h.record("a".into());
        h.record("b".into());
        h.record("a".into()); // already present → moves to front
        assert_eq!(h.len(), 2);
        assert_eq!(h.iter().collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn caps_at_the_maximum_dropping_oldest() {
        let mut h = ClipHistory::new();
        for i in 0..(CLIP_HISTORY_MAX + 5) {
            h.record(format!("clip{i}"));
        }
        assert_eq!(h.len(), CLIP_HISTORY_MAX);
        assert_eq!(h.get(0), Some(format!("clip{}", CLIP_HISTORY_MAX + 4).as_str()));
        assert_eq!(h.iter().last(), Some("clip5")); // clip0..clip4 evicted
    }

    #[test]
    fn clear_empties_it() {
        let mut h = ClipHistory::new();
        h.record("x".into());
        h.clear();
        assert!(h.is_empty());
        assert_eq!(h.get(0), None);
    }

    #[test]
    fn preview_is_one_line_and_truncated() {
        // Newlines become ↵, tabs become spaces, kept on one line.
        assert_eq!(preview("git log\n--oneline", 40), "git log↵--oneline");
        assert_eq!(preview("a\tb", 40), "a b");
        // Truncation adds an ellipsis and never exceeds max_cols chars.
        let p = preview("0123456789abcdef", 8);
        assert_eq!(p.chars().count(), 8);
        assert!(p.ends_with('…'));
        assert_eq!(p, "0123456…");
    }

    #[test]
    fn badge_counts_chars_and_lines() {
        assert_eq!(badge("hello"), "5c");
        assert_eq!(badge("a\nb\nc"), "3L·5c");
    }
}
