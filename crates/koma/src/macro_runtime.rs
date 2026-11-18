//! Ring 0 `Phodopus` host-action executor, kinematic pointer stepper, window damage-settle
//! detector, and `calloop` one-shot timer trampoline (`ARCHITECTURE.md` §2.2, §5.2, §5.3).

use std::{process::Command, sync::Arc, time::Duration};

use calloop::{
    RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use koma_lua::{HostBridge, phodopus_bridge::QueuedHostAction, schema::LuaKinematicsInput};
use koma_model::{
    AnchorPoint, HandPathModulation, LogicalPoint, LogicalRect, PathSplineKind, PointerPathTrack,
    PointerTrajectorySpec, WindowMatchContext,
};
use koma_proto::{
    Epoch, ErrorSymbol, EventTopic, IpcError, TextInjectStrategy, WorkspaceEvent, WorkspaceId,
};
use regex::Regex;
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, InputTime, KeyState, Keycode},
    input::{
        keyboard::{FilterResult, Keysym, xkb},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::RegionAttributes,
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
        selection::data_device::set_data_device_selection,
    },
};
use tracing::{debug, info, warn};

use crate::{
    desktop_protocols::{clamp_local_point_to_constraint, surface_logical_bounds},
    state::{AgentCursorRuntime, KomaState},
};

const KINEMATIC_TICK_MS: u64 = 4; // 250 Hz smooth pointer interpolation step
const DEFAULT_SETTLE_QUIET_MS: u64 = 40;
const EVDEV_LEFT_SHIFT: u32 = 42;
const TERMINAL_APP_IDS: &[&str] = &[
    "foot",
    "footclient",
    "bitty",
    "run.bitty.Bitty",
    "alacritty",
    "kitty",
    "wezterm",
    "ghostty",
    "konsole",
    "gnome-terminal",
    "kgx",
    "console",
    "xterm",
    "urxvt",
    "xfce4-terminal",
    "tilix",
    "terminator",
    "rio",
    "contour",
    "cosmic-term",
    "blackbox",
];

/// Per-workspace surface damage tracker driving `window.settled` events and `WaitDamageSettle` wakes.
#[derive(Debug, Default)]
pub struct WorkspaceDamageTracker {
    pub last_commit_ms: u64,
    pub dirty_unsettled: bool,
    pub settle_timer_token: Option<RegistrationToken>,
}

/// In-flight smooth `vSeat` pointer trajectory (`minimum_jerk`, `bezier`, or 2D damped Hooke `spring`)
/// following a multi-waypoint spline (`ArcLengthSpline` + `PointerPathTrack`).
#[derive(Debug, Clone)]
pub struct ActivePointerAnimation {
    pub ws_id: WorkspaceId,
    pub expected_epoch: Epoch,
    pub track: PointerPathTrack,
    pub hold_end_ms: u64,
    pub release_at_ms: Option<u64>,
    pub release_button_on_finish: Option<String>,
    pub held_modifiers: Vec<String>,
}

/// Parameters controlling a queued single-target or multi-waypoint `vSeat` pointer trajectory.
#[derive(Debug, Clone, Default)]
pub(crate) struct PointerPathLaunchOpts {
    pub spline: PathSplineKind,
    pub kinematics: LuaKinematicsInput,
    pub hold_start_ms: u64,
    pub hold_end_ms: u64,
    pub press_button_at_start: Option<String>,
    pub release_button_on_finish: Option<String>,
    pub modifiers: Vec<String>,
    pub label: Option<String>,
    pub modulation: HandPathModulation,
}

/// Read-only `HostBridge` view over `KomaState` passed into `PhodopusSandbox::drive_ready_coroutines`.
struct StateEpochView<'a> {
    workspaces: &'a std::collections::HashMap<WorkspaceId, koma_model::WorkspaceState>,
    pointer_animations: &'a [ActivePointerAnimation],
}

impl HostBridge for StateEpochView<'_> {
    fn current_epoch(&self, ws_id: &WorkspaceId) -> Option<Epoch> {
        self.workspaces.get(ws_id).map(|ws| ws.lease.epoch())
    }

    fn pointer_trajectory_settled(&self, ws_id: &WorkspaceId) -> Option<bool> {
        Some(!self.pointer_animations.iter().any(|a| &a.ws_id == ws_id))
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
    let snap = state.capture_lua_desktop_snapshot(None);
    state.lua_sandbox.set_desktop_snapshot(snap);
    let report = {
        let view = StateEpochView {
            workspaces: &state.workspaces,
            pointer_animations: &state.pointer_animations,
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
    state.reconcile_session_lock();
    match action {
        QueuedHostAction::SpawnProcess { argv } => {
            if state.is_locked_or_abandoned() {
                warn!("blocked SpawnProcess action while session is locked");
                return;
            }
            if let Some((prog, args)) = argv.split_first() {
                let active_ws = state.active_ws_id.clone();
                let mut cmd = Command::new(prog);
                cmd.args(args);
                state.configure_child_env(&mut cmd, Some(&active_ws));
                let _ = crate::child_reaper::spawn_detached(&mut cmd, "macro.spawn");
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
            ensure_workspace_keyboard_focus(state, &ws_id);
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
            let norm = key.trim().to_ascii_lowercase();
            if !norm.is_empty() {
                ensure_workspace_keyboard_focus(state, &ws_id);
                if let Some(ws) = state.workspaces.get(&ws_id)
                    && let Some(ledger) = state.vseats.get_mut(&ws.vseat_id)
                {
                    ledger.record_key_down(&norm);
                }
                synthesize_single_key_edge(state, &norm, KeyState::Pressed);
            }
        }

        QueuedHostAction::KeyUp {
            ws_id,
            expected_epoch,
            key,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            let norm = key.trim().to_ascii_lowercase();
            if !norm.is_empty() {
                if let Some(ws) = state.workspaces.get(&ws_id)
                    && let Some(ledger) = state.vseats.get_mut(&ws.vseat_id)
                {
                    ledger.record_key_up(&norm);
                }
                synthesize_single_key_edge(state, &norm, KeyState::Released);
            }
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
            if let Err(err) =
                apply_move_pointer(state, ws_id, expected_epoch, target, &kinematics, None)
            {
                warn!("{err}");
            }
        }

        QueuedHostAction::MovePointerPath {
            ws_id,
            expected_epoch,
            waypoints,
            spline,
            kinematics,
        } => {
            if let Err(err) = queue_pointer_path_trajectory(
                state,
                ws_id,
                expected_epoch,
                waypoints,
                PointerPathLaunchOpts {
                    spline,
                    kinematics,
                    ..Default::default()
                },
            ) {
                warn!("{}", err.message);
            }
        }

        QueuedHostAction::SettleAgentCursor {
            ws_id,
            expected_epoch,
            immediate,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            settle_agent_cursor(state, &ws_id, immediate);
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
            apply_vseat_pointer_button(
                state,
                &ws_id,
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
            let _ = apply_vseat_click_with_modifiers(
                state,
                &ws_id,
                expected_epoch,
                target,
                &opts.button,
                opts.clicks,
                &[],
                None,
            );
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
            if let Some(pointer) = state.ptr() {
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
            strategy,
        } => {
            if !verify_action_epoch_and_protection(state, &ws_id, expected_epoch) {
                return;
            }
            if utf8.is_empty() {
                return;
            }
            match strategy {
                TextInjectStrategy::Clipboard => {
                    paste_utf8_via_clipboard(state, &ws_id, utf8);
                }
                TextInjectStrategy::XkbKeymap => {
                    if can_type_via_ascii_xkb(&utf8) {
                        synthesize_ascii_xkb_typing(state, &ws_id, &utf8);
                    } else {
                        paste_utf8_via_clipboard(state, &ws_id, utf8);
                    }
                }
                TextInjectStrategy::Auto | TextInjectStrategy::TextInputV3 => {
                    if try_commit_via_text_input_v3(state, &ws_id, &utf8) {
                        state.queue_redraw();
                    } else if can_type_via_ascii_xkb(&utf8) {
                        synthesize_ascii_xkb_typing(state, &ws_id, &utf8);
                    } else {
                        paste_utf8_via_clipboard(state, &ws_id, utf8);
                    }
                }
            }
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
            ensure_workspace_keyboard_focus(state, &ws_id);
            let mut mimes = vec![mime_type];
            if !mimes.iter().any(|m| m.starts_with("text/plain")) {
                mimes.push("text/plain;charset=utf-8".to_owned());
            }
            let buf: Arc<[u8]> = Arc::from(payload);
            set_data_device_selection(&state.display_handle, &state.seat, mimes, buf);
            let chord = paste_chord_for_workspace(state, &ws_id);
            synthesize_chord_tap(state, &ws_id, chord);
        }

        QueuedHostAction::DispatchAction {
            ws_id,
            expected_epoch,
            action,
        } => {
            if state.is_locked_or_abandoned() {
                warn!("blocked DispatchAction while session is locked");
                return;
            }
            let valid = state.workspaces.get(&ws_id).is_some_and(|ws| {
                ws.lease.epoch() == expected_epoch
                    && (!matches!(
                        ws.presentation,
                        koma_model::PresentationTarget::Offscreen { .. }
                    ) || !ws.lease.is_human_preempted())
            });
            if !valid {
                let _ = state.lua_sandbox.cancel_workspace_coroutines(
                    &ws_id,
                    "ERR_STALE_EPOCH: action dropped due to workspace epoch or takeover barrier",
                );
                return;
            }
            crate::keybind_dispatcher::execute_action_descriptor(state, action, false);
            let snap = state.capture_lua_desktop_snapshot(None);
            state.lua_sandbox.set_desktop_snapshot(snap);
        }
    }
}

/// Validates both the `Epoch` fence AND the Protected Surface Blackout Invariant before delivering
/// any macro action (`IPC_SPEC.md` §3.2 & §4.2), returning a structured `IpcError` on denial.
pub(crate) fn verify_action_epoch_and_protection_result(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    expected_epoch: Epoch,
) -> Result<(), IpcError> {
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        let _ = state.lua_sandbox.cancel_workspace_coroutines(
            ws_id,
            "ERR_SESSION_LOCKED: macro action blocked while session is locked",
        );
        return Err(IpcError::new(
            ErrorSymbol::ErrHumanPreempted,
            "macro action blocked while session is locked",
        ));
    }
    let Some(ws) = state.workspaces.get(ws_id) else {
        return Err(IpcError::new(ErrorSymbol::ErrNotFound, "unknown workspace"));
    };
    if ws.lease.is_human_preempted() {
        let cur_epoch = ws.lease.epoch();
        let _ = state.lua_sandbox.cancel_workspace_coroutines(
            ws_id,
            "ERR_STALE_EPOCH: action dropped due to workspace epoch or takeover barrier",
        );
        return Err(IpcError::human_preempted(cur_epoch));
    }
    if ws.lease.epoch() != expected_epoch {
        let cur_epoch = ws.lease.epoch();
        let _ = state.lua_sandbox.cancel_workspace_coroutines(
            ws_id,
            "ERR_STALE_EPOCH: action dropped due to workspace epoch or takeover barrier",
        );
        return Err(IpcError::stale_epoch(cur_epoch, expected_epoch));
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
            return Err(IpcError::new(
                ErrorSymbol::ErrProtectedSurface,
                "target surface is protected against synthetic input",
            ));
        }
    }

    Ok(())
}

/// Validates both the `Epoch` fence AND the Protected Surface Blackout Invariant before delivering
/// any macro action (`IPC_SPEC.md` §3.2 & §4.2).
fn verify_action_epoch_and_protection(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    expected_epoch: Epoch,
) -> bool {
    verify_action_epoch_and_protection_result(state, ws_id, expected_epoch).is_ok()
}

fn apply_move_pointer(
    state: &mut KomaState,
    ws_id: WorkspaceId,
    expected_epoch: Epoch,
    target: AnchorPoint,
    kinematics: &LuaKinematicsInput,
    release_button_on_finish: Option<String>,
) -> Result<(), String> {
    launch_pointer_path_inner(
        state,
        ws_id,
        expected_epoch,
        &[target],
        PointerPathLaunchOpts {
            spline: PathSplineKind::Auto,
            kinematics: kinematics.clone(),
            release_button_on_finish,
            ..Default::default()
        },
    )
    .map(|_| ())
}

fn launch_pointer_path_inner(
    state: &mut KomaState,
    ws_id: WorkspaceId,
    expected_epoch: Epoch,
    waypoints: &[AnchorPoint],
    mut opts: PointerPathLaunchOpts,
) -> Result<usize, String> {
    if waypoints.is_empty() {
        return Err("pointer trajectory requires at least one waypoint".to_owned());
    }
    let win_rect = target_window_rect(state, &ws_id);
    let mut resolved_pts = Vec::with_capacity(waypoints.len());
    for wp in waypoints {
        let pt = wp
            .resolve_in(win_rect)
            .map_err(|e| format!("invalid waypoint anchor for {ws_id}: {e}"))?;
        resolved_pts.push(pt);
    }

    if let Some(label_text) = opts.label.take() {
        set_agent_cursor_label(state, &ws_id, Some(label_text), None);
    }

    let anim_cfg = state.lua_sandbox.animations_snapshot();
    let spec = opts.kinematics.resolve_spec(&anim_cfg);
    let now_ms = state.elapsed_ms();
    let bounds = state
        .workspaces
        .get(&ws_id)
        .map(|ws| ws.viewport.full_rect());

    let (held_modifiers, mut prev_anim) = if let Some(btn) = &opts.press_button_at_start {
        finalize_workspace_pointer_animations(state, &ws_id);
        let held = press_vseat_modifiers(state, &ws_id, &opts.modifiers);
        apply_vseat_pointer_motion(
            state,
            &ws_id,
            resolved_pts[0],
            LogicalPoint { x: 0.0, y: 0.0 },
            None,
        );
        let btn_code = linux_button_code(btn);
        if let Some(ws) = state.workspaces.get(&ws_id)
            && let Some(ledger) = state.vseats.get_mut(&ws.vseat_id)
        {
            ledger.record_button_down(btn_code);
        }
        apply_vseat_pointer_button(state, &ws_id, btn_code, ButtonState::Pressed);
        (held, None)
    } else {
        if matches!(spec, PointerTrajectorySpec::Instant)
            && opts.hold_start_ms == 0
            && opts.hold_end_ms == 0
        {
            finalize_workspace_pointer_animations(state, &ws_id);
            let held = press_vseat_modifiers(state, &ws_id, &opts.modifiers);
            let last = *resolved_pts.last().expect("checked non-empty");
            apply_vseat_pointer_motion(state, &ws_id, last, LogicalPoint { x: 0.0, y: 0.0 }, None);
            if let Some(button) = opts.release_button_on_finish {
                release_pointer_button(state, &ws_id, &button);
            }
            release_vseat_modifiers(state, &ws_id, &held);
            return Ok(resolved_pts.len());
        }

        if resolved_pts.len() == 1
            && opts.hold_start_ms == 0
            && opts.hold_end_ms == 0
            && opts.modifiers.is_empty()
            && opts.modulation.is_zero()
            && let Some(existing) = state
                .pointer_animations
                .iter_mut()
                .find(|a| a.ws_id == ws_id)
        {
            existing.expected_epoch = expected_epoch;
            existing
                .track
                .retarget_waypoints(&resolved_pts, opts.spline, bounds, now_ms, spec);
            existing.hold_end_ms = 0;
            existing.release_at_ms = None;
            existing.release_button_on_finish = opts.release_button_on_finish;
            if existing.track.is_settled() {
                let release_btn = existing.release_button_on_finish.take();
                let held_mods = std::mem::take(&mut existing.held_modifiers);
                state.pointer_animations.retain(|a| a.ws_id != ws_id);
                apply_vseat_pointer_motion(
                    state,
                    &ws_id,
                    resolved_pts[0],
                    LogicalPoint { x: 0.0, y: 0.0 },
                    None,
                );
                if let Some(button) = release_btn {
                    release_pointer_button(state, &ws_id, &button);
                }
                release_vseat_modifiers(state, &ws_id, &held_mods);
            }
            return Ok(1);
        }

        let mut prev = state
            .pointer_animations
            .iter()
            .position(|a| a.ws_id == ws_id)
            .map(|idx| state.pointer_animations.swap_remove(idx));
        if let Some(prev_entry) = prev.as_mut() {
            if let Some(button) = prev_entry.release_button_on_finish.take() {
                release_pointer_button(state, &ws_id, &button);
            }
            let prev_mods = std::mem::take(&mut prev_entry.held_modifiers);
            release_vseat_modifiers(state, &ws_id, &prev_mods);
        }
        let held = press_vseat_modifiers(state, &ws_id, &opts.modifiers);
        (held, prev)
    };

    if matches!(spec, PointerTrajectorySpec::Instant)
        && opts.hold_start_ms == 0
        && opts.hold_end_ms == 0
    {
        let last = *resolved_pts.last().expect("checked non-empty");
        apply_vseat_pointer_motion(state, &ws_id, last, LogicalPoint { x: 0.0, y: 0.0 }, None);
        if let Some(button) = opts.release_button_on_finish {
            release_pointer_button(state, &ws_id, &button);
        }
        release_vseat_modifiers(state, &ws_id, &held_modifiers);
        return Ok(resolved_pts.len());
    }

    let current_ws_pos = state
        .agent_cursors
        .get(&ws_id)
        .map(|ac| ac.ws_local_pos)
        .unwrap_or(LogicalPoint { x: 0.0, y: 0.0 });

    let launch_pts: Vec<LogicalPoint> = if resolved_pts.len() == 1 {
        vec![current_ws_pos, resolved_pts[0]]
    } else {
        resolved_pts.clone()
    };

    let (init_vel, init_heading) = match (&opts.press_button_at_start, prev_anim.take()) {
        (None, Some(prev)) => {
            let prev_pos = prev.track.target_point();
            let p0 = launch_pts[0];
            if (prev_pos.x - p0.x).hypot(prev_pos.y - p0.y) <= 2.0 || resolved_pts.len() == 1 {
                (
                    prev.track.current_velocity(),
                    prev.track.current_heading_rad(),
                )
            } else {
                (LogicalPoint { x: 0.0, y: 0.0 }, None)
            }
        }
        _ => (LogicalPoint { x: 0.0, y: 0.0 }, None),
    };

    if resolved_pts.len() >= 2 && opts.press_button_at_start.is_none() {
        apply_vseat_pointer_motion(state, &ws_id, launch_pts[0], init_vel, init_heading);
    }

    let launch_ms = now_ms.saturating_add(opts.hold_start_ms);
    let track = PointerPathTrack::launch_waypoints(
        &launch_pts,
        opts.spline,
        init_vel,
        init_heading,
        bounds,
        launch_ms,
        spec,
    )
    .with_modulation(opts.modulation);

    if track.is_settled() && opts.hold_start_ms == 0 && opts.hold_end_ms == 0 {
        let last = track.target_point();
        apply_vseat_pointer_motion(state, &ws_id, last, LogicalPoint { x: 0.0, y: 0.0 }, None);
        if let Some(button) = opts.release_button_on_finish {
            release_pointer_button(state, &ws_id, &button);
        }
        release_vseat_modifiers(state, &ws_id, &held_modifiers);
    } else {
        let release_at_ms = if track.is_settled()
            && (opts.release_button_on_finish.is_some() || !held_modifiers.is_empty())
        {
            Some(launch_ms.saturating_add(opts.hold_end_ms))
        } else {
            None
        };
        if track.is_settled() {
            let last = track.target_point();
            apply_vseat_pointer_motion(state, &ws_id, last, LogicalPoint { x: 0.0, y: 0.0 }, None);
        }
        state.pointer_animations.push(ActivePointerAnimation {
            ws_id,
            expected_epoch,
            track,
            hold_end_ms: opts.hold_end_ms,
            release_at_ms,
            release_button_on_finish: opts.release_button_on_finish,
            held_modifiers,
        });
    }
    Ok(resolved_pts.len())
}

pub(crate) fn queue_pointer_trajectory(
    state: &mut KomaState,
    ws_id: WorkspaceId,
    expected_epoch: Epoch,
    target: AnchorPoint,
    kinematics: &LuaKinematicsInput,
    release_button_on_finish: Option<String>,
) -> Result<(), IpcError> {
    queue_pointer_path_trajectory(
        state,
        ws_id,
        expected_epoch,
        vec![target],
        PointerPathLaunchOpts {
            spline: PathSplineKind::Auto,
            kinematics: kinematics.clone(),
            release_button_on_finish,
            ..Default::default()
        },
    )
    .map(|_| ())
}

pub(crate) fn queue_pointer_path_trajectory(
    state: &mut KomaState,
    ws_id: WorkspaceId,
    expected_epoch: Epoch,
    waypoints: Vec<AnchorPoint>,
    opts: PointerPathLaunchOpts,
) -> Result<usize, IpcError> {
    verify_action_epoch_and_protection_result(state, &ws_id, expected_epoch)?;
    let count = launch_pointer_path_inner(state, ws_id, expected_epoch, &waypoints, opts)
        .map_err(|msg| IpcError::new(ErrorSymbol::ErrInvalidParams, msg))?;
    rearm_trampoline_timer(state);
    Ok(count)
}

fn finalize_workspace_pointer_animations(state: &mut KomaState, ws_id: &WorkspaceId) {
    let mut idx = 0;
    while idx < state.pointer_animations.len() {
        if &state.pointer_animations[idx].ws_id == ws_id {
            let anim = state.pointer_animations.swap_remove(idx);
            apply_vseat_pointer_motion(
                state,
                ws_id,
                anim.track.target_point(),
                LogicalPoint { x: 0.0, y: 0.0 },
                None,
            );
            if let Some(button) = anim.release_button_on_finish {
                release_pointer_button(state, ws_id, &button);
            }
            release_vseat_modifiers(state, ws_id, &anim.held_modifiers);
        } else {
            idx += 1;
        }
    }
}

pub(crate) fn settle_agent_cursor(state: &mut KomaState, ws_id: &WorkspaceId, immediate: bool) {
    let now_ms = state.elapsed_ms();
    if immediate {
        finalize_workspace_pointer_animations(state, ws_id);
    }
    if let Some(ac) = state.agent_cursors.get_mut(ws_id) {
        ac.visual.begin_settle(now_ms, immediate);
    }
    state.queue_redraw();
}

pub(crate) fn set_agent_cursor_label(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    label: Option<String>,
    caret_rect: Option<LogicalRect>,
) {
    let now_ms = state.elapsed_ms();
    let default_pos = state
        .agent_cursors
        .get(ws_id)
        .map(|ac| ac.ws_local_pos)
        .unwrap_or(LogicalPoint { x: 0.0, y: 0.0 });
    let ac = state
        .agent_cursors
        .entry(ws_id.clone())
        .or_insert_with(|| AgentCursorRuntime::new(default_pos));
    ac.visual.set_label(label, caret_rect, now_ms);
    state.queue_redraw();
}

fn release_pointer_button(state: &mut KomaState, ws_id: &WorkspaceId, button: &str) {
    let btn_code = linux_button_code(button);
    if let Some(ws) = state.workspaces.get(ws_id)
        && let Some(ledger) = state.vseats.get_mut(&ws.vseat_id)
    {
        ledger.record_button_up(btn_code);
    }
    apply_vseat_pointer_button(state, ws_id, btn_code, ButtonState::Released);
}

fn step_pointer_animations(state: &mut KomaState, now_ms: u64) {
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        let affected: Vec<WorkspaceId> = state
            .pointer_animations
            .drain(..)
            .map(|a| a.ws_id)
            .collect();
        for ws_id in affected {
            release_all_vseat_held_inputs(state, &ws_id);
        }
        return;
    }
    if state.pointer_animations.is_empty() {
        return;
    }

    let mut remaining = Vec::new();
    let mut motions: Vec<(WorkspaceId, LogicalPoint, LogicalPoint, Option<f64>)> = Vec::new();
    let mut finished_releases: Vec<(WorkspaceId, String)> = Vec::new();
    let mut finished_modifier_releases: Vec<(WorkspaceId, Vec<String>)> = Vec::new();
    let mut dropped_workspaces: Vec<WorkspaceId> = Vec::new();

    for mut anim in state.pointer_animations.drain(..) {
        let epoch_valid = state.workspaces.get(&anim.ws_id).is_some_and(|ws| {
            !ws.lease.is_human_preempted() && ws.lease.epoch() == anim.expected_epoch
        });
        if !epoch_valid {
            dropped_workspaces.push(anim.ws_id);
            continue;
        }

        if let Some(rel_ms) = anim.release_at_ms {
            if now_ms >= rel_ms {
                if let Some(btn) = anim.release_button_on_finish.take() {
                    finished_releases.push((anim.ws_id.clone(), btn));
                }
                if !anim.held_modifiers.is_empty() {
                    finished_modifier_releases
                        .push((anim.ws_id.clone(), std::mem::take(&mut anim.held_modifiers)));
                }
            } else {
                remaining.push(anim);
            }
            continue;
        }

        let pt = anim.track.step(now_ms);
        let vel = anim.track.current_velocity();
        let heading = anim.track.current_heading_rad();
        motions.push((anim.ws_id.clone(), pt, vel, heading));

        if anim.track.is_settled() || now_ms >= anim.track.max_finish_at_ms() {
            if anim.release_button_on_finish.is_some() || !anim.held_modifiers.is_empty() {
                if anim.hold_end_ms == 0 {
                    if let Some(btn) = anim.release_button_on_finish.take() {
                        finished_releases.push((anim.ws_id.clone(), btn));
                    }
                    if !anim.held_modifiers.is_empty() {
                        finished_modifier_releases
                            .push((anim.ws_id.clone(), std::mem::take(&mut anim.held_modifiers)));
                    }
                } else {
                    anim.release_at_ms = Some(now_ms.saturating_add(anim.hold_end_ms));
                    remaining.push(anim);
                }
            }
        } else {
            remaining.push(anim);
        }
    }

    state.pointer_animations = remaining;
    for ws_id in dropped_workspaces {
        release_all_vseat_held_inputs(state, &ws_id);
    }
    for (ws_id, pt, vel, heading) in motions {
        apply_vseat_pointer_motion(state, &ws_id, pt, vel, heading);
    }
    for (ws_id, btn) in finished_releases {
        release_pointer_button(state, &ws_id, &btn);
    }
    for (ws_id, mods) in finished_modifier_releases {
        release_vseat_modifiers(state, &ws_id, &mods);
    }
}

/// Arms a single precision `calloop::timer::Timer` matching the earliest of:
/// - `PhodopusSandbox::next_wakeup_deadline_ms()` (`SleepTimer`, `PointerTrajectory`, `WaitDamageSettle`)
/// - `now_ms + KINEMATIC_TICK_MS` (`4 ms`) while smooth pointer animations are in flight.
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

fn try_commit_via_text_input_v3(state: &KomaState, ws_id: &WorkspaceId, utf8: &str) -> bool {
    use smithay::reexports::wayland_server::Resource as _;

    let Some(target_wl) = active_window_in_ws(state, ws_id)
        .and_then(|m| m.window.toplevel().map(|t| t.wl_surface().clone()))
    else {
        return false;
    };

    let Some(text_input) = state.text_input() else {
        return false;
    };
    let mut committed = false;
    {
        // The closure runs with the text-input mutex held: any nested text-input acquisition
        // inside it would self-deadlock, so the scope is marked for its whole duration.
        let _held = crate::seat_guard::text_input_callback();
        text_input.with_active_text_input(|ti, surface| {
            if !committed {
                let mut root = surface.clone();
                while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
                    root = parent;
                }
                if root.id() == target_wl.id() {
                    ti.commit_string(Some(utf8.to_owned()));
                    committed = true;
                }
            }
        });
    }
    if committed {
        text_input.done(false);
    }
    committed
}

pub(crate) fn target_window_rect(state: &KomaState, ws_id: &WorkspaceId) -> LogicalRect {
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

fn update_agent_cursor_target(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    pt: LogicalPoint,
    vel: LogicalPoint,
    heading_rad: Option<f64>,
    now_ms: u64,
) {
    let ws_local = Point::from((pt.x, pt.y));
    let hit = state.surface_under_in_workspace(ws_id, ws_local);
    let anchored_window = hit.as_ref().and_then(|(win_id, _, _)| {
        let rect = state.workspaces.get(ws_id).and_then(|ws| {
            ws.compute_visible_geometries_at(now_ms)
                .into_iter()
                .find(|(id, _, _)| *id == *win_id)
                .map(|(_, r, _)| r)
        })?;
        let u = (pt.x - f64::from(rect.x)) / f64::from(rect.w.max(1));
        let v = (pt.y - f64::from(rect.y)) / f64::from(rect.h.max(1));
        Some((*win_id, u, v))
    });

    let ac = state
        .agent_cursors
        .entry(ws_id.clone())
        .or_insert_with(|| AgentCursorRuntime::new(pt));
    ac.ws_local_pos = pt;
    ac.anchored_window = anchored_window;
    ac.visual.set_target(pt, vel, heading_rad, now_ms);
}

fn dispatch_vseat_pointer_motion_inner(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    candidate_pt: LogicalPoint,
    delta: Point<f64, Logical>,
    delta_unaccel: Point<f64, Logical>,
    vel: LogicalPoint,
    heading_rad: Option<f64>,
) {
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        state.break_active_pointer_constraints();
        return;
    }
    let now_ms = state.elapsed_ms();
    let out_origin = state.workspace_output_origin(ws_id);
    let serial = SERIAL_COUNTER.next_serial();
    let time = InputTime::now();
    let rel_event = RelativeMotionEvent {
        delta,
        delta_unaccel,
        time,
    };

    if let Some(pointer) = state.ptr()
        && !pointer.is_grabbed()
        && let Some(focused) = pointer.current_focus()
    {
        enum MotionConstraintMode {
            Locked,
            Confined(Option<RegionAttributes>),
            None,
        }
        let mode = with_pointer_constraint(&focused, &pointer, |c| match c.as_deref() {
            Some(p @ PointerConstraint::Locked(_)) if p.is_active() => MotionConstraintMode::Locked,
            Some(p @ PointerConstraint::Confined(conf)) if p.is_active() => {
                MotionConstraintMode::Confined(conf.region().cloned())
            }
            _ => MotionConstraintMode::None,
        });

        match mode {
            MotionConstraintMode::Locked => {
                let origin = state.surface_global_origin(&focused).unwrap_or(out_origin);
                pointer.relative_motion(state, Some((focused, origin)), &rel_event);
                pointer.frame(state);
                state.queue_redraw();
                return;
            }
            MotionConstraintMode::Confined(region) => {
                if let Some(origin) = state.surface_global_origin(&focused) {
                    let candidate_global =
                        Point::from((candidate_pt.x, candidate_pt.y)) + out_origin;
                    let bounds = surface_logical_bounds(&focused)
                        .unwrap_or_else(|| Rectangle::from_size((1, 1).into()));
                    let clamped_local = clamp_local_point_to_constraint(
                        bounds,
                        candidate_global - origin,
                        region.as_ref(),
                    );
                    let clamped_global = origin + clamped_local;
                    let clamped_ws = clamped_global - out_origin;
                    let clamped_pt = LogicalPoint {
                        x: clamped_ws.x,
                        y: clamped_ws.y,
                    };
                    update_agent_cursor_target(state, ws_id, clamped_pt, vel, heading_rad, now_ms);
                    pointer.motion(
                        state,
                        Some((focused.clone(), origin)),
                        &MotionEvent {
                            location: clamped_global,
                            serial,
                            time,
                        },
                    );
                    pointer.relative_motion(state, Some((focused, origin)), &rel_event);
                    pointer.frame(state);
                    state.queue_redraw();
                    return;
                }
            }
            MotionConstraintMode::None => {}
        }
    }

    update_agent_cursor_target(state, ws_id, candidate_pt, vel, heading_rad, now_ms);
    let ws_local = Point::from((candidate_pt.x, candidate_pt.y));
    let delivery_pos = ws_local + out_origin;
    let under = state
        .surface_under_in_workspace(ws_id, ws_local)
        .map(|(_, surf, origin)| (surf, origin));
    if let Some(pointer) = state.ptr() {
        pointer.motion(
            state,
            under.clone(),
            &MotionEvent {
                location: delivery_pos,
                serial,
                time,
            },
        );
        pointer.relative_motion(state, under, &rel_event);
        pointer.frame(state);
    }
    state.maybe_activate_pointer_constraint_at(delivery_pos);
    state.queue_redraw();
}

fn apply_vseat_pointer_motion(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    pt: LogicalPoint,
    vel: LogicalPoint,
    heading_rad: Option<f64>,
) {
    let prev_ws_pos = state
        .agent_cursors
        .get(ws_id)
        .map_or(pt, |ac| ac.ws_local_pos);
    let delta = Point::from((pt.x - prev_ws_pos.x, pt.y - prev_ws_pos.y));
    dispatch_vseat_pointer_motion_inner(state, ws_id, pt, delta, delta, vel, heading_rad);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_vseat_relative_move(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    expected_epoch: Epoch,
    dx: f64,
    dy: f64,
    dx_unaccel: Option<f64>,
    dy_unaccel: Option<f64>,
    modifiers: &[String],
) -> Result<LogicalPoint, IpcError> {
    verify_action_epoch_and_protection_result(state, ws_id, expected_epoch)?;
    if !dx.is_finite()
        || !dy.is_finite()
        || !dx_unaccel.is_none_or(f64::is_finite)
        || !dy_unaccel.is_none_or(f64::is_finite)
    {
        return Err(IpcError::new(
            ErrorSymbol::ErrInvalidParams,
            "relative_move deltas must be finite",
        ));
    }

    finalize_workspace_pointer_animations(state, ws_id);
    let prev = state
        .agent_cursors
        .get(ws_id)
        .map_or(LogicalPoint { x: 0.0, y: 0.0 }, |ac| ac.ws_local_pos);
    let mut candidate = LogicalPoint {
        x: prev.x + dx,
        y: prev.y + dy,
    };
    if let Some(ws) = state.workspaces.get(ws_id) {
        let vp = ws.viewport.full_rect();
        let min_x = f64::from(vp.x);
        let min_y = f64::from(vp.y);
        let max_x = f64::from(vp.x + vp.w.saturating_sub(1) as i32).max(min_x);
        let max_y = f64::from(vp.y + vp.h.saturating_sub(1) as i32).max(min_y);
        candidate.x = candidate.x.clamp(min_x, max_x);
        candidate.y = candidate.y.clamp(min_y, max_y);
    }

    let delta = Point::from((dx, dy));
    let delta_unaccel = Point::from((dx_unaccel.unwrap_or(dx), dy_unaccel.unwrap_or(dy)));
    let held = press_vseat_modifiers(state, ws_id, modifiers);
    dispatch_vseat_pointer_motion_inner(
        state,
        ws_id,
        candidate,
        delta,
        delta_unaccel,
        LogicalPoint { x: 0.0, y: 0.0 },
        None,
    );
    release_vseat_modifiers(state, ws_id, &held);
    Ok(state
        .agent_cursors
        .get(ws_id)
        .map_or(candidate, |ac| ac.ws_local_pos))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_vseat_wheel(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    expected_epoch: Epoch,
    target: Option<AnchorPoint>,
    dx: f64,
    dy: f64,
    discrete_x: i32,
    discrete_y: i32,
    modifiers: &[String],
) -> Result<(), IpcError> {
    verify_action_epoch_and_protection_result(state, ws_id, expected_epoch)?;
    if !dx.is_finite() || !dy.is_finite() {
        return Err(IpcError::new(
            ErrorSymbol::ErrInvalidParams,
            "wheel deltas must be finite",
        ));
    }

    if let Some(anchor) = target {
        let win_rect = target_window_rect(state, ws_id);
        let dst_pt = anchor.resolve_in(win_rect).map_err(|e| {
            IpcError::new(
                ErrorSymbol::ErrInvalidParams,
                format!("invalid wheel anchor for {ws_id}: {e}"),
            )
        })?;
        finalize_workspace_pointer_animations(state, ws_id);
        apply_vseat_pointer_motion(state, ws_id, dst_pt, LogicalPoint { x: 0.0, y: 0.0 }, None);
    }

    let held = press_vseat_modifiers(state, ws_id, modifiers);
    if let Some(pointer) = state.ptr() {
        let mut frame = AxisFrame::new(InputTime::now()).source(AxisSource::Wheel);
        let mut has_axis = false;
        if discrete_x != 0 || dx.abs() > f64::EPSILON {
            let val = if dx.abs() > f64::EPSILON {
                dx
            } else {
                f64::from(discrete_x) * 15.0
            };
            let v120 = if discrete_x != 0 {
                discrete_x.saturating_mul(120)
            } else {
                ((dx / 15.0) * 120.0).round() as i32
            };
            frame = frame
                .value(Axis::Horizontal, val)
                .v120(Axis::Horizontal, v120);
            has_axis = true;
        }
        if discrete_y != 0 || dy.abs() > f64::EPSILON {
            let val = if dy.abs() > f64::EPSILON {
                dy
            } else {
                f64::from(discrete_y) * 15.0
            };
            let v120 = if discrete_y != 0 {
                discrete_y.saturating_mul(120)
            } else {
                ((dy / 15.0) * 120.0).round() as i32
            };
            frame = frame.value(Axis::Vertical, val).v120(Axis::Vertical, v120);
            has_axis = true;
        }
        if has_axis {
            pointer.axis(state, frame);
            pointer.frame(state);
        }
    }
    release_vseat_modifiers(state, ws_id, &held);
    state.queue_redraw();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_vseat_click_with_modifiers(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    expected_epoch: Epoch,
    target: AnchorPoint,
    button: &str,
    clicks: u8,
    modifiers: &[String],
    label: Option<String>,
) -> Result<(), IpcError> {
    verify_action_epoch_and_protection_result(state, ws_id, expected_epoch)?;
    let win_rect = target_window_rect(state, ws_id);
    let dst_pt = target.resolve_in(win_rect).map_err(|e| {
        IpcError::new(
            ErrorSymbol::ErrInvalidParams,
            format!("invalid click anchor for {ws_id}: {e}"),
        )
    })?;
    finalize_workspace_pointer_animations(state, ws_id);
    if let Some(label_text) = label {
        set_agent_cursor_label(state, ws_id, Some(label_text), None);
    }
    let held = press_vseat_modifiers(state, ws_id, modifiers);
    apply_vseat_pointer_motion(state, ws_id, dst_pt, LogicalPoint { x: 0.0, y: 0.0 }, None);
    let btn_code = linux_button_code(button);
    for _ in 0..clicks.max(1) {
        apply_vseat_pointer_button(state, ws_id, btn_code, ButtonState::Pressed);
        apply_vseat_pointer_button(state, ws_id, btn_code, ButtonState::Released);
    }
    release_vseat_modifiers(state, ws_id, &held);
    Ok(())
}

pub(crate) fn press_vseat_modifiers(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    modifiers: &[String],
) -> Vec<String> {
    let mut newly_pressed = Vec::new();
    if modifiers.is_empty() {
        return newly_pressed;
    }
    let Some(vseat_id) = state.workspaces.get(ws_id).map(|ws| ws.vseat_id.clone()) else {
        return newly_pressed;
    };
    for raw in modifiers {
        let norm = raw.trim().to_ascii_lowercase();
        if norm.is_empty() {
            continue;
        }
        if let Some(ledger) = state.vseats.get_mut(&vseat_id)
            && !ledger.is_key_held(&norm)
        {
            ledger.record_key_down(&norm);
            newly_pressed.push(norm);
        }
    }
    if !newly_pressed.is_empty() {
        ensure_workspace_keyboard_focus(state, ws_id);
        for key in &newly_pressed {
            synthesize_single_key_edge(state, key, KeyState::Pressed);
        }
    }
    newly_pressed
}

pub(crate) fn release_vseat_modifiers(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    modifiers: &[String],
) {
    if modifiers.is_empty() {
        return;
    }
    let Some(vseat_id) = state.workspaces.get(ws_id).map(|ws| ws.vseat_id.clone()) else {
        return;
    };
    for raw in modifiers.iter().rev() {
        let norm = raw.trim().to_ascii_lowercase();
        if norm.is_empty() {
            continue;
        }
        let was_held = state.vseats.get_mut(&vseat_id).is_some_and(|ledger| {
            let held = ledger.is_key_held(&norm);
            if held {
                ledger.record_key_up(&norm);
            }
            held
        });
        if was_held {
            synthesize_single_key_edge(state, &norm, KeyState::Released);
        }
    }
}

pub(crate) fn release_all_vseat_held_inputs(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
) -> Vec<String> {
    let Some(vseat_id) = state.workspaces.get(ws_id).map(|ws| ws.vseat_id.clone()) else {
        return Vec::new();
    };
    let Some((held_keys, held_buttons)) =
        state.vseats.get_mut(&vseat_id).map(|v| v.flush_all_held())
    else {
        return Vec::new();
    };
    for btn_code in held_buttons {
        apply_vseat_pointer_button(state, ws_id, btn_code, ButtonState::Released);
    }
    for key in held_keys.iter().rev() {
        synthesize_single_key_edge(state, key, KeyState::Released);
    }
    held_keys
}

pub(crate) fn release_all_vseat_held_inputs_everywhere(state: &mut KomaState) {
    let ws_ids: Vec<WorkspaceId> = state.workspaces.keys().cloned().collect();
    for ws_id in ws_ids {
        let _ = release_all_vseat_held_inputs(state, &ws_id);
    }
}

fn apply_vseat_pointer_button(
    state: &mut KomaState,
    ws_id: &WorkspaceId,
    button: u32,
    btn_state: ButtonState,
) {
    state.reconcile_session_lock();
    if btn_state == ButtonState::Pressed && state.is_locked_or_abandoned() {
        return;
    }
    let now_ms = state.elapsed_ms();
    let ac_cfg = state.agent_cursor_config();
    let ws_local_pt = state
        .agent_cursors
        .get(ws_id)
        .map(|ac| ac.ws_local_pos)
        .unwrap_or(LogicalPoint { x: 0.0, y: 0.0 });
    let ws_local = Point::from((ws_local_pt.x, ws_local_pt.y));

    if btn_state == ButtonState::Pressed {
        if let Some((win_id, _, _)) = state.surface_under_in_workspace(ws_id, ws_local) {
            if let Some(ws) = state.workspaces.get_mut(ws_id) {
                ws.activate_window(win_id, now_ms);
            }
            state.sync_workspace_geometries(ws_id);
            if *ws_id == state.effective_interactive_ws_id() {
                state.focus_active_deck_window();
            }
        }
        let ac = state
            .agent_cursors
            .entry(ws_id.clone())
            .or_insert_with(|| AgentCursorRuntime::new(ws_local_pt));
        ac.visual.trigger_click(ws_local_pt, now_ms, &ac_cfg);
    }

    if let Some(pointer) = state.ptr() {
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
    state.queue_redraw();
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

/// Returns true when `app_id` identifies a Wayland terminal emulator requiring `Ctrl+Shift+V` paste.
fn is_terminal_app_id(app_id: &str) -> bool {
    let normalized = app_id.trim().to_ascii_lowercase();
    TERMINAL_APP_IDS.iter().any(|id| normalized == *id) || normalized.contains("terminal")
}

/// Selects the workspace paste shortcut (`Ctrl+Shift+V` for terminals, `Ctrl+V` otherwise).
fn paste_chord_for_workspace(state: &KomaState, ws_id: &WorkspaceId) -> &'static str {
    if active_window_in_ws(state, ws_id).is_some_and(|m| is_terminal_app_id(&m.app_id)) {
        "Ctrl+Shift+V"
    } else {
        "Ctrl+V"
    }
}

/// Restores keyboard focus to the active window when targeting the interactive workspace without focus.
fn ensure_workspace_keyboard_focus(state: &mut KomaState, ws_id: &WorkspaceId) {
    let needs_focus = state.kb().is_some_and(|kb| kb.current_focus().is_none());
    if needs_focus && *ws_id == state.effective_interactive_ws_id() {
        state.focus_active_deck_window();
    }
}

/// Maps printable US QWERTY ASCII characters, newline/CR, and tab to `(evdev_code, needs_shift)`.
fn ascii_char_to_evdev(ch: char) -> Option<(u32, bool)> {
    let needs_shift = matches!(
        ch,
        'A'..='Z'
            | '!'
            | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '_'
            | '+'
            | '{'
            | '}'
            | ':'
            | '"'
            | '~'
            | '|'
            | '<'
            | '>'
            | '?'
    );
    let evdev = match ch.to_ascii_lowercase() {
        '1' | '!' => 2,
        '2' | '@' => 3,
        '3' | '#' => 4,
        '4' | '$' => 5,
        '5' | '%' => 6,
        '6' | '^' => 7,
        '7' | '&' => 8,
        '8' | '*' => 9,
        '9' | '(' => 10,
        '0' | ')' => 11,
        '-' | '_' => 12,
        '=' | '+' => 13,
        '\t' => 15,
        'q' => 16,
        'w' => 17,
        'e' => 18,
        'r' => 19,
        't' => 20,
        'y' => 21,
        'u' => 22,
        'i' => 23,
        'o' => 24,
        'p' => 25,
        '[' | '{' => 26,
        ']' | '}' => 27,
        '\n' | '\r' => 28,
        'a' => 30,
        's' => 31,
        'd' => 32,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        ';' | ':' => 39,
        '\'' | '"' => 40,
        '`' | '~' => 41,
        '\\' | '|' => 43,
        'z' => 44,
        'x' => 45,
        'c' => 46,
        'v' => 47,
        'b' => 48,
        'n' => 49,
        'm' => 50,
        ',' | '<' => 51,
        '.' | '>' => 52,
        '/' | '?' => 53,
        ' ' => 57,
        _ => return None,
    };
    Some((evdev, needs_shift))
}

/// Returns true when `utf8` is non-empty and every character can be typed synchronously via XKB keycodes.
fn can_type_via_ascii_xkb(utf8: &str) -> bool {
    !utf8.is_empty() && utf8.chars().all(|c| ascii_char_to_evdev(c).is_some())
}

fn synthesize_xkb_keycode_edge(state: &mut KomaState, xkb_keycode: u32, edge: KeyState) {
    if let Some(keyboard) = state.kb() {
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        let _ = keyboard.input::<(), _>(
            state,
            Keycode::new(xkb_keycode),
            edge,
            serial,
            time,
            |_, _, _| FilterResult::Forward,
        );
    }
}

fn synthesize_ascii_xkb_typing(state: &mut KomaState, ws_id: &WorkspaceId, utf8: &str) {
    ensure_workspace_keyboard_focus(state, ws_id);
    for ch in utf8.chars() {
        let Some((evdev, needs_shift)) = ascii_char_to_evdev(ch) else {
            continue;
        };
        if needs_shift {
            synthesize_xkb_keycode_edge(state, EVDEV_LEFT_SHIFT + 8, KeyState::Pressed);
        }
        synthesize_xkb_keycode_edge(state, evdev + 8, KeyState::Pressed);
        synthesize_xkb_keycode_edge(state, evdev + 8, KeyState::Released);
        if needs_shift {
            synthesize_xkb_keycode_edge(state, EVDEV_LEFT_SHIFT + 8, KeyState::Released);
        }
    }
}

fn paste_utf8_via_clipboard(state: &mut KomaState, ws_id: &WorkspaceId, utf8: String) {
    ensure_workspace_keyboard_focus(state, ws_id);
    let mimes = vec![
        "text/plain;charset=utf-8".to_owned(),
        "UTF8_STRING".to_owned(),
    ];
    let payload: Arc<[u8]> = Arc::from(utf8.into_bytes());
    set_data_device_selection(&state.display_handle, &state.seat, mimes, payload);
    let chord = paste_chord_for_workspace(state, ws_id);
    synthesize_chord_tap(state, ws_id, chord);
}

fn synthesize_single_key_edge(state: &mut KomaState, key_name: &str, edge: KeyState) {
    let Some(keycode) = linux_evdev_keycode_plus_8(key_name) else {
        let sym = xkb::keysym_from_name(key_name, xkb::KEYSYM_CASE_INSENSITIVE);
        if sym != Keysym::new(0) {
            info!(key = %key_name, ?edge, "resolved non-ASCII XKB keysym for ephemeral keymap tier");
        }
        return;
    };
    synthesize_xkb_keycode_edge(state, keycode, edge);
}

/// Maps standard chord tokens (`Ctrl`, `Alt`, `Shift`, `Return`, `A..Z`, `0..9`, punctuation, F1..F12, arrows) to XKB keycodes (`evdev + 8`).
fn linux_evdev_keycode_plus_8(name: &str) -> Option<u32> {
    let evdev: u32 = match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "ctrl_l" | "control_l" => 29,
        "ctrl_r" | "control_r" => 97,
        "shift" | "shift_l" => 42,
        "shift_r" => 54,
        "alt" | "alt_l" => 56,
        "alt_r" => 100,
        "super" | "super_l" | "mod" | "meta" | "meta_l" | "win" => 125,
        "super_r" | "meta_r" => 126,
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
        "minus" | "-" => 12,
        "equal" | "=" => 13,
        "backspace" => 14,
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
        "leftbrace" | "bracketleft" | "[" => 26,
        "rightbrace" | "bracketright" | "]" => 27,
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
        "semicolon" | ";" => 39,
        "apostrophe" | "'" => 40,
        "grave" | "`" => 41,
        "backslash" | "\\" => 43,
        "z" => 44,
        "x" => 45,
        "c" => 46,
        "v" => 47,
        "b" => 48,
        "n" => 49,
        "m" => 50,
        "comma" | "," => 51,
        "dot" | "period" | "." => 52,
        "slash" | "/" => 53,
        "space" => 57,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        "home" => 102,
        "up" => 103,
        "pageup" | "page_up" | "prior" => 104,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        "pagedown" | "page_down" | "next" => 109,
        "insert" => 110,
        "delete" => 111,
        _ => return None,
    };
    Some(evdev + 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_char_to_evdev_covers_all_printable_ascii_and_control_whitespace() {
        for byte in 0x20u8..=0x7Eu8 {
            let ch = byte as char;
            assert!(
                ascii_char_to_evdev(ch).is_some(),
                "missing evdev mapping for ASCII printable char {ch:?} (0x{byte:02x})"
            );
        }
        assert!(can_type_via_ascii_xkb(
            "echo JIKU_E2E_OK > /tmp/jiku-proof.txt\n\t"
        ));
        assert!(!can_type_via_ascii_xkb(""));
        assert!(!can_type_via_ascii_xkb("hello 世界"));
        assert_eq!(ascii_char_to_evdev('>'), Some((52, true)));
        assert_eq!(ascii_char_to_evdev('/'), Some((53, false)));
        assert_eq!(ascii_char_to_evdev('-'), Some((12, false)));
        assert_eq!(ascii_char_to_evdev('_'), Some((12, true)));
        assert_eq!(ascii_char_to_evdev('J'), Some((36, true)));
    }

    #[test]
    fn terminal_app_id_detection_is_case_insensitive() {
        assert!(is_terminal_app_id("foot"));
        assert!(is_terminal_app_id("Foot"));
        assert!(is_terminal_app_id("org.gnome.Terminal"));
        assert!(is_terminal_app_id("Alacritty"));
        assert!(!is_terminal_app_id("firefox"));
        assert!(!is_terminal_app_id("code"));
    }

    #[test]
    fn kinematic_tick_runs_at_250hz_and_modifier_aliases_resolve() {
        assert_eq!(KINEMATIC_TICK_MS, 4);
        for token in [
            "ctrl", "ctrl_r", "shift", "shift_r", "alt", "alt_r", "super", "super_r",
        ] {
            assert!(
                linux_evdev_keycode_plus_8(token).is_some(),
                "missing evdev mapping for modifier token {token}"
            );
        }
    }
}
