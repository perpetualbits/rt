//! rt's native macOS Settings window: the AppKit half.
//!
//! ⌘, opens a real `NSWindow` with real AppKit controls, the way every Mac
//! application's settings do. On Linux nothing here exists and the self-drawn
//! `chrome::prefs` dialog is untouched.
//!
//! **This file contains no decisions.** Which rows there are and in what order
//! is [`crate::chrome::prefs::rows`] — the *same* function the Linux dialog
//! builds from, so the two can never carry different rows. Which AppKit control
//! each row becomes, what a control's request does to the settings, and what a
//! popup may offer is [`crate::prefs_native`], which is not `cfg`'d and is
//! unit-tested on Linux. Here there is only objc2: build the views, catch the
//! action, hand the request back to the run loop. Same split as
//! `menubar.rs` / `menubar_model.rs` and `vibrancy.rs` / `vibrancy_policy.rs`,
//! for the same reason — no CI compiles this file.
//!
//! ## Nothing here writes a setting
//!
//! A control's action becomes an [`Edit`] in a queue and a
//! `EventLoopProxy::wake_up`; `main.rs` drains the queue on the next turn of the
//! winit loop and applies each edit through `prefs_native`, which applies it
//! through `prefs_model::step`, which is what the Linux dialog's arrow keys call.
//! One model, one commit path, one `config.toml` writer.
//!
//! That is also how the **debounce** survives. An `NSSlider` sends its action on
//! every pixel of a drag; a sixty-hertz drag would otherwise mean sixty palette
//! rebuilds and sixty `config.toml` writes. Because an edit only ever lands in
//! `Active::prefs_pending` and re-arms `PREFS_SETTLE`, the terminal behind the
//! window still recolours — and the file is still written — exactly once, 150 ms
//! after the drag stops, precisely as it does for the Linux dialog's held arrow
//! key. `wake_up` coalesces, so a whole frame's worth of slider ticks is drained
//! in one pass.
//!
//! ## Lifetime
//!
//! The window is built once and kept. `setReleasedWhenClosed:NO` is not
//! optional: this struct holds a `Retained<NSWindow>`, and AppKit's default
//! (release the window when its close button is pressed) would leave that
//! `Retained` pointing at freed memory the moment the user clicked the red dot.
//! With it off, closing merely orders the window out and re-opening is
//! `makeKeyAndOrderFront:` on the same object. rt quitting with the window open
//! is a plain process exit — `main.rs` orders it out first so it does not
//! outlive the terminal on screen for a frame.

use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSColor, NSColorSpace, NSColorWell, NSControl, NSFont, NSPopUpButton,
    NSScrollView, NSSlider, NSStepper, NSSwitch, NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use winit::event_loop::EventLoopProxy;

use crate::chrome::prefs::{Row, RowKind};
use crate::prefs_native::{self, Control};
use crate::prefs_model::PrefRow;

/// What a native control is asking the model for. Never a value the model has
/// already accepted — see the module docs.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    /// An `NSSwitch` was flipped.
    Flag(PrefRow, bool),
    /// An `NSSlider` was dragged to this value.
    Number(PrefRow, f64),
    /// An `NSStepper` was clicked this many notches (±1 per click).
    ///
    /// A stepper is the one control that speaks the model's own language, so it
    /// is passed through as steps rather than as a value: that is what lets the
    /// geometric scrollback ladder (one click = one doubling) be a stepper at
    /// all.
    Step(PrefRow, i32),
    /// An `NSPopUpButton` item was picked.
    Choice(PrefRow, String),
    /// An `NSColorWell` (and behind it the system `NSColorPanel`) changed the
    /// swatch at this index of the prefs palette row — `0` foreground, `1`
    /// background, `2..18` the sixteen ANSI colours, exactly the indexing
    /// `chrome::colour_picker::Slot::from_swatch_index` uses.
    Colour(usize, [u8; 3]),
}

/// Tags below this are row indices; at and above, colour-well swatch indices.
const WELL_TAG_BASE: isize = 10_000;

// --- geometry -------------------------------------------------------------

const WIN_W: f64 = 560.0;
const WIN_H: f64 = 640.0;
const PAD: f64 = 20.0;
const LABEL_W: f64 = 210.0;
const CTRL_X: f64 = PAD + LABEL_W + 12.0;
const CTRL_W: f64 = WIN_W - CTRL_X - PAD - 18.0; // 18 = room for the scroller
const ROW_H: f64 = 28.0;
const SECTION_H: f64 = 34.0;
const DISPLAY_H: f64 = 20.0;
const SWATCH: f64 = 22.0;

/// The shared state an AppKit action and the winit loop both touch. Tiny, and
/// no lock is ever held across a call back into AppKit.
struct Bridge {
    proxy: EventLoopProxy,
    /// Tag → which row that control edits, and how to read it. Rebuilt whenever
    /// the window's shape is rebuilt.
    kinds: Mutex<Vec<Option<(PrefRow, Control)>>>,
    /// Tag → that popup's item titles, in menu order.
    choices: Mutex<Vec<Vec<String>>>,
    /// Requests the loop has not collected yet.
    pending: Mutex<Vec<Edit>>,
}

impl Bridge {
    fn push(&self, e: Edit) {
        if let Ok(mut q) = self.pending.lock() {
            q.push(e);
        }
        // Only after the payload is in place, as winit's `proxy_wake_up` docs
        // require — otherwise the loop can wake on an empty queue.
        self.proxy.wake_up();
    }
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - PrefsTarget does not implement Drop.
    // - MainThreadOnly because AppKit only ever sends a control's action on the
    //   main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RtPrefsTarget"]
    #[ivars = Bridge]
    struct PrefsTarget;

    impl PrefsTarget {
        /// Every switch, slider, stepper and popup points here; the tag says
        /// which row, and `kinds` says how to read the value.
        #[unsafe(method(rtPrefsAction:))]
        fn rt_prefs_action(&self, sender: Option<&NSControl>) {
            let Some(c) = sender else { return };
            let tag = c.tag();
            let Ok(kinds) = self.ivars().kinds.lock() else { return };
            let Some(Some((row, kind))) = usize::try_from(tag).ok().and_then(|i| kinds.get(i).copied())
            else {
                log::debug!("settings window: action on tag {tag} with no row");
                return;
            };
            drop(kinds);
            let edit = match kind {
                // NSSwitch's on/off reaches NSControl as its integer value.
                Control::Switch => Edit::Flag(row, c.integerValue() != 0),
                Control::Slider => Edit::Number(row, c.doubleValue()),
                // The stepper is zeroed on every refresh, so its value IS the
                // notches turned since the last one. Zeroing it here as well
                // keeps a run of clicks additive rather than cumulative.
                Control::Stepper => {
                    let notches = c.doubleValue().round() as i32;
                    c.setDoubleValue(0.0);
                    if notches == 0 {
                        return;
                    }
                    Edit::Step(row, notches)
                }
                Control::PopUp => {
                    let i = unsafe { msg_send![c, indexOfSelectedItem] };
                    let i: isize = i;
                    let Ok(list) = self.ivars().choices.lock() else { return };
                    let Some(name) = usize::try_from(tag)
                        .ok()
                        .and_then(|t| list.get(t))
                        .and_then(|v| usize::try_from(i).ok().and_then(|i| v.get(i)))
                        .cloned()
                    else {
                        return;
                    };
                    drop(list);
                    Edit::Choice(row, name)
                }
                Control::Swatches | Control::Dismiss => return,
            };
            self.ivars().push(edit);
        }

        /// The colour wells have their own selector: a colour is not an
        /// `NSControl` value, and the swatch index is not a `PrefRow`.
        #[unsafe(method(rtColourAction:))]
        fn rt_colour_action(&self, sender: Option<&NSColorWell>) {
            let Some(w) = sender else { return };
            let Ok(i) = usize::try_from(w.tag() - WELL_TAG_BASE) else { return };
            let Some(rgb) = srgb(&w.color()) else { return };
            self.ivars().push(Edit::Colour(i, rgb));
        }
    }

    unsafe impl NSObjectProtocol for PrefsTarget {}
);

define_class!(
    // SAFETY: as above; a plain NSView that reports a top-left origin.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RtFlippedView"]
    // No state of its own, but objc2 0.6 only offers `msg_send![super(..), init]`
    // on a `PartialInit`, which is what `set_ivars` produces — so the ivar type
    // is the unit.
    #[ivars = ()]
    struct FlippedView;

    impl FlippedView {
        /// Lay the rows out from the TOP. Without this every frame in the file
        /// would have to be mirrored through the document height, which is the
        /// classic way a scrolling settings pane ends up upside down.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }

    unsafe impl NSObjectProtocol for FlippedView {}
);

/// A built control, kept so a refresh can update it in place rather than
/// rebuilding the window under the user's pointer.
enum Widget {
    Switch(Retained<NSSwitch>),
    Slider(Retained<NSSlider>),
    /// The stepper, and the read-only field showing the value it steps. The
    /// field's text is `chrome::prefs::Row::value` — the very string the Linux
    /// dialog draws, so "10k" means the same thing on both platforms.
    Stepper(Retained<NSStepper>, Retained<NSTextField>),
    PopUp(Retained<NSPopUpButton>),
    /// A read-only line: the version header, the scrollback guardrail, the TERM
    /// advisory, the font-family advisory. Their TEXT changes as settings
    /// change, so they are kept too.
    Display(Retained<NSTextField>),
}

/// rt's Settings window, once built. Owned by `App` for the life of the process.
pub struct SettingsWindow {
    window: Retained<NSWindow>,
    /// Held for the same reason `menubar::MenuBar` holds its target:
    /// `NSControl.target` is a **weak, unretained** reference.
    target: Retained<PrefsTarget>,
    widgets: Vec<Option<Widget>>,
    wells: Vec<Retained<NSColorWell>>,
    /// What the current view hierarchy was built from. A refresh rebuilds only
    /// when this changes — which is rare (an advisory line appearing) and never
    /// mid-drag.
    shape: Vec<(RowKind, String, Option<PrefRow>)>,
    mtm: MainThreadMarker,
}

impl SettingsWindow {
    /// Build the window (hidden) for `rows`. `None` off the main thread.
    pub fn new(rows: &[Row], choices: Choices<'_>, proxy: EventLoopProxy) -> Option<SettingsWindow> {
        let mtm = MainThreadMarker::new()?;
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIN_W, WIN_H));
        // SAFETY: the designated initialiser, with a style mask AppKit accepts.
        // An RtPanelWindow, so ⌘W closes it — see native_window.rs.
        let window: Retained<NSWindow> = crate::native_window::panel_window(mtm, frame, style);
        window.setTitle(&NSString::from_str("rt Settings"));
        // See the module docs: this struct holds the only strong reference.
        // SAFETY: the opposite of the hazard — see the module docs. Unsafe only
        // because AppKit's default is the one that can dangle.
        unsafe { window.setReleasedWhenClosed(false) };
        window.center();

        let target = PrefsTarget::new(
            mtm,
            Bridge {
                proxy,
                kinds: Mutex::new(Vec::new()),
                choices: Mutex::new(Vec::new()),
                pending: Mutex::new(Vec::new()),
            },
        );
        let mut w = SettingsWindow {
            window,
            target,
            widgets: Vec::new(),
            wells: Vec::new(),
            shape: Vec::new(),
            mtm,
        };
        w.build(rows, choices);
        Some(w)
    }

    /// Show it, or bring it forward if it is already up.
    pub fn show(&self) {
        self.window.makeKeyAndOrderFront(None);
    }

    /// Hide it — used on rt's way out, so the Settings window does not outlive
    /// the terminal it configures.
    pub fn hide(&self) {
        self.window.orderOut(None);
    }

    /// Is it on screen? `main.rs` skips the whole refresh when it is not.
    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    /// Is this the window the keyboard is talking to?
    ///
    /// `main.rs` asks on every turn of the loop: while a native rt window owns
    /// the keyboard, the menu bar must be greyed so that ⌘C/⌘V/⌘F act on the
    /// text field under the cursor instead of firing rt's own bindings at the
    /// terminal behind. Same reason the overlay-driven `suspended` flag exists;
    /// see `App::menu_bar_state`.
    pub fn is_key(&self) -> bool {
        self.window.isKeyWindow()
    }

    /// Take the requests made since the last call, oldest first.
    pub fn take_pending(&self) -> Vec<Edit> {
        self.target.ivars().pending.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }

    /// Push the current settings at the window: values, enabled/disabled, popup
    /// menus, and the read-only advisory lines.
    ///
    /// Called once per turn of the event loop while the window is up. Cheap: it
    /// writes into existing controls and only rebuilds the hierarchy when the
    /// ROW SHAPE changes (an advisory line coming or going).
    pub fn refresh(&mut self, rows: &[Row], choices: Choices<'_>, swatches: &[[u8; 3]]) {
        if shape_of(rows) != self.shape {
            self.build(rows, choices);
        } else {
            self.sync(rows, choices);
        }
        for (i, w) in self.wells.iter().enumerate() {
            let Some(c) = swatches.get(i) else { continue };
            // Writing the colour back is what makes a preset change move the
            // wells, and what makes the model the source of truth even for the
            // row the system panel edits.
            if srgb(&w.color()) != Some(*c) {
                w.setColor(&ns_colour(*c));
            }
        }
    }
}

/// The lists a popup row needs, passed in rather than looked up here: the
/// installed monospace families and this machine's terminfo names both live on
/// `Active`, and this file stays free of anything but AppKit.
#[derive(Clone, Copy)]
pub struct Choices<'a> {
    pub settings: &'a rt_config::Settings,
    pub families: &'a [String],
    pub terms: &'a [String],
}

fn shape_of(rows: &[Row]) -> Vec<(RowKind, String, Option<PrefRow>)> {
    rows.iter().map(|r| (r.kind, r.label.clone(), r.pref)).collect()
}

impl SettingsWindow {
    /// (Re)build the whole view hierarchy from `rows`.
    fn build(&mut self, rows: &[Row], ch: Choices<'_>) {
        let mtm = self.mtm;
        self.widgets = Vec::new();
        self.wells = Vec::new();
        let mut kinds: Vec<Option<(PrefRow, Control)>> = Vec::new();
        let mut menus: Vec<Vec<String>> = Vec::new();

        // Measure first so the document view is exactly as tall as its content.
        let height: f64 = rows.iter().map(row_height).sum::<f64>() + PAD * 2.0;
        // `init` then `setFrame:` rather than `initWithFrame:` through
        // `super(..)`: objc2 0.6 only accepts the no-argument init family across
        // a super send, and `-[NSView init]` is `initWithFrame:NSZeroRect`.
        let doc: Retained<FlippedView> = {
            let this = FlippedView::alloc(mtm).set_ivars(());
            unsafe { msg_send![super(this), init] }
        };
        doc.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIN_W - 18.0, height)));

        let mut y = PAD;
        for (i, row) in rows.iter().enumerate() {
            let tag = i as isize;
            kinds.push(None);
            menus.push(Vec::new());
            let h = row_height(row);
            let ctrl = prefs_native::row_control(row.kind, row.pref);
            // The Close row is the one row a native window does not need: its
            // title bar has a close button. (Not ⌘W — that chord belongs to a
            // menu item, and winit's default application menu has no Close row
            // to lend. The red dot is the way out.)
            if ctrl == Some(Control::Dismiss) {
                self.widgets.push(None);
                y += h;
                continue;
            }
            match row.kind {
                RowKind::Section => {
                    let f = label_field(mtm, &row.label, true);
                    f.setFrame(NSRect::new(NSPoint::new(PAD, y + 10.0), NSSize::new(WIN_W - PAD * 2.0, 18.0)));
                    doc.addSubview(&f);
                    self.widgets.push(None);
                }
                RowKind::Display => {
                    let f = label_field(mtm, &row.value, false);
                    f.setTextColor(Some(&NSColor::secondaryLabelColor()));
                    f.setFrame(NSRect::new(NSPoint::new(PAD, y), NSSize::new(WIN_W - PAD * 2.0 - 18.0, 16.0)));
                    doc.addSubview(&f);
                    self.widgets.push(Some(Widget::Display(f)));
                }
                RowKind::Swatches => {
                    let f = label_field(mtm, &row.label, false);
                    f.setFrame(NSRect::new(NSPoint::new(PAD, y + 4.0), NSSize::new(LABEL_W, 18.0)));
                    doc.addSubview(&f);
                    // Eighteen wells: foreground, background, then the sixteen
                    // ANSI colours, in `Slot::from_swatch_index` order.
                    for k in 0..18usize {
                        let well = NSColorWell::new(mtm);
                        well.setTag(WELL_TAG_BASE + k as isize);
                        let (col, rowk) = (k % 9, k / 9);
                        well.setFrame(NSRect::new(
                            NSPoint::new(CTRL_X + col as f64 * (SWATCH + 4.0), y + rowk as f64 * (SWATCH + 4.0)),
                            NSSize::new(SWATCH, SWATCH),
                        ));
                        unsafe {
                            well.setTarget(Some(self.target.as_ref()));
                            well.setAction(Some(sel!(rtColourAction:)));
                        }
                        doc.addSubview(&well);
                        self.wells.push(well);
                    }
                    self.widgets.push(None);
                }
                RowKind::Toggle | RowKind::Step | RowKind::Action => {
                    let Some(pref) = row.pref else {
                        self.widgets.push(None);
                        y += h;
                        continue;
                    };
                    let f = label_field(mtm, &row.label, false);
                    f.setFrame(NSRect::new(NSPoint::new(PAD, y + 3.0), NSSize::new(LABEL_W, 18.0)));
                    doc.addSubview(&f);
                    let control = prefs_native::control(pref);
                    kinds[i] = Some((pref, control));
                    let widget = match control {
                        Control::Switch => {
                            let sw = NSSwitch::new(mtm);
                            sw.setFrame(NSRect::new(NSPoint::new(CTRL_X, y), NSSize::new(40.0, 22.0)));
                            wire(&sw, tag, &self.target);
                            doc.addSubview(&sw);
                            Widget::Switch(sw)
                        }
                        Control::Slider => {
                            let sl = NSSlider::new(mtm);
                            sl.setFrame(NSRect::new(NSPoint::new(CTRL_X, y), NSSize::new(CTRL_W, 22.0)));
                            // Continuous: the action fires all through the drag,
                            // so the terminal behind changes under the pointer.
                            // The cost of that is paid by PREFS_SETTLE, not here.
                            sl.setContinuous(true);
                            wire(&sl, tag, &self.target);
                            doc.addSubview(&sl);
                            Widget::Slider(sl)
                        }
                        Control::Stepper => {
                            let field = label_field(mtm, &row.value, false);
                            field.setAlignment(objc2_app_kit::NSTextAlignment::Right);
                            field.setFrame(NSRect::new(NSPoint::new(CTRL_X, y + 3.0), NSSize::new(90.0, 18.0)));
                            doc.addSubview(&field);
                            let st = NSStepper::new(mtm);
                            st.setFrame(NSRect::new(NSPoint::new(CTRL_X + 96.0, y), NSSize::new(19.0, 24.0)));
                            // A pure ±1 emitter, not a value: see `Edit::Step`.
                            // The travel is wide and the value re-zeroed on every
                            // action, so it never runs into its own limits.
                            st.setMinValue(-1.0e6);
                            st.setMaxValue(1.0e6);
                            st.setIncrement(1.0);
                            st.setValueWraps(false);
                            st.setDoubleValue(0.0);
                            wire(&st, tag, &self.target);
                            doc.addSubview(&st);
                            Widget::Stepper(st, field)
                        }
                        Control::PopUp => {
                            let pu = NSPopUpButton::new(mtm);
                            pu.setFrame(NSRect::new(NSPoint::new(CTRL_X, y - 2.0), NSSize::new(CTRL_W, 25.0)));
                            let list = prefs_native::choices(ch.settings, pref, ch.families, ch.terms);
                            fill_menu(&pu, &list);
                            menus[i] = list;
                            wire(&pu, tag, &self.target);
                            doc.addSubview(&pu);
                            Widget::PopUp(pu)
                        }
                        Control::Swatches | Control::Dismiss => unreachable!("handled above"),
                    };
                    self.widgets.push(Some(widget));
                }
            }
            y += h;
        }

        if let Ok(mut k) = self.target.ivars().kinds.lock() {
            *k = kinds;
        }
        if let Ok(mut m) = self.target.ivars().choices.lock() {
            *m = menus;
        }

        let scroll = NSScrollView::new(mtm);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(true);
        scroll.setDocumentView(Some(&doc));
        self.window.setContentView(Some(&scroll));
        self.shape = shape_of(rows);
        self.sync(rows, ch);
    }

    /// Write the current settings into the controls already on screen.
    fn sync(&mut self, rows: &[Row], ch: Choices<'_>) {
        let s = ch.settings;
        let mut menus: Option<Vec<Vec<String>>> = None;
        for (i, row) in rows.iter().enumerate() {
            let Some(w) = self.widgets.get(i).and_then(|w| w.as_ref()) else { continue };
            match w {
                Widget::Display(f) => set_text(f, &row.value),
                Widget::Switch(sw) => {
                    let Some(pref) = row.pref else { continue };
                    let on = prefs_native::flag(s, pref).unwrap_or(false);
                    sw.setState(if on {
                        objc2_app_kit::NSControlStateValueOn
                    } else {
                        objc2_app_kit::NSControlStateValueOff
                    });
                    sw.setEnabled(row.enabled);
                }
                Widget::Slider(sl) => {
                    let Some(pref) = row.pref else { continue };
                    // The travel is the MODEL's, probed off it — the opacity
                    // floor moves with the blur toggle, so a slider built from a
                    // constant would offer a drag the model then refuses.
                    if let Some((lo, hi)) = prefs_native::range(s, pref) {
                        sl.setMinValue(lo);
                        sl.setMaxValue(hi);
                    }
                    if let Some(v) = prefs_native::number(s, pref) {
                        // Only when it differs: writing during a drag would fight
                        // the user's thumb. It differs exactly when the model
                        // refused or snapped the request, which is when the thumb
                        // SHOULD move.
                        if (sl.doubleValue() - v).abs() > 1e-9 {
                            sl.setDoubleValue(v);
                        }
                    }
                    sl.setEnabled(row.enabled);
                }
                Widget::Stepper(st, field) => {
                    // The value shown is the row's own string — `fmt_lines`,
                    // `{:.0}`, and the rest, straight from `chrome::prefs::rows`.
                    set_text(field, &row.value);
                    st.setEnabled(row.enabled);
                    field.setEnabled(row.enabled);
                }
                Widget::PopUp(pu) => {
                    let Some(pref) = row.pref else { continue };
                    let list = prefs_native::choices(s, pref, ch.families, ch.terms);
                    let menus = menus.get_or_insert_with(|| {
                        self.target.ivars().choices.lock().map(|m| m.clone()).unwrap_or_default()
                    });
                    if menus.get(i) != Some(&list) {
                        fill_menu(pu, &list);
                        if i < menus.len() {
                            menus[i] = list.clone();
                        }
                    }
                    if let Some(now) = prefs_native::label(s, pref) {
                        if let Some(at) = list.iter().position(|c| *c == now) {
                            if pu.indexOfSelectedItem() != at as isize {
                                pu.selectItemAtIndex(at as isize);
                            }
                        }
                    }
                    pu.setEnabled(row.enabled);
                }
            }
        }
        if let Some(m) = menus {
            if let Ok(mut slot) = self.target.ivars().choices.lock() {
                *slot = m;
            }
        }
    }
}

fn row_height(row: &Row) -> f64 {
    match row.kind {
        RowKind::Section => SECTION_H,
        RowKind::Display => DISPLAY_H,
        RowKind::Swatches => (SWATCH + 4.0) * 2.0 + 6.0,
        RowKind::Action => 0.0, // the Close row has no native control
        _ => ROW_H,
    }
}

/// A non-editable, non-selectable text label.
fn label_field(mtm: MainThreadMarker, text: &str, bold: bool) -> Retained<NSTextField> {
    let f = NSTextField::new(mtm);
    f.setStringValue(&NSString::from_str(text));
    f.setBezeled(false);
    f.setDrawsBackground(false);
    f.setEditable(false);
    f.setSelectable(false);
    let font = if bold {
        NSFont::boldSystemFontOfSize(NSFont::systemFontSize())
    } else {
        NSFont::systemFontOfSize(NSFont::smallSystemFontSize())
    };
    f.setFont(Some(&font));
    f
}

fn set_text(f: &NSTextField, text: &str) {
    if f.stringValue().to_string() != text {
        f.setStringValue(&NSString::from_str(text));
    }
}

/// Point a control at the one action selector, tagged with its row index.
fn wire(c: &NSControl, tag: isize, target: &PrefsTarget) {
    c.setTag(tag);
    // SAFETY: `target` outlives every control — `SettingsWindow` holds the only
    // strong reference and lives on `App`. This is the reason it does:
    // `NSControl.target` is weak (unretained).
    unsafe {
        c.setTarget(Some(target.as_ref()));
        c.setAction(Some(sel!(rtPrefsAction:)));
    }
}

fn fill_menu(pu: &NSPopUpButton, items: &[String]) {
    pu.removeAllItems();
    for it in items {
        // `addItemWithTitle:` silently drops a duplicate title, which would
        // shift every later index; rt's rings have no duplicates, and the
        // selection is re-derived from the model on every refresh anyway.
        pu.addItemWithTitle(&NSString::from_str(it));
    }
}

/// An `NSColor` in rt's own 8-bit sRGB, or `None` if AppKit cannot convert it
/// (a pattern colour, say — the system panel can produce one).
fn srgb(c: &NSColor) -> Option<[u8; 3]> {
    let c = c.colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())?;
    let to8 = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some([to8(c.redComponent()), to8(c.greenComponent()), to8(c.blueComponent())])
}

fn ns_colour(c: [u8; 3]) -> Retained<NSColor> {
    let f = |v: u8| v as f64 / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(f(c[0]), f(c[1]), f(c[2]), 1.0)
}

impl PrefsTarget {
    fn new(mtm: MainThreadMarker, bridge: Bridge) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(bridge);
        unsafe { msg_send![super(this), init] }
    }
}
