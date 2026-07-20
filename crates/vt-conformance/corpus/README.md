# Replay corpora

Captured raw terminal byte streams from real programs (vim, tmux, emacs, htop, git,
cargo, …), replayed through every engine and diffed against the oracle. Drop `*.bytes`
fixtures here; a replay test (to be added) feeds each through both engines and asserts
identical `ScreenState`. Empty for now — plumbing lands with the first fixtures.
