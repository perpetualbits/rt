//! The border instruments + patch-bay, drawn with `Backend` primitives on BOTH
//! backends (GL and XRender) — the same meters/wires/beziers, no egui. Colours
//! and geometry come from the shared `main.rs` helpers, so the two paths draw
//! identically.
use std::collections::HashMap;
use crate::backend::Backend;
use crate::render::Color;
use crate::{content_bounds, cubic_bezier, flow_point, heat_color, latency_color, Meter, Wire,
            BUSY_WAKEUPS, FLOW_PACKETS, WIRE_BUSY_BYTES, WIRE_PACKETS,
            JACK_R_BACK, JACK_R_FILL, JACK_R_RING, JACK_RING_W};
use rt_core::{PaneId, Rect};
use crate::Stream;

/// Exactly the `Active` state the instrument draw reads, borrowed field-by-field
/// so the caller can pass `&mut active.backend` alongside (disjoint borrows).
/// `rects` is precomputed by the caller (`session.visible_rects`) so no `session`
/// borrow lingers here.
pub struct InstrCtx<'a> {
    pub rects: &'a [(PaneId, Rect)],
    pub meters: &'a HashMap<PaneId, Meter>,
    pub wires: &'a [Wire],
    pub heat: &'a HashMap<PaneId, f32>,
    pub inst_output: bool,
    pub inst_heat: bool,
    pub inst_latency: bool,
    pub show_jacks: bool,
    pub wiring_from: Option<(PaneId, Stream)>,
    pub drag_cursor: Option<(f32, f32)>,
    pub lat_phase: f32,
    pub stall: f32,
    pub size: winit::dpi::PhysicalSize<u32>,
}

/// Draw all enabled instruments over the already-drawn grid (physical pixels).
/// Reads ONLY through `ctx` (never `&Active`) so `be` can come from the same Active.
pub fn draw(be: &mut dyn Backend, ctx: &InstrCtx) {
    let rects = ctx.rects;
    let size = ctx.size;
    let (inst_output, inst_heat, inst_latency) = (ctx.inst_output, ctx.inst_heat, ctx.inst_latency);

    // Per-pane heat borders + orbiting output packets.
    for (id, rect) in rects {
        let m = ctx.meters.get(id).copied().unwrap_or_default();
        let act = (m.rate / BUSY_WAKEUPS).clamp(0.0, 1.0);
        let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
        if inst_heat {
            let load = ctx.heat.get(id).copied().unwrap_or(0.0);
            let c = heat_color(load);
            let t = 2.4;
            be.fill_rect(x, y, w, t, c); // top
            be.fill_rect(x, y + h - t, w, t, c); // bottom
            be.fill_rect(x, y, t, h, c); // left
            be.fill_rect(x + w - t, y, t, h, c); // right
        }
        if inst_output {
            for k in 0..FLOW_PACKETS {
                let tt = (m.phase + k as f32 / FLOW_PACKETS as f32).fract();
                let p = flow_point(x, y, w, h, tt);
                let a = 0.30 + 0.70 * act;
                let n = |v: u8| v as f32 / 255.0;
                let glow = Color(n(0x28), n(0xc0), n(0x48), a * 110.0 / 255.0);
                let core = Color(n(0x66), n(0xff), n(0x7a), a);
                be.fill_circle(p.0, p.1, 9.0, glow);
                be.fill_circle(p.0, p.1, 3.4, core);
            }
        }
    }

    // Patch-bay jack positions (physical px).
    let jack_pos = |r: &rt_core::Rect, which: u8| -> (f32, f32) {
        let (x, y, w, h) = (r.x, r.y, r.w, r.h);
        match which {
            0 => (x, y + h * 0.5),
            1 => (x + w, y + h / 3.0),
            _ => (x + w, y + 2.0 * h / 3.0),
        }
    };
    let rect_of = |id: rt_core::PaneId| rects.iter().find(|&&(i, _)| i == id).map(|(_, r)| r);

    // Wires (under the jacks): stream-colored bezier flow.
    for w in ctx.wires {
        let (Some(sr), Some(dr)) = (rect_of(w.src), rect_of(w.dst)) else { continue };
        let p0 = jack_pos(sr, if w.stream == Stream::Stdout { 1 } else { 2 });
        let p3 = jack_pos(dr, 0);
        let ext = ((p3.0 - p0.0).abs() * 0.4 + 40.0).min(180.0);
        let p1 = (p0.0 + ext, p0.1);
        let p2 = (p3.0 - ext, p3.1);
        let hue = if w.stream == Stream::Stdout { (0x40u8, 0xc0u8, 0x54u8) } else { (0xd0, 0x54, 0x30) };
        let act = (w.rate / WIRE_BUSY_BYTES).clamp(0.0, 1.0);
        const N: u32 = 32; // bezier segments per wire (trimmed from 56 to cut ssh -X wire volume; still smooth)
        let mut prev = p0;
        for i in 1..=N {
            let t = i as f32 / N as f32;
            let pt = cubic_bezier(p0, p1, p2, p3, t);
            let mut best = 0.0f32;
            for k in 0..WIRE_PACKETS {
                let pp = (w.phase + k as f32 / WIRE_PACKETS as f32).fract();
                let d = (t - pp).abs();
                best = best.max((-d * d / (2.0 * 0.05 * 0.05)).exp());
            }
            let b = 0.22 + 0.78 * best * (0.30 + 0.70 * act);
            let c = Color::rgb(
                (hue.0 as f32 * b) as u8, (hue.1 as f32 * b) as u8, (hue.2 as f32 * b) as u8);
            be.stroke_line(prev.0, prev.1, pt.0, pt.1, 2.0, c);
            prev = pt;
        }
    }

    // Rubber-band wire (dashed) while dragging.
    if let (Some((src, stream)), Some((cx, cy))) = (ctx.wiring_from, ctx.drag_cursor) {
        if let Some(sr) = rect_of(src) {
            let p0 = jack_pos(sr, if stream == Stream::Stdout { 1 } else { 2 });
            let p3 = (cx, cy);
            let ext = ((p3.0 - p0.0).abs() * 0.4 + 40.0).min(180.0);
            let p1 = (p0.0 + ext, p0.1);
            let p2 = (p3.0 - ext, p3.1);
            let (hr, hg, hb) = if stream == Stream::Stdout { (0x40u8, 0xc0u8, 0x54u8) } else { (0xd0, 0x54, 0x30) };
            let n = |v: u8| v as f32 / 255.0;
            let c = Color(n(hr), n(hg), n(hb), 180.0 / 255.0);
            let mut prev = p0;
            for i in 1..=40u32 {
                let t = i as f32 / 40.0;
                let pt = cubic_bezier(p0, p1, p2, p3, t);
                if i % 2 == 0 { be.stroke_line(prev.0, prev.1, pt.0, pt.1, 1.6, c); }
                prev = pt;
            }
        }
    }

    // Latency: the content-region perimeter, drawn last.
    if inst_latency {
        let cb = content_bounds(size);
        let (fx, fy, fw, fh) = (cb.x, cb.y, cb.w, cb.h);
        let per = 2.0 * (fw + fh);
        let corners = [
            ((fx, fy), 0.0),
            ((fx + fw, fy), fw),
            ((fx + fw, fy + fh), fw + fh),
            ((fx, fy + fh), 2.0 * fw + fh),
            ((fx, fy), per),
        ];
        const SUB: u32 = 14; // latency-frame segments per edge (trimmed from 26; a thin frame reads fine)
        for e in 0..4 {
            let (pa, da) = corners[e];
            let (pb, db) = corners[e + 1];
            let mut prev = pa;
            for s in 1..=SUB {
                let f = s as f32 / SUB as f32;
                let pt = (pa.0 + (pb.0 - pa.0) * f, pa.1 + (pb.1 - pa.1) * f);
                let mid_t = (da + (db - da) * (f - 0.5 / SUB as f32)) / per;
                let c = latency_color(mid_t, ctx.lat_phase, ctx.stall);
                be.stroke_line(prev.0, prev.1, pt.0, pt.1, 2.0, c);
                prev = pt;
            }
        }
    }

    // Jack dots on every pane. Drawn LAST so nothing crosses them: the latency
    // frame and heat borders run along the very pane edges the jacks sit on, and
    // would otherwise slice a vertical line through each disc.
    if ctx.show_jacks {
        for (id, r) in rects {
            let has_in = ctx.wires.iter().any(|w| w.dst == *id);
            let has_out = ctx.wires.iter().any(|w| w.src == *id && w.stream == Stream::Stdout);
            let has_err = ctx.wires.iter().any(|w| w.src == *id && w.stream == Stream::Stderr);
            let mut jack = |p: (f32, f32), filled: bool, c: crate::render::Color| {
                be.fill_circle(p.0, p.1, JACK_R_BACK, crate::render::Color(0.0, 0.0, 0.0, 0.70));
                if filled { be.fill_circle(p.0, p.1, JACK_R_FILL, c); }
                else { be.stroke_circle(p.0, p.1, JACK_R_RING, JACK_RING_W, c); }
            };
            jack(jack_pos(r, 0), has_in, crate::render::Color::rgb(0x88, 0x88, 0x98));
            jack(jack_pos(r, 1), has_out, crate::render::Color::rgb(0x40, 0xc0, 0x54));
            jack(jack_pos(r, 2), has_err, crate::render::Color::rgb(0xd0, 0x54, 0x30));
        }
    }
}
