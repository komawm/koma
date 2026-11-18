//! Dynamic XKB keybind, pointer button (`MouseLeft` / `MouseRight` / `MouseMiddle`), and
//! pointer-scroll compiler (`WheelScroll*` / `TouchpadScroll*`), priority `modified_sym -> raw_syms`
//! matcher, Niri `ScrollTracker` discrete tick accumulator, submap `koma.mode` state machine, and
//! Ring 0 `ActionDescriptor` / `Phodopus` coroutine executor
//! (modeled directly on `niri/src/input/{mod.rs, scroll_tracker.rs}` & `docs/LUA_API_SPEC.md` §3).

use std::{
    collections::{HashMap, HashSet},
    process::Command,
};

use koma_lua::{
    ActionDescriptor, BindHandleId, BindOptions, BindRepeat, BindSpec, schema::TriggerEdge,
};
use koma_model::{CardinalDir, LogicalPoint, PresentationTarget, WorkspaceState};
use koma_proto::WorkspaceId;
use smithay::{
    backend::input::{
        Axis, AxisSource, ButtonState, Event as _, InputBackend, InputTime, KeyState,
        PointerAxisEvent as _,
    },
    input::{
        keyboard::{FilterResult, KeyboardSource, Keycode, Keysym, ModifiersState, keysyms, xkb},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::SERIAL_COUNTER,
};
use tracing::{debug, info, warn};

use crate::{
    backend_winit::toggle_ring0_preempt_takeover,
    keybind_repeat::{KeyRepeatState, RepeatRequest, arm_keybind_repeat_timer},
    macro_runtime::{execute_queued_host_actions, rearm_trampoline_timer},
    pointer_grabs::{
        BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE, cancel_active_pointer_grab,
        start_compositor_move_grab, start_compositor_resize_grab, start_frame_edge_resize_grab,
    },
    state::KomaState,
};

pub(crate) const MOD_CTRL: u8 = 1 << 0;
pub(crate) const MOD_SHIFT: u8 = 1 << 1;
pub(crate) const MOD_ALT: u8 = 1 << 2;
pub(crate) const MOD_LOGO: u8 = 1 << 3;
pub(crate) const MOD_ISO_LEVEL3: u8 = 1 << 4;
pub(crate) const MOD_COMPOSITOR: u8 = 1 << 5; // Abstract `Mod` (`Super` on TTY, `Alt` on Winit, or `input.mod_key`)

/// Standard Wayland `wl_pointer.axis_value120` discrete wheel notch (`niri/src/niri.rs`: `ScrollTracker::new(120)`).
pub const WHEEL_V120_TICK: f64 = 120.0;
/// Logical-pixel threshold per discrete `TouchpadScroll*` tick (`niri/src/niri.rs`: `ScrollTracker::new(10)`).
pub const TOUCHPAD_SCROLL_TICK: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalModKey {
    Super,
    Alt,
    Ctrl,
    Shift,
    IsoLevel3,
}

impl PhysicalModKey {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "super" | "win" | "logo" | "meta" | "mod4" => Ok(Self::Super),
            "alt" | "mod1" => Ok(Self::Alt),
            "ctrl" | "control" => Ok(Self::Ctrl),
            "shift" => Ok(Self::Shift),
            "mod5" | "altgr" | "iso_level3_shift" => Ok(Self::IsoLevel3),
            other => Err(format!(
                "invalid `input.mod_key` `{other}` (expected Super, Alt, Ctrl, Shift, or Mod5)"
            )),
        }
    }

    #[must_use]
    pub const fn bit(self) -> u8 {
        match self {
            Self::Ctrl => MOD_CTRL,
            Self::Shift => MOD_SHIFT,
            Self::Alt => MOD_ALT,
            Self::Super => MOD_LOGO,
            Self::IsoLevel3 => MOD_ISO_LEVEL3,
        }
    }
}

/// Pointer button triggers (`Mod + MouseLeft`, `Mod + MouseRight`, Hyprland `mouse:272` / `mouse:273`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButtonTrigger {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl MouseButtonTrigger {
    #[must_use]
    pub fn try_parse(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "mouseleft" | "mouse_left" | "mouse1" | "lmb" | "btn_left" | "mouse:272" => {
                Some(Self::Left)
            }
            "mouseright" | "mouse_right" | "mouse2" | "rmb" | "btn_right" | "mouse:273" => {
                Some(Self::Right)
            }
            "mousemiddle" | "mouse_middle" | "mouse3" | "mmb" | "btn_middle" | "mouse:274" => {
                Some(Self::Middle)
            }
            "mouseback" | "mouse_back" | "mouse4" | "btn_side" | "mouse:275" => Some(Self::Back),
            "mouseforward" | "mouse_forward" | "mouse5" | "btn_extra" | "mouse:276" => {
                Some(Self::Forward)
            }
            _ => None,
        }
    }

    #[must_use]
    pub const fn from_button_code(code: u32) -> Option<Self> {
        match code {
            BTN_LEFT => Some(Self::Left),
            BTN_RIGHT => Some(Self::Right),
            BTN_MIDDLE => Some(Self::Middle),
            BTN_SIDE => Some(Self::Back),
            BTN_EXTRA => Some(Self::Forward),
            _ => None,
        }
    }

    #[must_use]
    #[allow(dead_code)] // Public evdev button code accessor retained for mouse-bind introspection.
    pub const fn button_code(self) -> u32 {
        match self {
            Self::Left => BTN_LEFT,
            Self::Right => BTN_RIGHT,
            Self::Middle => BTN_MIDDLE,
            Self::Back => BTN_SIDE,
            Self::Forward => BTN_EXTRA,
        }
    }
}

/// Pointer wheel & two-finger touchpad scroll triggers (`niri_config::Trigger` parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollTriggerKind {
    WheelScrollUp,
    WheelScrollDown,
    WheelScrollLeft,
    WheelScrollRight,
    TouchpadScrollUp,
    TouchpadScrollDown,
    TouchpadScrollLeft,
    TouchpadScrollRight,
}

impl ScrollTriggerKind {
    pub fn try_parse(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "wheelscrollup" | "wheel_scroll_up" | "mousewheelup" => Some(Self::WheelScrollUp),
            "wheelscrolldown" | "wheel_scroll_down" | "mousewheeldown" => {
                Some(Self::WheelScrollDown)
            }
            "wheelscrollleft" | "wheel_scroll_left" | "mousewheelleft" => {
                Some(Self::WheelScrollLeft)
            }
            "wheelscrollright" | "wheel_scroll_right" | "mousewheelright" => {
                Some(Self::WheelScrollRight)
            }
            "touchpadscrollup" | "touchpad_scroll_up" => Some(Self::TouchpadScrollUp),
            "touchpadscrolldown" | "touchpad_scroll_down" => Some(Self::TouchpadScrollDown),
            "touchpadscrollleft" | "touchpad_scroll_left" => Some(Self::TouchpadScrollLeft),
            "touchpadscrollright" | "touchpad_scroll_right" => Some(Self::TouchpadScrollRight),
            _ => None,
        }
    }
}

/// Unified chord trigger supporting XKB keyboard keysyms, pointer buttons, and pointer scroll ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindTrigger {
    Keysym {
        sym: Keysym,
        /// True if `sym` itself implies `Shift` (e.g. `@`, `?`, `+`), allowing `Mod + Plus`
        /// or `Mod + Shift + Equal` to match cleanly (`niri` shifted-key rule).
        is_pre_shifted: bool,
    },
    MouseButton(MouseButtonTrigger),
    Scroll(ScrollTriggerKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompiledChord {
    pub mods: u8,
    pub trigger: BindTrigger,
}

/// Discrete scroll tick accumulator ported directly from `niri/src/input/scroll_tracker.rs` (lines 1-39).
#[derive(Debug, Clone, Copy)]
pub struct ScrollTracker {
    tick: f64,
    last: f64,
    acc: f64,
}

impl ScrollTracker {
    #[must_use]
    pub const fn new(tick: f64) -> Self {
        Self {
            tick,
            last: 0.0,
            acc: 0.0,
        }
    }

    /// Accumulates `amount` and returns the signed count of discrete ticks crossed (`> 0` for
    /// down/right, `< 0` for up/left), resetting fractional state on direction sign reversal.
    pub fn accumulate(&mut self, amount: f64) -> i8 {
        if amount == 0.0 || !amount.is_finite() {
            return 0;
        }

        // Reset accumulator if scroll direction reversed (`niri/src/input/scroll_tracker.rs` line 18)
        if (self.last > 0.0 && amount < 0.0) || (self.last < 0.0 && amount > 0.0) {
            self.acc = 0.0;
        }

        self.acc += amount;

        let rv = if self.acc.abs() >= self.tick {
            let max_ticks = 20.0 * self.tick;
            let clamped = self.acc.clamp(-max_ticks, max_ticks);
            let ticks = (clamped / self.tick).trunc() as i8;
            self.acc %= self.tick;
            ticks
        } else {
            0
        };

        self.last = amount;
        rv
    }

    pub fn reset(&mut self) {
        self.last = 0.0;
        self.acc = 0.0;
    }
}

#[derive(Debug, Clone)]
pub struct CompiledBind {
    pub id: BindHandleId,
    pub mode: Option<String>,
    pub enabled: bool,
    // Original chord string retained for diagnostics/introspection.
    #[allow(dead_code)]
    pub raw_chord: String,
    pub chord: CompiledChord,
    pub press_action: Option<ActionDescriptor>,
    pub release_action: Option<ActionDescriptor>,
    pub options: BindOptions,
    /// Resolved repeat class: explicit `repeat` option, else derived from the action.
    pub repeat: BindRepeat,
}

impl CompiledBind {
    /// Repeat request for a `WhileHeld` press of this chord on `keycode` with `mods` held.
    ///
    /// `None` for `Once` binds, for binds without a press action, and for `pass_through`
    /// binds: those forward the key to the focused client, which repeats it from the
    /// `wl_keyboard.repeat_info` Koma advertises, so a compositor repeat would double-dispatch.
    #[must_use]
    pub fn repeat_request(&self, keycode: u32, mods: u8) -> Option<RepeatRequest> {
        if !self.repeat.repeats() || self.options.pass_through {
            return None;
        }
        let action = self.press_action.clone()?;
        Some(RepeatRequest {
            bind_id: self.id,
            keycode,
            mods,
            action,
            allow_when_locked: self.options.allow_when_locked,
        })
    }
}

#[derive(Debug, Clone)]
pub struct PendingRelease {
    pub action: ActionDescriptor,
    pub allow_when_locked: bool,
}

/// Pure lock-screen filter: blocks non-`allow_when_locked` key/mouse/scroll binds while
/// `Locked` or `Abandoned`, allowing only explicitly opted-in lock-screen binds.
#[must_use]
pub const fn is_keybind_allowed_for_lock_state(
    is_locked_or_abandoned: bool,
    allow_when_locked: bool,
) -> bool {
    !is_locked_or_abandoned || allow_when_locked
}

/// Recognizes hardware VT switch keysyms (`XF86Switch_VT_1..=12`, e.g. `Ctrl+Alt+F1..F12`),
/// which remain enabled even on locked TTY sessions.
#[must_use]
pub const fn parse_vt_switch_keysym(raw_sym: u32) -> Option<i32> {
    #[allow(non_upper_case_globals)]
    match raw_sym {
        keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12 => {
            Some((raw_sym - keysyms::KEY_XF86Switch_VT_1 + 1) as i32)
        }
        _ => None,
    }
}

/// Runtime keybind, mouse-button & scroll-bind table stored on `KomaState` and atomically swapped on hot-reload.
#[derive(Debug)]
pub struct KeybindTable {
    pub binds: Vec<CompiledBind>,
    pub mod_key_override: Option<PhysicalModKey>,
    pub active_mode: Option<String>,
    pub cooldowns_ms: HashMap<BindHandleId, u64>,
    pub pending_releases: HashMap<u32, PendingRelease>,
    pub pending_mouse_releases: HashMap<u32, PendingRelease>,
    pub suppressed_buttons: HashSet<u32>,
    /// Repeat-while-held state shared by every keyboard bind (`komawm/koma#76`).
    pub repeat: KeyRepeatState,
    pub horizontal_wheel_tracker: ScrollTracker,
    pub vertical_wheel_tracker: ScrollTracker,
    pub horizontal_finger_tracker: ScrollTracker,
    pub vertical_finger_tracker: ScrollTracker,
}

impl Default for KeybindTable {
    fn default() -> Self {
        Self::with_builtin_defaults()
    }
}

impl KeybindTable {
    /// Starter fallback keyboard, `Mod + MouseLeft/Right`, and `Mod + WheelScroll*` binds active
    /// when `~/.config/koma/init.lua` does not exist yet or defines zero binds.
    #[must_use]
    pub fn with_builtin_defaults() -> Self {
        let defaults: &[(u64, &str, ActionDescriptor, Option<u64>)] = &[
            (
                1,
                "Mod + Return",
                ActionDescriptor::Spawn {
                    argv: vec!["bitty".to_owned()],
                },
                None,
            ),
            (2, "Mod + Q", ActionDescriptor::WindowClose, None),
            (3, "Mod + V", ActionDescriptor::WindowFloatToggle, None),
            (4, "Mod + F", ActionDescriptor::WindowFullscreenToggle, None),
            (5, "Mod + P", ActionDescriptor::WindowPseudotileToggle, None),
            (6, "Mod + E", ActionDescriptor::DwindleToggleSplit, None),
            (
                28,
                "Mod + R",
                ActionDescriptor::ScrollingSwitchPresetWidth { forward: true },
                None,
            ),
            (
                29,
                "Mod + Ctrl + R",
                ActionDescriptor::ScrollingSwitchPresetWidth { forward: false },
                None,
            ),
            (30, "Mod + C", ActionDescriptor::ScrollingCenterColumn, None),
            (31, "Mod + M", ActionDescriptor::WindowMaximizeToggle, None),
            (
                36,
                "Mod + Shift + M",
                ActionDescriptor::ScrollingMaximizeColumn,
                None,
            ),
            (7, "Mod + S", ActionDescriptor::ScratchpadToggle(None), None),
            (
                12,
                "Mod + Shift + S",
                ActionDescriptor::ScratchpadMoveWindow {
                    name: None,
                    silent: false,
                },
                None,
            ),
            (
                13,
                "Mod + H",
                ActionDescriptor::FocusDirection("left".to_owned()),
                None,
            ),
            (
                14,
                "Mod + J",
                ActionDescriptor::FocusDirection("down".to_owned()),
                None,
            ),
            (
                15,
                "Mod + K",
                ActionDescriptor::FocusDirection("up".to_owned()),
                None,
            ),
            (
                16,
                "Mod + L",
                ActionDescriptor::FocusDirection("right".to_owned()),
                None,
            ),
            (
                17,
                "Mod + Shift + H",
                ActionDescriptor::WindowMoveDirection("left".to_owned()),
                None,
            ),
            (
                18,
                "Mod + Shift + J",
                ActionDescriptor::WindowMoveDirection("down".to_owned()),
                None,
            ),
            (
                19,
                "Mod + Shift + K",
                ActionDescriptor::WindowMoveDirection("up".to_owned()),
                None,
            ),
            (
                20,
                "Mod + Shift + L",
                ActionDescriptor::WindowMoveDirection("right".to_owned()),
                None,
            ),
            (
                21,
                "Mod + BracketLeft",
                ActionDescriptor::ScrollingConsumeOrExpelLeft,
                None,
            ),
            (
                22,
                "Mod + BracketRight",
                ActionDescriptor::ScrollingConsumeOrExpelRight,
                None,
            ),
            (
                32,
                "Mod + Comma",
                ActionDescriptor::ScrollingConsumeIntoColumn,
                None,
            ),
            (
                33,
                "Mod + Period",
                ActionDescriptor::ScrollingExpelFromColumn,
                None,
            ),
            (
                34,
                "Mod + Ctrl + Shift + R",
                ActionDescriptor::ScrollingSwitchPresetHeight { forward: true },
                None,
            ),
            (
                35,
                "Mod + W",
                ActionDescriptor::ScrollingToggleColumnTabbed,
                None,
            ),
            (23, "Mod + Tab", ActionDescriptor::OverviewToggle, None),
            (24, "Mod + Shift + R", ActionDescriptor::ReloadConfig, None),
            (
                25,
                "Mod + Shift + E",
                ActionDescriptor::CompositorQuit,
                None,
            ),
            (
                26,
                "Mod + MouseLeft",
                ActionDescriptor::WindowDragMove,
                None,
            ),
            (
                27,
                "Mod + MouseRight",
                ActionDescriptor::WindowDragResize,
                None,
            ),
            (
                8,
                "Mod + WheelScrollDown",
                ActionDescriptor::WorkspaceFocusNext,
                Some(150),
            ),
            (
                9,
                "Mod + WheelScrollUp",
                ActionDescriptor::WorkspaceFocusPrev,
                Some(150),
            ),
            (
                10,
                "Mod + WheelScrollRight",
                ActionDescriptor::DeckCycleNext,
                Some(150),
            ),
            (
                11,
                "Mod + WheelScrollLeft",
                ActionDescriptor::DeckCyclePrev,
                Some(150),
            ),
        ];

        let mut binds = Vec::with_capacity(defaults.len() + 18);
        for (id_num, chord_str, act, cooldown_ms) in defaults {
            if let Ok(chord) = parse_chord_spec(chord_str) {
                let options = BindOptions {
                    cooldown_ms: *cooldown_ms,
                    ..BindOptions::default()
                };
                binds.push(CompiledBind {
                    id: BindHandleId(*id_num),
                    mode: None,
                    enabled: true,
                    raw_chord: (*chord_str).to_owned(),
                    chord,
                    press_action: Some(act.clone()),
                    release_action: None,
                    repeat: options.effective_repeat(act),
                    options,
                });
            }
        }

        // Hyprland default workspace focus (`Mod + 1..=9`) & window move (`Mod + Shift + 1..=9`)
        for idx in 1_u64..=9 {
            let ws_name = idx.to_string();
            let focus_chord_str = format!("Mod + {ws_name}");
            if let Ok(chord) = parse_chord_spec(&focus_chord_str) {
                binds.push(CompiledBind {
                    id: BindHandleId(100 + idx),
                    mode: None,
                    enabled: true,
                    raw_chord: focus_chord_str,
                    chord,
                    press_action: Some(ActionDescriptor::WorkspaceFocus(ws_name.clone())),
                    release_action: None,
                    repeat: BindRepeat::Once,
                    options: BindOptions::default(),
                });
            }
            let move_chord_str = format!("Mod + Shift + {ws_name}");
            if let Ok(chord) = parse_chord_spec(&move_chord_str) {
                binds.push(CompiledBind {
                    id: BindHandleId(110 + idx),
                    mode: None,
                    enabled: true,
                    raw_chord: move_chord_str,
                    chord,
                    press_action: Some(ActionDescriptor::WindowMoveToWorkspace {
                        workspace: ws_name,
                        follow: true,
                    }),
                    release_action: None,
                    repeat: BindRepeat::Once,
                    options: BindOptions::default(),
                });
            }
        }

        Self {
            binds,
            mod_key_override: None,
            active_mode: None,
            cooldowns_ms: HashMap::new(),
            pending_releases: HashMap::new(),
            pending_mouse_releases: HashMap::new(),
            suppressed_buttons: HashSet::new(),
            repeat: KeyRepeatState::default(),
            horizontal_wheel_tracker: ScrollTracker::new(WHEEL_V120_TICK),
            vertical_wheel_tracker: ScrollTracker::new(WHEEL_V120_TICK),
            horizontal_finger_tracker: ScrollTracker::new(TOUCHPAD_SCROLL_TICK),
            vertical_finger_tracker: ScrollTracker::new(TOUCHPAD_SCROLL_TICK),
        }
    }

    /// Validates and pre-compiles all `BindSpec` chords (XKB keysyms + `MouseLeft/Right` + `WheelScroll*` / `TouchpadScroll*`),
    /// inheriting the complete Hyprland default keymap with zero unbinds.
    #[allow(dead_code)]
    pub fn compile(specs: &[BindSpec], mod_key_cfg: Option<&str>) -> Result<Self, String> {
        Self::compile_with_options(specs, &[], true, mod_key_cfg)
    }

    /// Validates and compiles `specs` on top of optional `with_builtin_defaults()`, applying
    /// `unbinds` removal and in-place chord overrides.
    pub fn compile_with_options(
        specs: &[BindSpec],
        unbinds: &[String],
        inherit_default_binds: bool,
        mod_key_cfg: Option<&str>,
    ) -> Result<Self, String> {
        let mod_key_override = match mod_key_cfg.filter(|s| !s.trim().is_empty()) {
            Some(s) if s.trim().eq_ignore_ascii_case("auto") => None,
            None => None,
            Some(explicit) => Some(PhysicalModKey::parse(explicit)?),
        };

        let mut parsed_unbinds = Vec::with_capacity(unbinds.len());
        for raw_u in unbinds {
            let chord = parse_chord_spec(raw_u).map_err(|e| format!("in unbind `{raw_u}`: {e}"))?;
            parsed_unbinds.push(chord);
        }

        let mut binds = if inherit_default_binds {
            Self::with_builtin_defaults().binds
        } else {
            Vec::with_capacity(specs.len() + 2)
        };

        if !parsed_unbinds.is_empty() {
            binds.retain(|b| {
                if b.mode.is_some() {
                    return true;
                }
                !parsed_unbinds
                    .iter()
                    .any(|&u| chords_match_under_mod_override(b.chord, u, mod_key_override))
            });
        }

        for spec in specs {
            let chord = parse_chord_spec(&spec.chord)
                .map_err(|e| format!("in bind `{}`: {e}", spec.chord))?;

            if matches!(chord.trigger, BindTrigger::Scroll(_))
                && spec.options.edge != TriggerEdge::Press
            {
                return Err(format!(
                    "in bind `{}`: pointer scroll triggers only support `edge = \"press\"`",
                    spec.chord
                ));
            }

            let (press_action, release_action) = match spec.options.edge {
                TriggerEdge::Press => (Some(spec.action.clone()), spec.release_action.clone()),
                TriggerEdge::Release => (
                    None,
                    spec.release_action
                        .clone()
                        .or_else(|| Some(spec.action.clone())),
                ),
                TriggerEdge::Both => (
                    Some(spec.action.clone()),
                    spec.release_action
                        .clone()
                        .or_else(|| Some(spec.action.clone())),
                ),
            };

            let compiled_bind = CompiledBind {
                id: spec.id,
                mode: spec.mode.clone(),
                enabled: spec.enabled,
                raw_chord: spec.chord.clone(),
                chord,
                press_action,
                release_action,
                repeat: spec.options.effective_repeat(&spec.action),
                options: spec.options.clone(),
            };

            if let Some(existing) = binds.iter_mut().find(|b| {
                b.mode == compiled_bind.mode
                    && b.options.edge == compiled_bind.options.edge
                    && chords_match_under_mod_override(
                        b.chord,
                        compiled_bind.chord,
                        mod_key_override,
                    )
            }) {
                *existing = compiled_bind;
            } else {
                binds.push(compiled_bind);
            }
        }

        // Like Niri (`niri/src/input/mod.rs` lines 3316-3415), keep `Mod + MouseLeft` move and
        // `Mod + MouseRight` resize available unless explicitly unbound or rebound!
        let lmb_chord = CompiledChord {
            mods: MOD_COMPOSITOR,
            trigger: BindTrigger::MouseButton(MouseButtonTrigger::Left),
        };
        let lmb_unbound = parsed_unbinds
            .iter()
            .any(|&u| chords_match_under_mod_override(lmb_chord, u, mod_key_override));
        let has_mod_lmb = binds.iter().any(|b| {
            b.mode.is_none()
                && chords_match_under_mod_override(b.chord, lmb_chord, mod_key_override)
        });
        if !has_mod_lmb && !lmb_unbound {
            binds.push(CompiledBind {
                id: BindHandleId(u64::MAX - 1),
                mode: None,
                enabled: true,
                raw_chord: "Mod + MouseLeft".to_owned(),
                chord: lmb_chord,
                press_action: Some(ActionDescriptor::WindowDragMove),
                release_action: None,
                repeat: BindRepeat::Once,
                options: BindOptions::default(),
            });
        }

        let rmb_chord = CompiledChord {
            mods: MOD_COMPOSITOR,
            trigger: BindTrigger::MouseButton(MouseButtonTrigger::Right),
        };
        let rmb_unbound = parsed_unbinds
            .iter()
            .any(|&u| chords_match_under_mod_override(rmb_chord, u, mod_key_override));
        let has_mod_rmb = binds.iter().any(|b| {
            b.mode.is_none()
                && chords_match_under_mod_override(b.chord, rmb_chord, mod_key_override)
        });
        if !has_mod_rmb && !rmb_unbound {
            binds.push(CompiledBind {
                id: BindHandleId(u64::MAX),
                mode: None,
                enabled: true,
                raw_chord: "Mod + MouseRight".to_owned(),
                chord: rmb_chord,
                press_action: Some(ActionDescriptor::WindowDragResize),
                release_action: None,
                repeat: BindRepeat::Once,
                options: BindOptions::default(),
            });
        }

        Ok(Self {
            binds,
            mod_key_override,
            active_mode: None,
            cooldowns_ms: HashMap::new(),
            pending_releases: HashMap::new(),
            pending_mouse_releases: HashMap::new(),
            suppressed_buttons: HashSet::new(),
            repeat: KeyRepeatState::default(),
            horizontal_wheel_tracker: ScrollTracker::new(WHEEL_V120_TICK),
            vertical_wheel_tracker: ScrollTracker::new(WHEEL_V120_TICK),
            horizontal_finger_tracker: ScrollTracker::new(TOUCHPAD_SCROLL_TICK),
            vertical_finger_tracker: ScrollTracker::new(TOUCHPAD_SCROLL_TICK),
        })
    }

    #[must_use]
    pub fn effective_mod_key(&self, is_bare_metal_tty: bool) -> PhysicalModKey {
        self.mod_key_override.unwrap_or(if is_bare_metal_tty {
            PhysicalModKey::Super
        } else {
            PhysicalModKey::Alt
        })
    }

    /// Resets all discrete wheel & touchpad scroll accumulators (called on modifier release,
    /// focus loss, or VT session switch).
    pub fn reset_scroll_trackers(&mut self) {
        self.horizontal_wheel_tracker.reset();
        self.vertical_wheel_tracker.reset();
        self.horizontal_finger_tracker.reset();
        self.vertical_finger_tracker.reset();
    }
}

/// Parses a human-readable chord such as `"Mod + Shift + Return"`, `"Mod + MouseLeft"`,
/// `"Mod + MouseRight"`, `"Mod + WheelScrollDown"`, or `"XF86AudioRaiseVolume"` into a normalized
/// modifier mask and `BindTrigger`.
pub fn parse_chord_spec(raw_chord: &str) -> Result<CompiledChord, String> {
    let tokens: Vec<&str> = raw_chord
        .split('+')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let Some((&trigger_tok, mod_tokens)) = tokens.split_last() else {
        return Err("empty keybind chord".to_owned());
    };

    let mut mods: u8 = 0;
    for &m in mod_tokens {
        match m.to_ascii_lowercase().as_str() {
            "mod" => mods |= MOD_COMPOSITOR,
            "ctrl" | "control" => mods |= MOD_CTRL,
            "shift" => mods |= MOD_SHIFT,
            "alt" | "mod1" => mods |= MOD_ALT,
            "super" | "win" | "logo" | "meta" | "mod4" => mods |= MOD_LOGO,
            "mod5" | "altgr" | "iso_level3_shift" => mods |= MOD_ISO_LEVEL3,
            other => {
                return Err(format!(
                    "unknown modifier `{other}` (valid: Mod, Super, Alt, Ctrl, Shift, Mod5)"
                ));
            }
        }
    }

    let trigger = if let Some(mouse_btn) = MouseButtonTrigger::try_parse(trigger_tok) {
        BindTrigger::MouseButton(mouse_btn)
    } else if let Some(scroll_kind) = ScrollTriggerKind::try_parse(trigger_tok) {
        BindTrigger::Scroll(scroll_kind)
    } else {
        let (sym, is_pre_shifted) = resolve_xkb_keysym_token(trigger_tok)?;
        BindTrigger::Keysym {
            sym,
            is_pre_shifted,
        }
    };

    Ok(CompiledChord { mods, trigger })
}

fn resolve_xkb_keysym_token(token: &str) -> Result<(Keysym, bool), String> {
    let canonical = match token.to_ascii_lowercase().as_str() {
        "enter" | "return" => "Return",
        "esc" | "escape" => "Escape",
        "space" => "space",
        "tab" => "Tab",
        "backspace" | "bs" => "BackSpace",
        "del" | "delete" => "Delete",
        "bracketleft" | "[" => "bracketleft",
        "bracketright" | "]" => "bracketright",
        "slash" | "/" => "slash",
        "backslash" | "\\" => "backslash",
        "minus" | "-" => "minus",
        "equal" | "=" => "equal",
        "plus" => "plus",
        "comma" | "," => "comma",
        "period" | "." => "period",
        "semicolon" | ";" => "semicolon",
        "apostrophe" | "'" => "apostrophe",
        "grave" | "`" => "grave",
        "pageup" | "page_up" | "pgup" => "Page_Up",
        "pagedown" | "page_down" | "pgdn" => "Page_Down",
        "printscreen" | "print_screen" | "print" | "sysrq" => "Print",
        "xf86audioprevious" | "audioprev" | "audioprevious" => "XF86AudioPrev",
        "audionext" => "XF86AudioNext",
        "audioplay" => "XF86AudioPlay",
        "audiopause" => "XF86AudioPause",
        "audiostop" => "XF86AudioStop",
        "audiomute" => "XF86AudioMute",
        "audiomicmute" => "XF86AudioMicMute",
        "audioraisevolume" => "XF86AudioRaiseVolume",
        "audiolowervolume" => "XF86AudioLowerVolume",
        "monbrightnessup" | "brightnessup" => "XF86MonBrightnessUp",
        "monbrightnessdown" | "brightnessdown" => "XF86MonBrightnessDown",
        "kbdbrightnessup" => "XF86KbdBrightnessUp",
        "kbdbrightnessdown" => "XF86KbdBrightnessDown",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        _ => token,
    };

    // 1. Exact case-sensitive XKB lookup (`libxkbcommon` recommended fast-path)
    let mut sym = xkb::keysym_from_name(canonical, xkb::KEYSYM_NO_FLAGS);
    if sym.raw() == keysyms::KEY_NoSymbol {
        // 2. Case-insensitive fallback (e.g. `xf86audioraisevolume`, `f12`)
        sym = xkb::keysym_from_name(canonical, xkb::KEYSYM_CASE_INSENSITIVE);
    }
    if sym.raw() == keysyms::KEY_NoSymbol {
        return Err(format!(
            "unknown XKB key, mouse button, or scroll trigger `{token}` (mouse: MouseLeft/Right/Middle/Back/Forward; scroll: WheelScrollUp/Down/Left/Right, TouchpadScrollUp/Down/Left/Right)"
        ));
    }

    // Normalize ASCII uppercase `KEY_A..=KEY_Z` (`0x41..=0x5a`) to lowercase `KEY_a..=KEY_z`
    // so `Mod + Q` and `Mod + q` behave identically and never depend on CapsLock!
    let raw_val = sym.raw();
    if (keysyms::KEY_A..=keysyms::KEY_Z).contains(&raw_val) {
        let lower = Keysym::new(raw_val + 32);
        return Ok((lower, false));
    }

    // Punctuation symbols produced by Shift on US QWERTY (e.g. `@`, `?`, `+`, `!`, `:`)
    let pre_shifted = sym
        .key_char()
        .is_some_and(|ch| ch.is_ascii_punctuation() && r#"!@#$%^&*()_+{}|:"<>?~"#.contains(ch));
    Ok((sym, pre_shifted))
}

fn normalize_candidate_sym(sym: Keysym) -> Keysym {
    let r = sym.raw();
    if (keysyms::KEY_A..=keysyms::KEY_Z).contains(&r) {
        Keysym::new(r + 32)
    } else {
        sym
    }
}

pub(crate) fn modifiers_mask_from_smithay(mods: &ModifiersState) -> u8 {
    // Deliberately ignore `caps_lock` and `num_lock` (`Mod2`) per Niri/COSMIC XKB best practices!
    let mut out = 0_u8;
    if mods.ctrl {
        out |= MOD_CTRL;
    }
    if mods.shift {
        out |= MOD_SHIFT;
    }
    if mods.alt {
        out |= MOD_ALT;
    }
    if mods.logo {
        out |= MOD_LOGO;
    }
    if mods.iso_level3_shift {
        out |= MOD_ISO_LEVEL3;
    }
    out
}

pub(crate) fn expand_bind_modifiers(bind_mods: u8, phys_mod: PhysicalModKey) -> u8 {
    if (bind_mods & MOD_COMPOSITOR) != 0 {
        (bind_mods & !MOD_COMPOSITOR) | phys_mod.bit()
    } else {
        bind_mods
    }
}

fn chords_match_under_mod_override(
    a: CompiledChord,
    b: CompiledChord,
    mod_key_override: Option<PhysicalModKey>,
) -> bool {
    if a.trigger != b.trigger {
        return false;
    }
    if a.mods == b.mods {
        return true;
    }
    if let Some(phys) = mod_key_override {
        return expand_bind_modifiers(a.mods, phys) == expand_bind_modifiers(b.mods, phys);
    }
    false
}

/// Unified `PointerButton` handler shared by `backend_winit.rs` and `backend_tty.rs`
/// (`niri/src/input/mod.rs` `on_pointer_button` lines 3180-3500).
///
/// Order of operations:
/// 1. If a `PointerGrab` (`MoveWindowGrab` / `ResizeWindowGrab` / DnD) is already active, routes
///    `ButtonEvent` straight to `PointerHandle::button` so the grab handles release or opposite-button
///    mid-drag floating toggle!
/// 2. On `Released`, swallows any button previously suppressed by a non-grab mouse bind.
/// 3. On `Pressed`, evaluates `KeybindTable` mouse-button chords (`Mod + MouseLeft` ->
///    `MoveWindowGrab`, `Mod + MouseRight` -> `ResizeWindowGrab`, or custom `koma.bind("Mod + MouseMiddle", ...)`).
/// 4. Otherwise focuses the clicked window (`focus_window_at_point`) and forwards the click to the client!
pub fn process_pointer_button_event(
    state: &mut KomaState,
    button_code: u32,
    btn_state: ButtonState,
    time: InputTime,
    is_bare_metal_tty: bool,
) {
    state.reconcile_session_lock();
    state.notify_user_activity();
    let serial = SERIAL_COUNTER.next_serial();
    // Held across key-binding dispatch and `refresh_pointer_hover_focus`, which re-acquires the
    // pointer; Smithay releases its pointer mutex between calls, so no scope is claimed here.
    let Some(pointer) = state.pointer_handle() else {
        return;
    };

    // 1. Active PointerGrab (`MoveWindowGrab` / `ResizeWindowGrab` / Popup / DnD) owns all buttons!
    if pointer.is_grabbed() {
        pointer.button(
            state,
            &ButtonEvent {
                button: button_code,
                state: btn_state,
                serial,
                time,
            },
        );
        pointer.frame(state);
        return;
    }

    // 2. Swallow orphan button release from a previously intercepted non-grab mouse shortcut
    if btn_state == ButtonState::Released {
        let was_suppressed = state.keybinds.suppressed_buttons.remove(&button_code);
        let release_act = state.keybinds.pending_mouse_releases.remove(&button_code);
        if let Some(rel) = release_act
            && is_keybind_allowed_for_lock_state(
                state.is_locked_or_abandoned(),
                rel.allow_when_locked,
            )
        {
            execute_action_descriptor(state, rel.action, is_bare_metal_tty);
        }
        if was_suppressed {
            return;
        }
        pointer.button(
            state,
            &ButtonEvent {
                button: button_code,
                state: btn_state,
                serial,
                time,
            },
        );
        pointer.frame(state);
        return;
    }

    // 3. ButtonState::Pressed while ungrabbed: check mouse-button binds (`Mod + MouseLeft/Right/...`)
    if let Some(btn_trigger) = MouseButtonTrigger::from_button_code(button_code) {
        let held_mask = state
            .kb()
            .map(|kb| modifiers_mask_from_smithay(&kb.modifier_state()))
            .unwrap_or(0);
        let phys_mod = state.keybinds.effective_mod_key(is_bare_metal_tty);

        if let Some(idx) = find_mouse_button_bind_idx(state, btn_trigger, held_mask, phys_mod) {
            let bind = state.keybinds.binds[idx].clone();
            let now_ms = state.elapsed_ms();
            let cooling_down = bind.options.cooldown_ms.is_some_and(|cd| {
                state
                    .keybinds
                    .cooldowns_ms
                    .get(&bind.id)
                    .is_some_and(|&last| now_ms.saturating_sub(last) < cd)
            });

            if !cooling_down {
                if bind.options.cooldown_ms.is_some() {
                    state.keybinds.cooldowns_ms.insert(bind.id, now_ms);
                }
                if let Some(rel) = bind.release_action.clone() {
                    state.keybinds.pending_mouse_releases.insert(
                        button_code,
                        PendingRelease {
                            action: rel,
                            allow_when_locked: bind.options.allow_when_locked,
                        },
                    );
                }

                match bind.press_action {
                    Some(ActionDescriptor::WindowDragMove) => {
                        if start_compositor_move_grab(state, button_code, serial) {
                            // Forward press into newly installed `MoveWindowGrab` so `current_pressed()`
                            // records `button_code` while `Focus::Clear` prevents client click leak!
                            pointer.button(
                                state,
                                &ButtonEvent {
                                    button: button_code,
                                    state: btn_state,
                                    serial,
                                    time,
                                },
                            );
                            pointer.frame(state);
                            return;
                        }
                    }
                    Some(ActionDescriptor::WindowDragResize) => {
                        if start_compositor_resize_grab(state, button_code, serial) {
                            pointer.button(
                                state,
                                &ButtonEvent {
                                    button: button_code,
                                    state: btn_state,
                                    serial,
                                    time,
                                },
                            );
                            pointer.frame(state);
                            return;
                        }
                    }
                    Some(other_action) => {
                        execute_action_descriptor(state, other_action, is_bare_metal_tty);
                        if !bind.options.pass_through {
                            state.keybinds.suppressed_buttons.insert(button_code);
                            return;
                        }
                    }
                    None if !bind.options.pass_through => {
                        state.keybinds.suppressed_buttons.insert(button_code);
                        return;
                    }
                    None => {}
                }
            } else if !bind.options.pass_through {
                state.keybinds.suppressed_buttons.insert(button_code);
                return;
            }
        }
    }

    // 4. Frame edge/corner drag starts an interactive resize with no modifier
    //    (`Hyprland` border-drag parity, komawm/koma#42).
    if button_code == BTN_LEFT
        && let Some(edge) = state.frame_edge_under_pointer(state.pointer_location)
        && start_frame_edge_resize_grab(state, button_code, serial, edge)
    {
        pointer.button(
            state,
            &ButtonEvent {
                button: button_code,
                state: btn_state,
                serial,
                time,
            },
        );
        pointer.frame(state);
        return;
    }

    // 5. Normal unintercepted click: clear any stale suppression, apply layer/window focus, and forward!
    state.keybinds.suppressed_buttons.remove(&button_code);
    let cursor_pos = state.pointer_location;
    state.handle_pointer_button_focus(cursor_pos);
    refresh_pointer_hover_focus(state, time);

    pointer.button(
        state,
        &ButtonEvent {
            button: button_code,
            state: btn_state,
            serial,
            time,
        },
    );
    pointer.frame(state);
}

fn find_mouse_button_bind_idx(
    state: &KomaState,
    btn: MouseButtonTrigger,
    held_mask: u8,
    phys_mod: PhysicalModKey,
) -> Option<usize> {
    let active_mode = &state.keybinds.active_mode;
    let is_locked = state.is_locked_or_abandoned();
    let is_preempted = state
        .workspaces
        .get(&state.active_ws_id)
        .is_some_and(|ws| ws.lease.is_human_preempted());

    state.keybinds.binds.iter().position(|bind| {
        if !bind.enabled || &bind.mode != active_mode {
            return false;
        }
        if !is_keybind_allowed_for_lock_state(is_locked, bind.options.allow_when_locked) {
            return false;
        }
        if is_preempted && bind.options.when_preempted == "ignore" {
            return false;
        }
        bind.chord.trigger == BindTrigger::MouseButton(btn)
            && expand_bind_modifiers(bind.chord.mods, phys_mod) == held_mask
    })
}

/// Unified keyboard event entry point shared by both `backend_winit.rs` and `backend_tty.rs`.
pub fn process_keyboard_key_event(
    state: &mut KomaState,
    keycode: Keycode,
    key_state: KeyState,
    time: InputTime,
    is_bare_metal_tty: bool,
) {
    state.reconcile_session_lock();
    state.notify_user_activity();
    if state.virtual_keyboard_keymap_active {
        state.force_reapply_configured_xkb_config();
    }
    let serial = SERIAL_COUNTER.next_serial();
    // Held across the key-binding filter, which runs with Smithay's keyboard mutex released and
    // may re-apply keyboard focus/configuration; no scope is claimed here.
    let Some(keyboard) = state.keyboard_handle() else {
        return;
    };

    enum InterceptOutcome {
        SwitchVt(i32),
        TogglePreempt,
        CancelPointerGrab,
        ExitSubmapMode,
        FireAction {
            bind_id: Option<BindHandleId>,
            action: ActionDescriptor,
            /// `Some` when the press starts a `WhileHeld` repeat (`#76`).
            repeat: Option<RepeatRequest>,
        },
    }

    let now_ms = state.elapsed_ms();
    let raw_keycode = keycode.raw();

    // 1. On physical key release, dispatch pending `TriggerEdge::Release`/`Both` action and
    //    reset scroll trackers if any modifier bit changed (`niri` modifier release rule).
    //    Fail-closed on locked sessions: never deliver lock-screen key events into an active IME grab!
    if key_state == KeyState::Released {
        let prev_mask = modifiers_mask_from_smithay(&keyboard.modifier_state());
        if state.keybinds.repeat.is_repeating_keycode(raw_keycode) {
            state.cancel_keybind_repeat("release");
        }
        let release_act = state.keybinds.pending_releases.remove(&raw_keycode);
        if state.is_locked_or_abandoned() {
            if let Some(((), mods_changed)) = keyboard.input_intercept(
                smithay::input::keyboard::KeyboardSource::MAIN,
                state,
                keycode,
                key_state,
                |_, _, _| (),
            ) {
                keyboard.input_forward_bypassing_grab(
                    state,
                    keycode,
                    key_state,
                    serial,
                    time,
                    mods_changed,
                );
            }
        } else {
            let _ = keyboard.input::<(), _>(state, keycode, key_state, serial, time, |_, _, _| {
                FilterResult::Forward
            });
        }
        let next_mask = modifiers_mask_from_smithay(&keyboard.modifier_state());
        if prev_mask != next_mask {
            state.keybinds.reset_scroll_trackers();
            // Any modifier transition cancels a pending repeat: the held chord no longer
            // matches the modifiers that armed it.
            state.cancel_keybind_repeat("modifier_change");
        }
        if let Some(rel) = release_act
            && is_keybind_allowed_for_lock_state(
                state.is_locked_or_abandoned(),
                rel.allow_when_locked,
            )
        {
            execute_action_descriptor(state, rel.action, is_bare_metal_tty);
        }
        return;
    }

    // 2. Evaluate Ring 0 safety chords & `KeybindTable`, bypassing any IME grab when session-locked.
    let outcome = if let Some((filter_result, mods_changed)) = keyboard.input_intercept(
        smithay::input::keyboard::KeyboardSource::MAIN,
        state,
        keycode,
        key_state,
        |state, modifiers, handle| {
            let modified_sym = handle.modified_sym();
            let mod_raw = modified_sym.raw();
            let is_locked = state.is_locked_or_abandoned();

            // Gate 0a: Hardware Linux VT switch (`Ctrl + Alt + F1..F12` -> `XF86Switch_VT_1..12`)
            if is_bare_metal_tty && let Some(vt) = parse_vt_switch_keysym(mod_raw) {
                return FilterResult::Intercept(InterceptOutcome::SwitchVt(vt));
            }

            // Gate 0a2: Cancel active interactive Move/Resize grab on `Escape` (`niri/src/input/mod.rs` line 688)
            if !is_locked
                && mod_raw == keysyms::KEY_Escape
                && state.ptr().is_some_and(|p| p.is_grabbed())
            {
                return FilterResult::Intercept(InterceptOutcome::CancelPointerGrab);
            }

            let held_mask = modifiers_mask_from_smithay(modifiers);
            let phys_mod = state.keybinds.effective_mod_key(is_bare_metal_tty);

            // A modifier pressed (or a chord re-pressed) outside the repeating chord cancels it.
            if state
                .keybinds
                .repeat
                .cancel_if_modifiers_changed(held_mask)
                .is_some()
            {
                state.cancel_keybind_repeat("modifier_change");
            }

            // Gate 0b: Ring 0 Human Preemptive Takeover (`Mod + G`) — blocked when locked!
            let raw_syms = handle.raw_syms();
            let candidate_syms = std::iter::once((modified_sym, true))
                .chain(raw_syms.iter().copied().map(|s| (s, false)));

            if !is_locked && held_mask == phys_mod.bit() {
                for (sym, _) in candidate_syms.clone() {
                    if matches!(sym.raw(), keysyms::KEY_g | keysyms::KEY_G) {
                        return FilterResult::Intercept(InterceptOutcome::TogglePreempt);
                    }
                }
            }

            // Gate 0c: Honor active `zwp_keyboard_shortcuts_inhibit_v1` for unrestricted focused surfaces
            // while keeping Ring-0 VT switching and `Mod + G` human preempt takeover un-inhibitable.
            if !is_locked && state.are_shortcuts_inhibited_for_focused_surface() {
                return FilterResult::Forward;
            }

            // Gate 1: Match user-configured XKB keysym binds (`modified_sym` -> `raw_syms` fallback)
            let active_mode = state.keybinds.active_mode.clone();
            let is_preempted = state
                .workspaces
                .get(&state.active_ws_id)
                .is_some_and(|ws| ws.lease.is_human_preempted());

            let mut matched_idx: Option<usize> = None;
            'sym_loop: for (cand_sym, is_modified_level) in candidate_syms {
                let norm_sym = normalize_candidate_sym(cand_sym);
                for (idx, bind) in state.keybinds.binds.iter().enumerate() {
                    if !bind.enabled || bind.mode != active_mode {
                        continue;
                    }
                    if !is_keybind_allowed_for_lock_state(is_locked, bind.options.allow_when_locked)
                    {
                        continue;
                    }
                    if is_preempted && bind.options.when_preempted == "ignore" {
                        continue;
                    }
                    let BindTrigger::Keysym {
                        sym: bind_sym,
                        is_pre_shifted,
                    } = bind.chord.trigger
                    else {
                        continue;
                    };
                    if bind_sym != norm_sym {
                        continue;
                    }

                    let required_mask = expand_bind_modifiers(bind.chord.mods, phys_mod);
                    let mods_match = if is_modified_level && is_pre_shifted {
                        (held_mask & !MOD_SHIFT) == (required_mask & !MOD_SHIFT)
                    } else {
                        held_mask == required_mask
                    };

                    if mods_match {
                        matched_idx = Some(idx);
                        break 'sym_loop;
                    }
                }
            }

            let Some(idx) = matched_idx else {
                if !is_locked
                    && active_mode.is_some()
                    && mod_raw == keysyms::KEY_Escape
                    && held_mask == 0
                {
                    return FilterResult::Intercept(InterceptOutcome::ExitSubmapMode);
                }
                return FilterResult::Forward;
            };

            let bind = &state.keybinds.binds[idx];
            let bind_id = bind.id;
            let pass_through = bind.options.pass_through;

            // Enforce optional `cooldown_ms` rate-limiting (`niri/src/input/mod.rs` line 5082)
            if let Some(cd_ms) = bind.options.cooldown_ms
                && let Some(&last_ms) = state.keybinds.cooldowns_ms.get(&bind_id)
                && now_ms.saturating_sub(last_ms) < cd_ms
            {
                return if pass_through {
                    FilterResult::Forward
                } else {
                    FilterResult::Intercept(InterceptOutcome::FireAction {
                        bind_id: None,
                        action: ActionDescriptor::ModeExit,
                        repeat: None,
                    })
                };
            }

            let press_act = bind.press_action.clone();
            let release_act = bind.release_action.clone();
            let allow_when_locked = bind.options.allow_when_locked;

            if bind.options.cooldown_ms.is_some() {
                state.keybinds.cooldowns_ms.insert(bind_id, now_ms);
            }
            if let Some(rel) = release_act {
                state.keybinds.pending_releases.insert(
                    raw_keycode,
                    PendingRelease {
                        action: rel,
                        allow_when_locked,
                    },
                );
            }

            if let Some(action) = press_act {
                if pass_through {
                    state.pending_passthrough_actions.push(action);
                    FilterResult::Forward
                } else {
                    let repeat = state.keybinds.binds[idx].repeat_request(raw_keycode, held_mask);
                    FilterResult::Intercept(InterceptOutcome::FireAction {
                        bind_id: Some(bind_id),
                        action,
                        repeat,
                    })
                }
            } else if pass_through {
                FilterResult::Forward
            } else {
                FilterResult::Intercept(InterceptOutcome::FireAction {
                    bind_id: None,
                    action: ActionDescriptor::ModeExit,
                    repeat: None,
                })
            }
        },
    ) {
        match filter_result {
            FilterResult::Intercept(val) => Some(val),
            FilterResult::Forward => {
                if state.is_locked_or_abandoned() {
                    keyboard.input_forward_bypassing_grab(
                        state,
                        keycode,
                        key_state,
                        serial,
                        time,
                        mods_changed,
                    );
                } else {
                    keyboard.input_forward(state, keycode, key_state, serial, time, mods_changed);
                }
                None
            }
        }
    } else {
        None
    };

    if !state.pending_passthrough_actions.is_empty() {
        let passthrough = std::mem::take(&mut state.pending_passthrough_actions);
        for act in passthrough {
            execute_action_descriptor(state, act, is_bare_metal_tty);
        }
    }

    let Some(intercept) = outcome else {
        return;
    };

    match intercept {
        InterceptOutcome::SwitchVt(vt) => {
            state.keybinds.reset_scroll_trackers();
            if let Some(kb) = state.kb() {
                kb.release_source(state, KeyboardSource::MAIN);
            }
            state.change_vt(vt);
        }
        InterceptOutcome::TogglePreempt => {
            let label = if is_bare_metal_tty {
                "Super+G"
            } else {
                "Alt+G"
            };
            toggle_ring0_preempt_takeover(state, label);
        }
        InterceptOutcome::CancelPointerGrab => {
            let _ = cancel_active_pointer_grab(state, serial, time);
        }
        InterceptOutcome::ExitSubmapMode => {
            info!(prev_mode = ?state.keybinds.active_mode, "exited keybind submap mode via Escape");
            state.keybinds.active_mode = None;
        }
        InterceptOutcome::FireAction {
            bind_id: Some(_),
            action,
            repeat,
        } => {
            execute_action_descriptor(state, action, is_bare_metal_tty);
            if let Some(request) = repeat {
                state
                    .keybinds
                    .repeat
                    .set_timing(state.keybind_repeat_timing());
                let now_ms = state.elapsed_ms();
                state.keybinds.repeat.arm(request, now_ms);
                let delay_ms = state.keybinds.repeat.timing().delay_ms;
                arm_keybind_repeat_timer(state, is_bare_metal_tty, delay_ms);
            }
        }
        InterceptOutcome::FireAction { bind_id: None, .. } => {}
    }
}

/// Unified `PointerAxis` handler shared by `backend_tty.rs` and `backend_winit.rs`
/// (`niri/src/input/mod.rs` `on_pointer_axis` lines 3500-3925).
pub fn process_pointer_axis_event<B: InputBackend>(
    state: &mut KomaState,
    event: &B::PointerAxisEvent,
    is_bare_metal_tty: bool,
) {
    state.reconcile_session_lock();
    state.notify_user_activity();
    let source = event.source();
    let time = event.time();

    let held_mask = state
        .kb()
        .map(|kb| modifiers_mask_from_smithay(&kb.modifier_state()))
        .unwrap_or(0);
    let phys_mod = state.keybinds.effective_mod_key(is_bare_metal_tty);

    let mut horizontal_amount = event.amount(Axis::Horizontal).unwrap_or(0.0);
    let mut vertical_amount = event.amount(Axis::Vertical).unwrap_or(0.0);
    let mut horizontal_v120 = event.amount_v120(Axis::Horizontal);
    let mut vertical_v120 = event.amount_v120(Axis::Vertical);

    let mut stop_horizontal = matches!(source, AxisSource::Finger | AxisSource::Continuous)
        && event.amount(Axis::Horizontal) == Some(0.0);
    let mut stop_vertical = matches!(source, AxisSource::Finger | AxisSource::Continuous)
        && event.amount(Axis::Vertical) == Some(0.0);

    // 1. Discrete mouse wheel bindings (`WheelScrollLeft/Right/Up/Down`)
    if matches!(source, AxisSource::Wheel | AxisSource::WheelTilt) {
        let left_idx = find_scroll_bind_idx(
            state,
            ScrollTriggerKind::WheelScrollLeft,
            held_mask,
            phys_mod,
        );
        let right_idx = find_scroll_bind_idx(
            state,
            ScrollTriggerKind::WheelScrollRight,
            held_mask,
            phys_mod,
        );

        if left_idx.is_some() || right_idx.is_some() {
            let raw_h_v120 =
                horizontal_v120.unwrap_or_else(|| horizontal_amount * (WHEEL_V120_TICK / 15.0));
            let ticks = state
                .keybinds
                .horizontal_wheel_tracker
                .accumulate(raw_h_v120);
            let mut consume_axis =
                bind_consumes_axis(state, left_idx) || bind_consumes_axis(state, right_idx);

            if ticks > 0 {
                if let Some(idx) = right_idx {
                    consume_axis =
                        fire_scroll_bind_ticks(state, idx, ticks as u8, is_bare_metal_tty);
                }
            } else if ticks < 0
                && let Some(idx) = left_idx
            {
                consume_axis =
                    fire_scroll_bind_ticks(state, idx, (-ticks) as u8, is_bare_metal_tty);
            }

            if consume_axis {
                horizontal_amount = 0.0;
                horizontal_v120 = None;
            }
        } else {
            state.keybinds.horizontal_wheel_tracker.reset();
        }

        let up_idx =
            find_scroll_bind_idx(state, ScrollTriggerKind::WheelScrollUp, held_mask, phys_mod);
        let down_idx = find_scroll_bind_idx(
            state,
            ScrollTriggerKind::WheelScrollDown,
            held_mask,
            phys_mod,
        );

        if up_idx.is_some() || down_idx.is_some() {
            let raw_v_v120 =
                vertical_v120.unwrap_or_else(|| vertical_amount * (WHEEL_V120_TICK / 15.0));
            let ticks = state.keybinds.vertical_wheel_tracker.accumulate(raw_v_v120);
            let mut consume_axis =
                bind_consumes_axis(state, up_idx) || bind_consumes_axis(state, down_idx);

            if ticks > 0 {
                if let Some(idx) = down_idx {
                    consume_axis =
                        fire_scroll_bind_ticks(state, idx, ticks as u8, is_bare_metal_tty);
                }
            } else if ticks < 0
                && let Some(idx) = up_idx
            {
                consume_axis =
                    fire_scroll_bind_ticks(state, idx, (-ticks) as u8, is_bare_metal_tty);
            }

            if consume_axis {
                vertical_amount = 0.0;
                vertical_v120 = None;
            }
        } else {
            state.keybinds.vertical_wheel_tracker.reset();
        }

        if horizontal_amount == 0.0
            && vertical_amount == 0.0
            && horizontal_v120.is_none()
            && vertical_v120.is_none()
        {
            return;
        }
    }
    // 2. Two-finger touchpad & continuous scroll bindings (`TouchpadScrollLeft/Right/Up/Down`)
    else if matches!(source, AxisSource::Finger | AxisSource::Continuous) {
        if stop_horizontal && stop_vertical {
            state.keybinds.horizontal_finger_tracker.reset();
            state.keybinds.vertical_finger_tracker.reset();
        }

        let tp_left_idx = find_scroll_bind_idx(
            state,
            ScrollTriggerKind::TouchpadScrollLeft,
            held_mask,
            phys_mod,
        );
        let tp_right_idx = find_scroll_bind_idx(
            state,
            ScrollTriggerKind::TouchpadScrollRight,
            held_mask,
            phys_mod,
        );

        if tp_left_idx.is_some() || tp_right_idx.is_some() {
            let ticks = state
                .keybinds
                .horizontal_finger_tracker
                .accumulate(horizontal_amount);
            let mut consume_axis =
                bind_consumes_axis(state, tp_left_idx) || bind_consumes_axis(state, tp_right_idx);

            if ticks > 0 {
                if let Some(idx) = tp_right_idx {
                    consume_axis =
                        fire_scroll_bind_ticks(state, idx, ticks as u8, is_bare_metal_tty);
                }
            } else if ticks < 0
                && let Some(idx) = tp_left_idx
            {
                consume_axis =
                    fire_scroll_bind_ticks(state, idx, (-ticks) as u8, is_bare_metal_tty);
            }

            if consume_axis {
                horizontal_amount = 0.0;
                horizontal_v120 = None;
                stop_horizontal = false;
            }
        } else {
            state.keybinds.horizontal_finger_tracker.reset();
        }

        let tp_up_idx = find_scroll_bind_idx(
            state,
            ScrollTriggerKind::TouchpadScrollUp,
            held_mask,
            phys_mod,
        );
        let tp_down_idx = find_scroll_bind_idx(
            state,
            ScrollTriggerKind::TouchpadScrollDown,
            held_mask,
            phys_mod,
        );

        if tp_up_idx.is_some() || tp_down_idx.is_some() {
            let ticks = state
                .keybinds
                .vertical_finger_tracker
                .accumulate(vertical_amount);
            let mut consume_axis =
                bind_consumes_axis(state, tp_up_idx) || bind_consumes_axis(state, tp_down_idx);

            if ticks > 0 {
                if let Some(idx) = tp_down_idx {
                    consume_axis =
                        fire_scroll_bind_ticks(state, idx, ticks as u8, is_bare_metal_tty);
                }
            } else if ticks < 0
                && let Some(idx) = tp_up_idx
            {
                consume_axis =
                    fire_scroll_bind_ticks(state, idx, (-ticks) as u8, is_bare_metal_tty);
            }

            if consume_axis {
                vertical_amount = 0.0;
                vertical_v120 = None;
                stop_vertical = false;
            }
        } else {
            state.keybinds.vertical_finger_tracker.reset();
        }

        if horizontal_amount == 0.0 && vertical_amount == 0.0 && !stop_horizontal && !stop_vertical
        {
            return;
        }
    }

    // 3. Forward unconsumed scroll delta(s) to the focused Wayland client surface
    refresh_pointer_hover_focus(state, time);

    let scroll_factor = {
        let input_cfg = &state.lua_sandbox.config_snapshot().input;
        match source {
            AxisSource::Wheel | AxisSource::WheelTilt => input_cfg.mouse_scroll_factor(),
            AxisSource::Finger | AxisSource::Continuous => input_cfg.touchpad_scroll_factor(),
        }
    };

    let scaled_h = horizontal_amount * scroll_factor;
    let scaled_v = vertical_amount * scroll_factor;
    let scaled_h_v120 = horizontal_v120.map(|v| v * scroll_factor);
    let scaled_v_v120 = vertical_v120.map(|v| v * scroll_factor);

    let mut frame = AxisFrame::new(time).source(source);
    if scaled_h != 0.0 || scaled_h_v120.is_some() {
        frame = frame
            .relative_direction(Axis::Horizontal, event.relative_direction(Axis::Horizontal))
            .value(Axis::Horizontal, scaled_h);
        if let Some(v120) = scaled_h_v120 {
            frame = frame.v120(Axis::Horizontal, v120 as i32);
        }
    }
    if scaled_v != 0.0 || scaled_v_v120.is_some() {
        frame = frame
            .relative_direction(Axis::Vertical, event.relative_direction(Axis::Vertical))
            .value(Axis::Vertical, scaled_v);
        if let Some(v120) = scaled_v_v120 {
            frame = frame.v120(Axis::Vertical, v120 as i32);
        }
    }
    if stop_horizontal {
        frame = frame.stop(Axis::Horizontal);
    }
    if stop_vertical {
        frame = frame.stop(Axis::Vertical);
    }

    if let Some(pointer) = state.ptr() {
        pointer.axis(state, frame);
        pointer.frame(state);
    }
}

fn find_scroll_bind_idx(
    state: &KomaState,
    kind: ScrollTriggerKind,
    held_mask: u8,
    phys_mod: PhysicalModKey,
) -> Option<usize> {
    let active_mode = &state.keybinds.active_mode;
    let is_locked = state.is_locked_or_abandoned();
    let is_preempted = state
        .workspaces
        .get(&state.active_ws_id)
        .is_some_and(|ws| ws.lease.is_human_preempted());

    state.keybinds.binds.iter().position(|bind| {
        if !bind.enabled || &bind.mode != active_mode {
            return false;
        }
        if !is_keybind_allowed_for_lock_state(is_locked, bind.options.allow_when_locked) {
            return false;
        }
        if is_preempted && bind.options.when_preempted == "ignore" {
            return false;
        }
        bind.chord.trigger == BindTrigger::Scroll(kind)
            && expand_bind_modifiers(bind.chord.mods, phys_mod) == held_mask
    })
}

fn bind_consumes_axis(state: &KomaState, idx: Option<usize>) -> bool {
    idx.and_then(|i| state.keybinds.binds.get(i))
        .is_some_and(|b| !b.options.pass_through)
}

fn fire_scroll_bind_ticks(
    state: &mut KomaState,
    bind_idx: usize,
    tick_count: u8,
    is_bare_metal_tty: bool,
) -> bool {
    let Some(bind) = state.keybinds.binds.get(bind_idx).cloned() else {
        return false;
    };
    let consumed = !bind.options.pass_through;
    let Some(action) = bind.press_action else {
        return consumed;
    };

    for _ in 0..tick_count {
        let now_ms = state.elapsed_ms();
        if let Some(cd_ms) = bind.options.cooldown_ms {
            if let Some(&last_ms) = state.keybinds.cooldowns_ms.get(&bind.id)
                && now_ms.saturating_sub(last_ms) < cd_ms
            {
                continue;
            }
            state.keybinds.cooldowns_ms.insert(bind.id, now_ms);
        }
        execute_action_descriptor(state, action.clone(), is_bare_metal_tty);
    }

    consumed
}

/// Ensures Smithay's `PointerHandle` hover target matches the current Z-order hit-tested surface
/// (LockSurface > Overlay > Top > windows > Bottom > Background) before forwarding pointer events.
pub(crate) fn refresh_pointer_hover_focus(state: &mut KomaState, time: InputTime) {
    if state.has_active_locked_pointer_constraint() {
        return;
    }
    let Some(pointer) = state.ptr() else {
        return;
    };
    if pointer.is_grabbed() {
        return;
    }
    let pos = state.pointer_location;
    let under = state.surface_under(pos);
    let current_ok = match (&under, pointer.current_focus()) {
        (Some((surf_a, _)), Some(surf_b)) => surf_a == &surf_b,
        (None, None) => true,
        _ => false,
    };
    if !current_ok {
        let serial = SERIAL_COUNTER.next_serial();
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

/// Executes any `ActionDescriptor` (`koma.act.*` fast-path OR `LuaCallback` coroutine!).
pub fn execute_action_descriptor(
    state: &mut KomaState,
    action: ActionDescriptor,
    _is_bare_metal_tty: bool,
) {
    match action {
        ActionDescriptor::Spawn { argv } => {
            let Some((prog, args)) = argv.split_first() else {
                return;
            };
            let active_ws = state.effective_interactive_ws_id();
            let mut cmd = Command::new(prog);
            cmd.args(args);
            state.configure_child_env(&mut cmd, Some(&active_ws));
            if let Err(err) = crate::child_reaper::spawn_detached(&mut cmd, "keybind.spawn") {
                // The built-in `Mod + Return` terminal is Bitty. Keep a single
                // conservative fallback so a machine without it still gets a shell,
                // and log both failures instead of failing silently.
                if prog == "bitty" && args.is_empty() {
                    let mut fallback = Command::new("foot");
                    state.configure_child_env(&mut fallback, Some(&active_ws));
                    if let Err(fallback_err) =
                        crate::child_reaper::spawn_detached(&mut fallback, "keybind.spawn.foot")
                    {
                        warn!(
                            program = %prog,
                            error = %err,
                            fallback_error = %fallback_err,
                            "failed to spawn the default terminal or its `foot` fallback; install Bitty or bind your own terminal"
                        );
                    }
                } else {
                    warn!(
                        program = %prog,
                        error = %err,
                        "failed to spawn the program bound to this key; check that it is installed and on PATH"
                    );
                }
            }
        }

        ActionDescriptor::WindowClose => {
            let ws_id = state.effective_interactive_ws_id();
            let active_id = state
                .workspaces
                .get(&ws_id)
                .and_then(WorkspaceState::active_window);
            if let Some(id) = active_id
                && let Some(m) = state.mapped_windows.iter().find(|m| m.win_id == id)
                && let Some(t) = m.window.toplevel()
            {
                t.send_close();
            }
        }

        ActionDescriptor::WindowFloatToggle => {
            let ws_id = state.effective_interactive_ws_id();
            let active_id = state
                .workspaces
                .get(&ws_id)
                .and_then(WorkspaceState::active_window);
            let Some(win_id) = active_id else {
                return;
            };
            let pref = state
                .mapped_windows
                .iter()
                .find(|m| m.win_id == win_id)
                .map(|m| m.preferred_size);
            let cursor = LogicalPoint {
                x: state.pointer_location.x,
                y: state.pointer_location.y,
            };
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                ws.toggle_floating(win_id, pref, Some(cursor), now_ms);
            }
            state.sync_workspace_geometries(&ws_id);
            state.focus_active_deck_window();
            state.queue_redraw();
        }

        ActionDescriptor::WindowPseudotileToggle => {
            let ws_id = state.effective_interactive_ws_id();
            let active_id = state
                .workspaces
                .get(&ws_id)
                .and_then(WorkspaceState::active_window);
            let Some(win_id) = active_id else {
                return;
            };
            let pref = state
                .mapped_windows
                .iter()
                .find(|m| m.win_id == win_id)
                .map(|m| m.preferred_size)
                .unwrap_or((800, 560));
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                ws.toggle_pseudotile(win_id, pref);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::WindowFullscreenToggle => {
            let ws_id = state.effective_interactive_ws_id();
            let active_id = state
                .workspaces
                .get(&ws_id)
                .and_then(WorkspaceState::active_window);
            if let Some(id) = active_id {
                if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                    ws.toggle_fullscreen(id);
                }
                state.sync_workspace_geometries(&ws_id);
                state.sync_foreign_toplevels();
                state.queue_redraw();
            }
        }

        ActionDescriptor::WindowMaximizeToggle => {
            let ws_id = state.effective_interactive_ws_id();
            let active_id = state
                .workspaces
                .get(&ws_id)
                .and_then(WorkspaceState::active_window);
            if let Some(id) = active_id {
                state.toggle_window_maximized(id);
            }
        }

        ActionDescriptor::WindowMoveToWorkspace { workspace, follow } => {
            let dst_ws_id = state.ensure_workspace_by_name_or_id(&workspace);
            let src_ws_id = state.effective_interactive_ws_id();
            let _ = state
                .move_active_window_between_workspaces_with_follow(&src_ws_id, &dst_ws_id, follow);
        }

        ActionDescriptor::WindowMoveToMonitor(target_monitor) => {
            let _ = state.move_active_window_to_output(&target_monitor);
        }

        ActionDescriptor::WindowMoveToMonitorDirection(dir_raw) => {
            if let Some(dir) = CardinalDir::parse(&dir_raw)
                && let Some(cur_out) = state.focused_output_name()
                && let Some(next_out) = state
                    .output_layout
                    .neighbor_in_direction(&cur_out, dir)
                    .map(str::to_owned)
            {
                let _ = state.move_active_window_to_output(&next_out);
            }
        }

        ActionDescriptor::MonitorFocus(target_monitor) => {
            let _ = state.focus_output(&target_monitor);
        }

        ActionDescriptor::MonitorFocusDirection(dir_raw) => {
            if let Some(dir) = CardinalDir::parse(&dir_raw)
                && let Some(cur_out) = state.focused_output_name()
                && let Some(next_out) = state
                    .output_layout
                    .neighbor_in_direction(&cur_out, dir)
                    .map(str::to_owned)
            {
                let _ = state.focus_output(&next_out);
            }
        }

        ActionDescriptor::FocusDirection(dir_raw) => {
            let Some(dir) = CardinalDir::parse(&dir_raw) else {
                return;
            };
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                ws.focus_direction(dir, now_ms);
            }
            state.sync_workspace_geometries(&ws_id);
            state.focus_active_deck_window();
            state.queue_redraw();
        }

        ActionDescriptor::WindowSwapDirection(dir_raw) => {
            let Some(dir) = CardinalDir::parse(&dir_raw) else {
                return;
            };
            let ws_id = state.effective_interactive_ws_id();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && let Some(cur) = ws.active_window()
            {
                let vp = ws.viewport;
                ws.dwindle.swap_in_direction(cur, dir, vp);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::WindowMoveDirection(dir_raw) => {
            let Some(dir) = CardinalDir::parse(&dir_raw) else {
                return;
            };
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                ws.move_window_direction(dir, now_ms);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingSwitchPresetWidth { forward } => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && ws.layout_mode == koma_model::LayoutMode::Scrolling
            {
                let vp = ws.viewport;
                let _ = ws.scrolling.switch_preset_column_width(forward, vp, now_ms);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingMaximizeColumn => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && ws.layout_mode == koma_model::LayoutMode::Scrolling
            {
                let vp = ws.viewport;
                let _ = ws.scrolling.toggle_maximize_column(vp, now_ms);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingCenterColumn => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && ws.layout_mode == koma_model::LayoutMode::Scrolling
            {
                let vp = ws.viewport;
                let _ = ws.scrolling.center_active_column(vp, now_ms);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingMoveColumnLeft => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                let vp = ws.viewport;
                let _ = ws.scrolling.move_column_left(vp, now_ms);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingMoveColumnRight => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                let vp = ws.viewport;
                let _ = ws.scrolling.move_column_right(vp, now_ms);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingSwitchPresetHeight { forward } => {
            let ws_id = state.effective_interactive_ws_id();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && ws.layout_mode == koma_model::LayoutMode::Scrolling
            {
                let vp = ws.viewport;
                let _ = ws.scrolling.switch_preset_window_height(forward, vp);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingResetWindowHeight => {
            let ws_id = state.effective_interactive_ws_id();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && ws.layout_mode == koma_model::LayoutMode::Scrolling
            {
                let _ = ws.scrolling.reset_window_height();
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingToggleColumnTabbed => {
            let ws_id = state.effective_interactive_ws_id();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && ws.layout_mode == koma_model::LayoutMode::Scrolling
            {
                let _ = ws.scrolling.toggle_column_tabbed_display();
                ws.focused_window = ws.scrolling.active_window();
            }
            state.sync_workspace_geometries(&ws_id);
            state.focus_active_deck_window();
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingConsumeIntoColumn => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && ws.layout_mode == koma_model::LayoutMode::Scrolling
            {
                let vp = ws.viewport;
                if ws.scrolling.consume_window_into_column(vp, now_ms) {
                    ws.focused_window = ws.scrolling.active_window();
                }
            }
            state.sync_workspace_geometries(&ws_id);
            state.focus_active_deck_window();
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingExpelFromColumn => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && ws.layout_mode == koma_model::LayoutMode::Scrolling
            {
                let vp = ws.viewport;
                if ws.scrolling.expel_window_from_column(vp, now_ms) {
                    ws.focused_window = ws.scrolling.active_window();
                }
            }
            state.sync_workspace_geometries(&ws_id);
            state.focus_active_deck_window();
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingConsumeOrExpelLeft => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                match ws.layout_mode {
                    koma_model::LayoutMode::Scrolling => {
                        let vp = ws.viewport;
                        if ws.scrolling.consume_or_expel_window_left(vp, now_ms) {
                            ws.focused_window = ws.scrolling.active_window();
                        }
                    }
                    koma_model::LayoutMode::Deck => {
                        ws.focused_window = ws.deck.cycle_prev();
                    }
                    koma_model::LayoutMode::Dwindle => {
                        let _ = ws.move_window_direction(CardinalDir::Left, now_ms);
                    }
                }
            }
            state.sync_workspace_geometries(&ws_id);
            state.focus_active_deck_window();
            state.queue_redraw();
        }

        ActionDescriptor::ScrollingConsumeOrExpelRight => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                match ws.layout_mode {
                    koma_model::LayoutMode::Scrolling => {
                        let vp = ws.viewport;
                        if ws.scrolling.consume_or_expel_window_right(vp, now_ms) {
                            ws.focused_window = ws.scrolling.active_window();
                        }
                    }
                    koma_model::LayoutMode::Deck => {
                        ws.focused_window = ws.deck.cycle_next();
                    }
                    koma_model::LayoutMode::Dwindle => {
                        let _ = ws.move_window_direction(CardinalDir::Right, now_ms);
                    }
                }
            }
            state.sync_workspace_geometries(&ws_id);
            state.focus_active_deck_window();
            state.queue_redraw();
        }

        ActionDescriptor::DwindleToggleSplit => {
            let ws_id = state.effective_interactive_ws_id();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && let Some(cur) = ws.active_window()
            {
                let vp = ws.viewport;
                ws.dwindle.toggle_split(cur, vp);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::DwindleSwapSplit => {
            let ws_id = state.effective_interactive_ws_id();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && let Some(cur) = ws.active_window()
            {
                let vp = ws.viewport;
                ws.dwindle.swap_split(cur, vp);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::DwindleSplitRatio(delta) => {
            let ws_id = state.effective_interactive_ws_id();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && let Some(cur) = ws.active_window()
            {
                let vp = ws.viewport;
                ws.dwindle.adjust_split_ratio(cur, delta, vp);
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::ScratchpadToggle(opt_name) => {
            let sp_id = state.ensure_special_workspace(opt_name.as_deref());
            if state.active_special_ws_id.as_ref() == Some(&sp_id) {
                state.active_special_ws_id = None;
            } else {
                state.active_special_ws_id = Some(sp_id.clone());
                state.sync_workspace_geometries(&sp_id);
            }
            state.focus_active_deck_window();
            // Toggling the overlay changes which workspace is displayed, outside `focus_workspace`.
            crate::ext_workspace::sync_ext_workspaces(state);
            state.queue_redraw();
        }

        ActionDescriptor::ScratchpadMoveWindow { name, silent } => {
            let sp_id = state.ensure_special_workspace(name.as_deref());
            let src_ws_id = state.effective_interactive_ws_id();
            if src_ws_id == sp_id {
                let dst_ws_id = state.active_ws_id.clone();
                let _ = state
                    .move_active_window_between_workspaces_with_follow(&sp_id, &dst_ws_id, false);
            } else {
                let _ = state
                    .move_active_window_between_workspaces_with_follow(&src_ws_id, &sp_id, false);
                if !silent {
                    state.active_special_ws_id = Some(sp_id);
                }
            }
            state.focus_active_deck_window();
            // A non-silent stash also opens the overlay, so republish the displayed workspace.
            crate::ext_workspace::sync_ext_workspaces(state);
            state.queue_redraw();
        }

        ActionDescriptor::DeckCyclePrev => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                match ws.layout_mode {
                    koma_model::LayoutMode::Scrolling => {
                        let vp = ws.viewport;
                        ws.focused_window = ws.scrolling.focus_prev(vp, now_ms);
                    }
                    koma_model::LayoutMode::Dwindle => {
                        ws.focus_direction(CardinalDir::Left, now_ms);
                    }
                    koma_model::LayoutMode::Deck => {
                        ws.focused_window = ws.deck.cycle_prev();
                    }
                }
            }
            state.sync_workspace_geometries(&ws_id);
            state.focus_active_deck_window();
            state.queue_redraw();
        }

        ActionDescriptor::DeckCycleNext | ActionDescriptor::ScrollStripSwipe => {
            let ws_id = state.effective_interactive_ws_id();
            let now_ms = state.elapsed_ms();
            if let Some(ws) = state.workspaces.get_mut(&ws_id) {
                match ws.layout_mode {
                    koma_model::LayoutMode::Scrolling => {
                        let vp = ws.viewport;
                        ws.focused_window = ws.scrolling.focus_next(vp, now_ms);
                    }
                    koma_model::LayoutMode::Dwindle => {
                        ws.focus_direction(CardinalDir::Right, now_ms);
                    }
                    koma_model::LayoutMode::Deck => {
                        ws.focused_window = ws.deck.cycle_next();
                    }
                }
            }
            state.sync_workspace_geometries(&ws_id);
            state.focus_active_deck_window();
            state.queue_redraw();
        }

        ActionDescriptor::OverviewToggle | ActionDescriptor::OverviewOpen => {
            // The daemon is supervised by the compositor: the toggle is written straight to its
            // control socket, a dead daemon is restarted with backoff, and the press is delivered
            // once it answers (komawm/koma#75).
            crate::overview_daemon::request_toggle(state);
        }

        ActionDescriptor::WorkspaceFocus(target) | ActionDescriptor::WorkspacePeek(target) => {
            let ws_id = state.ensure_workspace_by_name_or_id(&target);
            let _ = state.focus_workspace(&ws_id);
        }

        ActionDescriptor::WorkspaceFocusNext => {
            cycle_workspace_focus(state, true);
        }

        ActionDescriptor::WorkspaceFocusPrev => {
            cycle_workspace_focus(state, false);
        }

        ActionDescriptor::WindowFocus(win_slot) => {
            if !state.activate_window_by_slot(win_slot) {
                warn!(win_slot, "WindowFocus target window not found");
            }
        }

        ActionDescriptor::WorkspaceSetLayout { workspace, layout } => {
            if !state.set_workspace_layout(workspace.as_deref(), layout) {
                warn!(
                    ?workspace,
                    ?layout,
                    "WorkspaceSetLayout target workspace not found"
                );
            }
        }

        ActionDescriptor::ModeEnter(mode_name) => {
            debug!(mode = %mode_name, "entering keybind submap mode");
            state.keybinds.active_mode = Some(mode_name.clone());
            let ws_id = state.active_ws_id.clone();
            let epoch = state
                .workspaces
                .get(&ws_id)
                .map(|w| w.lease.epoch())
                .unwrap_or_default();
            let now_ms = state.elapsed_ms();
            let snap = state.capture_lua_desktop_snapshot(Some(&ws_id));
            state.lua_sandbox.set_desktop_snapshot(snap);
            match state
                .lua_sandbox
                .start_mode_enter_hook_tx(&mode_name, ws_id, epoch, now_ms)
            {
                Ok(Some(_)) => {
                    execute_queued_host_actions(state);
                    rearm_trampoline_timer(state);
                }
                Ok(None) => {}
                Err(err) => warn!(mode = %mode_name, "koma.mode on_enter callback error: {err}"),
            }
        }

        ActionDescriptor::ModeExit => {
            debug!(prev_mode = ?state.keybinds.active_mode, "exited keybind submap mode");
            state.keybinds.active_mode = None;
        }

        ActionDescriptor::ReloadConfig => {
            if let Err(err) = crate::config_watcher::reload_active_config_now(state, None) {
                warn!("manual ReloadConfig hotkey failed: {err}");
            }
        }

        ActionDescriptor::CompositorQuit => {
            info!("CompositorQuit action triggered; stopping Koma event loop");
            state.loop_signal.stop();
        }

        ActionDescriptor::RunMacro { macro_id, args } => {
            let ws_id = state.active_ws_id.clone();
            let epoch = state
                .workspaces
                .get(&ws_id)
                .map(|w| w.lease.epoch())
                .unwrap_or_default();
            let now_ms = state.elapsed_ms();
            let snap = state.capture_lua_desktop_snapshot(Some(&ws_id));
            state.lua_sandbox.set_desktop_snapshot(snap);
            match state
                .lua_sandbox
                .start_macro_tx(ws_id, epoch, &macro_id, &args, now_ms)
            {
                Ok(_) => {
                    execute_queued_host_actions(state);
                    rearm_trampoline_timer(state);
                }
                Err(err) => warn!(%macro_id, "keybind RunMacro error: {err}"),
            }
        }

        ActionDescriptor::LuaCallback(bind_id) => {
            let ws_id = state.active_ws_id.clone();
            let epoch = state
                .workspaces
                .get(&ws_id)
                .map(|w| w.lease.epoch())
                .unwrap_or_default();
            let now_ms = state.elapsed_ms();
            let snap = state.capture_lua_desktop_snapshot(Some(&ws_id));
            state.lua_sandbox.set_desktop_snapshot(snap);
            match state
                .lua_sandbox
                .start_bind_callback_tx(bind_id, ws_id, epoch, now_ms)
            {
                Ok(mode) => {
                    debug!(
                        bind_id = bind_id.0,
                        ?mode,
                        "executed Lua keybind/gesture coroutine callback"
                    );
                    execute_queued_host_actions(state);
                    rearm_trampoline_timer(state);
                }
                Err(err) => warn!(
                    bind_id = bind_id.0,
                    "Lua keybind/gesture callback failed: {err}"
                ),
            }
        }

        ActionDescriptor::WindowResizeDelta { width, height } => {
            let ws_id = state.effective_interactive_ws_id();
            if let Some(ws) = state.workspaces.get_mut(&ws_id)
                && let Some(cur) = ws.active_window()
            {
                let vp = ws.viewport;
                if ws.floating.contains(cur) {
                    ws.floating.resize_by(cur, width, height, vp);
                } else {
                    ws.dwindle
                        .resize_target(cur, f64::from(width), f64::from(height), vp);
                }
            }
            state.sync_workspace_geometries(&ws_id);
            state.queue_redraw();
        }

        ActionDescriptor::WindowDragMove => {
            let serial = SERIAL_COUNTER.next_serial();
            let _ = start_compositor_move_grab(state, BTN_LEFT, serial);
        }

        ActionDescriptor::WindowDragResize => {
            let serial = SERIAL_COUNTER.next_serial();
            let _ = start_compositor_resize_grab(state, BTN_RIGHT, serial);
        }
    }
}

fn cycle_workspace_focus(state: &mut KomaState, forward: bool) {
    let mut ids: Vec<WorkspaceId> = state
        .workspaces
        .iter()
        .filter(|(_, ws)| {
            !ws.is_special && matches!(ws.presentation, PresentationTarget::Physical(_))
        })
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    if ids.len() <= 1 {
        return;
    }
    let cur_pos = ids
        .iter()
        .position(|id| id == &state.active_ws_id)
        .unwrap_or(0);
    let next_pos = if forward {
        (cur_pos + 1) % ids.len()
    } else {
        (cur_pos + ids.len() - 1) % ids.len()
    };
    // Route through the single workspace-switch path: it performs the output mapping, geometry
    // resync, transition, `workspace.focused` event and `ext-workspace-v1` sync, so a bar that
    // drives `ext/workspaces` stays in step with the keybind.
    let next_ws = ids[next_pos].clone();
    state.focus_workspace_with_direction(&next_ws, if forward { 1 } else { -1 });
}

#[cfg(test)]
mod tests {
    use koma_lua::KEYBIND_CATALOG;

    use super::*;
    use crate::keybind_repeat::RepeatTiming;

    #[test]
    fn scroll_tracker_accumulates_fractional_v120_and_resets_on_reversal() {
        let mut tracker = ScrollTracker::new(WHEEL_V120_TICK);
        assert_eq!(tracker.accumulate(40.0), 0);
        assert_eq!(tracker.accumulate(50.0), 0);
        assert_eq!(tracker.accumulate(40.0), 1);
        assert_eq!(tracker.accumulate(-30.0), 0);
        assert_eq!(tracker.accumulate(-95.0), -1);
    }

    #[test]
    fn parse_wheel_touchpad_and_mouse_button_chords() {
        let c1 = parse_chord_spec("Mod + WheelScrollDown").unwrap();
        assert_eq!(c1.mods, MOD_COMPOSITOR);
        assert_eq!(
            c1.trigger,
            BindTrigger::Scroll(ScrollTriggerKind::WheelScrollDown)
        );

        let c2 = parse_chord_spec("Mod + Shift + TouchpadScrollLeft").unwrap();
        assert_eq!(c2.mods, MOD_COMPOSITOR | MOD_SHIFT);
        assert_eq!(
            c2.trigger,
            BindTrigger::Scroll(ScrollTriggerKind::TouchpadScrollLeft)
        );

        let m_left = parse_chord_spec("Mod + MouseLeft").unwrap();
        assert_eq!(m_left.mods, MOD_COMPOSITOR);
        assert_eq!(
            m_left.trigger,
            BindTrigger::MouseButton(MouseButtonTrigger::Left)
        );

        let m_right = parse_chord_spec("Mod + mouse:273").unwrap();
        assert_eq!(m_right.mods, MOD_COMPOSITOR);
        assert_eq!(
            m_right.trigger,
            BindTrigger::MouseButton(MouseButtonTrigger::Right)
        );
    }

    #[test]
    fn builtin_mod_return_spawns_the_promoted_default_terminal() {
        let defaults = KeybindTable::with_builtin_defaults();
        let chord = parse_chord_spec("Mod + Return").unwrap();
        let bind = defaults
            .binds
            .iter()
            .find(|b| b.chord == chord)
            .expect("Mod + Return must stay bound in the built-in map");
        assert_eq!(
            bind.press_action,
            Some(ActionDescriptor::Spawn {
                argv: vec!["bitty".to_owned()]
            })
        );
    }

    #[test]
    fn builtin_mod_m_maximizes_and_mod_f_stays_fullscreen() {
        let defaults = KeybindTable::with_builtin_defaults();
        let action_for = |spec: &str| {
            let chord = parse_chord_spec(spec).expect("valid chord");
            defaults
                .binds
                .iter()
                .find(|b| b.chord == chord)
                .and_then(|b| b.press_action.clone())
        };

        assert_eq!(
            action_for("Mod + M"),
            Some(ActionDescriptor::WindowMaximizeToggle),
            "Mod + M must toggle the maximize presentation state"
        );
        assert_eq!(
            action_for("Mod + F"),
            Some(ActionDescriptor::WindowFullscreenToggle),
            "Mod + F must stay fullscreen"
        );
        assert_eq!(
            action_for("Mod + Shift + M"),
            Some(ActionDescriptor::ScrollingMaximizeColumn),
            "the scrolling-only maximize column action must remain reachable"
        );
    }

    #[test]
    fn builtin_defaults_include_hyprland_workspace_1_to_9_and_compile_with_options_overrides_and_unbinds()
     {
        let defaults = KeybindTable::with_builtin_defaults();
        for idx in 1..=9 {
            let ws = idx.to_string();
            let focus_chord = parse_chord_spec(&format!("Mod + {ws}")).unwrap();
            let move_chord = parse_chord_spec(&format!("Mod + Shift + {ws}")).unwrap();
            assert!(defaults.binds.iter().any(|b| {
                b.chord == focus_chord
                    && b.press_action == Some(ActionDescriptor::WorkspaceFocus(ws.clone()))
            }));
            assert!(defaults.binds.iter().any(|b| {
                b.chord == move_chord
                    && b.press_action
                        == Some(ActionDescriptor::WindowMoveToWorkspace {
                            workspace: ws.clone(),
                            follow: true,
                        })
            }));
        }

        // Override Mod + Return to foot and unbind Mod + Q while keeping other defaults
        let custom_spec = BindSpec {
            id: BindHandleId(500),
            mode: None,
            enabled: true,
            chord: "Mod + Return".to_owned(),
            action: ActionDescriptor::Spawn {
                argv: vec!["foot".to_owned()],
            },
            release_action: None,
            options: BindOptions::default(),
        };
        let table = KeybindTable::compile_with_options(
            std::slice::from_ref(&custom_spec),
            &["Mod + Q".to_owned()],
            true,
            Some("Alt"),
        )
        .unwrap();

        assert_eq!(table.mod_key_override, Some(PhysicalModKey::Alt));
        let q_chord = parse_chord_spec("Mod + Q").unwrap();
        assert!(
            !table.binds.iter().any(|b| b.chord == q_chord),
            "Mod + Q should be removed by unbinds"
        );
        let ret_chord = parse_chord_spec("Mod + Return").unwrap();
        let ret_binds: Vec<_> = table
            .binds
            .iter()
            .filter(|b| b.chord == ret_chord)
            .collect();
        assert_eq!(
            ret_binds.len(),
            1,
            "Mod + Return should be overridden in-place without duplication"
        );
        assert_eq!(
            ret_binds[0].press_action,
            Some(ActionDescriptor::Spawn {
                argv: vec!["foot".to_owned()]
            })
        );

        // When inherit_default_binds = false, only explicit user binds (+ Mod + MouseLeft/Right) remain
        let minimal =
            KeybindTable::compile_with_options(&[custom_spec], &[], false, Some("Super")).unwrap();
        assert_eq!(minimal.binds.len(), 3);
    }

    #[test]
    fn locked_session_allows_only_allow_when_locked_and_vt_switch() {
        assert!(is_keybind_allowed_for_lock_state(false, false));
        assert!(is_keybind_allowed_for_lock_state(false, true));
        assert!(!is_keybind_allowed_for_lock_state(true, false));
        assert!(is_keybind_allowed_for_lock_state(true, true));

        assert_eq!(
            parse_vt_switch_keysym(keysyms::KEY_XF86Switch_VT_1),
            Some(1)
        );
        assert_eq!(
            parse_vt_switch_keysym(keysyms::KEY_XF86Switch_VT_12),
            Some(12)
        );
        assert_eq!(parse_vt_switch_keysym(keysyms::KEY_Return), None);
        assert_eq!(parse_vt_switch_keysym(keysyms::KEY_g), None);

        let allowed_opts: BindOptions =
            serde_json::from_str(r#"{"allow_when_locked": true}"#).expect("valid BindOptions JSON");
        assert!(allowed_opts.allow_when_locked);
        let default_opts: BindOptions =
            serde_json::from_str(r#"{}"#).expect("default BindOptions JSON");
        assert!(!default_opts.allow_when_locked);

        let specs = vec![
            BindSpec {
                id: BindHandleId(700),
                mode: None,
                enabled: true,
                chord: "XF86AudioRaiseVolume".to_owned(),
                action: ActionDescriptor::Spawn {
                    argv: vec!["wpctl".to_owned(), "set-volume".to_owned()],
                },
                release_action: None,
                options: allowed_opts,
            },
            BindSpec {
                id: BindHandleId(701),
                mode: None,
                enabled: true,
                chord: "Mod + Return".to_owned(),
                action: ActionDescriptor::Spawn {
                    argv: vec!["foot".to_owned()],
                },
                release_action: None,
                options: default_opts,
            },
        ];
        let table = KeybindTable::compile_with_options(&specs, &[], false, Some("Super"))
            .expect("keybind compilation succeeds");

        let vol = table
            .binds
            .iter()
            .find(|b| b.id == BindHandleId(700))
            .expect("volume bind present");
        assert!(vol.options.allow_when_locked);
        assert!(is_keybind_allowed_for_lock_state(
            true,
            vol.options.allow_when_locked
        ));

        let term = table
            .binds
            .iter()
            .find(|b| b.id == BindHandleId(701))
            .expect("terminal bind present");
        assert!(!term.options.allow_when_locked);
        assert!(!is_keybind_allowed_for_lock_state(
            true,
            term.options.allow_when_locked
        ));
    }

    #[test]
    fn parses_macbook_and_laptop_xf86_media_brightness_and_print_keysyms() {
        let cases = [
            ("XF86AudioRaiseVolume", keysyms::KEY_XF86AudioRaiseVolume),
            ("xf86audioraisevolume", keysyms::KEY_XF86AudioRaiseVolume),
            ("AudioRaiseVolume", keysyms::KEY_XF86AudioRaiseVolume),
            ("XF86AudioLowerVolume", keysyms::KEY_XF86AudioLowerVolume),
            ("AudioLowerVolume", keysyms::KEY_XF86AudioLowerVolume),
            ("XF86AudioMute", keysyms::KEY_XF86AudioMute),
            ("AudioMute", keysyms::KEY_XF86AudioMute),
            ("XF86AudioMicMute", keysyms::KEY_XF86AudioMicMute),
            ("AudioMicMute", keysyms::KEY_XF86AudioMicMute),
            ("XF86AudioPlay", keysyms::KEY_XF86AudioPlay),
            ("AudioPlay", keysyms::KEY_XF86AudioPlay),
            ("XF86AudioPause", keysyms::KEY_XF86AudioPause),
            ("AudioPause", keysyms::KEY_XF86AudioPause),
            ("XF86AudioNext", keysyms::KEY_XF86AudioNext),
            ("AudioNext", keysyms::KEY_XF86AudioNext),
            ("XF86AudioPrev", keysyms::KEY_XF86AudioPrev),
            ("XF86AudioPrevious", keysyms::KEY_XF86AudioPrev),
            ("AudioPrev", keysyms::KEY_XF86AudioPrev),
            ("XF86AudioStop", keysyms::KEY_XF86AudioStop),
            ("AudioStop", keysyms::KEY_XF86AudioStop),
            ("XF86MonBrightnessUp", keysyms::KEY_XF86MonBrightnessUp),
            ("MonBrightnessUp", keysyms::KEY_XF86MonBrightnessUp),
            ("XF86MonBrightnessDown", keysyms::KEY_XF86MonBrightnessDown),
            ("MonBrightnessDown", keysyms::KEY_XF86MonBrightnessDown),
            ("XF86KbdBrightnessUp", keysyms::KEY_XF86KbdBrightnessUp),
            ("KbdBrightnessUp", keysyms::KEY_XF86KbdBrightnessUp),
            ("XF86KbdBrightnessDown", keysyms::KEY_XF86KbdBrightnessDown),
            ("KbdBrightnessDown", keysyms::KEY_XF86KbdBrightnessDown),
            ("Print", keysyms::KEY_Print),
            ("print_screen", keysyms::KEY_Print),
            ("PrintScreen", keysyms::KEY_Print),
        ];

        for (token, expected_raw) in cases {
            let (sym, _) = resolve_xkb_keysym_token(token)
                .unwrap_or_else(|e| panic!("failed resolving `{token}`: {e}"));
            assert_eq!(sym.raw(), expected_raw, "token `{token}` mismatch");

            let chord = parse_chord_spec(token)
                .unwrap_or_else(|e| panic!("failed parsing chord `{token}`: {e}"));
            match chord.trigger {
                BindTrigger::Keysym { sym, .. } => assert_eq!(
                    sym.raw(),
                    expected_raw,
                    "chord trigger mismatch for `{token}`"
                ),
                other => panic!("expected Keysym trigger for `{token}`, got {other:?}"),
            }
        }
    }

    /// Cross-checks the single Rust keybind catalogue against the runtime fallback
    /// keymap. A catalogue entry without a matching `with_builtin_defaults()` bind,
    /// or a built-in bind missing from the catalogue, fails this test.
    #[test]
    fn builtin_keybind_catalog_matches_runtime_defaults() {
        use std::collections::BTreeSet;

        use koma_lua::KEYBIND_CATALOG;

        let defaults = KeybindTable::with_builtin_defaults();
        assert_eq!(
            defaults.binds.len(),
            KEYBIND_CATALOG.len(),
            "the catalogue and `with_builtin_defaults()` disagree on the number of binds"
        );

        let mut catalog_ids = BTreeSet::new();
        for entry in KEYBIND_CATALOG {
            assert!(
                catalog_ids.insert(entry.id),
                "duplicate catalogue id {} for `{}`",
                entry.id,
                entry.chord
            );
            let bind = defaults
                .binds
                .iter()
                .find(|bind| bind.raw_chord == entry.chord)
                .unwrap_or_else(|| {
                    panic!(
                        "catalogue chord `{}` is missing from `with_builtin_defaults()`",
                        entry.chord
                    )
                });
            assert_eq!(
                bind.id.0, entry.id,
                "catalogue id for `{}` does not match the runtime bind id",
                entry.chord
            );
            assert_eq!(
                bind.options.cooldown_ms, entry.cooldown_ms,
                "catalogue cooldown for `{}` does not match the runtime bind",
                entry.chord
            );
            assert!(
                bind.press_action.is_some(),
                "built-in bind `{}` has no press action",
                entry.chord
            );
        }

        for bind in &defaults.binds {
            assert!(
                KEYBIND_CATALOG
                    .iter()
                    .any(|entry| entry.chord == bind.raw_chord),
                "built-in bind `{}` is missing from the keybind catalogue",
                bind.raw_chord
            );
        }
    }

    /// The generated `keybinds.lua` must reproduce exactly the runtime fallback
    /// actions, so `inherit_default_binds = false` is a true full expansion.
    #[test]
    fn scaffolded_keymap_reproduces_every_builtin_action() {
        use std::{collections::BTreeMap, fs, path::PathBuf};

        use koma_lua::{
            DEFAULT_HEAP_LIMIT_BYTES, KEYBIND_CATALOG, phodopus_bridge::PhodopusSandbox,
            scaffold_config_tree,
        };

        let root: PathBuf = std::path::Path::new("/tmp/koma").join(format!(
            "action-parity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos())
        ));
        let _ = fs::remove_dir_all(&root);
        scaffold_config_tree(&root, false).expect("scaffold config tree");

        let init_lua = root.join("init.lua");
        let source = fs::read_to_string(&init_lua).expect("read init.lua");
        let mut sandbox = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, Some(&root))
            .expect("build sandbox with generated modules");
        let registry = sandbox
            .eval_init_lua(&source)
            .expect("generated tree must evaluate");
        let _ = fs::remove_dir_all(&root);

        assert_eq!(
            registry.binds.len(),
            KEYBIND_CATALOG.len(),
            "generated keymap must bind every catalogue entry"
        );
        assert!(
            !registry.input.inherit_default_binds,
            "the scaffold must disable inherited built-in binds"
        );
        assert!(
            registry.monitors.is_empty(),
            "the scaffold must not declare a monitor block"
        );

        let generated: BTreeMap<&str, &ActionDescriptor> = registry
            .binds
            .iter()
            .map(|bind| (bind.chord.as_str(), &bind.action))
            .collect();
        let defaults = KeybindTable::with_builtin_defaults();
        for bind in &defaults.binds {
            let action = generated
                .get(bind.raw_chord.as_str())
                .unwrap_or_else(|| panic!("generated keymap is missing `{}`", bind.raw_chord));
            assert_eq!(
                *action,
                bind.press_action
                    .as_ref()
                    .expect("runtime bind press action"),
                "generated action for `{}` differs from the runtime default",
                bind.raw_chord
            );
        }
        assert_eq!(
            generated.len(),
            defaults.binds.len(),
            "generated keymap must not add binds beyond the built-in catalogue"
        );

        // The generated map must repeat exactly the catalogue's `WhileHeld` rows.
        for bind in &registry.binds {
            let entry = KEYBIND_CATALOG
                .iter()
                .find(|entry| entry.chord == bind.chord)
                .expect("generated bind has a catalogue entry");
            assert_eq!(
                bind.options.effective_repeat(&bind.action),
                entry.repeat,
                "generated repeat class for `{}` differs from the catalogue",
                bind.chord
            );
        }
    }

    fn find_bind<'a>(table: &'a KeybindTable, chord: &str) -> &'a CompiledBind {
        let chord = parse_chord_spec(chord).expect("valid chord");
        table
            .binds
            .iter()
            .find(|bind| bind.chord == chord)
            .unwrap_or_else(|| panic!("`{chord:?}` is missing from the compiled table"))
    }

    /// Simulates a physical press followed by a `hold_ms` hold at 1 ms resolution.
    ///
    /// The returned timestamps include the initial press at `0`; a `Once` bind therefore
    /// yields exactly one entry no matter how long the chord stays held.
    fn simulate_hold(bind: &CompiledBind, timing: RepeatTiming, hold_ms: u64) -> Vec<u64> {
        let mut machine = KeyRepeatState::default();
        machine.set_timing(timing);
        let mut dispatches = Vec::new();
        if bind.press_action.is_some() {
            dispatches.push(0);
        }
        if let Some(request) = bind.repeat_request(42, bind.chord.mods) {
            machine.arm(request, 0);
        }
        // `hold_ms` is the release instant: a repeat due exactly then never fires.
        for now_ms in 1..hold_ms {
            if machine.take_due(now_ms).is_some() {
                dispatches.push(now_ms);
            }
        }
        machine.cancel();
        assert_eq!(
            machine.take_due(hold_ms + 5_000),
            None,
            "no repeat may fire after release"
        );
        dispatches
    }

    #[test]
    fn once_binds_dispatch_exactly_once_while_held() {
        let table = KeybindTable::with_builtin_defaults();
        let timing = RepeatTiming::new(600, 25);
        for chord in [
            "Mod + Return",
            "Mod + Q",
            "Mod + Shift + E",
            "Mod + Shift + R",
        ] {
            let bind = find_bind(&table, chord);
            assert_eq!(
                bind.repeat,
                BindRepeat::Once,
                "`{chord}` must never repeat while held"
            );
            assert!(bind.repeat_request(42, bind.chord.mods).is_none());
            let dispatches = simulate_hold(bind, timing, 2_000);
            assert_eq!(
                dispatches,
                vec![0],
                "holding `{chord}` for 2 s must fire its action exactly once"
            );
        }
    }

    #[test]
    fn while_held_binds_repeat_at_the_configured_delay_and_rate() {
        let table = KeybindTable::with_builtin_defaults();
        let timing = RepeatTiming::new(600, 25);

        let focus = find_bind(&table, "Mod + H");
        assert_eq!(focus.repeat, BindRepeat::WhileHeld);
        let dispatches = simulate_hold(focus, timing, 1_000);
        assert_eq!(
            dispatches,
            std::iter::once(0)
                .chain((0..10).map(|i| 600 + i * 40))
                .collect::<Vec<_>>(),
            "a 1000 ms hold at 600 ms / 25 Hz is the press plus ten repeats"
        );

        let movement = find_bind(&table, "Mod + Shift + J");
        assert_eq!(movement.repeat, BindRepeat::WhileHeld);
        assert_eq!(simulate_hold(movement, timing, 700), vec![0, 600, 640, 680]);
    }

    #[test]
    fn release_stops_the_repeat_immediately() {
        let table = KeybindTable::with_builtin_defaults();
        let bind = find_bind(&table, "Mod + Shift + L");
        let mut machine = KeyRepeatState::default();
        machine.set_timing(RepeatTiming::new(600, 25));
        machine.arm(
            bind.repeat_request(42, bind.chord.mods)
                .expect("movement bind repeats"),
            0,
        );
        assert!(machine.take_due(600).is_some());
        assert_eq!(machine.next_deadline_ms(), Some(640));

        // Physical release of the repeating keycode cancels through the same path the
        // keyboard handler uses (`is_repeating_keycode` -> `cancel_keybind_repeat`).
        assert!(machine.is_repeating_keycode(42));
        assert!(machine.cancel().is_some());
        assert!(!machine.is_repeating_keycode(42));
        assert_eq!(machine.take_due(640), None);
        assert_eq!(machine.take_due(1_000), None);
    }

    #[test]
    fn explicit_repeat_option_overrides_the_action_class() {
        let forced_once = BindSpec {
            id: BindHandleId(900),
            mode: None,
            enabled: true,
            chord: "Mod + Shift + H".to_owned(),
            action: ActionDescriptor::WindowMoveDirection("left".to_owned()),
            release_action: None,
            options: BindOptions {
                repeat_while_held: Some(false),
                ..BindOptions::default()
            },
        };
        let forced_repeat = BindSpec {
            id: BindHandleId(901),
            mode: None,
            enabled: true,
            chord: "Mod + Return".to_owned(),
            action: ActionDescriptor::Spawn {
                argv: vec!["bitty".to_owned()],
            },
            release_action: None,
            options: BindOptions {
                repeat_while_held: Some(true),
                ..BindOptions::default()
            },
        };
        let table = KeybindTable::compile_with_options(
            &[forced_once, forced_repeat],
            &[],
            false,
            Some("Super"),
        )
        .expect("compile");

        assert_eq!(
            find_bind(&table, "Mod + Shift + H").repeat,
            BindRepeat::Once
        );
        assert_eq!(
            find_bind(&table, "Mod + Return").repeat,
            BindRepeat::WhileHeld
        );
        assert_eq!(
            simulate_hold(
                find_bind(&table, "Mod + Shift + H"),
                RepeatTiming::new(600, 25),
                1_000
            ),
            vec![0]
        );
        assert_eq!(
            simulate_hold(
                find_bind(&table, "Mod + Return"),
                RepeatTiming::new(600, 25),
                1_000
            )
            .len(),
            11,
            "an explicit `repeat = true` makes an otherwise one-shot action repeat"
        );
    }

    #[test]
    fn pass_through_binds_leave_repeat_to_the_focused_client() {
        let spec = BindSpec {
            id: BindHandleId(902),
            mode: None,
            enabled: true,
            chord: "Mod + H".to_owned(),
            action: ActionDescriptor::FocusDirection("left".to_owned()),
            release_action: None,
            options: BindOptions {
                pass_through: true,
                ..BindOptions::default()
            },
        };
        let table = KeybindTable::compile_with_options(&[spec], &[], false, Some("Super"))
            .expect("compile");
        let bind = find_bind(&table, "Mod + H");
        assert_eq!(bind.repeat, BindRepeat::WhileHeld);
        assert!(
            bind.repeat_request(42, bind.chord.mods).is_none(),
            "the forwarded key already repeats through `wl_keyboard.repeat_info`; \
             a compositor repeat would double-dispatch"
        );
        assert_eq!(
            simulate_hold(bind, RepeatTiming::new(600, 25), 1_000),
            vec![0]
        );
    }

    #[test]
    fn catalog_repeat_classes_match_the_runtime_derivation() {
        let defaults = KeybindTable::with_builtin_defaults();
        for bind in &defaults.binds {
            let entry = KEYBIND_CATALOG
                .iter()
                .find(|entry| entry.chord == bind.raw_chord)
                .unwrap_or_else(|| {
                    panic!("`{}` is missing from the keybind catalogue", bind.raw_chord)
                });
            assert_eq!(
                entry.repeat, bind.repeat,
                "catalogue repeat class for `{}` drifted from the action-derived class",
                bind.raw_chord
            );
        }
    }
}
