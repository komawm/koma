//! Ring 0 `Phodopus` host-action executor, kinematic pointer stepper, window damage-settle
//! detector, and `calloop` one-shot timer trampoline (`ARCHITECTURE.md` §2.2, §5.2, §5.3).

use std::{process::Command, sync::Arc, time::Duration};

use calloop::{
    RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use koma_lua::{HostBridge, phodopus_bridge::QueuedHostAction, schema::LuaKinematicsInput};
use koma_model::{
    AnchorPoint, LogicalPoint, LogicalRect, MinimumJerkSolver, SpringSolver, TrajectorySolver as _,
    WindowMatchContext,
};
use koma_proto::{
    Epoch, EventTopic, IpcError, KinematicProfileKind, TextInjectStrategy, WorkspaceEvent,
    WorkspaceId,
};
use regex::Regex;
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, InputTime, KeyState, Keycode},
    input::{
        keyboard::{FilterResult, Keysym, xkb},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::{Point, SERIAL_COUNTER},
    wayland::selection::data_device::set_data_device_selection,
};
use tracing::{debug, info, warn};

use crate::state::KomaState;

const KINEMATIC_TICK_MS: u64 = 8; // ~125 Hz smooth pointer interpolation step
const DEFAULT_SETTLE_QUIET_MS: u64 = 40;

/// Per-workspace surface damage tracker driving `window.settled` events and `WaitDamageSettle` wakes.
#[derive(Debug, Default)]
pub struct WorkspaceDamageTracker {
    pub last_commit_ms: u64,
    pub dirty_unsettled: bool,
    pub settle_timer_token: Option<RegistrationToken>,
}

/// In-flight smooth `vSeat` pointer animation (`minimum_jerk` or `spring`).
#[derive(Debug, Clone)]
pub struct ActivePointerAnimation {
    pub ws_id: WorkspaceId,
    pub expected_epoch: Epoch,
    pub from: LogicalPoint,
    pub to: LogicalPoint,
    pub start_ms: u64,
    pub duration_ms: u64,
    pub kinematics: LuaKinematicsInput,
}

/// Read-only `HostBridge` view over `KomaState` passed into `PhodopusSandbox::drive_ready_coroutines`.
struct StateEpochView<'a> {
    workspaces: &'a std::collections::HashMap<WorkspaceId, koma_model::WorkspaceState>,
}

impl HostBridge for StateEpochView<'_> {
    fn current_epoch(&self, ws_id: &WorkspaceId) -> Option<Epoch> {
        self.workspaces.get(ws_id).map(|ws| ws.lease.epoch())
    }

    fn focused_app_id(&self, _ws_id: &WorkspaceId) -> Option<String> {
        None
    }

    fn tap_key_chord(&mut self, _ws_id: &WorkspaceId, _chord: &str) -> Result<(), IpcError> {
        Ok(())
    }

    fn move_pointer(
        &mut self,
        _ws_id: &WorkspaceId,
        _target: &AnchorPoint,
    ) -> Result<u64, IpcError> {
        Ok(0)
    }

    fn inject_text(
        &mut self,
        _ws_id: &WorkspaceId,
        _utf8: &str,
        _strategy: TextInjectStrategy,
    ) -> Result<(), IpcError> {
        Ok(())
    }

    fn paste_mime(
        &mut self,
        _ws_id: &WorkspaceId,
        _mime_type: &str,
        _payload: &[u8],
    ) -> Result<(), IpcError> {
        Ok(())
    }
}

/// Invoked from `CompositorHandler::commit` whenever a mapped surface or popup on `ws_id` commits
/// a new buffer. Resets the workspace's quiet-window debounce timer on `calloop`.
pub fn notify_workspace_surface_damage(state: &mut KomaState, ws_id: &WorkspaceId) {
    let now_ms = state.elapsed_ms();
    let quiet_ms = state
        .lua_sandbox
        .pending_settle_quiet_ms(ws_id)
        .unwrap_or(DEFAULT_SETTLE_QUIET_MS)
        .max(10);

    let prev_token = {
        let tracker = state.damage_trackers.entry(ws_id.clone()).or_default();
        tracker.last_commit_ms = now_ms;
        tracker.dirty_unsettled = true;
        tracker.settle_timer_token.take()
    };

    if let Some(loop_handle) = state.loop_handle.clone() {
        if let Some(tok) = prev_token {
            loop_handle.remove(tok);
        }
        let ws_target = ws_id.clone();
        match loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(quiet_ms)),
            move |_, _, state| {
                on_damage_quiet_timer_fired(state, &ws_target, quiet_ms);
                TimeoutAction::Drop
            },
        ) {
            Ok(new_tok) => {
                if let Some(tracker) = state.damage_trackers.get_mut(ws_id) {
                    tracker.settle_timer_token = Some(new_tok);
                }
            }
            Err(err) => warn!(%ws_id, "failed arming damage-settle timer: {err}"),
        }
    }
}

fn on_damage_quiet_timer_fired(state: &mut KomaState, ws_id: &WorkspaceId, quiet_ms: u64) {
    let now_ms = state.elapsed_ms();
    let settled = if let Some(tracker) = state.damage_trackers.get_mut(ws_id) {
        tracker.settle_timer_token = None;
        if now_ms.saturating_sub(tracker.last_commit_ms) >= quiet_ms {
            tracker.dirty_unsettled = false;
            true
        } else {
            false
        }
    } else {
        true
    };

    if !settled {
        return;
    }

    if let Some(epoch) = state.workspaces.get(ws_id).map(|ws| ws.lease.epoch()) {
        state.broadcast_workspace_event(
            EventTopic::WindowDamageSettled,
            ws_id,
            epoch,
            WorkspaceEvent::WindowSettled { quiet_ms },
        );
    }

    // Wake any `Phodopus` macro coroutine parked on `PendingHostOp::WaitDamageSettle` for `ws_id`!
    drive_macro_tick(state, std::slice::from_ref(ws_id));
}

/// Executes one full non-blocking cycle of the macro runtime:
/// 1. Advances in-flight kinematic pointer trajectories;
/// 2. Collects workspaces whose surface damage has already been quiet for >= `quiet_ms`;
/// 3. Resumes all ready `TrampolineScheduler` `HostOpHandle` coroutines in `PhodopusSandbox`;
/// 4. Executes all newly emitted `QueuedHostAction` commands against Ring 0 seats/surfaces;
/// 5. Rearms a single exact one-shot `calloop::timer::Timer` for the next earliest deadline.
pub fn drive_macro_tick(state: &mut KomaState, explicit_settled: &[WorkspaceId]) {
    let now_ms = state.elapsed_ms();

    // 1. Step smooth kinematic pointer trajectories (`minimum_jerk` / `spring`)
    step_pointer_animations(state, now_ms);

    // 2. Include workspaces that had zero recent damage for >= their requested `quiet_ms`
    //    so `ctx:wait_settle()` never stalls when the UI was already stationary!
    let mut settled_ws: Vec<WorkspaceId> = explicit_settled.to_vec();
    for ws_id in state.workspaces.keys() {
        if let Some(req_quiet) = state.lua_sandbox.pending_settle_quiet_ms(ws_id) {
            let is_quiet = state.damage_trackers.get(ws_id).is_none_or(|t| {
                !t.dirty_unsettled && now_ms.saturating_sub(t.last_commit_ms) >= req_quiet
            });
            if is_quiet && !settled_ws.contains(ws_id) {
                settled_ws.push(ws_id.clone());
            }
        }
    }

    // 3. Resume ready `Phodopus` coroutines & cancel stale-epoch ones
    let report = {
        let view = StateEpochView {
            workspaces: &state.workspaces,
        };
        state
            .lua_sandbox
            .drive_ready_coroutines(now_ms, &settled_ws, &view)
    };
    if report.resumed_slices > 0 || report.cancelled_stale > 0 {
        debug!(
            resumed = report.resumed_slices,
            completed = report.completed_macros,
            stale_cancelled = report.cancelled_stale,
            "stepped TrampolineScheduler macro coroutines"
        );
    }

    // 4. Drain & execute Ring 0 side effects emitted by resumed/started coroutines
    execute_queued_host_actions(state);
    crate::config_watcher::promote_deferred_sandbox_if_idle(state);

    // 5. Rearm single precision `calloop` wakeup timer
    rearm_trampoline_timer(state);
}

/// Drains `PhodopusSandbox::drain_queued_actions()` and dispatches them onto Wayland surfaces & seats.
pub fn execute_queued_host_actions(state: &mut KomaState) {
    let actions = state.lua_sandbox.drain_queued_actions();
    if actions.is_empty() {
        return;
    }

    for action in actions {
        dispatch_single_host_action(state, action);
    }

    // Check if newly parked `WaitDamageSettle` ops need a quiet-check timer armed
    rearm_trampoline_timer(state);
}

pub(crate) fn dispatch_single_host_action(state: &mut KomaState, action: QueuedHostAction) {
    match action {
        QueuedHostAction::SpawnProcess { argv } => {
            if let Some((prog, args)) = argv.split_first() {
                let active_ws = state.active_ws_id.clone();
                let mut cmd = Command::new(prog);
                cmd.args(args);
                state.configure_child_env(&mut cmd, Some(&active_ws));
                let _ = cmd.spawn();
            }
        }

        QueuedHostAction::AssertFocusedApp {
            ws_id,
            expected_epoch,
            app_regex,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            let focused_app = active_window_in_ws(state, &ws_id)
                .map(|m| m.app_id.clone())
                .unwrap_or_default();
            let matched = Regex::new(&app_regex).is_ok_and(|re| re.is_match(&focused_app));
            if !matched {
                warn!(
                    %ws_id,
                    expected = %app_regex,
                    found = %focused_app,
                    "ctx:assert_app failed; cancelling workspace macro coroutines"
                );
                let _ = state.lua_sandbox.cancel_workspace_coroutines(
                    &ws_id,
                    &format!(
                        "ERR_PRECONDITION: focused app `{focused_app}` did not match `{app_regex}`"
                    ),
                );
            }
        }

        QueuedHostAction::TapKeyChord {
            ws_id,
            expected_epoch,
            chord,
            hold_ms: _,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            synthesize_chord_tap(state, &ws_id, &chord);
        }

        QueuedHostAction::KeyDown {
            ws_id,
            expected_epoch,
            key,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            if let Some(ws) = state.workspaces.get(&ws_id)
                && let Some(ledger) = state.vseats.get_mut(&ws.vseat_id)
            {
                ledger.record_key_down(&key);
            }
            synthesize_single_key_edge(state, &key, KeyState::Pressed);
        }

        QueuedHostAction::KeyUp {
            ws_id,
            expected_epoch,
            key,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            if let Some(ws) = state.workspaces.get(&ws_id)
                && let Some(ledger) = state.vseats.get_mut(&ws.vseat_id)
            {
                ledger.record_key_up(&key);
            }
            synthesize_single_key_edge(state, &key, KeyState::Released);
        }

        QueuedHostAction::MovePointer {
            ws_id,
            expected_epoch,
            target,
            kinematics,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            let win_rect = target_window_rect(state, &ws_id);
            let Ok(dst_pt) = target.resolve_in(win_rect) else {
                warn!(%ws_id, "invalid grid_225 label in MovePointer");
                return;
            };

            if kinematics.profile == KinematicProfileKind::Instant || kinematics.duration_ms == 0 {
                apply_pointer_motion(state, dst_pt);
            } else {
                let from = LogicalPoint {
                    x: state.pointer_location.x,
                    y: state.pointer_location.y,
                };
                let start_ms = state.elapsed_ms();
                state.pointer_animations.retain(|a| a.ws_id != ws_id);
                state.pointer_animations.push(ActivePointerAnimation {
                    ws_id,
                    expected_epoch,
                    from,
                    to: dst_pt,
                    start_ms,
                    duration_ms: kinematics.duration_ms,
                    kinematics,
                });
            }
        }

        QueuedHostAction::PointerButton {
            ws_id,
            expected_epoch,
            button,
            pressed,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            let btn_code = linux_button_code(&button);
            if let Some(ws) = state.workspaces.get(&ws_id)
                && let Some(ledger) = state.vseats.get_mut(&ws.vseat_id)
            {
                if pressed {
                    ledger.record_button_down(btn_code);
                } else {
                    ledger.record_button_up(btn_code);
                }
            }
            apply_pointer_button(
                state,
                btn_code,
                if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            );
        }

        QueuedHostAction::ClickPointer {
            ws_id,
            expected_epoch,
            target,
            opts,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            let win_rect = target_window_rect(state, &ws_id);
            if let Ok(dst_pt) = target.resolve_in(win_rect) {
                apply_pointer_motion(state, dst_pt);
                let btn_code = linux_button_code(&opts.button);
                for _ in 0..opts.clicks.max(1) {
                    apply_pointer_button(state, btn_code, ButtonState::Pressed);
                    apply_pointer_button(state, btn_code, ButtonState::Released);
                }
            }
        }

        QueuedHostAction::Scroll {
            ws_id,
            expected_epoch,
            dx,
            dy,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            if let Some(pointer) = state.seat.get_pointer() {
                let mut frame = AxisFrame::new(InputTime::now()).source(AxisSource::Wheel);
                if dx.abs() > f64::EPSILON {
                    frame = frame
                        .value(Axis::Horizontal, dx * 15.0)
                        .v120(Axis::Horizontal, (dx * 120.0).round() as i32);
                }
                if dy.abs() > f64::EPSILON {
                    frame = frame
                        .value(Axis::Vertical, dy * 15.0)
                        .v120(Axis::Vertical, (dy * 120.0).round() as i32);
                }
                pointer.axis(state, frame);
                pointer.frame(state);
            }
        }

        QueuedHostAction::InjectText {
            ws_id,
            expected_epoch,
            utf8,
            strategy: _,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            // Zero-IME clipboard payload write + Ctrl+V paste (bypasses Fcitx5/IBus popups)
            let mimes = vec![
                "text/plain;charset=utf-8".to_owned(),
                "UTF8_STRING".to_owned(),
            ];
            let payload: Arc<[u8]> = Arc::from(utf8.into_bytes());
            set_data_device_selection(&state.display_handle, &state.seat, mimes, payload);
            synthesize_chord_tap(state, &ws_id, "Ctrl+V");
        }

        QueuedHostAction::PasteMime {
            ws_id,
            expected_epoch,
            mime_type,
            payload,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            let mut mimes = vec![mime_type];
            if !mimes.iter().any(|m| m.starts_with("text/plain")) {
                mimes.push("text/plain;charset=utf-8".to_owned());
            }
            let buf: Arc<[u8]> = Arc::from(payload);
            set_data_device_selection(&state.display_handle, &state.seat, mimes, buf);
            synthesize_chord_tap(state, &ws_id, "Ctrl+V");
        }
    }
}

/// Validates both the `Epoch` fence AND the Protected Surface Blackout Invariant before delivering
/// any macro action (`IPC_SPEC.md` §3.2 & §4.2).
fn verify_action_epoch_and_protection(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    expected_epoch: Epoch,
) -> bool {
    let Some(ws) = state.workspaces.get(ws_id) else {
        return false;
    };
    if ws.lease.is_human_preempted() || ws.lease.epoch() != expected_epoch {
        let _ = state.lua_sandbox.cancel_workspace_coroutines(
            ws_id,
            "ERR_STALE_EPOCH: action dropped due to workspace epoch or takeover barrier",
        );
        return false;
    }

    // Protected Surface Guard: block input into Polkit / credential vaults & emit security alert
    if let Some(mapped) = active_window_in_ws(state, ws_id) {
        let win_id = mapped.win_id;
        let is_protected = state
            .rules
            .resolve_dynamic(&WindowMatchContext {
                app_id: &mapped.app_id,
                title: &mapped.title,
                is_floating: mapped.is_floating,
                is_focused: true,
                is_xwayland: false,
                is_modal_dialog: false,
                is_screencast_target: false,
                is_agent_workspace: true,
            })
            .block_out_from
            .is_some_and(|p| p.blocks_agent_capture());

        if is_protected {
            warn!(%ws_id, %win_id, "blocked macro input targeting Protected Surface");
            let _ = state.lua_sandbox.cancel_workspace_coroutines(
                ws_id,
                "ERR_PROTECTED_SURFACE: macro aborted on credential/privilege surface",
            );
            state.broadcast_workspace_event(
                EventTopic::SecurityAlerts,
                ws_id,
                expected_epoch,
                WorkspaceEvent::VerificationNeeded {
                    reason: "protected_surface_focused".to_owned(),
                    win_id,
                },
            );
            return false;
        }
    }

    true
}

fn step_pointer_animations(state: &mut KomaState, now_ms: u64) {
    if state.pointer_animations.is_empty() {
        return;
    }

    let mut remaining = Vec::new();
    let mut latest_pos: Option<LogicalPoint> = None;

    for anim in state.pointer_animations.drain(..) {
        let epoch_valid = state.workspaces.get(&anim.ws_id).is_some_and(|ws| {
            !ws.lease.is_human_preempted() && ws.lease.epoch() == anim.expected_epoch
        });
        if !epoch_valid {
            continue;
        }

        let elapsed = now_ms.saturating_sub(anim.start_ms);
        let t = (elapsed as f64 / anim.duration_ms.max(1) as f64).clamp(0.0, 1.0);
        let pt = match anim.kinematics.profile {
            KinematicProfileKind::MinimumJerk => MinimumJerkSolver {
                arc_curvature: anim.kinematics.arc_curvature,
            }
            .sample(anim.from, anim.to, t),
            KinematicProfileKind::Spring => SpringSolver::default().sample(anim.from, anim.to, t),
            KinematicProfileKind::Instant => anim.to,
        };
        latest_pos = Some(pt);

        if elapsed < anim.duration_ms {
            remaining.push(anim);
        }
    }

    state.pointer_animations = remaining;
    if let Some(pt) = latest_pos {
        apply_pointer_motion(state, pt);
    }
}

/// Arms a single precision `calloop::timer::Timer` matching the earliest of:
/// - `PhodopusSandbox::next_wakeup_deadline_ms()` (`SleepTimer`, `PointerTrajectory`, `WaitDamageSettle`)
/// - `now_ms + KINEMATIC_TICK_MS` (`8 ms`) while smooth pointer animations are in flight.
pub fn rearm_trampoline_timer(state: &mut KomaState) {
    let Some(loop_handle) = state.loop_handle.clone() else {
        return;
    };
    if let Some(prev) = state.trampoline_timer_token.take() {
        loop_handle.remove(prev);
    }

    let now_ms = state.elapsed_ms();
    let mut earliest = state.lua_sandbox.next_wakeup_deadline_ms();

    if !state.pointer_animations.is_empty() {
        let anim_tick = now_ms.saturating_add(KINEMATIC_TICK_MS);
        earliest = Some(earliest.map_or(anim_tick, |d| d.min(anim_tick)));
    }

    // Also ensure any parked `WaitDamageSettle` checks stationary windows within `quiet_ms`
    for ws_id in state.workspaces.keys() {
        if let Some(quiet_ms) = state.lua_sandbox.pending_settle_quiet_ms(ws_id) {
            let check_at = now_ms.saturating_add(quiet_ms.max(10));
            earliest = Some(earliest.map_or(check_at, |d| d.min(check_at)));
        }
    }

    let Some(deadline_ms) = earliest else {
        return;
    };
    let delay_ms = deadline_ms.saturating_sub(now_ms);
    match loop_handle.insert_source(
        Timer::from_duration(Duration::from_millis(delay_ms)),
        |_, _, state| {
            state.trampoline_timer_token = None;
            drive_macro_tick(state, &[]);
            TimeoutAction::Drop
        },
    ) {
        Ok(tok) => state.trampoline_timer_token = Some(tok),
        Err(err) => warn!("failed arming calloop trampoline timer: {err}"),
    }
}

fn active_window_in_ws<'a>(
    state: &'a KomaState,
    ws_id: &WorkspaceId,
) -> Option<&'a crate::state::MappedWindow> {
    let active_id = state.workspaces.get(ws_id)?.active_window()?;
    state.mapped_windows.iter().find(|m| m.win_id == active_id)
}

fn target_window_rect(state: &KomaState, ws_id: &WorkspaceId) -> LogicalRect {
    active_window_in_ws(state, ws_id)
        .map(|m| m.rect)
        .or_else(|| {
            state
                .workspaces
                .get(ws_id)
                .map(|ws| ws.viewport.full_rect())
        })
        .unwrap_or_else(|| LogicalRect::new(0, 0, 1920, 1080))
}

fn apply_pointer_motion(state: &mut KomaState, pt: LogicalPoint) {
    let pos = Point::from((pt.x, pt.y));
    state.pointer_location = pos;
    let under = crate::backend_winit::hit_test_active_deck_surface(state, pos);
    if let Some(pointer) = state.seat.get_pointer() {
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        pointer.motion(
            state,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        pointer.frame(state);
    }
}

fn apply_pointer_button(state: &mut KomaState, button: u32, btn_state: ButtonState) {
    if btn_state == ButtonState::Pressed {
        let pos = state.pointer_location;
        state.focus_window_at_point(pos);
    }
    if let Some(pointer) = state.seat.get_pointer() {
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        pointer.button(
            state,
            &ButtonEvent {
                button,
                state: btn_state,
                serial,
                time,
            },
        );
        pointer.frame(state);
    }
}

fn linux_button_code(name: &str) -> u32 {
    match name.to_ascii_lowercase().as_str() {
        "right" => 0x111,  // BTN_RIGHT (273)
        "middle" => 0x112, // BTN_MIDDLE (274)
        _ => 0x110,        // BTN_LEFT (272)
    }
}

fn synthesize_chord_tap(state: &mut KomaState, _ws_id: &WorkspaceId, chord: &str) {
    let parts: Vec<&str> = chord
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    for &part in &parts {
        synthesize_single_key_edge(state, part, KeyState::Pressed);
    }
    for &part in parts.iter().rev() {
        synthesize_single_key_edge(state, part, KeyState::Released);
    }
}

fn synthesize_single_key_edge(state: &mut KomaState, key_name: &str, edge: KeyState) {
    let Some(keycode) = linux_evdev_keycode_plus_8(key_name) else {
        let sym = xkb::keysym_from_name(key_name, xkb::KEYSYM_CASE_INSENSITIVE);
        if sym != Keysym::new(0) {
            info!(key = %key_name, ?edge, "resolved non-ASCII XKB keysym for ephemeral keymap tier");
        }
        return;
    };
    if let Some(keyboard) = state.seat.get_keyboard() {
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        let _ = keyboard.input::<(), _>(
            state,
            Keycode::new(keycode),
            edge,
            serial,
            time,
            |_, _, _| FilterResult::Forward,
        );
    }
}

/// Maps standard chord tokens (`Ctrl`, `Alt`, `Shift`, `Return`, `A..Z`, `0..9`, arrows) to XKB keycodes (`evdev + 8`).
fn linux_evdev_keycode_plus_8(name: &str) -> Option<u32> {
    let evdev: u32 = match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "ctrl_l" => 29,
        "shift" | "shift_l" => 42,
        "alt" | "alt_l" => 56,
        "super" | "mod" | "meta" => 125,
        "esc" | "escape" => 1,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        "tab" => 15,
        "q" => 16,
        "w" => 17,
        "e" => 18,
        "r" => 19,
        "t" => 20,
        "y" => 21,
        "u" => 22,
        "i" => 23,
        "o" => 24,
        "p" => 25,
        "return" | "enter" => 28,
        "a" => 30,
        "s" => 31,
        "d" => 32,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "z" => 44,
        "x" => 45,
        "c" => 46,
        "v" => 47,
        "b" => 48,
        "n" => 49,
        "m" => 50,
        "space" => 57,
        "home" => 102,
        "up" => 103,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        _ => return None,
    };
    Some(evdev + 8)
}
