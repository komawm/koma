//! Interactive Wayland `PointerGrab<KomaState>` implementations for `Mod + MouseLeft` window
//! dragging (floating 1:1 move + tiled `0.85x` `MAPDRAGHOTSPOT` detach + live 4-quadrant
//! `smart_split` drop preview + opposite-button float toggle) and `Mod + MouseRight` / CSD
//! `xdg_toplevel.{move,resize}` corner-aware split-ratio & floating edge resizing.
//!
//! Modeled directly on:
//! - `Hyprland/src/layout/supplementary/DragController.cpp` (proportional hotspot mapping lines
//!   97-111, nearest-corner quadrant detection lines 204-223, manual resize snap)
//! - `Hyprland/src/layout/algorithm/tiled/dwindle/DwindleAlgorithm.cpp` (4-triangle diagonal
//!   `smart_split` lines 179-206 & outer/inner `smart_resizing` lines 295-378)
//! - `niri/src/input/{move_grab.rs, resize_grab.rs}` & `niri/src/handlers/xdg_shell.rs`
//!   (`PointerGrab<State>` trait lifecycle, `64.0 px²` drag recognition hysteresis, opposite-button
//!   mid-drag floating toggle, `Focus::Clear` ungrab focus restoration).

use koma_model::{
    DropZonePreview, EdgeScrollStep, FloatingStack, LayoutMode, LogicalPoint, LogicalRect,
    PresetColumnWidth, ResizeCorner, ScrollingDropZone,
};
use koma_proto::{WindowId, WorkspaceId};
use smithay::{
    backend::input::{ButtonState, InputTime},
    input::{
        SeatHandler,
        pointer::{
            AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, Focus, GestureHoldBeginEvent,
            GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
            GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
            GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent,
            PointerGrab, PointerInnerHandle, RelativeMotionEvent,
        },
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, SERIAL_COUNTER, Serial},
};
use tracing::debug;

use crate::state::{KomaState, read_toplevel_min_max_size};

/// Standard Linux `input-event-codes.h` pointer button codes used by `libinput` & `winit`.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;

/// Squared cursor travel threshold (`8.0 * 8.0 = 64.0` logical px²) before a `Mod + MouseLeft`
/// click on a tiled window detaches it from `DwindleTree` (`niri/src/input/move_grab.rs` line 213).
pub const DRAG_RECOGNITION_THRESHOLD_SQ: f64 = 64.0;

/// Scale factor applied when untiling a `Dwindle` / `Scrolling` tile into a floating drag card
/// (`Hyprland` `DragController.cpp` line 97: `~0.85`).
pub const DETACHED_TILE_DRAG_SCALE: f64 = 0.85;

/// Maps `ResizeCorner` to its canonical Wayland cursor icon (`niri/src/utils/mod.rs` lines 129-141).
#[must_use]
pub const fn cursor_icon_for_corner(corner: ResizeCorner) -> CursorIcon {
    match corner {
        ResizeCorner::Left => CursorIcon::WResize,
        ResizeCorner::Right => CursorIcon::EResize,
        ResizeCorner::Top => CursorIcon::NResize,
        ResizeCorner::Bottom => CursorIcon::SResize,
        ResizeCorner::TopLeft => CursorIcon::NwResize,
        ResizeCorner::TopRight => CursorIcon::NeResize,
        ResizeCorner::BottomRight => CursorIcon::SeResize,
        ResizeCorner::BottomLeft => CursorIcon::SwResize,
        ResizeCorner::None => CursorIcon::Default,
    }
}

/// Converts a client `xdg_toplevel::ResizeEdge` request into Koma's `ResizeCorner`.
#[must_use]
pub const fn resize_corner_from_xdg_edge(edge: xdg_toplevel::ResizeEdge) -> Option<ResizeCorner> {
    match edge {
        xdg_toplevel::ResizeEdge::Top => Some(ResizeCorner::Top),
        xdg_toplevel::ResizeEdge::Bottom => Some(ResizeCorner::Bottom),
        xdg_toplevel::ResizeEdge::Left => Some(ResizeCorner::Left),
        xdg_toplevel::ResizeEdge::Right => Some(ResizeCorner::Right),
        xdg_toplevel::ResizeEdge::TopLeft => Some(ResizeCorner::TopLeft),
        xdg_toplevel::ResizeEdge::TopRight => Some(ResizeCorner::TopRight),
        xdg_toplevel::ResizeEdge::BottomLeft => Some(ResizeCorner::BottomLeft),
        xdg_toplevel::ResizeEdge::BottomRight => Some(ResizeCorner::BottomRight),
        _ => None,
    }
}

/// Initiates a compositor-bound (`Mod + MouseLeft`) interactive window move grab before
/// forwarding the triggering `ButtonEvent` to `PointerHandle::button`.
pub fn start_compositor_move_grab(state: &mut KomaState, button_code: u32, serial: Serial) -> bool {
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        return false;
    }
    // An interactive grab owns the pointer for the whole gesture: stop a pending keybind
    // repeat so the held chord cannot keep moving windows behind the user's back.
    state.cancel_keybind_repeat("pointer_grab");
    state.break_active_pointer_constraints();
    let Some(pointer) = state.ptr() else {
        return false;
    };
    if pointer.is_grabbed() {
        return false;
    }

    let cursor_loc = state.pointer_location;
    let Some((win_id, ws_id, _, _)) = state.hit_test_visible_surface(cursor_loc) else {
        return false;
    };

    if state.active_special_ws_id.is_some() && state.active_special_ws_id.as_ref() != Some(&ws_id) {
        state.active_special_ws_id = None;
    }
    let now_ms = state.elapsed_ms();
    if let Some(ws) = state.workspaces.get_mut(&ws_id) {
        ws.activate_window(win_id, now_ms);
    }
    state.focus_active_deck_window();

    let start_data = PointerGrabStartData {
        focus: None,
        button: button_code,
        location: cursor_loc,
    };

    let Some(grab) = MoveWindowGrab::new(state, start_data, win_id, ws_id) else {
        return false;
    };

    pointer.set_grab(state, grab, serial, Focus::Clear);
    state.set_cursor_status(CursorImageStatus::Named(CursorIcon::Grabbing));
    true
}

/// Initiates a compositor-bound (`Mod + MouseRight`) interactive window / split-ratio resize grab
/// before forwarding the triggering `ButtonEvent` to `PointerHandle::button`.
pub fn start_compositor_resize_grab(
    state: &mut KomaState,
    button_code: u32,
    serial: Serial,
) -> bool {
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        return false;
    }
    // An interactive grab owns the pointer for the whole gesture: stop a pending keybind
    // repeat so the held chord cannot keep moving windows behind the user's back.
    state.cancel_keybind_repeat("pointer_grab");
    state.break_active_pointer_constraints();
    let Some(pointer) = state.ptr() else {
        return false;
    };
    if pointer.is_grabbed() {
        return false;
    }

    let cursor_loc = state.pointer_location;
    let Some((win_id, ws_id, _, _)) = state.hit_test_visible_surface(cursor_loc) else {
        return false;
    };

    if state.active_special_ws_id.is_some() && state.active_special_ws_id.as_ref() != Some(&ws_id) {
        state.active_special_ws_id = None;
    }
    let now_ms = state.elapsed_ms();
    if let Some(ws) = state.workspaces.get_mut(&ws_id) {
        ws.activate_window(win_id, now_ms);
    }
    state.focus_active_deck_window();

    let start_data = PointerGrabStartData {
        focus: None,
        button: button_code,
        location: cursor_loc,
    };

    let Some((grab, corner)) = ResizeWindowGrab::new(state, start_data, win_id, ws_id, None) else {
        return false;
    };

    pointer.set_grab(state, grab, serial, Focus::Clear);
    state.set_cursor_status(CursorImageStatus::Named(cursor_icon_for_corner(corner)));
    true
}

/// Initiates an interactive resize from a frame edge/corner drag, no modifier required
/// (`Hyprland` `DragController` border-drag parity). Returns `true` when a grab was installed.
///
/// `Deck` has no resize semantics — the active window always fills the viewport — so the request is
/// refused with an explicit `debug!` instead of falling through the grab's silent `Deck => {}` arm.
pub fn start_frame_edge_resize_grab(
    state: &mut KomaState,
    button_code: u32,
    serial: Serial,
    edge: (WindowId, WorkspaceId, ResizeCorner),
) -> bool {
    let (win_id, ws_id, corner) = edge;
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        return false;
    }
    // An interactive grab owns the pointer for the whole gesture: stop a pending keybind
    // repeat so the held chord cannot keep moving windows behind the user's back.
    state.cancel_keybind_repeat("pointer_grab");
    state.break_active_pointer_constraints();
    let Some(pointer) = state.ptr() else {
        return false;
    };
    if pointer.is_grabbed() {
        return false;
    }

    let Some(ws) = state.workspaces.get(&ws_id) else {
        return false;
    };
    let is_floating = ws.floating.contains(win_id);
    if !ws.layout_mode.supports_frame_edge_resize(is_floating) {
        debug!(
            layout = ?ws.layout_mode,
            "Deck lays the focused window out full-viewport, so a frame edge cannot resize it; \
             float the window first (komawm/koma#42)"
        );
        return false;
    }

    let now_ms = state.elapsed_ms();
    if let Some(ws) = state.workspaces.get_mut(&ws_id) {
        ws.activate_window(win_id, now_ms);
    }
    state.focus_active_deck_window();

    let start_data = PointerGrabStartData {
        focus: None,
        button: button_code,
        location: state.pointer_location,
    };
    let Some((grab, corner)) =
        ResizeWindowGrab::new(state, start_data, win_id, ws_id, Some(corner))
    else {
        return false;
    };

    pointer.set_grab(state, grab, serial, Focus::Clear);
    state.set_cursor_status(CursorImageStatus::Named(cursor_icon_for_corner(corner)));
    state.frame_edge_hover = Some((win_id, corner));
    true
}

/// Initiates a client-requested CSD `xdg_toplevel.move` grab after validating serial & active
/// interactive workspace ownership (`niri/src/handlers/xdg_shell.rs` lines 71-182 & Hyprland `#15856`).
pub fn start_csd_move_grab(
    state: &mut KomaState,
    win_id: WindowId,
    start_data: PointerGrabStartData<KomaState>,
    serial: Serial,
) -> bool {
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        return false;
    }
    state.break_active_pointer_constraints();
    let Some(pointer) = state.ptr() else {
        return false;
    };
    let interactive_ws = state.effective_interactive_ws_id();
    let Some(ws_id) = state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win_id)
        .map(|m| m.ws_id.clone())
    else {
        return false;
    };
    if ws_id != interactive_ws {
        return false;
    }

    let clean_start = PointerGrabStartData {
        focus: None,
        button: start_data.button,
        location: start_data.location,
    };
    let Some(grab) = MoveWindowGrab::new(state, clean_start, win_id, ws_id) else {
        return false;
    };

    pointer.set_grab(state, grab, serial, Focus::Clear);
    state.set_cursor_status(CursorImageStatus::Named(CursorIcon::Grabbing));
    true
}

/// Initiates a client-requested CSD `xdg_toplevel.resize` grab after validating serial, edges,
/// and active interactive workspace ownership (`niri/src/handlers/xdg_shell.rs` lines 184-309).
pub fn start_csd_resize_grab(
    state: &mut KomaState,
    win_id: WindowId,
    start_data: PointerGrabStartData<KomaState>,
    serial: Serial,
    edges: xdg_toplevel::ResizeEdge,
) -> bool {
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        return false;
    }
    state.break_active_pointer_constraints();
    let Some(explicit_corner) = resize_corner_from_xdg_edge(edges) else {
        return false;
    };
    let Some(pointer) = state.ptr() else {
        return false;
    };
    let interactive_ws = state.effective_interactive_ws_id();
    let Some(ws_id) = state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win_id)
        .map(|m| m.ws_id.clone())
    else {
        return false;
    };
    if ws_id != interactive_ws {
        return false;
    }

    let clean_start = PointerGrabStartData {
        focus: None,
        button: start_data.button,
        location: start_data.location,
    };
    let Some((grab, corner)) =
        ResizeWindowGrab::new(state, clean_start, win_id, ws_id, Some(explicit_corner))
    else {
        return false;
    };

    pointer.set_grab(state, grab, serial, Focus::Clear);
    state.set_cursor_status(CursorImageStatus::Named(cursor_icon_for_corner(corner)));
    true
}

/// Cancels any active pointer grab on `Escape` press (`niri/src/input/mod.rs` lines 688-699).
pub fn cancel_active_pointer_grab(state: &mut KomaState, serial: Serial, time: InputTime) -> bool {
    let Some(pointer) = state.ptr() else {
        return false;
    };
    if !pointer.is_grabbed() {
        return false;
    }
    pointer.unset_grab(state, serial, time);
    state.keybinds.suppressed_buttons.insert(BTN_LEFT);
    state.keybinds.suppressed_buttons.insert(BTN_RIGHT);
    state.active_drop_preview = None;
    state.active_drag_edge_scroll = None;
    state.edge_scroll_keep_alive = false;
    for ws in state.workspaces.values_mut() {
        ws.scrolling.reset_edge_auto_scroll();
    }
    state.set_cursor_status(CursorImageStatus::default_named());
    true
}

/// Interactive window move grab supporting both `FloatingStack` windows and tiled
/// `Dwindle` / `Scrolling` / `Deck` windows.
pub struct MoveWindowGrab {
    start_data: PointerGrabStartData<KomaState>,
    win_id: WindowId,
    ws_id: WorkspaceId,
    // Initial floating state retained on grab for move diagnostics.
    #[allow(dead_code)]
    started_floating: bool,
    will_tile_on_drop: bool,
    drag_recognized: bool,
    saved_pseudotile_size: Option<(u32, u32)>,
    saved_scrolling_width: Option<PresetColumnWidth>,
    min_size: (u32, u32),
    initial_rect: LogicalRect,
    grab_hotspot: Point<f64, Logical>,
    current_location: Point<f64, Logical>,
    active_drop_zone: Option<DropZonePreview>,
    active_scrolling_drop_zone: Option<ScrollingDropZone>,
}

impl MoveWindowGrab {
    #[must_use]
    pub fn new(
        state: &KomaState,
        start_data: PointerGrabStartData<KomaState>,
        win_id: WindowId,
        ws_id: WorkspaceId,
    ) -> Option<Self> {
        let ws = state.workspaces.get(&ws_id)?;
        let mapped = state.mapped_windows.iter().find(|m| m.win_id == win_id)?;

        let started_floating = ws.floating.contains(win_id);
        let initial_rect = mapped.rect;
        let saved_pseudotile_size = ws.dwindle.pseudotile_size_of(win_id);
        let saved_scrolling_width = ws.scrolling.standalone_column_width_of(win_id);
        let min_size = mapped
            .window
            .toplevel()
            .map(|t| read_toplevel_min_max_size(t.wl_surface()).0)
            .unwrap_or((160, 120));

        let origin = state.workspace_output_origin(&ws_id);
        let local_start = start_data.location - origin;
        let grab_hotspot = Point::from((
            (local_start.x - f64::from(initial_rect.x))
                .clamp(0.0, f64::from(initial_rect.w.max(1))),
            (local_start.y - f64::from(initial_rect.y))
                .clamp(0.0, f64::from(initial_rect.h.max(1))),
        ));

        Some(Self {
            current_location: start_data.location,
            start_data,
            win_id,
            ws_id,
            started_floating,
            will_tile_on_drop: !started_floating,
            // Floating windows track cursor 1:1 from pixel 1; tiled windows wait for 8px hysteresis!
            drag_recognized: started_floating,
            saved_pseudotile_size,
            saved_scrolling_width,
            min_size,
            initial_rect,
            grab_hotspot,
            active_drop_zone: None,
            active_scrolling_drop_zone: None,
        })
    }

    fn detach_tiled_into_floating(&mut self, state: &mut KomaState) -> bool {
        let local_cursor = self.current_location - state.workspace_output_origin(&self.ws_id);
        let cursor = LogicalPoint {
            x: local_cursor.x,
            y: local_cursor.y,
        };
        let Some(ws) = state.workspaces.get_mut(&self.ws_id) else {
            return false;
        };
        if ws.presented_window_id() == Some(self.win_id) {
            ws.window_presentation = None;
        }
        let now_ms = state.elapsed_ms();
        let Some(ws) = state.workspaces.get_mut(&self.ws_id) else {
            return false;
        };
        if !ws.floating.contains(self.win_id) {
            let _ = ws.dwindle.remove(self.win_id, ws.viewport);
            let _ = ws.scrolling.remove(self.win_id, ws.viewport, now_ms);
            let _ = ws.deck.remove(self.win_id);

            let (detached_rect, hotspot) = FloatingStack::detached_tile_drag_rect(
                self.initial_rect,
                cursor,
                self.min_size,
                DETACHED_TILE_DRAG_SCALE,
            );
            self.grab_hotspot = Point::from((hotspot.x, hotspot.y));
            ws.floating.push_top(self.win_id, detached_rect);
            ws.focused_window = Some(self.win_id);
        }
        true
    }

    fn refresh_drop_preview(&mut self, state: &mut KomaState) {
        if !self.drag_recognized || !self.will_tile_on_drop {
            self.active_drop_zone = None;
            self.active_scrolling_drop_zone = None;
            state.active_drop_preview = None;
            state.active_drag_edge_scroll = None;
            state.edge_scroll_keep_alive = false;
            if let Some(ws) = state.workspaces.get_mut(&self.ws_id) {
                ws.scrolling.reset_edge_auto_scroll();
            }
            return;
        }

        let now_ms = state.elapsed_ms();
        let local_cursor = self.current_location - state.workspace_output_origin(&self.ws_id);
        let cursor = LogicalPoint {
            x: local_cursor.x,
            y: local_cursor.y,
        };
        let Some(ws) = state.workspaces.get_mut(&self.ws_id) else {
            self.active_drop_zone = None;
            self.active_scrolling_drop_zone = None;
            state.active_drop_preview = None;
            state.active_drag_edge_scroll = None;
            state.edge_scroll_keep_alive = false;
            return;
        };

        match ws.layout_mode {
            LayoutMode::Dwindle => {
                self.active_scrolling_drop_zone = None;
                state.active_drag_edge_scroll = None;
                state.edge_scroll_keep_alive = false;
                let dz = ws.dwindle.predict_drop_zone(cursor, ws.viewport);
                self.active_drop_zone = Some(dz);
                state.active_drop_preview = Some((self.ws_id.clone(), dz.preview_rect));
            }
            LayoutMode::Scrolling => {
                self.active_drop_zone = None;
                let vp = ws.viewport;
                let step = ws.scrolling.tick_edge_auto_scroll(cursor.x, vp, now_ms);
                state.active_drag_edge_scroll = Some((self.ws_id.clone(), cursor.x));
                state.edge_scroll_keep_alive = matches!(
                    step,
                    EdgeScrollStep::Debouncing | EdgeScrollStep::Scrolled(_)
                );
                let sdz = ws.scrolling.predict_drop_zone_2d(cursor, vp, now_ms);
                self.active_scrolling_drop_zone = Some(sdz);
                state.active_drop_preview = Some((self.ws_id.clone(), sdz.preview_rect));
            }
            LayoutMode::Deck => {
                self.active_drop_zone = None;
                self.active_scrolling_drop_zone = None;
                state.active_drag_edge_scroll = None;
                state.edge_scroll_keep_alive = false;
                state.active_drop_preview = Some((self.ws_id.clone(), ws.viewport.full_rect()));
            }
        }
    }

    fn on_toggle_floating_mid_drag(&mut self, state: &mut KomaState) -> bool {
        // Smithay runs grab callbacks with the pointer mutex held.
        let _held = crate::seat_guard::pointer_callback();
        if !state.mapped_windows.iter().any(|m| m.win_id == self.win_id) {
            return false;
        }
        if !self.drag_recognized {
            self.drag_recognized = true;
            if !self.detach_tiled_into_floating(state) {
                return false;
            }
        }
        self.will_tile_on_drop = !self.will_tile_on_drop;
        debug!(
            win_id = %self.win_id,
            will_tile_on_drop = self.will_tile_on_drop,
            "toggled interactive move drop mode via opposite mouse button"
        );
        self.refresh_drop_preview(state);
        state.sync_workspace_geometries_with_instant_window(&self.ws_id, Some(self.win_id));
        state.queue_redraw();
        true
    }

    fn on_frame(&mut self, state: &mut KomaState) -> bool {
        // Smithay runs grab callbacks with the pointer mutex held.
        let _held = crate::seat_guard::pointer_callback();
        if !state.mapped_windows.iter().any(|m| m.win_id == self.win_id)
            || !state.workspaces.contains_key(&self.ws_id)
        {
            state.active_drop_preview = None;
            return false;
        }

        if !self.drag_recognized {
            let delta = self.current_location - self.start_data.location;
            let dist_sq = delta.x * delta.x + delta.y * delta.y;
            if dist_sq < DRAG_RECOGNITION_THRESHOLD_SQ {
                return true;
            }
            self.drag_recognized = true;
            if !self.detach_tiled_into_floating(state) {
                return false;
            }
        }

        let local_cursor = self.current_location - state.workspace_output_origin(&self.ws_id);
        let target_x = (local_cursor.x - self.grab_hotspot.x).round() as i32;
        let target_y = (local_cursor.y - self.grab_hotspot.y).round() as i32;

        if let Some(ws) = state.workspaces.get_mut(&self.ws_id) {
            let vp = ws.viewport;
            ws.floating.raise_to_top(self.win_id);
            ws.floating
                .move_to_clamped(self.win_id, target_x, target_y, vp);
        }

        self.refresh_drop_preview(state);
        state.sync_workspace_geometries_with_instant_window(&self.ws_id, Some(self.win_id));
        state.queue_redraw();
        true
    }

    fn on_ungrab(&mut self, state: &mut KomaState) {
        // `PointerHandle::{set_grab,unset_grab}` call this with the pointer mutex held, so the
        // `focus_active_deck_window` below must not re-enter `PointerHandle::current_focus`.
        let _held = crate::seat_guard::pointer_callback();
        state.active_drop_preview = None;
        state.active_drag_edge_scroll = None;
        state.edge_scroll_keep_alive = false;
        if let Some(ws) = state.workspaces.get_mut(&self.ws_id) {
            ws.scrolling.reset_edge_auto_scroll();
        }
        state.set_cursor_status(CursorImageStatus::default_named());

        if !self.drag_recognized || !state.mapped_windows.iter().any(|m| m.win_id == self.win_id) {
            return;
        }

        if self.will_tile_on_drop {
            let local_cursor = self.current_location - state.workspace_output_origin(&self.ws_id);
            let cursor = LogicalPoint {
                x: local_cursor.x,
                y: local_cursor.y,
            };
            if let Some(ws) = state.workspaces.get_mut(&self.ws_id) {
                let vp = ws.viewport;
                let _ = ws.floating.remove(self.win_id);
                match ws.layout_mode {
                    LayoutMode::Dwindle => {
                        let dz = self
                            .active_drop_zone
                            .unwrap_or_else(|| ws.dwindle.predict_drop_zone(cursor, vp));
                        let _ = ws.dwindle.insert_at_drop_zone(self.win_id, dz, vp);
                        if let Some(ps) = self.saved_pseudotile_size {
                            let _ = ws.dwindle.set_pseudotile(self.win_id, Some(ps), vp);
                        }
                        ws.deck.push_deck_top(self.win_id);
                    }
                    LayoutMode::Scrolling => {
                        let now_ms = state.start_time.elapsed().as_millis() as u64;
                        let sdz = ws.scrolling.predict_drop_zone_2d(cursor, vp, now_ms);
                        ws.scrolling.insert_column_at_drop_zone(
                            self.win_id,
                            sdz,
                            self.saved_scrolling_width,
                            vp,
                            now_ms,
                        );
                        ws.deck.push_deck_top(self.win_id);
                    }
                    LayoutMode::Deck => {
                        ws.deck.push_deck_top(self.win_id);
                    }
                }
                ws.focused_window = Some(self.win_id);
            }
            // Normal spring enabled on drop so the window settles smoothly into its 4-quadrant tile!
            state.sync_workspace_geometries(&self.ws_id);
        } else {
            // Kept permanently floating: update remembered preferred_size
            if let Some(float_rect) = state
                .workspaces
                .get(&self.ws_id)
                .and_then(|ws| ws.floating.rect_of(self.win_id))
                && let Some(m) = state
                    .mapped_windows
                    .iter_mut()
                    .find(|m| m.win_id == self.win_id)
            {
                m.preferred_size = (float_rect.w, float_rect.h);
            }
            state.sync_workspace_geometries_with_instant_window(&self.ws_id, Some(self.win_id));
        }

        state.focus_active_deck_window();
        state.queue_redraw();
    }
}

impl PointerGrab<KomaState> for MoveWindowGrab {
    fn motion(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        _focus: Option<(
            <KomaState as SeatHandler>::PointerFocus,
            Point<f64, Logical>,
        )>,
        event: &MotionEvent,
    ) {
        // While the move grab is active, no Wayland client receives pointer focus (`Focus::Clear`).
        handle.motion(data, None, event);
        self.current_location = event.location;
    }

    fn relative_motion(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        _focus: Option<(
            <KomaState as SeatHandler>::PointerFocus,
            Point<f64, Logical>,
        )>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if !handle.current_pressed().contains(&self.start_data.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }

        // Pressing the opposite mouse button mid-drag toggles permanent floating vs 4-quadrant tiling
        // (`niri/src/input/move_grab.rs` lines 347-359).
        let toggle_floating_btn = if self.start_data.button == BTN_LEFT {
            BTN_RIGHT
        } else {
            BTN_LEFT
        };
        if event.state == ButtonState::Pressed
            && event.button == toggle_floating_btn
            && !self.on_toggle_floating_mid_drag(data)
        {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut KomaState, handle: &mut PointerInnerHandle<'_, KomaState>) {
        handle.frame(data);
        if !self.on_frame(data) {
            handle.unset_grab(
                self,
                data,
                SERIAL_COUNTER.next_serial(),
                InputTime::now(),
                true,
            );
        }
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<KomaState> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut KomaState) {
        self.on_ungrab(data);
    }
}

/// Interactive window & `Dwindle` split-ratio resize grab (`Mod + MouseRight` & `xdg_toplevel.resize`).
pub struct ResizeWindowGrab {
    start_data: PointerGrabStartData<KomaState>,
    win_id: WindowId,
    ws_id: WorkspaceId,
    is_floating: bool,
    corner: ResizeCorner,
    initial_rect: LogicalRect,
    initial_pseudotile_size: Option<(u32, u32)>,
    min_size: (u32, u32),
    max_size: (u32, u32),
    last_applied_location: Point<f64, Logical>,
    current_location: Point<f64, Logical>,
}

impl ResizeWindowGrab {
    #[must_use]
    pub fn new(
        state: &KomaState,
        start_data: PointerGrabStartData<KomaState>,
        win_id: WindowId,
        ws_id: WorkspaceId,
        explicit_corner: Option<ResizeCorner>,
    ) -> Option<(Self, ResizeCorner)> {
        let ws = state.workspaces.get(&ws_id)?;
        if ws.presented_window_id() == Some(win_id) {
            return None;
        }
        let mapped = state.mapped_windows.iter().find(|m| m.win_id == win_id)?;
        let initial_rect = mapped.rect;
        let is_floating = ws.floating.contains(win_id);
        let initial_pseudotile_size = ws.dwindle.pseudotile_size_of(win_id);

        let (min_size, max_size) = mapped
            .window
            .toplevel()
            .map(|t| read_toplevel_min_max_size(t.wl_surface()))
            .unwrap_or(((96, 72), (0, 0)));

        let local_start = start_data.location - state.workspace_output_origin(&ws_id);
        let corner = explicit_corner.unwrap_or_else(|| {
            ResizeCorner::nearest_quadrant(
                initial_rect,
                LogicalPoint {
                    x: local_start.x,
                    y: local_start.y,
                },
            )
        });

        let loc = start_data.location;
        Some((
            Self {
                start_data,
                win_id,
                ws_id,
                is_floating,
                corner,
                initial_rect,
                initial_pseudotile_size,
                min_size,
                max_size,
                last_applied_location: loc,
                current_location: loc,
            },
            corner,
        ))
    }

    fn on_frame(&mut self, state: &mut KomaState) -> bool {
        // Smithay runs grab callbacks with the pointer mutex held.
        let _held = crate::seat_guard::pointer_callback();
        if !state.mapped_windows.iter().any(|m| m.win_id == self.win_id)
            || !state.workspaces.contains_key(&self.ws_id)
        {
            return false;
        }

        let inc = self.current_location - self.last_applied_location;
        if inc.x.abs() < 0.01 && inc.y.abs() < 0.01 {
            return true;
        }
        self.last_applied_location = self.current_location;
        let total = self.current_location - self.start_data.location;

        if self.is_floating {
            let mut updated_rect = None;
            if let Some(ws) = state.workspaces.get_mut(&self.ws_id) {
                let vp = ws.viewport;
                let next_rect = FloatingStack::compute_corner_resize_rect(
                    self.initial_rect,
                    self.corner,
                    total.x,
                    total.y,
                    self.min_size,
                    self.max_size,
                    vp,
                );
                ws.floating.set_rect(self.win_id, next_rect);
                updated_rect = Some(next_rect);
            }
            if let Some(r) = updated_rect
                && let Some(m) = state
                    .mapped_windows
                    .iter_mut()
                    .find(|m| m.win_id == self.win_id)
            {
                m.preferred_size = (r.w, r.h);
            }
            state.sync_workspace_geometries_with_instant_window(&self.ws_id, Some(self.win_id));
        } else if let Some((init_pw, init_ph)) = self.initial_pseudotile_size {
            // Centered `pseudotile` resize (`DwindleAlgorithm.cpp` lines 290-294)
            let signed_dx = if self.corner.has_left() && !self.corner.has_right() {
                -total.x
            } else {
                total.x
            };
            let signed_dy = if self.corner.has_top() && !self.corner.has_bottom() {
                -total.y
            } else {
                total.y
            };
            let next_pw = (init_pw as i32 + signed_dx.round() as i32).max(96) as u32;
            let next_ph = (init_ph as i32 + signed_dy.round() as i32).max(72) as u32;
            if let Some(ws) = state.workspaces.get_mut(&self.ws_id) {
                let vp = ws.viewport;
                let _ = ws
                    .dwindle
                    .set_pseudotile(self.win_id, Some((next_pw, next_ph)), vp);
            }
            state.sync_workspace_geometries_all_instant(&self.ws_id);
        } else {
            // Tiled `Dwindle` / `Scrolling` split-ratio resize (`DwindleAlgorithm.cpp` lines 295-378)
            if let Some(ws) = state.workspaces.get_mut(&self.ws_id) {
                let vp = ws.viewport;
                match ws.layout_mode {
                    LayoutMode::Dwindle => {
                        let _ = ws.dwindle.resize_target_from_corner(
                            self.win_id,
                            inc.x,
                            inc.y,
                            self.corner,
                            vp,
                        );
                    }
                    LayoutMode::Scrolling => {
                        let now_ms = state.start_time.elapsed().as_millis() as u64;
                        let _ = ws.scrolling.adjust_tile_or_column_size(
                            self.win_id,
                            inc.x,
                            inc.y,
                            self.corner,
                            vp,
                            now_ms,
                        );
                    }
                    LayoutMode::Deck => {
                        // Deck keeps the focused window full-viewport, so a split-ratio resize has
                        // no target. Log instead of silently doing nothing (komawm/koma#42).
                        debug!(
                            win_id = ?self.win_id,
                            "Deck has no split ratio to resize; the frame keeps the viewport geometry"
                        );
                    }
                }
            }
            // Snap sibling dividers 1:1 with the pointer during manual drag (`PANIMATE == 0` default)
            state.sync_workspace_geometries_all_instant(&self.ws_id);
        }

        state.queue_redraw();
        true
    }

    fn on_ungrab(&mut self, state: &mut KomaState) {
        // Smithay runs grab callbacks with the pointer mutex held.
        let _held = crate::seat_guard::pointer_callback();
        state.set_cursor_status(CursorImageStatus::default_named());
    }
}

impl PointerGrab<KomaState> for ResizeWindowGrab {
    fn motion(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        _focus: Option<(
            <KomaState as SeatHandler>::PointerFocus,
            Point<f64, Logical>,
        )>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        self.current_location = event.location;
    }

    fn relative_motion(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        _focus: Option<(
            <KomaState as SeatHandler>::PointerFocus,
            Point<f64, Logical>,
        )>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        if !handle.current_pressed().contains(&self.start_data.button)
            || handle.current_pressed().is_empty()
        {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut KomaState, handle: &mut PointerInnerHandle<'_, KomaState>) {
        handle.frame(data);
        if !self.on_frame(data) {
            handle.unset_grab(
                self,
                data,
                SERIAL_COUNTER.next_serial(),
                InputTime::now(),
                true,
            );
        }
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut KomaState,
        handle: &mut PointerInnerHandle<'_, KomaState>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<KomaState> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut KomaState) {
        self.on_ungrab(data);
    }
}
