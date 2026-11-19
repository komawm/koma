//! Smithay-backed desktop protocols:
//! - `zxdg_decoration_manager_v1` (SSD-default negotiation)
//! - `xdg_toplevel_icon_v1` (validated icon metadata/SHM buffers)
//! - `xdg_activation_v1` (TTL/seat/jail-validated focus & lock-screen deferral)
//! - `ext_foreign_toplevel_list_v1` (read-only handle state)
//! - `ext_idle_notify_v1` & `zwp_idle_inhibit_manager_v1` (real input activity + active inhibitor check)
//! - `zwp_pointer_constraints_v1` & `zwp_relative_pointer_manager_v1` (lock/confine/hint/break)

use std::time::{Duration, Instant};

use smithay::{
    desktop::layer_map_for_output,
    input::pointer::{PointerHandle, RelativeMotionEvent},
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
            shell::server::xdg_toplevel::XdgToplevel,
        },
        wayland_server::{Resource, protocol::wl_surface::WlSurface},
    },
    utils::{IsAlive as _, Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::{RectangleKind, RegionAttributes, get_parent, with_states},
        foreign_toplevel_list::{ForeignToplevelListHandler, ForeignToplevelListState},
        idle_inhibit::IdleInhibitHandler,
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
        pointer_constraints::{
            ConstraintRemove, PointerConstraint, PointerConstraintsHandler, with_pointer_constraint,
        },
        shell::xdg::{ToplevelSurface, decoration::XdgDecorationHandler},
        shm,
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
        xdg_toplevel_icon::{ToplevelIconCachedState, XdgToplevelIconHandler},
    },
};
use tracing::debug;

use crate::state::{ClientState, KomaState, PointerHitTarget};
use koma_lua::FollowMouseMode;
use koma_model::WorkspaceState;
use koma_proto::WorkspaceId;
use koma_render::frame_composer::surface_origin;

pub const XDG_ACTIVATION_TOKEN_TTL: Duration = Duration::from_secs(10);
pub const MAX_DEFERRED_ACTIVATIONS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedIconBuffer {
    pub width: i32,
    pub height: i32,
    pub scale: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoredToplevelIcon {
    pub name: Option<String>,
    pub buffers: Vec<ValidatedIconBuffer>,
}

#[derive(Debug, Clone)]
pub struct DeferredXdgActivation {
    pub surface: WlSurface,
    pub created_at: Instant,
}

/// Creator sandbox/jail policy snapshotted when an `xdg_activation_token_v1` is created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationTokenPolicy {
    pub restricted: bool,
    pub jailed_workspace: Option<WorkspaceId>,
    pub created_at: Instant,
}

impl XdgDecorationHandler for KomaState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        let already_configured = self.is_toplevel_configured(toplevel.wl_surface());
        let default_mode = DecorationMode::ServerSide;
        toplevel.with_pending_state(|s| {
            s.decoration_mode = Some(default_mode);
        });
        self.record_toplevel_decoration_mode(toplevel.wl_surface(), default_mode);
        if already_configured {
            toplevel.send_configure();
        }
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        let chosen = match mode {
            DecorationMode::ClientSide => DecorationMode::ClientSide,
            _ => DecorationMode::ServerSide,
        };
        toplevel.with_pending_state(|s| {
            s.decoration_mode = Some(chosen);
        });
        self.record_toplevel_decoration_mode(toplevel.wl_surface(), chosen);
        if self.is_toplevel_configured(toplevel.wl_surface()) {
            toplevel.send_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| {
            s.decoration_mode = Some(DecorationMode::ServerSide);
        });
        self.record_toplevel_decoration_mode(toplevel.wl_surface(), DecorationMode::ServerSide);
        if self.is_toplevel_configured(toplevel.wl_surface()) {
            toplevel.send_configure();
        }
    }
}

impl XdgToplevelIconHandler for KomaState {
    fn set_icon(&mut self, _toplevel: XdgToplevel, wl_surface: WlSurface) {
        self.refresh_toplevel_icon(&wl_surface);
    }
}

/// Validates cached `ToplevelIconCachedState` cleanly without panicking on empty or malformed SHM buffers.
#[must_use]
pub fn extract_validated_toplevel_icon(wl_surface: &WlSurface) -> Option<StoredToplevelIcon> {
    with_states(wl_surface, |states| {
        let mut cached = states.cached_state.get::<ToplevelIconCachedState>();
        let current = cached.current();
        let name = current
            .icon_name()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let mut buffers = Vec::new();
        for (buf, scale) in current.buffers() {
            if *scale <= 0 || !buf.is_alive() {
                continue;
            }
            if let Ok(Some(valid)) = shm::with_buffer_contents(buf, |_, pool_len, data| {
                if data.width <= 0
                    || data.height <= 0
                    || data.width != data.height
                    || data.stride < data.width.saturating_mul(4)
                    || data.offset < 0
                {
                    return None;
                }
                let needed = (data.offset as usize)
                    .checked_add((data.height as usize).checked_mul(data.stride as usize)?)?;
                if needed > pool_len {
                    return None;
                }
                Some(ValidatedIconBuffer {
                    width: data.width,
                    height: data.height,
                    scale: *scale,
                })
            }) {
                buffers.push(valid);
            }
        }
        if name.is_none() && buffers.is_empty() {
            None
        } else {
            Some(StoredToplevelIcon { name, buffers })
        }
    })
}

impl XdgActivationHandler for KomaState {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        self.xdg_activation_state
            .retain_tokens(|_, info| info.timestamp.elapsed() < XDG_ACTIVATION_TOKEN_TTL);
        self.xdg_activation_tokens
            .retain(|_, policy| policy.created_at.elapsed() < XDG_ACTIVATION_TOKEN_TTL);

        // If a serial + seat was supplied, ensure it matches the compositor's seat.
        if let Some((_, ref wl_seat)) = data.serial
            && smithay::input::Seat::<Self>::from_resource(wl_seat).is_none_or(|s| s != self.seat)
        {
            return false;
        }
        if let Some(ref src_surface) = data.surface
            && !src_surface.alive()
        {
            return false;
        }

        let Some(ref client_id) = data.client_id else {
            return false;
        };
        let Ok(client_data) = self
            .display_handle
            .backend_handle()
            .get_client_data(client_id.clone())
        else {
            return false;
        };
        let Some(requester_state) = client_data.downcast_ref::<ClientState>() else {
            return false;
        };

        self.xdg_activation_tokens.insert(
            token,
            ActivationTokenPolicy {
                restricted: requester_state.restricted,
                jailed_workspace: requester_state.jailed_workspace.clone(),
                created_at: data.timestamp,
            },
        );
        true
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let expired = token_data.timestamp.elapsed() >= XDG_ACTIVATION_TOKEN_TTL;
        let _ = self.xdg_activation_state.remove_token(&token);
        let policy = self.xdg_activation_tokens.remove(&token);
        self.xdg_activation_state
            .retain_tokens(|_, info| info.timestamp.elapsed() < XDG_ACTIVATION_TOKEN_TTL);
        self.xdg_activation_tokens
            .retain(|_, p| p.created_at.elapsed() < XDG_ACTIVATION_TOKEN_TTL);
        if expired || !surface.alive() {
            return;
        }

        // Fail closed when creator policy could not be snapshotted at token creation.
        let Some(requester_policy) = policy else {
            debug!("blocked xdg_activation request missing snapshotted creator policy");
            return;
        };

        // Prevent cross-sandbox / cross-jail activation across isolated clients even if the
        // requester disconnected before submitting `activate`.
        let target_client = surface.client();
        let target_state = target_client
            .as_ref()
            .and_then(|c| c.get_data::<ClientState>());
        if (requester_policy.restricted || requester_policy.jailed_workspace.is_some())
            && target_state.is_none()
        {
            debug!("blocked xdg_activation request from restricted creator with unresolved target");
            return;
        }
        let target_jail = target_state.and_then(|d| d.jailed_workspace.as_ref());
        let target_restricted = target_state.is_some_and(|d| d.restricted);
        if (requester_policy.jailed_workspace.is_some()
            && requester_policy.jailed_workspace.as_ref() != target_jail)
            || (requester_policy.restricted && !target_restricted)
        {
            debug!("blocked cross-jail/cross-sandbox xdg_activation request");
            return;
        }

        self.reconcile_session_lock();
        if self.is_locked_or_abandoned() {
            if !self
                .deferred_xdg_activations
                .iter()
                .any(|d| d.surface == surface)
            {
                if self.deferred_xdg_activations.len() >= MAX_DEFERRED_ACTIVATIONS {
                    self.deferred_xdg_activations.remove(0);
                }
                self.deferred_xdg_activations.push(DeferredXdgActivation {
                    surface,
                    created_at: Instant::now(),
                });
            }
            debug!("deferred xdg_activation request while session locked");
            return;
        }

        self.activate_window_by_surface(&surface);
    }
}

impl ForeignToplevelListHandler for KomaState {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

impl IdleNotifierHandler for KomaState {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

impl IdleInhibitHandler for KomaState {
    fn inhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.insert(surface);
        self.reconcile_idle_inhibitors();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.remove(&surface);
        self.reconcile_idle_inhibitors();
    }
}

impl PointerConstraintsHandler for KomaState {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        self.reconcile_session_lock();
        if self.is_locked_or_abandoned() || !surface.alive() {
            return;
        }
        try_activate_constraint_on_focused(self, surface, pointer);
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let Some((focused, origin)) = pointer.current_focus().and_then(|f| {
            if &f == surface {
                self.surface_global_origin(surface).map(|orig| (f, orig))
            } else {
                None
            }
        }) else {
            return;
        };
        let is_locked = with_pointer_constraint(
            &focused,
            pointer,
            |c| matches!(c.as_deref(), Some(p @ PointerConstraint::Locked(_)) if p.is_active()),
        );
        if is_locked {
            let global = self.clamp_pointer_to_outputs(origin + location);
            pointer.set_location(global);
            self.pointer_location = global;
            self.queue_redraw();
        }
    }

    fn remove_constraint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        constraint_remove: ConstraintRemove,
    ) {
        if let ConstraintRemove::Destroyed(PointerConstraint::Locked(locked)) = &constraint_remove
            && let Some(local_hint) = locked.cursor_position_hint()
            && let Some(origin) = self.surface_global_origin(surface)
        {
            let global = self.clamp_pointer_to_outputs(origin + local_hint);
            pointer.set_location(global);
            self.pointer_location = global;
            self.queue_redraw();
        }
        debug!(?constraint_remove, "removed pointer constraint");
    }
}

fn apply_locked_cursor_hint(
    state: &mut KomaState,
    surface: &WlSurface,
    pointer: &PointerHandle<KomaState>,
) {
    let hint = with_pointer_constraint(surface, pointer, |c| match c.as_deref() {
        Some(PointerConstraint::Locked(locked)) => locked.cursor_position_hint(),
        _ => None,
    });
    if let (Some(local_hint), Some(origin)) = (hint, state.surface_global_origin(surface)) {
        let global = state.clamp_pointer_to_outputs(origin + local_hint);
        pointer.set_location(global);
        state.pointer_location = global;
        state.queue_redraw();
    }
}

fn try_activate_constraint_on_focused(
    state: &KomaState,
    surface: &WlSurface,
    pointer: &PointerHandle<KomaState>,
) {
    try_activate_constraint_on_focused_at(state, surface, pointer, state.pointer_location);
}

fn try_activate_constraint_on_focused_at(
    state: &KomaState,
    surface: &WlSurface,
    pointer: &PointerHandle<KomaState>,
    global_pos: Point<f64, Logical>,
) {
    if state.is_locked_or_abandoned() || pointer.is_grabbed() {
        return;
    }
    let Some(focused) = pointer.current_focus() else {
        return;
    };
    if &focused != surface {
        return;
    }
    let Some(origin) = state.surface_global_origin(surface) else {
        return;
    };
    let local_pos = global_pos - origin;
    let local_i32: Point<i32, Logical> =
        (local_pos.x.floor() as i32, local_pos.y.floor() as i32).into();
    if let Some(bounds) = surface_logical_bounds(surface)
        && !bounds.contains(local_i32)
    {
        return;
    }
    with_pointer_constraint(surface, pointer, |constraint| {
        if let Some(constraint) = constraint
            && !constraint.is_active()
            && constraint
                .region()
                .is_none_or(|reg| reg.contains(local_i32))
        {
            constraint.activate();
        }
    });
}

/// Evaluates whether a surface-local coordinate intersects the surface bounds and optional constraint region.
#[cfg_attr(not(test), allow(dead_code))]
#[must_use]
pub fn point_inside_surface_constraint(
    surface: &WlSurface,
    local_pos: Point<f64, Logical>,
    region: Option<&RegionAttributes>,
) -> bool {
    let local_i32: Point<i32, Logical> =
        (local_pos.x.floor() as i32, local_pos.y.floor() as i32).into();
    if let Some(bounds) = surface_logical_bounds(surface)
        && !bounds.contains(local_i32)
    {
        return false;
    }
    region.is_none_or(|reg| reg.contains(local_i32))
}

fn consider_constraint_candidate(
    bounds: Rectangle<i32, Logical>,
    reg: &RegionAttributes,
    origin: Point<f64, Logical>,
    candidate: Point<f64, Logical>,
    best: &mut Option<(f64, Point<f64, Logical>)>,
) {
    let pt_i32: Point<i32, Logical> =
        (candidate.x.floor() as i32, candidate.y.floor() as i32).into();
    if !bounds.contains(pt_i32) || !reg.contains(pt_i32) {
        return;
    }
    let dx = candidate.x - origin.x;
    let dy = candidate.y - origin.y;
    let dist2 = dx * dx + dy * dy;
    if best.is_none_or(|(best_d, _)| dist2 < best_d) {
        *best = Some((dist2, candidate));
    }
}

/// Clamps `candidate_local` cleanly to the intersection of the surface bounds and optional region.
#[must_use]
pub fn clamp_local_point_to_constraint(
    bounds: Rectangle<i32, Logical>,
    candidate_local: Point<f64, Logical>,
    region: Option<&RegionAttributes>,
) -> Point<f64, Logical> {
    let mut target_rect = bounds;
    if let Some(reg) = region {
        let local_i32: Point<i32, Logical> = (
            candidate_local.x.floor() as i32,
            candidate_local.y.floor() as i32,
        )
            .into();
        if bounds.contains(local_i32) && reg.contains(local_i32) {
            return candidate_local;
        }

        let mut best: Option<(f64, Point<f64, Logical>)> = None;
        let mut bbox: Option<Rectangle<i32, Logical>> = None;
        for (kind, rect) in &reg.rects {
            if !matches!(kind, RectangleKind::Add) {
                continue;
            }
            bbox = Some(match bbox {
                Some(prev) => prev.merge(*rect),
                None => *rect,
            });
            if let Some(inter) = bounds.intersection(*rect) {
                let pt = clamp_point_to_rect(inter, candidate_local);
                consider_constraint_candidate(bounds, reg, candidate_local, pt, &mut best);
                // If `pt` fell inside a `RectangleKind::Subtract` hole, project onto its 4 outer edges.
                if best.is_none() || pt != best.expect("checked").1 {
                    for (sub_kind, sub) in &reg.rects {
                        if !matches!(sub_kind, RectangleKind::Subtract) {
                            continue;
                        }
                        let candidates = [
                            clamp_point_to_rect(inter, (f64::from(sub.loc.x - 1), pt.y).into()),
                            clamp_point_to_rect(
                                inter,
                                (f64::from(sub.loc.x + sub.size.w), pt.y).into(),
                            ),
                            clamp_point_to_rect(inter, (pt.x, f64::from(sub.loc.y - 1)).into()),
                            clamp_point_to_rect(
                                inter,
                                (pt.x, f64::from(sub.loc.y + sub.size.h)).into(),
                            ),
                        ];
                        for cand in candidates {
                            consider_constraint_candidate(
                                bounds,
                                reg,
                                candidate_local,
                                cand,
                                &mut best,
                            );
                        }
                    }
                }
            }
        }
        if let Some((_, best_pt)) = best {
            return best_pt;
        }
        if let Some(region_box) = bbox
            && let Some(inter) = target_rect.intersection(region_box)
        {
            target_rect = inter;
        }
    }

    clamp_point_to_rect(target_rect, candidate_local)
}

fn clamp_point_to_rect(
    rect: Rectangle<i32, Logical>,
    pt: Point<f64, Logical>,
) -> Point<f64, Logical> {
    let min_x = f64::from(rect.loc.x);
    let min_y = f64::from(rect.loc.y);
    let max_x = f64::from((rect.loc.x + rect.size.w - 1).max(rect.loc.x));
    let max_y = f64::from((rect.loc.y + rect.size.h - 1).max(rect.loc.y));
    Point::from((pt.x.clamp(min_x, max_x), pt.y.clamp(min_y, max_y)))
}

pub(crate) fn surface_logical_bounds(surface: &WlSurface) -> Option<Rectangle<i32, Logical>> {
    let bbox = smithay::desktop::utils::bbox_from_surface_tree(surface, (0, 0));
    if bbox.size.w > 0 && bbox.size.h > 0 {
        Some(bbox)
    } else {
        None
    }
}

impl KomaState {
    #[must_use]
    pub fn is_toplevel_configured(&self, wl_surface: &WlSurface) -> bool {
        if self
            .unmapped_windows
            .get(wl_surface)
            .is_some_and(|u| u.initial_configure_sent)
        {
            return true;
        }
        self.mapped_windows.iter().any(|m| {
            m.window
                .toplevel()
                .is_some_and(|t| t.wl_surface() == wl_surface)
        })
    }

    pub fn record_toplevel_decoration_mode(
        &mut self,
        wl_surface: &WlSurface,
        mode: DecorationMode,
    ) {
        if let Some(unmapped) = self.unmapped_windows.get_mut(wl_surface) {
            unmapped.decoration_mode = mode;
        }
        if let Some(mapped) = self.mapped_windows.iter_mut().find(|m| {
            m.window
                .toplevel()
                .is_some_and(|t| t.wl_surface() == wl_surface)
        }) {
            mapped.decoration_mode = mode;
        }
    }

    pub fn refresh_toplevel_icon(&mut self, wl_surface: &WlSurface) {
        let icon = extract_validated_toplevel_icon(wl_surface);
        if let Some(unmapped) = self.unmapped_windows.get_mut(wl_surface) {
            unmapped.icon = icon.clone();
        }
        if let Some(mapped) = self.mapped_windows.iter_mut().find(|m| {
            m.window
                .toplevel()
                .is_some_and(|t| t.wl_surface() == wl_surface)
        }) {
            mapped.icon = icon;
        }
    }

    /// Notifies `ext_idle_notify_v1` of genuine user input activity on the compositor seat.
    pub fn notify_user_activity(&mut self) {
        self.idle_notifier_state.notify_activity(&self.seat);
    }

    /// Recomputes whether any currently alive & visible/mapped surface inhibits idle timeouts.
    pub fn reconcile_idle_inhibitors(&mut self) {
        self.idle_inhibitors.retain(Resource::is_alive);

        let inhibited = if self.is_locked_or_abandoned() {
            self.idle_inhibitors
                .iter()
                .any(|surf| self.is_active_lock_surface_tree(surf))
        } else {
            self.idle_inhibitors
                .iter()
                .any(|surf| self.is_surface_mapped_and_visible(surf))
        };

        if self.idle_notifier_state.is_inhibited() != inhibited {
            self.idle_notifier_state.set_is_inhibited(inhibited);
        }
    }

    fn is_active_lock_surface_tree(&self, surface: &WlSurface) -> bool {
        let root = root_surface_of(surface);
        self.session_lock
            .surfaces
            .values()
            .any(|ls| *ls.wl_surface() == root)
    }

    fn is_surface_mapped_and_visible(&self, surface: &WlSurface) -> bool {
        let root = root_surface_of(surface);
        let active_ws = &self.active_ws_id;
        let special_ws = self.active_special_ws_id.as_ref();

        if self.mapped_windows.iter().any(|m| {
            !m.is_minimized
                && (&m.ws_id == active_ws || special_ws == Some(&m.ws_id))
                && m.window.toplevel().is_some_and(|t| *t.wl_surface() == root)
        }) {
            return true;
        }

        self.all_outputs().iter().any(|out| {
            let map = layer_map_for_output(out);
            map.layers().any(|layer| *layer.wl_surface() == root)
        })
    }

    /// Processes any valid, non-expired `xdg_activation_v1` requests queued while locked.
    pub fn flush_deferred_xdg_activations(&mut self) {
        if self.is_locked_or_abandoned() || self.deferred_xdg_activations.is_empty() {
            return;
        }
        let queued = std::mem::take(&mut self.deferred_xdg_activations);
        for item in queued {
            if item.created_at.elapsed() < XDG_ACTIVATION_TOKEN_TTL && item.surface.alive() {
                self.activate_window_by_surface(&item.surface);
            }
        }
    }

    #[must_use]
    pub fn has_active_locked_pointer_constraint(&self) -> bool {
        let Some(pointer) = self.ptr() else {
            return false;
        };
        let Some(focused) = pointer.current_focus() else {
            return false;
        };
        with_pointer_constraint(
            &focused,
            &pointer,
            |c| matches!(c.as_deref(), Some(p @ PointerConstraint::Locked(_)) if p.is_active()),
        )
    }

    /// Activates any dormant pointer constraint whose surface holds focus and contains the cursor.
    pub fn maybe_activate_pointer_constraint(&self) {
        self.maybe_activate_pointer_constraint_at(self.pointer_location);
    }

    /// Activates any dormant pointer constraint whose surface holds focus and contains `global_pos`.
    pub fn maybe_activate_pointer_constraint_at(&self, global_pos: Point<f64, Logical>) {
        let Some(pointer) = self.ptr() else {
            return;
        };
        if let Some(focused) = pointer.current_focus() {
            try_activate_constraint_on_focused_at(self, &focused, &pointer, global_pos);
        }
    }

    /// Deactivates active pointer constraints (e.g. on lock-screen, focus change, or compositor grab).
    pub fn break_active_pointer_constraints(&mut self) {
        let Some(pointer) = self.ptr() else {
            return;
        };
        let Some(focused) = pointer.current_focus() else {
            return;
        };
        let deactivated = with_pointer_constraint(&focused, &pointer, |c| {
            if let Some(constraint) = c
                && constraint.is_active()
            {
                constraint.deactivate();
                return true;
            }
            false
        });
        if deactivated {
            apply_locked_cursor_hint(self, &focused, &pointer);
        }
    }

    /// Centralized relative pointer motion supporting locked, confined, and free pointer constraints
    /// across 2D multi-output layouts (including non-contiguous gap crossing).
    #[cfg_attr(not(feature = "smithay-tty"), allow(dead_code))]
    pub fn handle_pointer_relative_motion(
        &mut self,
        delta: Point<f64, Logical>,
        delta_unaccel: Point<f64, Logical>,
        time: smithay::backend::input::InputTime,
    ) {
        let candidate = self.step_pointer_across_outputs(delta);
        self.dispatch_constrained_pointer_motion(candidate, delta, delta_unaccel, time);
    }

    /// Centralized absolute pointer motion (Winit / absolute tablets) honoring constraints & relative deltas.
    pub fn handle_pointer_absolute_motion(
        &mut self,
        target_pos: Point<f64, Logical>,
        time: smithay::backend::input::InputTime,
    ) {
        let delta = target_pos - self.pointer_location;
        self.dispatch_constrained_pointer_motion(target_pos, delta, delta, time);
    }

    fn dispatch_constrained_pointer_motion(
        &mut self,
        candidate_pos: Point<f64, Logical>,
        delta: Point<f64, Logical>,
        delta_unaccel: Point<f64, Logical>,
        time: smithay::backend::input::InputTime,
    ) {
        self.reconcile_session_lock();
        self.notify_user_activity();
        // Held across `follow_pointer_focus` and `maybe_activate_pointer_constraint`, both of which
        // acquire the pointer themselves; Smithay releases its pointer mutex between calls.
        let Some(pointer) = self.pointer_handle() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();

        // The keybind overview owns the pointer too: track the cursor for its own frame but never
        // forward motion or focus changes to the clients behind the scrim.
        if self.keybind_overlay_is_open() && !self.is_locked_or_abandoned() {
            self.pointer_location = self.clamp_pointer_to_outputs(candidate_pos);
            self.with_pointer_dispatch(|state, pointer| pointer.frame(state));
            self.queue_redraw();
            return;
        }

        let rel_event = RelativeMotionEvent {
            delta,
            delta_unaccel,
            time,
        };

        if !self.is_locked_or_abandoned()
            && !pointer.is_grabbed()
            && let Some(focused) = pointer.current_focus()
        {
            enum MotionConstraintMode {
                Locked,
                Confined(Option<RegionAttributes>),
                None,
            }
            let mode = with_pointer_constraint(&focused, &pointer, |c| match c.as_deref() {
                Some(p @ PointerConstraint::Locked(_)) if p.is_active() => {
                    MotionConstraintMode::Locked
                }
                Some(p @ PointerConstraint::Confined(conf)) if p.is_active() => {
                    MotionConstraintMode::Confined(conf.region().cloned())
                }
                _ => MotionConstraintMode::None,
            });

            match mode {
                MotionConstraintMode::Locked => {
                    let origin = self.surface_global_origin(&focused).unwrap_or_default();
                    self.with_pointer_dispatch(|state, pointer| {
                        pointer.relative_motion(state, Some((focused, origin)), &rel_event);
                        pointer.frame(state);
                    });
                    return;
                }
                MotionConstraintMode::Confined(region) => {
                    if let Some(origin) = self.surface_global_origin(&focused) {
                        let bounds = surface_logical_bounds(&focused)
                            .unwrap_or_else(|| Rectangle::from_size((1, 1).into()));
                        let clamped_local = clamp_local_point_to_constraint(
                            bounds,
                            candidate_pos - origin,
                            region.as_ref(),
                        );
                        let clamped_global = self.clamp_pointer_to_outputs(origin + clamped_local);
                        self.pointer_location = clamped_global;
                        self.with_pointer_dispatch(|state, pointer| {
                            pointer.motion(
                                state,
                                Some((focused.clone(), origin)),
                                &smithay::input::pointer::MotionEvent {
                                    location: clamped_global,
                                    serial,
                                    time,
                                },
                            );
                            pointer.relative_motion(state, Some((focused, origin)), &rel_event);
                            pointer.frame(state);
                        });
                        self.queue_redraw();
                        return;
                    }
                }
                MotionConstraintMode::None => {}
            }
        } else if self.is_locked_or_abandoned() {
            self.break_active_pointer_constraints();
        }

        let clamped = self.clamp_pointer_to_outputs(candidate_pos);
        self.pointer_location = clamped;
        if let Some(new_out) = self.output_at_point_strict(clamped) {
            let new_name = new_out.name();
            if self.focused_output_name.as_deref() != Some(new_name.as_str()) {
                self.focused_output_name = Some(new_name.clone());
                self.active_ws_id = self.active_workspace_for_output(&new_name);
            }
        }
        // Resolve the scene once: pointer dispatch and `follow_mouse` share this hit test, so the
        // motion hot path performs no extra hit-testing or allocation for focus decisions.
        let hit = self.hit_test_target(clamped);
        self.follow_pointer_focus(hit.as_ref(), clamped);
        let mut under = hit.map(|target| (target.surface().clone(), target.origin()));
        // A running client drag must not offer host data into a jailed sandbox workspace; the
        // grab then sees no focus and withdraws its offer.
        if self.dnd_drag.is_some()
            && under.as_ref().is_some_and(|(surface, _)| {
                !crate::dnd::surface_may_exchange_drag_data(self, surface)
            })
        {
            under = None;
        }
        self.with_pointer_dispatch(|state, pointer| {
            pointer.motion(
                state,
                under.clone(),
                &smithay::input::pointer::MotionEvent {
                    location: clamped,
                    serial,
                    time,
                },
            );
            pointer.relative_motion(state, under, &rel_event);
            pointer.frame(state);
        });
        self.refresh_frame_edge_hover(clamped);
        self.maybe_activate_pointer_constraint();
        self.queue_redraw();
    }

    /// Applies `koma.input({ pointer = { follow_mouse = ... } })` to a motion that landed on `hit`.
    ///
    /// Modes follow `Hyprland` `input:follow_mouse`: `off` is `0` (disabled: clicking is the only
    /// way to move focus), `normal` is `1` (`full`: the window under the cursor is focused *and*
    /// raised to the top of its workspace stack), and `focus_only` is `2` (the window under the
    /// cursor gains keyboard focus without being raised, so the workspace keeps its active-window
    /// stacking).
    ///
    /// Never acts over a gap or layer-shell surface, while locked/abandoned, while a
    /// `MoveWindowGrab`/`ResizeWindowGrab` owns the pointer, or when an open `special:<name>`
    /// overlay owns the visible stack and the hit window belongs to another workspace. Identity is
    /// checked before any mutation, so motion inside the already-focused window is a no-op.
    fn follow_pointer_focus(
        &mut self,
        hit: Option<&PointerHitTarget>,
        global_pos: Point<f64, Logical>,
    ) {
        let mode = self.follow_mouse_mode;
        if mode == FollowMouseMode::Off {
            return;
        }
        let Some(PointerHitTarget::Window { win_id, ws_id, .. }) = hit else {
            return;
        };
        if self.is_locked_or_abandoned() {
            return;
        }
        let Some(pointer) = self.ptr() else {
            return;
        };
        if pointer.is_grabbed() {
            return;
        }
        // A visible Scratchpad overlay stays authoritative: hovering the windows it covers is not a
        // focus request (a click intentionally dismisses the overlay through `focus_window_at_point`).
        if let Some(open_special) = &self.active_special_ws_id
            && open_special != ws_id
        {
            return;
        }
        match mode {
            FollowMouseMode::Normal => {
                if self
                    .workspaces
                    .get(ws_id)
                    .and_then(WorkspaceState::active_window)
                    == Some(*win_id)
                {
                    return;
                }
                self.activate_window_hit(*win_id, ws_id, global_pos);
            }
            FollowMouseMode::FocusOnly => {
                if self.focused_window_id() == Some(*win_id) {
                    return;
                }
                self.focus_window_by_pointer(*win_id, ws_id, global_pos);
            }
            FollowMouseMode::Off => {}
        }
    }

    #[must_use]
    pub fn step_pointer_across_outputs(&self, delta: Point<f64, Logical>) -> Point<f64, Logical> {
        if self.output_layout.enabled_entries().next().is_some() {
            let next = self.output_layout.step_pointer(
                koma_model::LogicalPoint {
                    x: self.pointer_location.x,
                    y: self.pointer_location.y,
                },
                koma_model::LogicalPoint {
                    x: delta.x,
                    y: delta.y,
                },
            );
            return Point::from((next.x, next.y));
        }
        self.clamp_pointer_to_outputs(self.pointer_location + delta)
    }

    #[must_use]
    pub fn clamp_pointer_to_outputs(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        if self.output_layout.enabled_entries().next().is_some() {
            let clamped = self
                .output_layout
                .clamp_pointer(koma_model::LogicalPoint { x: pos.x, y: pos.y });
            return Point::from((clamped.x, clamped.y));
        }
        let (max_w, max_h) = self
            .workspaces
            .get(&self.active_ws_id)
            .map(|ws| (f64::from(ws.viewport.width), f64::from(ws.viewport.height)))
            .unwrap_or((1920.0, 1080.0));
        Point::from((
            pos.x.clamp(0.0, (max_w - 1.0).max(0.0)),
            pos.y.clamp(0.0, (max_h - 1.0).max(0.0)),
        ))
    }

    #[must_use]
    pub fn surface_global_origin(&self, surface: &WlSurface) -> Option<Point<f64, Logical>> {
        let root = root_surface_of(surface);
        if let Some(popup) = self.popups.find_popup(&root)
            && let Ok(popup_root) = smithay::desktop::find_popup_root_surface(&popup)
            && let Some(root_surface_origin) = self.surface_global_origin(&popup_root)
        {
            // `popups_for_surface` positions are relative to the root's *window geometry*, while
            // this function returns the root's *surface* origin. Re-anchor by the root geometry
            // inset so popups stay over their parent's visible area for CSD roots too.
            let root_geometry_origin = root_surface_origin
                + self
                    .mapped_windows
                    .iter()
                    .find(|m| {
                        m.window
                            .toplevel()
                            .is_some_and(|t| *t.wl_surface() == popup_root)
                    })
                    .map(|m| m.window.geometry().loc)
                    .unwrap_or_default()
                    .to_f64();
            for (candidate, offset) in
                smithay::desktop::PopupManager::popups_for_surface(&popup_root)
            {
                if *candidate.wl_surface() == root {
                    return Some(
                        root_geometry_origin + (offset - candidate.geometry().loc).to_f64(),
                    );
                }
            }
            return Some(
                root_geometry_origin + smithay::desktop::get_popup_toplevel_coords(&popup).to_f64(),
            );
        }
        if let Some(m) = self.mapped_windows.iter().find(|m| {
            !m.is_minimized && m.window.toplevel().is_some_and(|t| *t.wl_surface() == root)
        }) {
            let ws_origin = self.workspace_output_origin(&m.ws_id);
            let border = self.lua_sandbox.config_snapshot().appearance.border;
            let b_off = if border.enabled {
                f64::from(border.width.max(0.0))
            } else {
                0.0
            };
            let inner_origin =
                Point::from((f64::from(m.rect.x) + b_off, f64::from(m.rect.y) + b_off));
            return Some(ws_origin + surface_origin(inner_origin, m.window.geometry().loc));
        }
        for out in self.all_outputs() {
            let map = layer_map_for_output(&out);
            for layer in map.layers() {
                if *layer.wl_surface() == root
                    && let Some(geo) = map.layer_geometry(layer)
                {
                    let origin = out.current_location().to_f64() + geo.loc.to_f64();
                    return Some(origin);
                }
            }
        }
        None
    }
}

fn root_surface_of(surface: &WlSurface) -> WlSurface {
    let mut cur = surface.clone();
    while let Some(parent) = get_parent(&cur) {
        cur = parent;
    }
    cur
}
