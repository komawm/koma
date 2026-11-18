//! Repeat-while-held state machine and timer for `BindRepeat::WhileHeld` binds (`komawm/koma#76`).
//!
//! Koma's binding layer never sees key-repeat events: the input backends deliver one
//! `KeyState::Pressed` per physical key, and Smithay only advertises
//! `wl_keyboard.repeat_info` to clients (it does not re-run the compositor key filter for
//! repeats). The double-dispatch risk is therefore not a native repeat event reaching the
//! binding layer, but a `pass_through` bind whose focused client repeats the forwarded key
//! from that same `repeat_info`. Such binds are never armed here — the client owns their
//! repeat — so exactly one layer dispatches every held chord.
//!
//! The state machine itself ([`KeyRepeatState`]) is pure: it takes `now_ms` and returns
//! dispatch decisions, so delay → rate → cancel is unit-tested without a seat. The calloop
//! timer only translates those decisions into one-shot wake-ups.

use std::time::Duration;

use koma_lua::{ActionDescriptor, BindHandleId};
use tracing::debug;

use crate::{
    keybind_dispatcher::{execute_action_descriptor, is_keybind_allowed_for_lock_state},
    state::KomaState,
};

/// Repeat timing derived from the existing `koma.input.keyboard.repeat_delay` /
/// `repeat_rate` knobs (600 ms / 25 Hz by default, matching Hyprland), so a held `j` in a
/// text field and a held `Mod + J` feel the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepeatTiming {
    /// Delay between the initial press and the first repeat (`ms`).
    pub delay_ms: u64,
    /// Interval between repeats (`ms`), the reciprocal of the configured rate.
    pub interval_ms: u64,
}

impl Default for RepeatTiming {
    fn default() -> Self {
        Self {
            delay_ms: koma_lua::DEFAULT_KEYBOARD_REPEAT_DELAY_MS as u64,
            interval_ms: 1_000 / koma_lua::DEFAULT_KEYBOARD_REPEAT_RATE_HZ as u64,
        }
    }
}

impl RepeatTiming {
    /// Builds a timing pair from a delay in milliseconds and a rate in Hertz.
    #[must_use]
    pub fn new(delay_ms: u64, rate_hz: u64) -> Self {
        Self {
            delay_ms: delay_ms.max(1),
            interval_ms: 1_000 / rate_hz.clamp(1, 1_000),
        }
    }

    /// Builds a timing pair from the effective input settings (`delay_ms`, `rate_hz`).
    #[must_use]
    pub fn from_input_settings(delay_ms: i32, rate_hz: i32) -> Self {
        Self::new(
            u64::try_from(delay_ms.max(1)).unwrap_or(1),
            u64::try_from(rate_hz.max(1)).unwrap_or(1),
        )
    }
}

/// Everything needed to start repeating one held chord.
#[derive(Debug, Clone, PartialEq)]
pub struct RepeatRequest {
    pub bind_id: BindHandleId,
    pub keycode: u32,
    pub mods: u8,
    pub action: ActionDescriptor,
    pub allow_when_locked: bool,
}

/// The repeat that just became due.
#[derive(Debug, Clone, PartialEq)]
pub struct RepeatFire {
    pub bind_id: BindHandleId,
    pub action: ActionDescriptor,
    pub allow_when_locked: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct ActiveRepeat {
    request: RepeatRequest,
    next_fire_ms: u64,
}

/// Pure repeat state machine: one active chord, one deadline.
#[derive(Debug, Clone, Default)]
pub struct KeyRepeatState {
    timing: RepeatTiming,
    active: Option<ActiveRepeat>,
    /// Nesting depth of repeat dispatches. While non-zero, focus/workspace changes are
    /// caused by the repeated action itself and must not cancel it.
    dispatch_depth: u32,
}

impl KeyRepeatState {
    /// Arms repeat for `request`, first firing `delay_ms` from `now_ms`.
    ///
    /// Replaces any chord that was still repeating.
    pub fn arm(&mut self, request: RepeatRequest, now_ms: u64) -> bool {
        let next_fire_ms = now_ms.saturating_add(self.timing.delay_ms);
        self.active = Some(ActiveRepeat {
            request,
            next_fire_ms,
        });
        true
    }

    /// Stops repeating and returns the chord that was active, if any.
    pub fn cancel(&mut self) -> Option<RepeatRequest> {
        self.active.take().map(|active| active.request)
    }

    /// Cancels because focus, workspace, grab, lock or configuration changed outside the
    /// repeated action. Repeat dispatch itself is exempt so a held focus/workspace action
    /// keeps running, and a nested dispatch cannot cancel its own parent.
    pub fn cancel_for_external_change(&mut self) -> Option<RepeatRequest> {
        if self.dispatch_depth > 0 {
            return None;
        }
        self.cancel()
    }

    /// Marks the start of a repeat dispatch (see [`Self::cancel_for_external_change`]).
    pub fn begin_dispatch(&mut self) {
        self.dispatch_depth = self.dispatch_depth.saturating_add(1);
    }

    /// Marks the end of a repeat dispatch.
    pub fn end_dispatch(&mut self) {
        self.dispatch_depth = self.dispatch_depth.saturating_sub(1);
    }

    /// `true` while a repeat dispatch is executing.
    #[must_use]
    #[allow(dead_code)] // State-machine introspection for unit tests and diagnostics.
    pub fn is_dispatching(&self) -> bool {
        self.dispatch_depth > 0
    }

    /// The chord currently repeating, if any.
    #[must_use]
    #[allow(dead_code)] // State-machine introspection for unit tests and diagnostics.
    pub fn active(&self) -> Option<&RepeatRequest> {
        self.active.as_ref().map(|active| &active.request)
    }

    /// `true` while a chord repeats.
    #[must_use]
    #[allow(dead_code)] // State-machine introspection for unit tests and diagnostics.
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// `true` when `keycode` is the key whose chord is repeating.
    #[must_use]
    pub fn is_repeating_keycode(&self, keycode: u32) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.request.keycode == keycode)
    }

    /// Cancels when the held modifier mask no longer matches the repeating chord.
    pub fn cancel_if_modifiers_changed(&mut self, held_mask: u8) -> Option<RepeatRequest> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.request.mods != held_mask)
        {
            return self.cancel();
        }
        None
    }

    /// The repeat timing in force.
    #[must_use]
    pub fn timing(&self) -> RepeatTiming {
        self.timing
    }

    /// Replaces the repeat timing, effective from the next arming.
    pub fn set_timing(&mut self, timing: RepeatTiming) {
        self.timing = timing;
    }

    /// Absolute deadline of the next repeat, if one is armed.
    #[must_use]
    pub fn next_deadline_ms(&self) -> Option<u64> {
        self.active.as_ref().map(|active| active.next_fire_ms)
    }

    /// Milliseconds until the next repeat, clamped to at least 1 so a due repeat still
    /// yields a positive timer duration.
    #[must_use]
    pub fn timer_delay_ms(&self, now_ms: u64) -> Option<u64> {
        self.next_deadline_ms()
            .map(|deadline| deadline.saturating_sub(now_ms).max(1))
    }

    /// Returns the due repeat and reschedules the next one one interval after `now_ms`.
    ///
    /// A late wake-up dispatches exactly once — never a catch-up burst — and re-phases the
    /// timer to the current time so the configured rate is preserved.
    pub fn take_due(&mut self, now_ms: u64) -> Option<RepeatFire> {
        let interval_ms = self.timing.interval_ms;
        let active = self.active.as_mut()?;
        if now_ms < active.next_fire_ms {
            return None;
        }
        active.next_fire_ms = now_ms.saturating_add(interval_ms);
        Some(RepeatFire {
            bind_id: active.request.bind_id,
            action: active.request.action.clone(),
            allow_when_locked: active.request.allow_when_locked,
        })
    }
}

/// Arms the single one-shot repeat timer for the earliest repeat deadline.
pub fn arm_keybind_repeat_timer(state: &mut KomaState, is_bare_metal_tty: bool, delay_ms: u64) {
    let Some(loop_handle) = state.loop_handle.clone() else {
        return;
    };
    if let Some(previous) = state.keybind_repeat_timer_token.take() {
        loop_handle.remove(previous);
    }
    let timer = calloop::timer::Timer::from_duration(Duration::from_millis(delay_ms.max(1)));
    match loop_handle.insert_source(timer, move |_, _, state| {
        state.keybind_repeat_timer_token = None;
        drive_keybind_repeat(state, is_bare_metal_tty);
        calloop::timer::TimeoutAction::Drop
    }) {
        Ok(token) => state.keybind_repeat_timer_token = Some(token),
        Err(err) => debug!(%err, "failed arming keybind repeat timer"),
    }
}

/// Timer wake-up: dispatch at most one due repeat and arm the next deadline.
pub fn drive_keybind_repeat(state: &mut KomaState, is_bare_metal_tty: bool) {
    let now_ms = state.elapsed_ms();
    let Some(fire) = state.keybinds.repeat.take_due(now_ms) else {
        // An early wake-up (or a replaced repeat) must not stall the chain: if a repeat is
        // still armed, wait out its remaining delay instead of dropping the timer.
        if let Some(delay) = state.keybinds.repeat.timer_delay_ms(now_ms) {
            arm_keybind_repeat_timer(state, is_bare_metal_tty, delay);
        }
        return;
    };
    if !is_keybind_allowed_for_lock_state(state.is_locked_or_abandoned(), fire.allow_when_locked) {
        state.cancel_keybind_repeat("lock_state");
        return;
    }

    debug!(
        bind_id = ?fire.bind_id,
        "dispatching repeated keybind action"
    );
    state.keybinds.repeat.begin_dispatch();
    execute_action_descriptor(state, fire.action, is_bare_metal_tty);
    state.keybinds.repeat.end_dispatch();

    if let Some(delay) = state.keybinds.repeat.timer_delay_ms(state.elapsed_ms()) {
        arm_keybind_repeat_timer(state, is_bare_metal_tty, delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(keycode: u32, mods: u8, action: ActionDescriptor) -> RepeatRequest {
        RepeatRequest {
            bind_id: BindHandleId(7),
            keycode,
            mods,
            action,
            allow_when_locked: false,
        }
    }

    /// Simulated 1 ms timeline: press at time zero, fire every due repeat, release at
    /// `hold_ms`. Returns the absolute timestamps of every repeat dispatch.
    fn simulate_hold(mut state: KeyRepeatState, hold_ms: u64) -> Vec<u64> {
        let mut fired = Vec::new();
        // `hold_ms` is the release instant, so a repeat due exactly then never fires.
        for now_ms in 1..hold_ms {
            if let Some(_fire) = state.take_due(now_ms) {
                fired.push(now_ms);
            }
        }
        state.cancel();
        assert!(state.take_due(hold_ms + 1_000).is_none());
        fired
    }

    #[test]
    fn repeat_timer_delays_then_fires_at_the_configured_rate() {
        let mut state = KeyRepeatState::default();
        state.set_timing(RepeatTiming::new(600, 25));
        assert_eq!(state.timing().interval_ms, 40);
        assert!(state.arm(
            request(13, 1, ActionDescriptor::FocusDirection("left".to_owned())),
            0
        ));

        // Nothing fires before the delay elapses.
        assert_eq!(state.take_due(599), None);
        assert_eq!(state.next_deadline_ms(), Some(600));

        // First repeat at 600 ms, then every 40 ms (25 Hz).
        let first = state
            .take_due(600)
            .expect("first repeat is due at the delay");
        assert_eq!(first.bind_id, BindHandleId(7));
        assert_eq!(
            first.action,
            ActionDescriptor::FocusDirection("left".to_owned())
        );
        assert_eq!(state.next_deadline_ms(), Some(640));
        assert_eq!(state.take_due(639), None);
        assert!(state.take_due(640).is_some());
        assert_eq!(state.next_deadline_ms(), Some(680));

        // A late wake-up dispatches once and re-phases instead of bursting.
        assert!(state.take_due(700).is_some());
        assert_eq!(state.next_deadline_ms(), Some(740));
        assert_eq!(state.take_due(701), None);
    }

    #[test]
    fn a_known_hold_produces_the_expected_number_of_repeats() {
        let mut state = KeyRepeatState::default();
        state.set_timing(RepeatTiming::new(600, 25));
        state.arm(
            request(
                17,
                1 | 2,
                ActionDescriptor::WindowMoveDirection("left".to_owned()),
            ),
            0,
        );
        let fired = simulate_hold(state, 1_000);
        assert_eq!(fired, (0..10).map(|i| 600 + i * 40).collect::<Vec<_>>());
        assert_eq!(fired.len(), 10, "1000 ms hold = 600 ms delay + 10 ticks");
    }

    #[test]
    fn release_cancels_so_no_action_fires_after_it() {
        let mut state = KeyRepeatState::default();
        state.set_timing(RepeatTiming::new(600, 25));
        state.arm(
            request(14, 1, ActionDescriptor::FocusDirection("down".to_owned())),
            0,
        );
        assert!(state.take_due(600).is_some());
        assert_eq!(state.next_deadline_ms(), Some(640));

        let cancelled = state.cancel().expect("the repeat was armed");
        assert_eq!(cancelled.keycode, 14);
        assert!(!state.is_active());
        assert_eq!(state.take_due(640), None);
        assert_eq!(state.timer_delay_ms(640), None);
    }

    #[test]
    fn modifier_change_and_external_state_changes_cancel_the_pending_repeat() {
        // Modifier change.
        let mut state = KeyRepeatState::default();
        state.set_timing(RepeatTiming::new(600, 25));
        state.arm(
            request(15, 1, ActionDescriptor::FocusDirection("up".to_owned())),
            0,
        );
        assert!(state.cancel_if_modifiers_changed(1).is_none());
        assert!(state.is_active());
        assert!(state.cancel_if_modifiers_changed(1 | 2).is_some());
        assert!(!state.is_active());

        // Lock, pointer grab and external workspace switch all route through the same
        // external-change cancel, so none of them can leave a repeat pending.
        for _trigger in ["session_lock", "pointer_grab", "workspace_change"] {
            let mut state = KeyRepeatState::default();
            state.set_timing(RepeatTiming::new(600, 25));
            state.arm(
                request(16, 1, ActionDescriptor::FocusDirection("right".to_owned())),
                0,
            );
            assert!(state.cancel_for_external_change().is_some());
            assert_eq!(state.take_due(600), None);
        }
    }

    #[test]
    fn action_induced_focus_changes_do_not_cancel_the_repeat_that_caused_them() {
        let mut state = KeyRepeatState::default();
        state.set_timing(RepeatTiming::new(600, 25));
        state.arm(
            request(13, 1, ActionDescriptor::FocusDirection("left".to_owned())),
            0,
        );
        let fire = state.take_due(600).expect("repeat due");
        state.begin_dispatch();
        // `focus.left()` moves focus and switches workspace internally; those callbacks hit
        // the external-change cancel while the repeat is dispatching.
        assert!(state.cancel_for_external_change().is_none());
        state.end_dispatch();
        assert!(state.is_active());
        assert_eq!(fire.bind_id, BindHandleId(7));
        assert_eq!(state.next_deadline_ms(), Some(640));
    }

    #[test]
    fn arming_a_new_chord_replaces_the_previous_one() {
        let mut state = KeyRepeatState::default();
        state.set_timing(RepeatTiming::new(200, 50));
        state.arm(
            request(13, 1, ActionDescriptor::FocusDirection("left".to_owned())),
            0,
        );
        state.arm(
            request(16, 1, ActionDescriptor::FocusDirection("right".to_owned())),
            100,
        );
        assert_eq!(state.next_deadline_ms(), Some(300));
        assert!(state.is_repeating_keycode(16));
        assert!(!state.is_repeating_keycode(13));
    }

    #[test]
    fn repeat_timing_comes_from_the_existing_input_settings() {
        let default = RepeatTiming::from_input_settings(
            koma_lua::DEFAULT_KEYBOARD_REPEAT_DELAY_MS,
            koma_lua::DEFAULT_KEYBOARD_REPEAT_RATE_HZ,
        );
        assert_eq!(default.delay_ms, 600);
        assert_eq!(default.interval_ms, 40);

        let custom = RepeatTiming::from_input_settings(180, 50);
        assert_eq!(custom.delay_ms, 180);
        assert_eq!(custom.interval_ms, 20);
    }
}
