//! Touch handling.

use std::time::Duration;

use dpi::LogicalPosition;
use sctk::compositor::SurfaceData;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::protocol::wl_touch::WlTouch;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};
use sctk::reexports::csd_frame::FrameClick;
use sctk::seat::touch::{TouchData, TouchHandler};
use tracing::warn;
use winit_core::event::{
    ButtonSource, ElementState, FingerId, PointerKind, PointerSource, WindowEvent,
};

use crate::state::WinitState;

/// The window a touched surface belongs to, and whether that surface is one of
/// the client-side decoration's subsurfaces rather than the window's own.
///
/// This is the resolution step the pointer handler has always done and the
/// touch handler never did: a touch landing on the frame reports the FRAME's
/// surface, which is not a window in `WinitState::windows`, so the event was
/// looked up, missed, and dropped. That — not any missing gesture — is why a
/// finger could not move, resize or close a winit window.
fn touched_window(surface: &WlSurface) -> Option<(crate::WindowId, bool)> {
    let data = surface.data::<SurfaceData>()?;
    match data.parent_surface() {
        Some(parent) => Some((crate::make_wid(parent), true)),
        None => Some((crate::make_wid(surface), false)),
    }
}

impl TouchHandler for WinitState {
    fn down(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        touch: &WlTouch,
        serial: u32,
        time: u32,
        surface: WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        let Some((window_id, on_decoration)) = touched_window(&surface) else { return };
        let scale_factor = match self.windows.get_mut().get(&window_id) {
            Some(window) => window.lock().unwrap().scale_factor(),
            None => return,
        };

        let seat_state = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => seat_state,
            None => {
                warn!("Received wl_touch::down without seat");
                return;
            },
        };

        // Update the state of the point.
        let location = LogicalPosition::<f64>::from(position);
        // Only update primary finger once we don't have any touch.
        if seat_state.touch_map.is_empty() {
            seat_state.first_touch_id = Some(id);
        }
        let primary = seat_state.first_touch_id == Some(id);
        // Kept so `up` and `motion` can find the point again — and, for a touch
        // on the frame, so they can tell it apart from one on the window.
        let frame_surface = on_decoration.then(|| surface.clone());
        seat_state.touch_map.insert(id, TouchPoint { surface, location });

        let position = location.to_physical(scale_factor);
        let finger_id = FingerId::from_raw(id as usize);

        // A touch on the decoration drives the frame, not the application: the
        // title bar moves the window, an edge resizes it, a button minimises,
        // maximises or closes. It is deliberately NOT also reported as pointer
        // input — the client never sees mouse presses on its frame either.
        if let Some(frame_surface) = frame_surface {
            let seat = touch.seat().clone();
            let time = Duration::from_millis(time as u64);
            if let Some(window) = self.windows.get_mut().get_mut(&window_id) {
                let mut window = window.lock().unwrap();
                // The frame acts on the part it last saw a point over, and a
                // finger has no hover to have established that. Tell the frame
                // where the touch landed before telling it that it was clicked;
                // without this the click resolves against stale hover state (or
                // none at all, on the first touch of the session).
                window.frame_point_moved(&seat, &frame_surface, time, location.x, location.y);
                window.frame_click(
                    FrameClick::Normal,
                    true,
                    &seat,
                    serial,
                    time,
                    window_id,
                    &mut self.window_compositor_updates,
                );
                // A title-bar click only ARMS the move; upstream fires it from
                // the next pointer motion, which is how a mouse drag begins.
                // Waiting for motion is wrong for a finger — the compositor
                // takes the grab the moment the move starts, so the motion that
                // would have triggered it may never be delivered. Feeding the
                // same point back in fires it here, on the press, and is a no-op
                // when the click armed nothing (a button, an edge, the body).
                window.frame_point_moved(&seat, &frame_surface, time, location.x, location.y);
            }
            return;
        }

        self.events_sink.push_window_event(
            WindowEvent::PointerEntered {
                device_id: None,
                primary,
                position,
                kind: PointerKind::Touch(finger_id),
            },
            window_id,
        );
        self.events_sink.push_window_event(
            WindowEvent::PointerButton {
                device_id: None,
                primary,
                state: ElementState::Pressed,
                position,
                button: ButtonSource::Touch { finger_id, force: None },
            },
            window_id,
        );
    }

    fn up(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        touch: &WlTouch,
        serial: u32,
        time: u32,
        id: i32,
    ) {
        let seat_state = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => seat_state,
            None => {
                warn!("Received wl_touch::up without seat");
                return;
            },
        };

        // Remove the touch point.
        let touch_point = match seat_state.touch_map.remove(&id) {
            Some(touch_point) => touch_point,
            None => return,
        };

        // Update the primary touch point.
        let primary = seat_state.first_touch_id == Some(id);
        // Reset primary finger once all the other fingers are lifted to not transfer primary
        // finger to some other finger and still accept it when it's briefly moved between the
        // windows.
        if seat_state.touch_map.is_empty() {
            seat_state.first_touch_id = None;
        }

        let Some((window_id, on_decoration)) = touched_window(&touch_point.surface) else { return };
        let scale_factor = match self.windows.get_mut().get(&window_id) {
            Some(window) => window.lock().unwrap().scale_factor(),
            None => return,
        };

        // The other half of a click on the frame. Some frame actions fire on
        // release rather than press, so the button-up matters as much as the
        // button-down; `frame_point_left` then clears the hover a finger leaves
        // behind, which a pointer would have cleared by moving away.
        if on_decoration {
            let seat = touch.seat().clone();
            if let Some(window) = self.windows.get_mut().get_mut(&window_id) {
                let mut window = window.lock().unwrap();
                window.frame_click(
                    FrameClick::Normal,
                    false,
                    &seat,
                    serial,
                    Duration::from_millis(time as u64),
                    window_id,
                    &mut self.window_compositor_updates,
                );
                // A tap that armed a move it never used must not leave the
                // serial behind for a later hover to trip over.
                window.frame_cancel_pending_move();
                window.frame_point_left();
            }
            return;
        }

        let position = touch_point.location.to_physical(scale_factor);
        let finger_id = FingerId::from_raw(id as usize);

        self.events_sink.push_window_event(
            WindowEvent::PointerButton {
                device_id: None,
                primary,
                state: ElementState::Released,
                position,
                button: ButtonSource::Touch { finger_id, force: None },
            },
            window_id,
        );
        self.events_sink.push_window_event(
            WindowEvent::PointerLeft {
                device_id: None,
                primary,
                position: Some(position),
                kind: PointerKind::Touch(finger_id),
            },
            window_id,
        );
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        touch: &WlTouch,
        time: u32,
        id: i32,
        position: (f64, f64),
    ) {
        let seat_state = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => seat_state,
            None => {
                warn!("Received wl_touch::motion without seat");
                return;
            },
        };

        // Remove the touch point.
        let touch_point = match seat_state.touch_map.get_mut(&id) {
            Some(touch_point) => touch_point,
            None => return,
        };

        let primary = seat_state.first_touch_id == Some(id);

        let Some((window_id, on_decoration)) = touched_window(&touch_point.surface) else { return };
        let scale_factor = match self.windows.get_mut().get(&window_id) {
            Some(window) => window.lock().unwrap().scale_factor(),
            None => return,
        };

        touch_point.location = LogicalPosition::<f64>::from(position);
        let location = touch_point.location;
        let frame_surface = on_decoration.then(|| touch_point.surface.clone());

        // Sliding a finger along the frame keeps the frame's idea of which part
        // is under it up to date, so a touch that starts on the title bar and
        // drifts onto a resize edge still ends where the finger is. Once a move
        // or resize actually begins the compositor takes the grab and no more
        // motion arrives here at all.
        if let Some(frame_surface) = frame_surface {
            let seat = touch.seat().clone();
            if let Some(window) = self.windows.get_mut().get_mut(&window_id) {
                window.lock().unwrap().frame_point_moved(
                    &seat,
                    &frame_surface,
                    Duration::from_millis(time as u64),
                    location.x,
                    location.y,
                );
            }
            return;
        }

        self.events_sink.push_window_event(
            WindowEvent::PointerMoved {
                device_id: None,
                primary,
                position: touch_point.location.to_physical(scale_factor),
                source: PointerSource::Touch {
                    finger_id: FingerId::from_raw(id as usize),
                    force: None,
                },
            },
            window_id,
        );
    }

    fn cancel(&mut self, _: &Connection, _: &QueueHandle<Self>, touch: &WlTouch) {
        let seat_state = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => seat_state,
            None => {
                warn!("Received wl_touch::cancel without seat");
                return;
            },
        };

        for (id, touch_point) in seat_state.touch_map.drain() {
            let Some((window_id, on_decoration)) = touched_window(&touch_point.surface) else {
                continue;
            };
            let scale_factor = match self.windows.get_mut().get(&window_id) {
                Some(window) => window.lock().unwrap().scale_factor(),
                None => return,
            };

            // A cancelled touch on the frame leaves no click behind — only the
            // hover to undo.
            if on_decoration {
                if let Some(window) = self.windows.get_mut().get_mut(&window_id) {
                    window.lock().unwrap().frame_point_left();
                }
                continue;
            }

            let primary = seat_state.first_touch_id == Some(id);
            let position = touch_point.location.to_physical(scale_factor);

            self.events_sink.push_window_event(
                WindowEvent::PointerLeft {
                    device_id: None,
                    primary,
                    position: Some(position),
                    kind: PointerKind::Touch(FingerId::from_raw(id as usize)),
                },
                window_id,
            );
        }

        seat_state.first_touch_id = None;
    }

    fn shape(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlTouch,
        _: i32,
        _: f64,
        _: f64,
    ) {
        // Blank.
    }

    fn orientation(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlTouch, _: i32, _: f64) {
        // Blank.
    }
}

/// The state of the touch point.
#[derive(Debug)]
pub struct TouchPoint {
    /// The surface on which the point is present.
    pub surface: WlSurface,

    /// The location of the point on the surface.
    pub location: LogicalPosition<f64>,
}

pub trait TouchDataExt {
    fn seat(&self) -> &WlSeat;
}

impl TouchDataExt for WlTouch {
    fn seat(&self) -> &WlSeat {
        self.data::<TouchData>().expect("failed to get touch data.").seat()
    }
}

sctk::delegate_touch!(WinitState);
