//! Multi-finger touchpad `GestureSpec` recognizer (`Swipe`, `Pinch`, `Hold`), Niri-faithful
//! 1:1 continuous `ScrollingStrip` pixel tracking + `0.997` velocity projection, and
//! `zwp_pointer_gestures_v1` Wayland client pass-through (`PointerGesturesState`),
//! modeled directly on `niri/src/input/{mod.rs, swipe_tracker.rs}` & `niri/src/layout/scrolling.rs`.
//!
//! ### Key Architectural Invariants
//! 1. **GNOME / Niri `16.0 * 16.0` Cumulative Squared-Distance Threshold**:
//!    - Multi-finger touchpad swipes accumulate `(cum_x, cum_y)` in unaccelerated touchpad space
//!      until `cum_x^2 + cum_y^2 >= 256.0` (`16 px`), locking cleanly onto the dominant axis.
//! 2. **1:1 Continuous Pixel Tracking & Velocity-Projected Snap on `LayoutMode::Scrolling`**:
//!    - When a horizontal swipe locks on a `Scrolling` workspace (`ScrollStripSwipe` / default
//!      3-finger horizontal gesture), Koma catches any in-flight `camera_spring` at its exact
//!      visual offset (`begin_view_offset_gesture`), zeros `SwipeTracker::pos` while preserving
//!      pre-lock `150 ms` velocity samples (`reset_pos_keep_history`), streams every
//!      `GestureSwipeUpdate` 1:1 into `update_view_offset_gesture` (`1200.0` working-area scale +
//!      `0.25x` boundary rubber-banding), and on `GestureSwipeEnd` projects the `0.997` deceleration
//!      resting point and seeds `camera_spring` with the exact release velocity!
//! 3. **Zero-Interference Client Gesture Pass-Through (`zwp_pointer_gestures_v1`)**:
//!    - Leaves **2-finger pinch-to-zoom / rotate** and **touchpad hold** unclaimed by default so
//!      Wayland clients (`Firefox`, `Chromium`, `imv`, `Loupe`, `Zathura`) receive native events.

#[cfg(feature = "smithay-tty")]
use std::any::Any;
use std::collections::HashMap;

use koma_lua::{ActionDescriptor, BindHandleId, GestureSpec, validate_gesture_spec};
use koma_model::{LayoutMode, SwipeTracker};
use koma_proto::WorkspaceId;
#[cfg(feature = "smithay-tty")]
use smithay::reexports::input::{self, event::gesture::GestureEventCoordinates as _};
use smithay::{
    backend::input::{
        Event as _, GestureBeginEvent as _, GestureEndEvent as _, GesturePinchUpdateEvent as _,
        GestureSwipeUpdateEvent as _, InputBackend,
    },
    input::pointer::{
        GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
        GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
        GestureSwipeUpdateEvent,
    },
    utils::SERIAL_COUNTER,
};
use tracing::debug;

use crate::{
    keybind_dispatcher::{execute_action_descriptor, refresh_pointer_hover_focus},
    state::KomaState,
};

/// Squared logical-pixel distance before committing a multi-finger swipe direction
/// (`16.0 * 16.0 = 256.0`, matching `niri/src/input/mod.rs` & GNOME Shell).
pub const SWIPE_DECISION_DIST_SQ: f64 = 16.0 * 16.0;

/// Cumulative `libinput` pinch scale thresholds (`1.0` = initial finger span at `PinchBegin`).
pub const PINCH_IN_SCALE_THRESHOLD: f64 = 0.85;
pub const PINCH_OUT_SCALE_THRESHOLD: f64 = 1.15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardinalSwipeDir {
    Left,
    Right,
    Up,
    Down,
}

impl CardinalSwipeDir {
    #[must_use]
    pub const fn is_horizontal(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeMatchPattern {
    Exact(CardinalSwipeDir),
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinchDir {
    In,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompiledGestureTrigger {
    Swipe {
        fingers: u8,
        pattern: SwipeMatchPattern,
    },
    Pinch {
        fingers: u8,
        dir: PinchDir,
    },
}

#[derive(Debug, Clone)]
pub struct CompiledGesture {
    pub slot_id: u64,
    pub enabled: bool,
    pub trigger: CompiledGestureTrigger,
    pub action: ActionDescriptor,
    pub cooldown_ms: Option<u64>,
    pub allow_when_locked: bool,
    pub ignore_when_preempted: bool,
}

#[derive(Debug, Clone)]
enum CompositorSwipePhase {
    /// Accumulating `(cum_x, cum_y)` until Euclidean radius reaches `16.0 px`.
    PendingThreshold { cum_x: f64, cum_y: f64 },
    /// Locked onto 1:1 continuous `ScrollingStrip` horizontal view tracking on `ws_id`.
    ContinuousScrolling { ws_id: WorkspaceId },
    /// Fired a discrete one-shot `ActionDescriptor` (`WorkspaceFocusNext`, `OverviewToggle`, etc.).
    DiscreteCommitted,
}

#[derive(Debug, Clone)]
enum ActiveSwipeSession {
    Compositor {
        fingers: u8,
        tracker_x: SwipeTracker,
        phase: CompositorSwipePhase,
    },
    ClientForwarded,
}

#[derive(Debug, Clone, Copy)]
enum ActivePinchSession {
    Compositor { fingers: u8, committed: bool },
    ClientForwarded,
}

/// Pre-compiled multi-finger touchpad gesture table & active gesture session state on `KomaState`.
#[derive(Debug)]
pub struct GestureTable {
    pub gestures: Vec<CompiledGesture>,
    pub cooldowns_ms: HashMap<u64, u64>,
    active_swipe: Option<ActiveSwipeSession>,
    active_pinch: Option<ActivePinchSession>,
}

impl Default for GestureTable {
    fn default() -> Self {
        Self::with_builtin_defaults()
    }
}

impl GestureTable {
    /// Out-of-the-box Niri-faithful 3-finger & 4-finger touchpad gestures active when
    /// `init.lua` defines zero `koma.gesture(...)` entries:
    /// - 3-finger horizontal -> continuous 1:1 view movement + velocity projection on `Scrolling`
    ///   workspaces (or `Deck` / `Dwindle` cycle on non-scrolling workspaces),
    /// - 3-finger vertical -> `WorkspaceFocusNext` / `WorkspaceFocusPrev`,
    /// - 4-finger vertical -> `OverviewToggle`,
    /// - 2-finger pinch remains unclaimed for Wayland client zoom/rotate!
    #[must_use]
    pub fn with_builtin_defaults() -> Self {
        let gestures = vec![
            CompiledGesture {
                slot_id: 1,
                enabled: true,
                trigger: CompiledGestureTrigger::Swipe {
                    fingers: 3,
                    pattern: SwipeMatchPattern::Horizontal,
                },
                action: ActionDescriptor::ScrollStripSwipe,
                cooldown_ms: None,
                allow_when_locked: false,
                ignore_when_preempted: false,
            },
            CompiledGesture {
                slot_id: 2,
                enabled: true,
                trigger: CompiledGestureTrigger::Swipe {
                    fingers: 3,
                    pattern: SwipeMatchPattern::Exact(CardinalSwipeDir::Up),
                },
                action: ActionDescriptor::WorkspaceFocusNext,
                cooldown_ms: None,
                allow_when_locked: false,
                ignore_when_preempted: false,
            },
            CompiledGesture {
                slot_id: 3,
                enabled: true,
                trigger: CompiledGestureTrigger::Swipe {
                    fingers: 3,
                    pattern: SwipeMatchPattern::Exact(CardinalSwipeDir::Down),
                },
                action: ActionDescriptor::WorkspaceFocusPrev,
                cooldown_ms: None,
                allow_when_locked: false,
                ignore_when_preempted: false,
            },
            CompiledGesture {
                slot_id: 4,
                enabled: true,
                trigger: CompiledGestureTrigger::Swipe {
                    fingers: 4,
                    pattern: SwipeMatchPattern::Vertical,
                },
                action: ActionDescriptor::OverviewToggle,
                cooldown_ms: Some(200),
                allow_when_locked: false,
                ignore_when_preempted: false,
            },
        ];

        Self {
            gestures,
            cooldowns_ms: HashMap::new(),
            active_swipe: None,
            active_pinch: None,
        }
    }

    /// Validates and compiles `GestureSpec` entries from `KomaConfigRegistry`.
    pub fn compile(specs: &[GestureSpec]) -> Result<Self, String> {
        if specs.is_empty() {
            return Ok(Self::with_builtin_defaults());
        }

        let mut gestures = Vec::with_capacity(specs.len());
        for (idx, spec) in specs.iter().enumerate() {
            let (fingers, canonical_dir) =
                validate_gesture_spec(i64::from(spec.fingers), &spec.direction)
                    .map_err(|e| format!("gesture #{idx}: {e}"))?;

            let trigger = match canonical_dir.as_str() {
                "left" => CompiledGestureTrigger::Swipe {
                    fingers,
                    pattern: SwipeMatchPattern::Exact(CardinalSwipeDir::Left),
                },
                "right" => CompiledGestureTrigger::Swipe {
                    fingers,
                    pattern: SwipeMatchPattern::Exact(CardinalSwipeDir::Right),
                },
                "up" => CompiledGestureTrigger::Swipe {
                    fingers,
                    pattern: SwipeMatchPattern::Exact(CardinalSwipeDir::Up),
                },
                "down" => CompiledGestureTrigger::Swipe {
                    fingers,
                    pattern: SwipeMatchPattern::Exact(CardinalSwipeDir::Down),
                },
                "horizontal" => CompiledGestureTrigger::Swipe {
                    fingers,
                    pattern: SwipeMatchPattern::Horizontal,
                },
                "vertical" => CompiledGestureTrigger::Swipe {
                    fingers,
                    pattern: SwipeMatchPattern::Vertical,
                },
                "pinch_in" => CompiledGestureTrigger::Pinch {
                    fingers,
                    dir: PinchDir::In,
                },
                "pinch_out" => CompiledGestureTrigger::Pinch {
                    fingers,
                    dir: PinchDir::Out,
                },
                other => {
                    return Err(format!("gesture #{idx}: unsupported direction `{other}`"));
                }
            };

            let slot_id = spec.id.map_or((idx + 1) as u64, |BindHandleId(raw)| raw);

            gestures.push(CompiledGesture {
                slot_id,
                enabled: spec.enabled,
                trigger,
                action: spec.action.clone(),
                cooldown_ms: spec.cooldown_ms,
                allow_when_locked: spec.allow_when_locked,
                ignore_when_preempted: spec.when_preempted == "ignore",
            });
        }

        Ok(Self {
            gestures,
            cooldowns_ms: HashMap::new(),
            active_swipe: None,
            active_pinch: None,
        })
    }

    /// Clears any in-flight compositor gesture tracking (e.g. on VT switch or session resume).
    pub fn reset_active_sessions(&mut self) {
        self.active_swipe = None;
        self.active_pinch = None;
    }

    /// Returns `true` if any active compositor rule claims `fingers`-finger swipe gestures.
    #[must_use]
    pub fn claims_swipe_fingers(&self, fingers: u8, is_locked: bool, is_preempted: bool) -> bool {
        self.gestures.iter().any(|g| {
            g.enabled
                && (!is_locked || g.allow_when_locked)
                && (!is_preempted || !g.ignore_when_preempted)
                && matches!(
                    g.trigger,
                    CompiledGestureTrigger::Swipe { fingers: f, .. } if f == fingers
                )
        })
    }

    /// Returns `true` if any active compositor rule claims `fingers`-finger pinch gestures.
    #[must_use]
    pub fn claims_pinch_fingers(&self, fingers: u8, is_locked: bool, is_preempted: bool) -> bool {
        self.gestures.iter().any(|g| {
            g.enabled
                && (!is_locked || g.allow_when_locked)
                && (!is_preempted || !g.ignore_when_preempted)
                && matches!(
                    g.trigger,
                    CompiledGestureTrigger::Pinch { fingers: f, .. } if f == fingers
                )
        })
    }

    /// Resolves a committed `(fingers, dir)` swipe, giving exact cardinal bindings priority over
    /// axis-level `"horizontal"` / `"vertical"` bindings.
    #[must_use]
    #[allow(dead_code)] // Public GestureTable API retained alongside `resolve_swipe_in_slice`.
    pub fn resolve_swipe_gesture(
        &self,
        fingers: u8,
        dir: CardinalSwipeDir,
        is_locked: bool,
        is_preempted: bool,
    ) -> Option<CompiledGesture> {
        resolve_swipe_in_slice(&self.gestures, fingers, dir, is_locked, is_preempted)
    }

    #[must_use]
    pub fn resolve_pinch_gesture(
        &self,
        fingers: u8,
        dir: PinchDir,
        is_locked: bool,
        is_preempted: bool,
    ) -> Option<CompiledGesture> {
        self.gestures
            .iter()
            .rev()
            .find(|g| {
                g.enabled
                    && (!is_locked || g.allow_when_locked)
                    && (!is_preempted || !g.ignore_when_preempted)
                    && g.trigger == CompiledGestureTrigger::Pinch { fingers, dir }
            })
            .cloned()
    }
}

/// Classifies cumulative `(cum_x, cum_y)` once squared distance reaches `SWIPE_DECISION_DIST_SQ` (`256.0`).
#[must_use]
fn resolve_swipe_in_slice(
    gestures: &[CompiledGesture],
    fingers: u8,
    dir: CardinalSwipeDir,
    is_locked: bool,
    is_preempted: bool,
) -> Option<CompiledGesture> {
    if let Some(exact) = gestures.iter().rev().find(|g| {
        g.enabled
            && (!is_locked || g.allow_when_locked)
            && (!is_preempted || !g.ignore_when_preempted)
            && g.trigger
                == CompiledGestureTrigger::Swipe {
                    fingers,
                    pattern: SwipeMatchPattern::Exact(dir),
                }
    }) {
        return Some(exact.clone());
    }

    let axis_pattern = match dir {
        CardinalSwipeDir::Left | CardinalSwipeDir::Right => SwipeMatchPattern::Horizontal,
        CardinalSwipeDir::Up | CardinalSwipeDir::Down => SwipeMatchPattern::Vertical,
    };

    gestures
        .iter()
        .rev()
        .find(|g| {
            g.enabled
                && (!is_locked || g.allow_when_locked)
                && (!is_preempted || !g.ignore_when_preempted)
                && g.trigger
                    == CompiledGestureTrigger::Swipe {
                        fingers,
                        pattern: axis_pattern,
                    }
        })
        .cloned()
}

pub fn classify_swipe_delta(cum_x: f64, cum_y: f64) -> Option<CardinalSwipeDir> {
    let dist_sq = cum_x * cum_x + cum_y * cum_y;
    if dist_sq < SWIPE_DECISION_DIST_SQ {
        return None;
    }
    if cum_x.abs() >= cum_y.abs() {
        if cum_x < 0.0 {
            Some(CardinalSwipeDir::Left)
        } else {
            Some(CardinalSwipeDir::Right)
        }
    } else if cum_y < 0.0 {
        Some(CardinalSwipeDir::Up)
    } else {
        Some(CardinalSwipeDir::Down)
    }
}

#[must_use]
const fn action_supports_continuous_scrolling(action: &ActionDescriptor) -> bool {
    matches!(
        action,
        ActionDescriptor::ScrollStripSwipe
            | ActionDescriptor::DeckCycleNext
            | ActionDescriptor::DeckCyclePrev
    )
}

fn active_ws_is_preempted(state: &KomaState) -> bool {
    state
        .workspaces
        .get(&state.active_ws_id)
        .is_some_and(|w| w.lease.is_human_preempted())
}

fn fire_compiled_gesture(
    state: &mut KomaState,
    gesture: CompiledGesture,
    resolved_swipe_dir: Option<CardinalSwipeDir>,
    is_bare_metal_tty: bool,
) {
    let now_ms = state.elapsed_ms();
    if let Some(cd_ms) = gesture.cooldown_ms
        && cd_ms > 0
    {
        if let Some(&prev_ms) = state.gestures.cooldowns_ms.get(&gesture.slot_id)
            && now_ms.saturating_sub(prev_ms) < cd_ms
        {
            debug!(
                slot_id = gesture.slot_id,
                cd_ms, "suppressing multi-finger gesture within cooldown window"
            );
            return;
        }
        state.gestures.cooldowns_ms.insert(gesture.slot_id, now_ms);
    }

    let effective_action = match (&gesture.action, resolved_swipe_dir) {
        (ActionDescriptor::ScrollStripSwipe, Some(CardinalSwipeDir::Left)) => {
            ActionDescriptor::DeckCycleNext
        }
        (ActionDescriptor::ScrollStripSwipe, Some(CardinalSwipeDir::Right)) => {
            ActionDescriptor::DeckCyclePrev
        }
        (ActionDescriptor::ScrollStripSwipe, Some(CardinalSwipeDir::Up)) => {
            ActionDescriptor::WorkspaceFocusNext
        }
        (ActionDescriptor::ScrollStripSwipe, Some(CardinalSwipeDir::Down)) => {
            ActionDescriptor::WorkspaceFocusPrev
        }
        _ => gesture.action,
    };

    debug!(
        slot_id = gesture.slot_id,
        ?resolved_swipe_dir,
        "firing matched discrete multi-finger touchpad gesture"
    );
    execute_action_descriptor(state, effective_action, is_bare_metal_tty);
}

pub fn process_gesture_swipe_begin<B: InputBackend>(
    state: &mut KomaState,
    event: &B::GestureSwipeBeginEvent,
) {
    state.reconcile_session_lock();
    state.keybinds.pending_releases.clear();
    let fingers = event.fingers().clamp(1, 8) as u8;
    let is_locked = state.is_locked_or_abandoned();
    let is_preempted = active_ws_is_preempted(state);

    if !is_locked
        && state
            .gestures
            .claims_swipe_fingers(fingers, is_locked, is_preempted)
    {
        let mut tracker_x = SwipeTracker::new();
        tracker_x.push(0.0, u64::from(event.time().millis()));
        state.gestures.active_swipe = Some(ActiveSwipeSession::Compositor {
            fingers,
            tracker_x,
            phase: CompositorSwipePhase::PendingThreshold {
                cum_x: 0.0,
                cum_y: 0.0,
            },
        });
        return;
    }

    state.gestures.active_swipe = Some(ActiveSwipeSession::ClientForwarded);
    let time = event.time();
    refresh_pointer_hover_focus(state, time);
    if let Some(pointer) = state.seat.get_pointer() {
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_swipe_begin(
            state,
            &GestureSwipeBeginEvent {
                serial,
                time,
                fingers: event.fingers(),
            },
        );
    }
}

pub fn process_gesture_swipe_update<B: InputBackend + 'static>(
    state: &mut KomaState,
    event: &B::GestureSwipeUpdateEvent,
    is_bare_metal_tty: bool,
) where
    B::Device: 'static,
{
    // Extract unaccelerated `libinput` deltas and device natural-scroll state on bare-metal TTY
    // (`niri/src/input/mod.rs` lines 3886-3893).
    #[cfg(feature = "smithay-tty")]
    let (raw_dx, raw_dy, device_natural_scroll) = {
        let mut dx = event.delta_x();
        let mut dy = event.delta_y();
        let mut nat = false;
        if let Some(libinput_ev) =
            (event as &dyn Any).downcast_ref::<input::event::gesture::GestureSwipeUpdateEvent>()
        {
            dx = libinput_ev.dx_unaccelerated();
            dy = libinput_ev.dy_unaccelerated();
        }
        let dev = event.device();
        if let Some(libinput_dev) = (&dev as &dyn Any).downcast_ref::<input::Device>() {
            nat = libinput_dev.config_scroll_natural_scroll_enabled();
        }
        (dx, dy, nat)
    };
    #[cfg(not(feature = "smithay-tty"))]
    let (raw_dx, raw_dy, device_natural_scroll) = (event.delta_x(), event.delta_y(), false);

    let (natural_scroll, scroll_factor) = {
        let input_cfg = &state.lua_sandbox.config_snapshot().input;
        (
            input_cfg
                .touchpad_natural_scroll()
                .unwrap_or(device_natural_scroll),
            input_cfg.touchpad_scroll_factor(),
        )
    };

    // View-movement sign convention:
    // - `natural_scroll = true`: pushing fingers LEFT (`raw_dx < 0`) pulls windows LEFT on screen,
    //   which increases `camera_x` (`view_scroll_dx = -raw_dx > 0`).
    // - `natural_scroll = false`: moving fingers RIGHT (`raw_dx > 0`) pans viewport camera RIGHT
    //   (`view_scroll_dx = +raw_dx > 0`).
    let view_scroll_dx = (if natural_scroll { -raw_dx } else { raw_dx }) * scroll_factor;
    let classify_dx = if natural_scroll { -raw_dx } else { raw_dx };
    let classify_dy = if natural_scroll { -raw_dy } else { raw_dy };
    let event_time_ms = u64::from(event.time().millis());

    enum PostSwipeStep {
        None,
        BeginContinuous {
            ws_id: WorkspaceId,
        },
        UpdateContinuous {
            ws_id: WorkspaceId,
            pos: f64,
        },
        FireDiscrete {
            matched: CompiledGesture,
            cardinal: CardinalSwipeDir,
        },
    }

    state.reconcile_session_lock();
    let is_locked = state.is_locked_or_abandoned();
    if is_locked
        && matches!(
            state.gestures.active_swipe,
            Some(ActiveSwipeSession::Compositor { .. })
        )
    {
        state.gestures.active_swipe = None;
        return;
    }
    let is_preempted = active_ws_is_preempted(state);
    let interactive_ws_id = state.effective_interactive_ws_id();
    let interactive_is_scrolling = state.workspaces.get(&interactive_ws_id).is_some_and(|ws| {
        ws.layout_mode == LayoutMode::Scrolling && ws.fullscreen_window.is_none()
    });

    let mut next_step = PostSwipeStep::None;
    let mut promote_to_phase: Option<CompositorSwipePhase> = None;

    let GestureTable {
        gestures,
        active_swipe,
        ..
    } = &mut state.gestures;
    if let Some(ActiveSwipeSession::Compositor {
        fingers,
        tracker_x,
        phase,
    }) = active_swipe
    {
        let f_count = *fingers;
        match phase {
            CompositorSwipePhase::PendingThreshold { cum_x, cum_y } => {
                tracker_x.push(view_scroll_dx, event_time_ms);
                *cum_x += classify_dx;
                *cum_y += classify_dy;
                if let Some(cardinal) = classify_swipe_delta(*cum_x, *cum_y) {
                    let matched = resolve_swipe_in_slice(
                        gestures,
                        f_count,
                        cardinal,
                        is_locked,
                        is_preempted,
                    );
                    if let Some(m) = matched {
                        if cardinal.is_horizontal()
                            && interactive_is_scrolling
                            && action_supports_continuous_scrolling(&m.action)
                        {
                            // Zero `pos` at threshold lock to avoid a 16px visual step while keeping
                            // pre-lock velocity samples inside `tracker_x`!
                            tracker_x.reset_pos_keep_history();
                            promote_to_phase = Some(CompositorSwipePhase::ContinuousScrolling {
                                ws_id: interactive_ws_id.clone(),
                            });
                            next_step = PostSwipeStep::BeginContinuous {
                                ws_id: interactive_ws_id,
                            };
                        } else {
                            promote_to_phase = Some(CompositorSwipePhase::DiscreteCommitted);
                            next_step = PostSwipeStep::FireDiscrete {
                                matched: m,
                                cardinal,
                            };
                        }
                    } else {
                        promote_to_phase = Some(CompositorSwipePhase::DiscreteCommitted);
                    }
                }
            }
            CompositorSwipePhase::ContinuousScrolling { ws_id } => {
                tracker_x.push(view_scroll_dx, event_time_ms);
                next_step = PostSwipeStep::UpdateContinuous {
                    ws_id: ws_id.clone(),
                    pos: tracker_x.pos(),
                };
            }
            CompositorSwipePhase::DiscreteCommitted => {}
        }

        if let Some(new_phase) = promote_to_phase {
            *phase = new_phase;
        }
    } else if matches!(
        state.gestures.active_swipe,
        Some(ActiveSwipeSession::ClientForwarded)
    ) {
        let time = event.time();
        refresh_pointer_hover_focus(state, time);
        if let Some(pointer) = state.seat.get_pointer() {
            pointer.gesture_swipe_update(
                state,
                &GestureSwipeUpdateEvent {
                    time,
                    delta: event.delta(),
                },
            );
        }
        return;
    }

    match next_step {
        PostSwipeStep::None => {}
        PostSwipeStep::BeginContinuous { ws_id } => {
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                ws.scrolling.begin_view_offset_gesture(now_ms);
            }
            state.queue_redraw();
        }
        PostSwipeStep::UpdateContinuous { ws_id, pos } => {
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                let vp = ws.viewport;
                ws.scrolling.update_view_offset_gesture(pos, vp);
            }
            state.queue_redraw();
        }
        PostSwipeStep::FireDiscrete { matched, cardinal } => {
            fire_compiled_gesture(state, matched, Some(cardinal), is_bare_metal_tty);
        }
    }
}

pub fn process_gesture_swipe_end<B: InputBackend>(
    state: &mut KomaState,
    event: &B::GestureSwipeEndEvent,
) {
    state.reconcile_session_lock();
    let prev = state.gestures.active_swipe.take();
    match prev {
        Some(ActiveSwipeSession::Compositor {
            mut tracker_x,
            phase: CompositorSwipePhase::ContinuousScrolling { ws_id },
            ..
        }) => {
            if state.is_locked_or_abandoned() {
                return;
            }
            let cancelled = event.cancelled();
            let event_time_ms = u64::from(event.time().millis());
            let release_vel = if cancelled {
                0.0
            } else {
                tracker_x.velocity(event_time_ms)
            };
            let now_ms = state.elapsed_ms();

            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                let vp = ws.viewport;
                if let Some(snapped_win) =
                    ws.scrolling
                        .end_view_offset_gesture(release_vel, cancelled, vp, now_ms)
                {
                    ws.focused_window = Some(snapped_win);
                    let _ = ws.deck.activate(snapped_win);
                }
            }
            state.sync_workspace_geometries(&ws_id);
            state.focus_active_deck_window();
            state.queue_redraw();
        }
        Some(ActiveSwipeSession::ClientForwarded) => {
            let time = event.time();
            refresh_pointer_hover_focus(state, time);
            if let Some(pointer) = state.seat.get_pointer() {
                let serial = SERIAL_COUNTER.next_serial();
                pointer.gesture_swipe_end(
                    state,
                    &GestureSwipeEndEvent {
                        serial,
                        time,
                        cancelled: event.cancelled(),
                    },
                );
            }
        }
        Some(ActiveSwipeSession::Compositor { .. }) | None => {}
    }
}

pub fn process_gesture_pinch_begin<B: InputBackend>(
    state: &mut KomaState,
    event: &B::GesturePinchBeginEvent,
) {
    state.reconcile_session_lock();
    state.keybinds.pending_releases.clear();
    let fingers = event.fingers().clamp(1, 8) as u8;
    let is_locked = state.is_locked_or_abandoned();
    let is_preempted = active_ws_is_preempted(state);

    if !is_locked
        && state
            .gestures
            .claims_pinch_fingers(fingers, is_locked, is_preempted)
    {
        state.gestures.active_pinch = Some(ActivePinchSession::Compositor {
            fingers,
            committed: false,
        });
        return;
    }

    state.gestures.active_pinch = Some(ActivePinchSession::ClientForwarded);
    let time = event.time();
    refresh_pointer_hover_focus(state, time);
    if let Some(pointer) = state.seat.get_pointer() {
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_pinch_begin(
            state,
            &GesturePinchBeginEvent {
                serial,
                time,
                fingers: event.fingers(),
            },
        );
    }
}

pub fn process_gesture_pinch_update<B: InputBackend>(
    state: &mut KomaState,
    event: &B::GesturePinchUpdateEvent,
    is_bare_metal_tty: bool,
) {
    state.reconcile_session_lock();
    let is_locked = state.is_locked_or_abandoned();
    match &mut state.gestures.active_pinch {
        Some(ActivePinchSession::Compositor { fingers, committed }) => {
            if is_locked || *committed {
                return;
            }
            let f_count = *fingers;
            let scale = event.scale();
            let dir = if scale <= PINCH_IN_SCALE_THRESHOLD {
                Some(PinchDir::In)
            } else if scale >= PINCH_OUT_SCALE_THRESHOLD {
                Some(PinchDir::Out)
            } else {
                None
            };

            if let Some(pinch_dir) = dir {
                *committed = true;
                let is_preempted = active_ws_is_preempted(state);
                if let Some(matched) = state.gestures.resolve_pinch_gesture(
                    f_count,
                    pinch_dir,
                    is_locked,
                    is_preempted,
                ) {
                    fire_compiled_gesture(state, matched, None, is_bare_metal_tty);
                }
            }
        }
        Some(ActivePinchSession::ClientForwarded) => {
            let time = event.time();
            refresh_pointer_hover_focus(state, time);
            let sensitivity = state
                .lua_sandbox
                .config_snapshot()
                .input
                .touchpad_pinch_sensitivity();
            if let Some(pointer) = state.seat.get_pointer() {
                pointer.gesture_pinch_update(
                    state,
                    &GesturePinchUpdateEvent {
                        time,
                        delta: event.delta(),
                        scale: event.scale().powf(sensitivity),
                        rotation: event.rotation(),
                    },
                );
            }
        }
        None => {}
    }
}

pub fn process_gesture_pinch_end<B: InputBackend>(
    state: &mut KomaState,
    event: &B::GesturePinchEndEvent,
) {
    let prev = state.gestures.active_pinch.take();
    if matches!(prev, Some(ActivePinchSession::ClientForwarded)) {
        let time = event.time();
        refresh_pointer_hover_focus(state, time);
        if let Some(pointer) = state.seat.get_pointer() {
            let serial = SERIAL_COUNTER.next_serial();
            pointer.gesture_pinch_end(
                state,
                &GesturePinchEndEvent {
                    serial,
                    time,
                    cancelled: event.cancelled(),
                },
            );
        }
    }
}

pub fn process_gesture_hold_begin<B: InputBackend>(
    state: &mut KomaState,
    event: &B::GestureHoldBeginEvent,
) {
    state.keybinds.pending_releases.clear();
    let time = event.time();
    refresh_pointer_hover_focus(state, time);
    if let Some(pointer) = state.seat.get_pointer() {
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_hold_begin(
            state,
            &GestureHoldBeginEvent {
                serial,
                time,
                fingers: event.fingers(),
            },
        );
    }
}

pub fn process_gesture_hold_end<B: InputBackend>(
    state: &mut KomaState,
    event: &B::GestureHoldEndEvent,
) {
    let time = event.time();
    refresh_pointer_hover_focus(state, time);
    if let Some(pointer) = state.seat.get_pointer() {
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_hold_end(
            state,
            &GestureHoldEndEvent {
                serial,
                time,
                cancelled: event.cancelled(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swipe_threshold_requires_16px_euclidean_radius() {
        assert_eq!(classify_swipe_delta(10.0, 10.0), None);
        assert_eq!(
            classify_swipe_delta(-17.0, 4.0),
            Some(CardinalSwipeDir::Left)
        );
        assert_eq!(
            classify_swipe_delta(3.0, 18.0),
            Some(CardinalSwipeDir::Down)
        );
    }

    #[test]
    fn exact_swipe_direction_takes_priority_over_horizontal_shorthand() {
        let specs = vec![
            GestureSpec {
                id: Some(BindHandleId(1)),
                enabled: true,
                fingers: 3,
                direction: "horizontal".to_owned(),
                action: ActionDescriptor::ScrollStripSwipe,
                cooldown_ms: None,
                allow_when_locked: false,
                when_preempted: "shield".to_owned(),
                desc: None,
            },
            GestureSpec {
                id: Some(BindHandleId(2)),
                enabled: true,
                fingers: 3,
                direction: "left".to_owned(),
                action: ActionDescriptor::OverviewToggle,
                cooldown_ms: None,
                allow_when_locked: false,
                when_preempted: "shield".to_owned(),
                desc: None,
            },
        ];

        let table = GestureTable::compile(&specs).expect("valid gesture table");
        let left_hit = table
            .resolve_swipe_gesture(3, CardinalSwipeDir::Left, false, false)
            .expect("left matched");
        assert_eq!(left_hit.action, ActionDescriptor::OverviewToggle);

        let right_hit = table
            .resolve_swipe_gesture(3, CardinalSwipeDir::Right, false, false)
            .expect("right matched");
        assert_eq!(right_hit.action, ActionDescriptor::ScrollStripSwipe);
        assert!(action_supports_continuous_scrolling(&right_hit.action));
    }

    #[test]
    fn two_finger_pinch_remains_unclaimed_by_default_for_client_zoom() {
        let defaults = GestureTable::with_builtin_defaults();
        assert!(!defaults.claims_pinch_fingers(2, false, false));
        assert!(defaults.claims_swipe_fingers(3, false, false));
        assert!(defaults.claims_swipe_fingers(4, false, false));
    }

    #[test]
    fn session_lock_blocks_swipe_and_pinch_gesture_execution() {
        let defaults = GestureTable::with_builtin_defaults();
        assert!(
            defaults
                .resolve_swipe_gesture(3, CardinalSwipeDir::Left, true, false)
                .is_none()
        );
        assert!(
            defaults
                .resolve_swipe_gesture(4, CardinalSwipeDir::Up, true, false)
                .is_none()
        );
        assert!(
            defaults
                .resolve_pinch_gesture(3, PinchDir::In, true, false)
                .is_none()
        );
        assert!(!defaults.claims_swipe_fingers(3, true, false));
    }
}
