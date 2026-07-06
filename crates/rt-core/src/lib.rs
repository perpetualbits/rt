//! `rt-core` — the pure model layer of `rt`.
//!
//! This crate holds everything that can be reasoned about without touching a
//! PTY, a GPU, or a windowing system: the recursive **layout tree** (rt's port
//! of Terminator's split/tab containers) and the lightweight identity types
//! that the GUI layers hang real panes off of.
//!
//! Keeping this logic dependency-free and side-effect-free is a deliberate
//! defence against the Terminator bug class documented in
//! `docs/TERMINATOR_BUGS.md`: the tree never holds a live widget, so it can
//! never use one after it was destroyed. It is also fully unit-testable in a
//! headless sandbox.

pub mod geom; // rectangle math shared by layout + rendering
pub mod layout; // the recursive split/tab tree — the core Terminator port

// Re-export the handful of types callers use most, so downstream crates can
// write `rt_core::Tree` instead of `rt_core::layout::Tree`.
pub use geom::Rect;
pub use layout::{Direction, DragHandle, Orientation, PaneId, Tab, TabBar, Tree};
