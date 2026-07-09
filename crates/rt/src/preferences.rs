//! The egui preferences dialog.
//!
//! Rendered as an egui overlay (ADR-0004) on top of the terminal when open. It
//! edits an `rt_config::Settings` in place; the caller diffs the result to
//! persist and apply changes. This is the first egui surface — colour pickers
//! and the palette editor join it as those settings land.

use rt_config::Settings;

/// Build the preferences window for this frame. Mutates `settings` directly
/// (sliders/checkboxes bind to its fields) and sets `close` true when the user
/// dismisses the dialog. Call once per frame from the egui run closure.
pub fn ui(ctx: &egui::Context, settings: &mut Settings, close: &mut bool, families: &[String]) {
    egui::Window::new("rt preferences")
        .collapsible(false)
        .resizable(false)
        .default_width(340.0)
        .show(ctx, |ui| {
            ui.heading("Font");
            // Size slider.
            ui.add(egui::Slider::new(&mut settings.font_size, 8.0..=48.0).text("Size (px)"));
            // Family combo, populated with the system's monospace families.
            egui::ComboBox::from_label("Family")
                .selected_text(settings.font_family.clone())
                .show_ui(ui, |ui| {
                    for fam in families {
                        ui.selectable_value(&mut settings.font_family, fam.clone(), fam);
                    }
                });

            ui.separator();
            ui.heading("Appearance");
            // Background opacity: 0.05 (see-through) .. 1.0 (opaque).
            ui.add(
                egui::Slider::new(
                    &mut settings.background_opacity,
                    Settings::MIN_OPACITY..=1.0,
                )
                .text("Background opacity"),
            );
            // Scrim: rt's portable blur stand-in (washes out what's behind).
            ui.add(
                egui::Slider::new(&mut settings.scrim_strength, 0.0..=Settings::MAX_SCRIM)
                    .text("Background scrim"),
            );
            // True compositor blur where the protocol exists (KDE 6.7+, COSMIC,
            // niri). Only takes effect while the background is translucent; a
            // silent no-op elsewhere.
            ui.checkbox(&mut settings.background_blur, "Background blur (if supported)");

            ui.separator();
            ui.heading("Colours");
            // Foreground / background swatches (egui colour pickers).
            ui.horizontal(|ui| {
                ui.label("Text");
                ui.color_edit_button_srgb(&mut settings.foreground);
                ui.label("Background");
                ui.color_edit_button_srgb(&mut settings.background);
            });
            // The 16 ANSI palette colours, in two rows of eight.
            ui.label("ANSI palette");
            ui.horizontal(|ui| {
                for c in settings.palette.iter_mut().take(8) {
                    ui.color_edit_button_srgb(c);
                }
            });
            ui.horizontal(|ui| {
                for c in settings.palette.iter_mut().skip(8) {
                    ui.color_edit_button_srgb(c);
                }
            });
            // Preset schemes (Terminator's `_Colors` menu): clicking one fills
            // fg/bg/palette, which the user can then tweak above.
            ui.horizontal_wrapped(|ui| {
                ui.label("Preset:");
                for scheme in rt_config::SCHEMES {
                    if ui.button(scheme.name).clicked() {
                        settings.foreground = scheme.foreground;
                        settings.background = scheme.background;
                        settings.palette = scheme.palette;
                    }
                }
            });

            ui.separator();
            ui.heading("Behaviour");
            // Focus mode: click-to-focus vs focus-follows-mouse (sloppy).
            ui.checkbox(&mut settings.focus_follows_mouse, "Focus follows mouse");
            // Per-pane titlebars (title + size + group) vs the borderless look.
            ui.checkbox(&mut settings.show_titlebar, "Show per-pane titlebars");
            // Scrollback buffer size (lines kept above the screen). Logarithmic
            // so the slider spans 1k…1M usefully. Applies to new terminals.
            ui.add(
                egui::Slider::new(&mut settings.scrollback, 1000..=Settings::MAX_SCROLLBACK)
                    .logarithmic(true)
                    .text("Scrollback (lines, new terminals)"),
            );

            ui.add_space(6.0);
            ui.heading("Border instruments");
            ui.checkbox(&mut settings.inst_output, "Output activity (green flow)");
            ui.checkbox(&mut settings.inst_heat, "CPU heat (blackbody border)");
            ui.checkbox(&mut settings.inst_latency, "Latency (violet window frame)");
            ui.checkbox(&mut settings.show_jacks, "Patch-bay jacks");

            ui.separator();
            // The dialog is dismissed by this button or the Escape key.
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    *close = true;
                }
                ui.label("(Esc closes)");
            });
        });
}
