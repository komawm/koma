//! Client `wl_data_device` drag-and-drop: grab installation, target offer routing and drag icon
//! rendering.
//!
//! Koma previously relied on Smithay's default `WaylandDndGrabHandler` implementation, which calls
//! `Source::cancel` as soon as a client requests a drag. That made every client DnD unusable:
//! Firefox/Chromium tore a dragged tab off into a new window, a file dragged from a file manager
//! into a browser transferred nothing, and dragging a URL or a text selection between applications
//! aborted immediately.
//!
//! This module installs Smithay's [`DnDGrab`] instead, which owns the drag for the whole gesture:
//!
//! - [`start_client_drag`] validates the implicit-grab serial, applies the session-lock gate and
//!   installs a pointer or touch [`DnDGrab`];
//! - Smithay's grab drives `DndFocus` for the surface under the cursor, so `wl_data_device.enter`,
//!   `.motion` and `.drop` reach the target and the target's `wl_data_offer` answers tell the
//!   compositor whether the drop was accepted;
//! - [`DndIcon`] tracks the client's drag icon surface and its `wl_surface` hot-spot offset, and
//!   [`icon_render_elements`] turns it into the same [`KomaRenderElement`] list the rest of the
//!   frame uses, drawn directly below the cursor;
//! - [`finish_client_drag`] clears the tracked drag when Smithay reports `dropped` or `cancelled`.
//!
//! Source lifecycle is entirely Smithay's: an accepted drop calls `Source::drop_performed` (the
//! target's `wl_data_offer.finish` request then calls `Source::finished`), while a rejected or
//! aborted drop calls `Source::cancel`. Koma must not call either itself, or a client would see the
//! terminal source event twice.
//!
//! Koma's `SeatHandler::PointerFocus` and `SeatHandler::TouchFocus` are both `WlSurface`, for which
//! Smithay already provides the `DndFocus` implementation, so implementing it again here is
//! neither possible nor needed. The Koma hit test is applied *before* dispatch: pointer and touch
//! motion resolve [`KomaState::hit_test_target`](crate::state::KomaState::hit_test_target) (lock
//! surface, then windows, then layer-shell tiers) and hand that surface to `PointerHandle::motion`
//! / `TouchHandle::motion`, which is exactly the focus the grab offers the drop to. No second
//! hit-testing path is introduced here.

use koma_render::elements::{KomaRenderElement, KomaRenderTarget};
use smithay::{
    backend::renderer::element::{
        Kind,
        surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
    },
    input::{
        Seat,
        dnd::{DnDGrab, GrabType, Source},
        pointer::Focus,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive as _, Logical, Point, Scale, Serial},
    wayland::compositor::{SurfaceAttributes, with_states},
};
use tracing::debug;

use crate::state::KomaState;

/// Client-provided drag icon surface together with its hot-spot offset from the cursor.
///
/// The icon surface carries Smithay's `dnd_icon` role, so it is never mapped, tiled or hit-tested
/// like a toplevel: it only exists as this overlay element. `wl_surface.attach`/`wl_surface.offset`
/// deltas move the icon relative to the cursor hot spot, which is how toolkits keep a dragged item
/// visually grabbed by its centre instead of its top-left corner.
#[derive(Debug, Clone)]
pub struct DndIcon {
    surface: WlSurface,
    offset: Point<i32, Logical>,
}

impl DndIcon {
    /// Tracks `surface` as a drag icon starting at the cursor hot spot (`offset = 0, 0`).
    #[must_use]
    pub fn new(surface: WlSurface) -> Self {
        Self {
            surface,
            offset: Point::from((0, 0)),
        }
    }

    /// The client's icon surface.
    #[must_use]
    pub fn surface(&self) -> &WlSurface {
        &self.surface
    }

    /// Accumulated icon offset from the cursor hot spot, in logical surface coordinates.
    #[must_use]
    pub fn offset(&self) -> Point<i32, Logical> {
        self.offset
    }

    /// Icon top-left corner in the same coordinate space as `pointer_location`.
    #[must_use]
    pub fn location(&self, pointer_location: Point<f64, Logical>) -> Point<f64, Logical> {
        icon_hot_spot(self.offset(), pointer_location)
    }

    /// Applies a committed `wl_surface` buffer delta to the icon hot-spot offset.
    pub fn apply_buffer_delta(&mut self, delta: Point<i32, Logical>) {
        self.offset += delta;
    }
}

/// Active client drag-and-drop session.
///
/// Koma hosts a single seat (`KomaState::seat`), which owns the drag grab, so the session only has
/// to remember whether a drag is running and which icon it uses:
/// `Some(ActiveDnd { icon: None })` is a legal icon-less drag and still marks the seat as owned.
#[derive(Debug)]
pub struct ActiveDnd {
    icon: Option<DndIcon>,
}

impl ActiveDnd {
    /// The client icon surface, when the drag provided one.
    #[must_use]
    pub fn icon(&self) -> Option<&DndIcon> {
        self.icon.as_ref()
    }

    /// Mutable access to the tracked icon, for committed hot-spot deltas.
    pub fn icon_mut(&mut self) -> Option<&mut DndIcon> {
        self.icon.as_mut()
    }
}

/// Places a drag icon's top-left corner relative to the pointer hot spot.
///
/// The protocol starts the icon at the hot spot and lets the client move it with
/// `wl_surface.attach`/`wl_surface.offset` deltas, so the icon position is simply
/// `pointer + accumulated delta`.
#[must_use]
pub fn icon_hot_spot(
    offset: Point<i32, Logical>,
    pointer_location: Point<f64, Logical>,
) -> Point<f64, Logical> {
    pointer_location + offset.to_f64()
}

/// Takes the committed buffer delta (attach offset or `wl_surface.offset`) of `surface`.
///
/// Called from the compositor commit handler for the tracked icon surface only, mirroring the
/// cursor hot-spot bookkeeping in [`CursorManager::on_surface_commit`](crate::cursor::CursorManager::on_surface_commit).
#[must_use]
pub fn take_buffer_delta(surface: &WlSurface) -> Option<Point<i32, Logical>> {
    with_states(surface, |states| {
        let mut cached = states.cached_state.get::<SurfaceAttributes>();
        cached.current().buffer_delta.take()
    })
}

/// Whether `surface` may take part in a host drag-and-drop operation.
///
/// Jailed sandbox clients never receive host data-device focus
/// (`set_data_device_focus` withholds it), so they must neither originate a drag nor be offered
/// one: either direction would move data across the sandbox boundary that the security context
/// deliberately closes. The check is intentionally the same predicate the clipboard gate uses.
#[must_use]
pub fn surface_may_exchange_drag_data(state: &KomaState, surface: &WlSurface) -> bool {
    crate::security_context::resolve_surface_jailed_workspace(state, surface).is_none()
}

/// Builds the drag icon element list for `pointer_location` (output-local logical coordinates).
///
/// The icon joins the ordinary frame element list through the same
/// [`WaylandSurfaceRenderElement`](smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement)
/// conversion used by windows, layer surfaces and client cursors, so it is composited by the
/// existing renderer path (`GlesRenderer` for nested/offscreen, `TtyMultiRenderer` for DRM/KMS)
/// without a second renderer.
#[must_use]
pub fn icon_render_elements<R>(
    icon: &DndIcon,
    renderer: &mut R,
    pointer_location: Point<f64, Logical>,
    output_scale: Scale<f64>,
) -> Vec<KomaRenderElement<R>>
where
    R: KomaRenderTarget,
    R::TextureId: Clone + Send + 'static,
{
    if !icon.surface.alive() {
        return Vec::new();
    }
    let location = icon
        .location(pointer_location)
        .to_physical_precise_round(output_scale);
    let elems: Vec<WaylandSurfaceRenderElement<R>> = render_elements_from_surface_tree(
        renderer,
        &icon.surface,
        location,
        output_scale,
        1.0,
        Kind::Unspecified,
    );
    elems.into_iter().map(KomaRenderElement::from).collect()
}

/// Installs the drag grab for a client `wl_data_device.start_drag` request.
///
/// Every precondition fails closed: the session lock gate, an already-active drag, a missing seat
/// handle, a serial that does not belong to the current implicit grab, a grab whose original focus
/// surface is already gone, and a jailed sandbox origin all cancel the source so the client cannot
/// leave a dangling `wl_data_source` behind.
///
/// Returns `true` when the grab was installed and the icon is now tracked.
pub fn start_client_drag<S: Source>(
    state: &mut KomaState,
    source: S,
    icon: Option<WlSurface>,
    seat: Seat<KomaState>,
    serial: Serial,
    type_: GrabType,
) -> bool {
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        debug!("refused client drag-and-drop: session is locked or abandoned");
        source.cancel();
        return false;
    }
    if state.dnd_drag.is_some() {
        debug!("refused client drag-and-drop: another drag already owns the seat");
        source.cancel();
        return false;
    }
    // The drag owns the pointer from here on, so a pending keybind repeat must stop.
    state.cancel_keybind_repeat("pointer_grab");

    match type_ {
        GrabType::Pointer => {
            let Some(pointer) = state.ptr() else {
                debug!("refused client drag-and-drop: seat has no pointer handle");
                source.cancel();
                return false;
            };
            if !pointer.has_grab(serial) {
                debug!(
                    ?serial,
                    "refused client drag-and-drop: serial is not the implicit grab"
                );
                source.cancel();
                return false;
            }
            let Some(start_data) = pointer.grab_start_data() else {
                debug!("refused client drag-and-drop: implicit grab has no start data");
                source.cancel();
                return false;
            };
            if !drag_origin_allowed(state, start_data.focus.as_ref().map(|(focus, _)| focus)) {
                source.cancel();
                return false;
            }
            let grab =
                DnDGrab::new_pointer(&state.display_handle, start_data, source, seat.clone());
            drop(pointer);
            state.with_pointer_dispatch(|state, pointer| {
                pointer.set_grab(state, grab, serial, Focus::Keep);
            });
        }
        GrabType::Touch => {
            let Some(touch) = state.touch() else {
                debug!("refused client drag-and-drop: seat has no touch handle");
                source.cancel();
                return false;
            };
            if !touch.has_grab(serial) {
                debug!(
                    ?serial,
                    "refused client drag-and-drop: serial is not the implicit grab"
                );
                source.cancel();
                return false;
            }
            let Some(start_data) = touch.grab_start_data() else {
                debug!("refused client drag-and-drop: implicit grab has no start data");
                source.cancel();
                return false;
            };
            if !drag_origin_allowed(state, start_data.focus.as_ref().map(|(focus, _)| focus)) {
                source.cancel();
                return false;
            }
            let grab = DnDGrab::new_touch(&state.display_handle, start_data, source, seat.clone());
            touch.set_grab(state, grab, serial);
        }
    }

    state.dnd_drag = Some(ActiveDnd {
        icon: icon.map(DndIcon::new),
    });
    state.queue_redraw();
    true
}

/// Validates the implicit-grab origin of a drag request.
///
/// The origin surface has to be alive (Smithay's grab keeps a handle to it) and must belong to a
/// client that is allowed to exchange data devices with the host; a jailed sandbox client gets a
/// cancellation instead of a drag.
fn drag_origin_allowed(state: &KomaState, focus: Option<&WlSurface>) -> bool {
    let Some(focus) = focus else {
        debug!("refused client drag-and-drop: implicit grab is not over a client surface");
        return false;
    };
    if !focus.alive() {
        debug!("refused client drag-and-drop: implicit grab focus surface is gone");
        return false;
    }
    if !surface_may_exchange_drag_data(state, focus) {
        debug!("refused client drag-and-drop from a jailed sandbox client");
        return false;
    }
    true
}

/// Clears the tracked drag after Smithay reported `dropped` or `cancelled`.
///
/// The icon must vanish as soon as the gesture ends, including when the grab is cancelled by the
/// session lock or a surface disappearing, so this is called from both `DndGrabHandler` hooks and
/// from every cancel path that releases the grab.
pub fn finish_client_drag(state: &mut KomaState, reason: &'static str) {
    if state.dnd_drag.take().is_some() {
        debug!(reason, "client drag-and-drop ended");
        state.queue_redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_hot_spot_tracks_the_pointer_by_the_committed_delta() {
        let pointer: Point<f64, Logical> = Point::from((100.0, 200.0));
        assert_eq!(icon_hot_spot((0, 0).into(), pointer), pointer);
        assert_eq!(
            icon_hot_spot((-8, -12).into(), pointer),
            Point::from((92.0, 188.0)),
            "a centred icon is translated left/up of the hot spot"
        );
    }
}
