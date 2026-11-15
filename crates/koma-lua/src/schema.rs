use std::collections::BTreeMap;

use koma_model::{
    AgentCursorConfig, Anchor9, AnchorPoint, AnimationConfig, BlockOutPolicy, BlurConfig,
    BorderConfig, CubicBezier, CurveSpec, DimConfig, DwindleConfig, FocusRingConfig,
    FrameClockConfig, HookeSpringParams, LayoutMode, LogicalViewport, OpenMode, OutputTransform,
    PathSplineKind, PointerTrajectorySpec, SafeAreaInsets, ScrollingConfig, ShadowConfig,
};
use koma_proto::{KinematicProfileKind, MacroId, OutputId, TextInjectStrategy};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BindHandleId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RuleHandleId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MacroHandleId(pub u64);

/// Typed handle returned to Lua by `koma.bind`, `koma.rule.*`, and `koma.macro.define`
/// supporting `:set_enabled(bool)` and `:remove()` (`LUA_API_SPEC.md` §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KomaHandleRef {
    Bind(BindHandleId),
    WindowRule(RuleHandleId),
    LayerRule(RuleHandleId),
    Macro(MacroHandleId),
}

/// Pre-compiled fast-path compositor actions (`koma.act.*` in `LUA_API_SPEC.md` §3.1)
/// plus custom stashed Lua coroutine callbacks (`LuaCallback`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ActionDescriptor {
    Spawn {
        argv: Vec<String>,
    },
    WindowClose,
    WindowFullscreenToggle,
    WindowFloatToggle,
    WindowPseudotileToggle,
    WindowDragMove,
    WindowDragResize,
    WindowResizeDelta {
        width: i32,
        height: i32,
    },
    WindowMoveToWorkspace(String),
    WindowSwapDirection(String),
    WindowMoveDirection(String),
    FocusDirection(String),
    DwindleToggleSplit,
    DwindleSwapSplit,
    DwindleSplitRatio(f64),
    ScrollingSwitchPresetWidth {
        forward: bool,
    },
    ScrollingSwitchPresetHeight {
        forward: bool,
    },
    ScrollingResetWindowHeight,
    ScrollingToggleColumnTabbed,
    ScrollingMaximizeColumn,
    ScrollingCenterColumn,
    ScrollingMoveColumnLeft,
    ScrollingMoveColumnRight,
    ScrollingConsumeIntoColumn,
    ScrollingExpelFromColumn,
    ScrollingConsumeOrExpelLeft,
    ScrollingConsumeOrExpelRight,
    ScratchpadToggle(Option<String>),
    ScratchpadMoveWindow {
        name: Option<String>,
        silent: bool,
    },
    DeckCycleNext,
    DeckCyclePrev,
    OverviewToggle,
    OverviewOpen,
    ScrollStripSwipe,
    WorkspaceFocus(String),
    WorkspaceFocusNext,
    WorkspaceFocusPrev,
    WorkspacePeek(String),
    WorkspaceSetLayout {
        workspace: Option<String>,
        layout: LayoutMode,
    },
    WindowFocus(u64),
    MonitorFocus(String),
    MonitorFocusDirection(String),
    WindowMoveToMonitor(String),
    WindowMoveToMonitorDirection(String),
    ModeEnter(String),
    ModeExit,
    ReloadConfig,
    CompositorQuit,
    RunMacro {
        macro_id: MacroId,
        args: serde_json::Value,
    },
    /// Invokes a stashed Lua coroutine callback `function(ctx) ... end` registered via `koma.bind`.
    LuaCallback(BindHandleId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerEdge {
    #[default]
    Press,
    Release,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BindOptions {
    pub edge: TriggerEdge,
    pub repeat_key: bool,
    pub cooldown_ms: Option<u64>,
    pub allow_when_locked: bool,
    pub allow_inhibiting: bool,
    pub pass_through: bool,
    pub when_preempted: String,
    pub desc: Option<String>,
}

impl Default for BindOptions {
    fn default() -> Self {
        Self {
            edge: TriggerEdge::Press,
            repeat_key: false,
            cooldown_ms: None,
            allow_when_locked: false,
            allow_inhibiting: true,
            pass_through: false,
            when_preempted: "shield".to_owned(),
            desc: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindSpec {
    pub id: BindHandleId,
    pub mode: Option<String>,
    pub enabled: bool,
    pub chord: String,
    pub action: ActionDescriptor,
    pub release_action: Option<ActionDescriptor>,
    pub options: BindOptions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EdidMatchSpec {
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum MonitorScaleSpec {
    Auto,
    Fixed(f64),
}

impl Default for MonitorScaleSpec {
    fn default() -> Self {
        Self::Fixed(1.0)
    }
}

impl MonitorScaleSpec {
    /// Resolves effective logical scale from explicit numeric scale or physical DPI heuristics.
    #[must_use]
    pub fn resolve(self, phys_w_px: u32, phys_h_px: u32, phys_w_mm: i32, phys_h_mm: i32) -> f64 {
        match self {
            Self::Auto => {
                koma_model::compute_auto_scale(phys_w_px, phys_h_px, phys_w_mm, phys_h_mm)
            }
            Self::Fixed(s) => {
                if s.is_finite() && (0.5..=4.0).contains(&s) {
                    s
                } else {
                    1.0
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for MonitorScaleSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ScaleVisitor;

        impl<'de> serde::de::Visitor<'de> for ScaleVisitor {
            type Value = MonitorScaleSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a numeric monitor scale or \"auto\"")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v.trim().eq_ignore_ascii_case("auto") {
                    Ok(MonitorScaleSpec::Auto)
                } else if let Ok(num) = v.trim().parse::<f64>() {
                    Ok(MonitorScaleSpec::Fixed(num))
                } else {
                    Err(E::custom(format!(
                        "invalid monitor scale string `{v}` (expected \"auto\" or a number)"
                    )))
                }
            }

            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MonitorScaleSpec::Fixed(v))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MonitorScaleSpec::Fixed(v as f64))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(MonitorScaleSpec::Fixed(v as f64))
            }
        }

        deserializer.deserialize_any(ScaleVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct MonitorPositionSpec {
    pub x: i32,
    pub y: i32,
}

impl<'de> Deserialize<'de> for MonitorPositionSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PosVisitor;

        impl<'de> serde::de::Visitor<'de> for PosVisitor {
            type Value = MonitorPositionSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "`{{ x = int, y = int }}` or `[x, y]`")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let x = seq.next_element::<i32>()?.unwrap_or(0);
                let y = seq.next_element::<i32>()?.unwrap_or(0);
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(MonitorPositionSpec { x, y })
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut x = 0_i32;
                let mut y = 0_i32;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "x" | "1" => x = map.next_value::<i32>()?,
                        "y" | "2" => y = map.next_value::<i32>()?,
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(MonitorPositionSpec { x, y })
            }
        }

        deserializer.deserialize_any(PosVisitor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorSpec {
    pub output: Option<OutputId>,
    #[serde(rename = "match")]
    pub edid_match: Option<EdidMatchSpec>,
    pub mode: String,
    pub scale: MonitorScaleSpec,
    pub transform: String,
    #[serde(skip)]
    pub has_explicit_transform: bool,
    pub position: Option<MonitorPositionSpec>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    #[serde(alias = "non_desktop")]
    pub allow_non_desktop: bool,
    pub safe_area: SafeAreaInsets,
    pub vrr: serde_json::Value,
    pub max_bpc: Option<u8>,
    pub drm_lease: bool,
    pub off: bool,
    pub focus_at_startup: bool,
    pub backdrop_color: Option<String>,
}

impl MonitorSpec {
    /// Returns explicit `(x, y)` output placement if specified via `position` or `x`/`y`.
    #[must_use]
    pub fn resolved_position(&self) -> Option<(i32, i32)> {
        if let Some(pos) = self.position {
            return Some((pos.x, pos.y));
        }
        if self.x.is_some() || self.y.is_some() {
            return Some((self.x.unwrap_or(0), self.y.unwrap_or(0)));
        }
        None
    }

    pub fn parsed_transform(&self) -> Result<OutputTransform, String> {
        OutputTransform::parse_str(&self.transform)
    }
}

impl Default for MonitorSpec {
    fn default() -> Self {
        Self {
            output: None,
            edid_match: None,
            mode: "preferred".to_owned(),
            scale: MonitorScaleSpec::default(),
            transform: "normal".to_owned(),
            has_explicit_transform: false,
            position: None,
            x: None,
            y: None,
            allow_non_desktop: false,
            safe_area: SafeAreaInsets::default(),
            vrr: serde_json::Value::Bool(false),
            max_bpc: None,
            drm_lease: false,
            off: false,
            focus_at_startup: false,
            backdrop_color: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GapsSpec {
    pub inner: u32,
    pub outer: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceSpec {
    pub name: String,
    pub presentation: String,
    pub preferred_output: Option<OutputId>,
    pub viewport: LogicalViewport,
    pub layout: LayoutMode,
    pub gaps: GapsSpec,
    pub gaps_in: Option<u32>,
    pub gaps_out: Option<u32>,
    #[serde(skip)]
    pub has_explicit_layout: bool,
    #[serde(skip)]
    pub has_explicit_gaps: bool,
    pub frame_clock: FrameClockConfig,
    pub isolate_vseat: bool,
}

impl Default for WorkspaceSpec {
    fn default() -> Self {
        Self {
            name: "1".to_owned(),
            presentation: "headed".to_owned(),
            preferred_output: None,
            viewport: LogicalViewport::default(),
            layout: LayoutMode::Dwindle,
            gaps: GapsSpec {
                inner: 6,
                outer: 10,
            },
            gaps_in: None,
            gaps_out: None,
            has_explicit_layout: false,
            has_explicit_gaps: false,
            frame_clock: FrameClockConfig::default(),
            isolate_vseat: false,
        }
    }
}

impl WorkspaceSpec {
    #[must_use]
    pub fn effective_inner_gap(&self) -> u32 {
        self.gaps_in.unwrap_or(self.gaps.inner)
    }

    #[must_use]
    pub fn effective_outer_gap(&self) -> u32 {
        self.gaps_out.unwrap_or(self.gaps.outer)
    }
}

const fn default_true() -> bool {
    true
}

/// Pointer acceleration profile exposed to `koma.input` and `koma.device`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccelProfileSpec {
    Adaptive,
    Flat,
}

impl<'de> Deserialize<'de> for AccelProfileSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "adaptive" => Ok(Self::Adaptive),
            "flat" => Ok(Self::Flat),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["adaptive", "flat"],
            )),
        }
    }
}

/// Pointer/touchpad scroll method exposed to `koma.input` and `koma.device`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollMethodSpec {
    NoScroll,
    TwoFinger,
    Edge,
    OnButtonDown,
}

impl<'de> Deserialize<'de> for ScrollMethodSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" | "no_scroll" | "noscroll" => Ok(Self::NoScroll),
            "two_finger" | "twofinger" | "2fg" => Ok(Self::TwoFinger),
            "edge" => Ok(Self::Edge),
            "on_button_down" | "onbuttondown" => Ok(Self::OnButtonDown),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["none", "no_scroll", "two_finger", "edge", "on_button_down"],
            )),
        }
    }
}

/// Clickpad software button method exposed to `koma.input.touchpad` and `koma.device`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClickMethodSpec {
    None,
    ButtonAreas,
    Clickfinger,
}

impl<'de> Deserialize<'de> for ClickMethodSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "button_areas" | "buttonareas" => Ok(Self::ButtonAreas),
            "clickfinger" | "click_finger" => Ok(Self::Clickfinger),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["none", "button_areas", "clickfinger"],
            )),
        }
    }
}

/// Accepts either a comma-separated string (`"altwin:swap_alt_win,caps:escape"`) or a Lua string array.
fn deserialize_opt_xkb_options<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OptionsValue {
        Single(String),
        List(Vec<String>),
    }

    match Option::<OptionsValue>::deserialize(deserializer)? {
        None => Ok(None),
        Some(OptionsValue::Single(s)) => Ok(Some(s)),
        Some(OptionsValue::List(items)) => Ok(Some(items.join(","))),
    }
}

/// Default keyboard repeat delay (`ms`) matching initial seat setup.
pub const DEFAULT_KEYBOARD_REPEAT_DELAY_MS: i32 = 220;
/// Default keyboard repeat rate (`Hz`) matching initial seat setup.
pub const DEFAULT_KEYBOARD_REPEAT_RATE_HZ: i32 = 35;

/// Typed XKB & repeat-rate settings under `koma.input({ keyboard = { ... } })`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardInputSpec {
    pub rules: Option<String>,
    pub model: Option<String>,
    pub layout: Option<String>,
    pub variant: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_xkb_options")]
    pub options: Option<String>,
    #[serde(alias = "repeat_delay_ms")]
    pub repeat_delay: Option<i32>,
    #[serde(alias = "repeat_rate_hz")]
    pub repeat_rate: Option<i32>,
    #[serde(alias = "numlock_on_startup")]
    pub numlock: Option<bool>,
}

impl KeyboardInputSpec {
    /// Returns the effective repeat delay in milliseconds, clamped to a positive range.
    #[must_use]
    pub fn effective_repeat_delay(&self) -> i32 {
        self.repeat_delay
            .unwrap_or(DEFAULT_KEYBOARD_REPEAT_DELAY_MS)
            .clamp(1, 10_000)
    }

    /// Returns the effective repeat rate in repeats per second, clamped to a positive range.
    #[must_use]
    pub fn effective_repeat_rate(&self) -> i32 {
        self.repeat_rate
            .unwrap_or(DEFAULT_KEYBOARD_REPEAT_RATE_HZ)
            .clamp(1, 1_000)
    }
}

/// Typed `libinput` pointer settings under `koma.input({ pointer = { ... } })`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PointerInputSpec {
    pub accel_speed: Option<f64>,
    pub accel_profile: Option<AccelProfileSpec>,
    pub natural_scroll: Option<bool>,
    pub left_handed: Option<bool>,
    pub scroll_method: Option<ScrollMethodSpec>,
    pub middle_emulation: Option<bool>,
    pub scroll_factor: Option<f64>,
    pub follow_mouse: Option<String>,
}

/// Typed `libinput` touchpad settings under `koma.input({ touchpad = { ... } })`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TouchpadInputSpec {
    #[serde(alias = "tap_to_click")]
    pub tap: Option<bool>,
    pub tap_and_drag: Option<bool>,
    pub tap_drag_lock: Option<bool>,
    #[serde(alias = "disable_while_typing")]
    pub dwt: Option<bool>,
    #[serde(alias = "disable_while_trackpointing")]
    pub dwtp: Option<bool>,
    pub natural_scroll: Option<bool>,
    pub accel_speed: Option<f64>,
    pub accel_profile: Option<AccelProfileSpec>,
    pub scroll_method: Option<ScrollMethodSpec>,
    pub click_method: Option<ClickMethodSpec>,
    pub left_handed: Option<bool>,
    pub middle_emulation: Option<bool>,
    pub disabled_on_external_mouse: Option<bool>,
    pub scroll_factor: Option<f64>,
    pub pinch_sensitivity: Option<f64>,
}

/// Typed drawing-tablet configuration under `koma.input({ tablet = { ... } })`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabletInputSpec {
    pub map_to_output: Option<OutputId>,
    pub left_handed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfigSpec {
    pub mod_key: Option<String>,
    #[serde(default = "default_true")]
    pub inherit_default_binds: bool,
    pub keyboard: KeyboardInputSpec,
    pub pointer: PointerInputSpec,
    pub touchpad: TouchpadInputSpec,
    pub tablet: TabletInputSpec,
}

impl Default for InputConfigSpec {
    fn default() -> Self {
        Self {
            mod_key: None,
            inherit_default_binds: true,
            keyboard: KeyboardInputSpec::default(),
            pointer: PointerInputSpec::default(),
            touchpad: TouchpadInputSpec::default(),
            tablet: TabletInputSpec::default(),
        }
    }
}

impl InputConfigSpec {
    #[must_use]
    pub fn touchpad_natural_scroll(&self) -> Option<bool> {
        self.touchpad.natural_scroll
    }

    #[must_use]
    pub fn mouse_scroll_factor(&self) -> f64 {
        self.pointer
            .scroll_factor
            .filter(|&f| f.is_finite() && f != 0.0)
            .unwrap_or(1.0)
    }

    #[must_use]
    pub fn touchpad_scroll_factor(&self) -> f64 {
        self.touchpad
            .scroll_factor
            .filter(|&f| f.is_finite() && f != 0.0)
            .unwrap_or(1.0)
    }

    #[must_use]
    pub fn touchpad_pinch_sensitivity(&self) -> f64 {
        self.touchpad
            .pinch_sensitivity
            .filter(|&f| f.is_finite() && f > 0.0)
            .unwrap_or(1.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceOverrideSpec {
    pub name: String,
    pub sensitivity: Option<f64>,
    pub map_to_output: Option<OutputId>,
    pub accel_speed: Option<f64>,
    pub accel_profile: Option<AccelProfileSpec>,
    pub natural_scroll: Option<bool>,
    pub left_handed: Option<bool>,
    #[serde(alias = "tap_to_click")]
    pub tap: Option<bool>,
    pub tap_and_drag: Option<bool>,
    pub tap_drag_lock: Option<bool>,
    #[serde(alias = "disable_while_typing")]
    pub dwt: Option<bool>,
    #[serde(alias = "disable_while_trackpointing")]
    pub dwtp: Option<bool>,
    pub scroll_method: Option<ScrollMethodSpec>,
    pub click_method: Option<ClickMethodSpec>,
    pub middle_emulation: Option<bool>,
    pub disabled_on_external_mouse: Option<bool>,
    pub scroll_factor: Option<f64>,
}

/// Hardware capabilities probed from a `libinput` device (or mocked in unit tests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibinputDeviceProbe {
    pub name: String,
    pub vendor_id: u32,
    pub product_id: u32,
    pub is_pointer: bool,
    pub tap_finger_count: u32,
    pub supports_clickfinger: bool,
    pub has_physical_buttons: bool,
    pub default_click_method: Option<ClickMethodSpec>,
    pub dwt_available: bool,
    pub dwtp_available: bool,
    pub supports_disabled_on_external_mouse: bool,
    pub is_trackball: bool,
    pub is_trackpoint: bool,
}

impl LibinputDeviceProbe {
    /// Returns `true` when this device acts as a touchpad (`Pointer` + tap fingers > 0).
    #[must_use]
    pub fn is_touchpad(&self) -> bool {
        self.is_pointer && self.tap_finger_count > 0
    }

    /// Returns `true` for buttonpads/Force Touch clickpads lacking separate physical mouse buttons
    /// and supporting libinput `Clickfinger` (e.g. Apple SPI/USB/Bluetooth Force Touch trackpads).
    #[must_use]
    pub fn is_force_touch_buttonpad(&self) -> bool {
        self.is_touchpad() && self.supports_clickfinger && !self.has_physical_buttons
    }
}

/// Effective per-device `libinput` configuration resolved from Lua input/device rules + hardware defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectiveLibinputSettings {
    pub is_touchpad: bool,
    pub accel_speed: Option<f64>,
    pub accel_profile: Option<AccelProfileSpec>,
    pub natural_scroll: Option<bool>,
    pub left_handed: Option<bool>,
    pub scroll_method: Option<ScrollMethodSpec>,
    pub middle_emulation: Option<bool>,
    pub tap: Option<bool>,
    pub tap_and_drag: Option<bool>,
    pub tap_drag_lock: Option<bool>,
    pub dwt: Option<bool>,
    pub dwtp: Option<bool>,
    pub click_method: Option<ClickMethodSpec>,
    pub disabled_on_external_mouse: Option<bool>,
}

/// Resolves effective `libinput` settings for a device from `koma.input`, `koma.device` overrides,
/// and MacBook Force Touch buttonpad defaults (`click_method = Clickfinger`, `dwt = true` when unset).
#[must_use]
pub fn resolve_effective_libinput_settings(
    input: &InputConfigSpec,
    devices: &[DeviceOverrideSpec],
    probe: &LibinputDeviceProbe,
) -> EffectiveLibinputSettings {
    if !probe.is_pointer {
        return EffectiveLibinputSettings::default();
    }

    let dev_override = devices
        .iter()
        .find(|d| !d.name.is_empty() && d.name == probe.name)
        .or_else(|| {
            devices
                .iter()
                .find(|d| !d.name.is_empty() && d.name.eq_ignore_ascii_case(&probe.name))
        });

    let override_accel_speed = dev_override
        .and_then(|d| d.accel_speed.or(d.sensitivity))
        .filter(|v| v.is_finite());

    if probe.is_touchpad() {
        let tp = &input.touchpad;
        let force_touch = probe.is_force_touch_buttonpad();

        let click_method = dev_override
            .and_then(|d| d.click_method)
            .or(tp.click_method)
            .or_else(|| force_touch.then_some(ClickMethodSpec::Clickfinger))
            .or(probe.default_click_method);

        let dwt = dev_override
            .and_then(|d| d.dwt)
            .or(tp.dwt)
            .or_else(|| (force_touch && probe.dwt_available).then_some(true));

        return EffectiveLibinputSettings {
            is_touchpad: true,
            accel_speed: override_accel_speed
                .or(tp.accel_speed.filter(|v| v.is_finite()))
                .map(|s| s.clamp(-1.0, 1.0)),
            accel_profile: dev_override
                .and_then(|d| d.accel_profile)
                .or(tp.accel_profile),
            natural_scroll: dev_override
                .and_then(|d| d.natural_scroll)
                .or(tp.natural_scroll),
            left_handed: dev_override.and_then(|d| d.left_handed).or(tp.left_handed),
            scroll_method: dev_override
                .and_then(|d| d.scroll_method)
                .or(tp.scroll_method),
            middle_emulation: dev_override
                .and_then(|d| d.middle_emulation)
                .or(tp.middle_emulation),
            tap: dev_override.and_then(|d| d.tap).or(tp.tap),
            tap_and_drag: dev_override
                .and_then(|d| d.tap_and_drag)
                .or(tp.tap_and_drag),
            tap_drag_lock: dev_override
                .and_then(|d| d.tap_drag_lock)
                .or(tp.tap_drag_lock),
            dwt,
            dwtp: dev_override.and_then(|d| d.dwtp).or(tp.dwtp),
            click_method,
            disabled_on_external_mouse: dev_override
                .and_then(|d| d.disabled_on_external_mouse)
                .or(tp.disabled_on_external_mouse),
        };
    }

    let ptr = &input.pointer;
    EffectiveLibinputSettings {
        is_touchpad: false,
        accel_speed: override_accel_speed
            .or(ptr.accel_speed.filter(|v| v.is_finite()))
            .map(|s| s.clamp(-1.0, 1.0)),
        accel_profile: dev_override
            .and_then(|d| d.accel_profile)
            .or(ptr.accel_profile),
        natural_scroll: dev_override
            .and_then(|d| d.natural_scroll)
            .or(ptr.natural_scroll),
        left_handed: dev_override.and_then(|d| d.left_handed).or(ptr.left_handed),
        scroll_method: dev_override
            .and_then(|d| d.scroll_method)
            .or(ptr.scroll_method),
        middle_emulation: dev_override
            .and_then(|d| d.middle_emulation)
            .or(ptr.middle_emulation),
        ..EffectiveLibinputSettings::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSpec {
    pub border: BorderConfig,
    pub focus_ring: FocusRingConfig,
    pub corner_radius: Option<u16>,
    pub shadow: ShadowConfig,
    pub dim: DimConfig,
    pub brightness: Option<f32>,
    pub dim_inactive: Option<bool>,
    pub dim_strength: Option<f32>,
    pub blur: BlurConfig,
    pub animations: AnimationConfig,
    pub agent_cursor: AgentCursorConfig,
}

impl Default for AppearanceSpec {
    fn default() -> Self {
        Self {
            border: BorderConfig::default(),
            focus_ring: FocusRingConfig::default(),
            corner_radius: Some(10),
            shadow: ShadowConfig::default(),
            dim: DimConfig::default(),
            brightness: None,
            dim_inactive: None,
            dim_strength: None,
            blur: BlurConfig::default(),
            animations: AnimationConfig::default(),
            agent_cursor: AgentCursorConfig::default(),
        }
    }
}

impl AppearanceSpec {
    #[must_use]
    pub fn effective_dim(&self) -> DimConfig {
        let mut dim = self.dim;
        if let Some(brightness) = self.brightness {
            dim.brightness = brightness;
        }
        if let Some(dim_inactive) = self.dim_inactive {
            dim.dim_inactive = dim_inactive;
        }
        if let Some(dim_strength) = self.dim_strength {
            dim.dim_strength = dim_strength;
        }
        dim
    }
}

const fn default_gesture_enabled() -> bool {
    true
}

fn default_gesture_when_preempted() -> String {
    "allow".to_owned()
}

/// Declarative multi-finger touchpad swipe/pinch binding registered via `koma.gesture(...)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GestureSpec {
    #[serde(default)]
    pub id: Option<BindHandleId>,
    #[serde(default = "default_gesture_enabled")]
    pub enabled: bool,
    pub fingers: u8,
    pub direction: String,
    pub action: ActionDescriptor,
    #[serde(default)]
    pub cooldown_ms: Option<u64>,
    #[serde(default)]
    pub allow_when_locked: bool,
    #[serde(default = "default_gesture_when_preempted")]
    pub when_preempted: String,
    #[serde(default)]
    pub desc: Option<String>,
}

/// Validates finger count (`2..=5`) and normalizes swipe/pinch direction names.
pub fn validate_gesture_spec(fingers: i64, raw_direction: &str) -> Result<(u8, String), String> {
    if !(2..=5).contains(&fingers) {
        return Err(format!(
            "invalid gesture `fingers = {fingers}` (expected 2..=5)"
        ));
    }
    let canonical = match raw_direction.trim().to_ascii_lowercase().as_str() {
        "left" => "left",
        "right" => "right",
        "up" => "up",
        "down" => "down",
        "horizontal" | "horiz" => "horizontal",
        "vertical" | "vert" => "vertical",
        "pinch_in" | "pinch-in" | "in" => "pinch_in",
        "pinch_out" | "pinch-out" | "out" => "pinch_out",
        other => {
            return Err(format!(
                "invalid gesture `direction = \"{other}\"` (expected left, right, up, down, horizontal, vertical, pinch_in, or pinch_out)"
            ));
        }
    };
    Ok((fingers as u8, canonical.to_owned()))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaWindowMatchInput {
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub tag: Option<String>,
    pub workspace: Option<String>,
    pub is_floating: Option<bool>,
    pub is_focused: Option<bool>,
    pub is_urgent: Option<bool>,
    pub is_modal_dialog: Option<bool>,
    pub is_xwayland: Option<bool>,
    pub is_screencast_target: Option<bool>,
    pub is_agent_workspace: Option<bool>,
    pub at_startup: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatingPosInput {
    pub x: i32,
    pub y: i32,
    #[serde(default = "default_top_left_anchor")]
    pub anchor: Anchor9,
}

const fn default_top_left_anchor() -> Anchor9 {
    Anchor9::TopLeft
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaInitialWindowRuleInput {
    pub open_on_output: Option<String>,
    pub open_on_workspace: Option<String>,
    pub open_mode: Option<OpenMode>,
    pub open_focused: Option<bool>,
    pub default_floating_pos: Option<FloatingPosInput>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaWindowRuleSpec {
    pub id: Option<RuleHandleId>,
    pub enabled: bool,
    pub name: Option<String>,
    pub matches: Vec<LuaWindowMatchInput>,
    pub excludes: Vec<LuaWindowMatchInput>,
    pub initial: LuaInitialWindowRuleInput,
    pub opacity: Option<f32>,
    pub inactive_opacity: Option<f32>,
    pub animation: Option<String>,
    pub no_blur: Option<bool>,
    pub no_shadow: Option<bool>,
    pub border_color: Option<String>,
    pub corner_radius: Option<u16>,
    pub clip_to_geometry: Option<bool>,
    pub focusable: Option<bool>,
    pub pin_on_output: Option<bool>,
    pub suppress_events: Vec<String>,
    pub block_out_from: Option<BlockOutPolicy>,
}

/// Read-only window snapshot exposed to Lua via `ctx:windows()` / `ctx:focused_window()`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LuaWindowInfo {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub workspace: String,
    pub floating: bool,
    pub fullscreen: bool,
    pub focused: bool,
}

/// Read-only output snapshot exposed to Lua via `ctx:outputs()`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LuaOutputInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub refresh_mhz: u32,
    pub active_workspace: String,
    pub focused: bool,
}

/// Read-only workspace snapshot exposed to Lua via `ctx:workspaces()`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LuaWorkspaceInfo {
    pub id: String,
    pub name: String,
    pub layout: String,
    pub presentation: String,
    pub focused_window_id: Option<u64>,
    pub window_count: usize,
    pub active: bool,
}

/// Consolidated read-only compositor snapshot synchronized into `PhodopusSandbox` before Lua slices.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DesktopQuerySnapshot {
    pub focused_window: Option<LuaWindowInfo>,
    pub windows: Vec<LuaWindowInfo>,
    pub outputs: Vec<LuaOutputInfo>,
    pub workspaces: Vec<LuaWorkspaceInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaLayerMatchInput {
    pub namespace: Option<String>,
    pub layer: Option<String>,
    pub at_startup: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaLayerRuleSpec {
    pub id: Option<RuleHandleId>,
    pub enabled: bool,
    pub name: Option<String>,
    pub matches: Vec<LuaLayerMatchInput>,
    pub excludes: Vec<LuaLayerMatchInput>,
    pub opacity: Option<f32>,
    pub corner_radius: Option<u16>,
    pub place_within_backdrop: Option<bool>,
    pub block_out_from: Option<BlockOutPolicy>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MacroParamSpec {
    #[serde(rename = "type")]
    pub param_type: String,
    pub required: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    #[serde(rename = "default")]
    pub default_value: Option<serde_json::Value>,
}

impl Default for MacroParamSpec {
    fn default() -> Self {
        Self {
            param_type: "string".to_owned(),
            required: false,
            min: None,
            max: None,
            default_value: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct MacroLimitsSpec {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub timeout_ms: u64,
}

impl Default for MacroLimitsSpec {
    fn default() -> Self {
        Self {
            fuel: 100_000,
            memory_bytes: 4 * 1024 * 1024,
            timeout_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacroDefinition {
    pub handle_id: MacroHandleId,
    pub id: MacroId,
    pub enabled: bool,
    pub app_id_regex: String,
    pub description: String,
    pub params: BTreeMap<String, MacroParamSpec>,
    pub limits: MacroLimitsSpec,
}

/// Lua table input representation for `AnchorPoint` (`LUA_API_SPEC.md` §5.1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaAnchorPointInput {
    pub anchor: Option<Anchor9>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub u: Option<f64>,
    pub v: Option<f64>,
    pub grid_225: Option<String>,
}

impl LuaAnchorPointInput {
    pub fn into_anchor_point(self) -> Result<AnchorPoint, String> {
        if let Some(cell) = self.grid_225 {
            return Ok(AnchorPoint::Grid225(cell));
        }
        if let (Some(u), Some(v)) = (self.u, self.v) {
            return Ok(AnchorPoint::Normalized { u, v });
        }
        let anchor = self.anchor.unwrap_or(Anchor9::TopLeft);
        Ok(AnchorPoint::AnchorOffset {
            anchor,
            dx: self.x.unwrap_or(0.0),
            dy: self.y.unwrap_or(0.0),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaKinematicsInput {
    pub profile: KinematicProfileKind,
    pub duration_ms: Option<u64>,
    pub arc_curvature: Option<f64>,
    pub curve: Option<String>,
    pub mass: Option<f64>,
    pub damping: Option<f64>,
    pub damping_ratio: Option<f64>,
    pub stiffness: Option<f64>,
    pub epsilon: Option<f64>,
    pub bezier: Option<[f64; 4]>,
}

impl Default for LuaKinematicsInput {
    fn default() -> Self {
        Self {
            profile: KinematicProfileKind::MinimumJerk,
            duration_ms: None,
            arc_curvature: None,
            curve: None,
            mass: None,
            damping: None,
            damping_ratio: None,
            stiffness: None,
            epsilon: None,
            bezier: None,
        }
    }
}

impl LuaKinematicsInput {
    /// Immediate pointer warp (`profile = "instant"`).
    #[must_use]
    pub fn instant() -> Self {
        Self {
            profile: KinematicProfileKind::Instant,
            duration_ms: Some(0),
            arc_curvature: Some(0.0),
            ..Self::default()
        }
    }

    fn resolve_hooke_params(&self, base: Option<HookeSpringParams>) -> HookeSpringParams {
        let default_eps = base.map_or(0.01, |p| p.epsilon.max(0.01));
        let eps = self.epsilon.unwrap_or(default_eps);

        if let Some(p) = base
            && self.mass.is_none()
            && self.damping.is_none()
            && self.damping_ratio.is_none()
            && self.stiffness.is_none()
        {
            return HookeSpringParams::from_physical(p.mass, p.damping, p.stiffness, eps);
        }

        if let Some(p) = base {
            let m = self.mass.unwrap_or(p.mass);
            let k = self.stiffness.unwrap_or(p.stiffness);
            return if let Some(c) = self.damping {
                HookeSpringParams::from_physical(m, c, k, eps)
            } else if let Some(zeta) = self.damping_ratio {
                HookeSpringParams::from_ratio(zeta, k, m, eps)
            } else {
                HookeSpringParams::from_physical(m, p.damping, k, eps)
            };
        }

        let has_physical = self.mass.is_some()
            || self.damping.is_some()
            || self.damping_ratio.is_some()
            || self.stiffness.is_some();
        if has_physical {
            let m = self.mass.unwrap_or(1.0);
            let k = self.stiffness.unwrap_or(380.0);
            return if let Some(c) = self.damping {
                HookeSpringParams::from_physical(m, c, k, eps)
            } else {
                HookeSpringParams::from_ratio(self.damping_ratio.unwrap_or(0.92), k, m, eps)
            };
        }

        if let Some(dur_ms) = self.duration_ms {
            let t_sec = (dur_ms.max(16) as f64) / 1000.0;
            let omega0 = (9.5 / t_sec).clamp(8.0, 120.0);
            return HookeSpringParams::from_ratio(1.0, omega0 * omega0, 1.0, eps);
        }

        HookeSpringParams::from_ratio(0.92, 380.0, 1.0, eps)
    }

    /// Resolves this Lua/IPC table against named curves in `AnimationConfig` into a `PointerTrajectorySpec`.
    #[must_use]
    pub fn resolve_spec(&self, animations: &AnimationConfig) -> PointerTrajectorySpec {
        let arc = self.arc_curvature.unwrap_or(0.0);
        if self.duration_ms == Some(0) {
            return PointerTrajectorySpec::Instant;
        }

        if let Some(name) = self.curve.as_deref()
            && let Some(curve) =
                animations.resolve_curve(name, self.duration_ms.map(|d| d.clamp(1, 5_000) as u32))
        {
            match curve {
                CurveSpec::Instant => return PointerTrajectorySpec::Instant,
                CurveSpec::Bezier {
                    bezier,
                    duration_ms,
                } => {
                    return PointerTrajectorySpec::Bezier {
                        bezier,
                        duration_ms: u64::from(duration_ms),
                        arc_curvature: arc,
                    };
                }
                CurveSpec::Spring(p) => {
                    let params = self.resolve_hooke_params(Some(p));
                    return PointerTrajectorySpec::HookeSpring {
                        params,
                        duration_cap_ms: self.duration_ms,
                        arc_curvature: arc,
                    };
                }
                CurveSpec::AnalyticalSpring(sp) => {
                    let base = HookeSpringParams::from_physical(
                        sp.mass,
                        sp.damping,
                        sp.stiffness,
                        sp.epsilon,
                    );
                    let params = self.resolve_hooke_params(Some(base));
                    return PointerTrajectorySpec::HookeSpring {
                        params,
                        duration_cap_ms: self.duration_ms,
                        arc_curvature: arc,
                    };
                }
            }
        }

        let inferred_spring = self.mass.is_some()
            || self.damping.is_some()
            || self.damping_ratio.is_some()
            || self.stiffness.is_some();

        let effective_profile = match self.profile {
            KinematicProfileKind::MinimumJerk if self.bezier.is_some() => {
                KinematicProfileKind::Bezier
            }
            KinematicProfileKind::MinimumJerk if inferred_spring => KinematicProfileKind::Spring,
            other => other,
        };

        match effective_profile {
            KinematicProfileKind::Instant => PointerTrajectorySpec::Instant,
            KinematicProfileKind::MinimumJerk => PointerTrajectorySpec::MinimumJerk {
                duration_ms: self.duration_ms.unwrap_or(110),
                arc_curvature: self.arc_curvature.unwrap_or(0.08),
            },
            KinematicProfileKind::Bezier => {
                let [x1, y1, x2, y2] = self.bezier.unwrap_or([0.22, 1.0, 0.36, 1.0]);
                PointerTrajectorySpec::Bezier {
                    bezier: CubicBezier::new(x1, y1, x2, y2),
                    duration_ms: self.duration_ms.unwrap_or(180).clamp(1, 5_000),
                    arc_curvature: arc,
                }
            }
            KinematicProfileKind::Spring => PointerTrajectorySpec::HookeSpring {
                params: self.resolve_hooke_params(None),
                duration_cap_ms: self.duration_ms,
                arc_curvature: arc,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaKeyOptsInput {
    pub hold_ms: u64,
    pub after_ms: u64,
}

impl Default for LuaKeyOptsInput {
    fn default() -> Self {
        Self {
            hold_ms: 15,
            after_ms: 20,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaClickOptsInput {
    pub button: String,
    pub clicks: u8,
    pub hold_ms: u64,
}

impl Default for LuaClickOptsInput {
    fn default() -> Self {
        Self {
            button: "left".to_owned(),
            clicks: 1,
            hold_ms: 25,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LuaDragSpecInput {
    #[serde(default = "default_left_button")]
    pub button: String,
    pub from: LuaAnchorPointInput,
    pub to: LuaAnchorPointInput,
    #[serde(default = "default_hold_before_move")]
    pub hold_before_move_ms: u64,
    #[serde(default)]
    pub kinematics: LuaKinematicsInput,
    #[serde(default = "default_hold_before_release")]
    pub hold_before_release_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LuaPathSpecInput {
    pub waypoints: Vec<LuaAnchorPointInput>,
    #[serde(default)]
    pub spline: PathSplineKind,
    #[serde(default)]
    pub button: Option<String>,
    #[serde(default, alias = "hold_start_ms")]
    pub hold_before_move_ms: u64,
    #[serde(default, alias = "hold_end_ms")]
    pub hold_before_release_ms: u64,
    #[serde(default)]
    pub kinematics: LuaKinematicsInput,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaMovePathOptsInput {
    pub spline: PathSplineKind,
    pub kinematics: Option<LuaKinematicsInput>,
    pub profile: Option<KinematicProfileKind>,
    pub duration_ms: Option<u64>,
    pub arc_curvature: Option<f64>,
    pub curve: Option<String>,
    pub mass: Option<f64>,
    pub damping: Option<f64>,
    pub damping_ratio: Option<f64>,
    pub stiffness: Option<f64>,
    pub epsilon: Option<f64>,
    pub bezier: Option<[f64; 4]>,
}

impl LuaMovePathOptsInput {
    #[must_use]
    pub fn into_parts(self) -> (PathSplineKind, LuaKinematicsInput) {
        let kin = match self.kinematics {
            Some(k) => k,
            None => LuaKinematicsInput {
                profile: self.profile.unwrap_or(KinematicProfileKind::MinimumJerk),
                duration_ms: self.duration_ms,
                arc_curvature: self.arc_curvature,
                curve: self.curve,
                mass: self.mass,
                damping: self.damping,
                damping_ratio: self.damping_ratio,
                stiffness: self.stiffness,
                epsilon: self.epsilon,
                bezier: self.bezier,
            },
        };
        (self.spline, kin)
    }
}

fn default_left_button() -> String {
    "left".to_owned()
}
const fn default_hold_before_move() -> u64 {
    60
}
const fn default_hold_before_release() -> u64 {
    30
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaScrollSpecInput {
    pub dx: f64,
    pub dy: f64,
    pub steps: u32,
    pub step_ms: u64,
}

impl Default for LuaScrollSpecInput {
    fn default() -> Self {
        Self {
            dx: 0.0,
            dy: 1.0,
            steps: 1,
            step_ms: 18,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaTextOptsInput {
    pub strategy: TextInjectStrategy,
}

impl Default for LuaTextOptsInput {
    fn default() -> Self {
        Self {
            strategy: TextInjectStrategy::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaSettleOptsInput {
    pub quiet_ms: u64,
    pub timeout_ms: u64,
}

impl Default for LuaSettleOptsInput {
    fn default() -> Self {
        Self {
            quiet_ms: 40,
            timeout_ms: 500,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct XwaylandConfigSpec {
    pub enabled: bool,
    pub path: String,
    pub isolate_agent_workspaces: bool,
}

impl Default for XwaylandConfigSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "xwayland-satellite".to_owned(),
            isolate_agent_workspaces: true,
        }
    }
}

/// Complete declarative state produced by evaluating `~/.config/koma/init.lua`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KomaConfigRegistry {
    pub env_vars: BTreeMap<String, String>,
    pub notifications: Vec<String>,
    pub monitors: Vec<MonitorSpec>,
    pub default_layout: LayoutMode,
    pub workspaces: Vec<WorkspaceSpec>,
    pub dwindle: DwindleConfig,
    pub scrolling: ScrollingConfig,
    pub input: InputConfigSpec,
    pub devices: Vec<DeviceOverrideSpec>,
    pub appearance: AppearanceSpec,
    pub xwayland: XwaylandConfigSpec,
    pub binds: Vec<BindSpec>,
    pub unbinds: Vec<String>,
    pub gestures: Vec<GestureSpec>,
    pub window_rules: Vec<LuaWindowRuleSpec>,
    pub layer_rules: Vec<LuaLayerRuleSpec>,
    pub macros: BTreeMap<MacroId, MacroDefinition>,
}

impl KomaConfigRegistry {
    /// Finds the declarative `koma.monitor({ output = "..." })` rule matching `connector_name`.
    #[must_use]
    pub fn find_monitor(&self, connector_name: &str) -> Option<&MonitorSpec> {
        self.monitors.iter().find(|m| {
            m.output
                .as_ref()
                .is_some_and(|out| out.as_str() == connector_name)
        })
    }

    /// Finds the declarative `koma.monitor(...)` rule matching `connector_name` or EDID metadata.
    #[must_use]
    pub fn find_monitor_for_output(
        &self,
        connector_name: &str,
        make: Option<&str>,
        model: Option<&str>,
        serial: Option<&str>,
    ) -> Option<&MonitorSpec> {
        if let Some(exact) = self.find_monitor(connector_name) {
            return Some(exact);
        }
        self.monitors.iter().find(|m| {
            if m.output.is_some() {
                return false;
            }
            let Some(em) = &m.edid_match else {
                return false;
            };
            let matches_make = em
                .make
                .as_ref()
                .is_none_or(|want| make.is_some_and(|got| got.eq_ignore_ascii_case(want.trim())));
            let matches_model = em.model.as_ref().is_none_or(|want| {
                model.is_some_and(|got| {
                    got.eq_ignore_ascii_case(want.trim())
                        || got
                            .to_ascii_lowercase()
                            .contains(&want.trim().to_ascii_lowercase())
                })
            });
            let matches_serial = em
                .serial
                .as_ref()
                .is_none_or(|want| serial.is_some_and(|got| got.eq_ignore_ascii_case(want.trim())));
            (em.make.is_some() || em.model.is_some() || em.serial.is_some())
                && matches_make
                && matches_model
                && matches_serial
        })
    }

    pub fn apply_handle_enabled(&mut self, handle: KomaHandleRef, enabled: bool) -> bool {
        match handle {
            KomaHandleRef::Bind(id) => {
                if let Some(b) = self.binds.iter_mut().find(|b| b.id == id) {
                    b.enabled = enabled;
                    return true;
                }
                if let Some(g) = self.gestures.iter_mut().find(|g| g.id == Some(id)) {
                    g.enabled = enabled;
                    return true;
                }
            }
            KomaHandleRef::WindowRule(id) => {
                if let Some(r) = self.window_rules.iter_mut().find(|r| r.id == Some(id)) {
                    r.enabled = enabled;
                    return true;
                }
            }
            KomaHandleRef::LayerRule(id) => {
                if let Some(r) = self.layer_rules.iter_mut().find(|r| r.id == Some(id)) {
                    r.enabled = enabled;
                    return true;
                }
            }
            KomaHandleRef::Macro(id) => {
                if let Some(m) = self.macros.values_mut().find(|m| m.handle_id == id) {
                    m.enabled = enabled;
                    return true;
                }
            }
        }
        false
    }

    pub fn remove_handle(&mut self, handle: KomaHandleRef) -> bool {
        match handle {
            KomaHandleRef::Bind(id) => {
                let before_b = self.binds.len();
                self.binds.retain(|b| b.id != id);
                let before_g = self.gestures.len();
                self.gestures.retain(|g| g.id != Some(id));
                self.binds.len() != before_b || self.gestures.len() != before_g
            }
            KomaHandleRef::WindowRule(id) => {
                let before = self.window_rules.len();
                self.window_rules.retain(|r| r.id != Some(id));
                self.window_rules.len() != before
            }
            KomaHandleRef::LayerRule(id) => {
                let before = self.layer_rules.len();
                self.layer_rules.retain(|r| r.id != Some(id));
                self.layer_rules.len() != before
            }
            KomaHandleRef::Macro(id) => {
                let key = self
                    .macros
                    .iter()
                    .find(|(_, m)| m.handle_id == id)
                    .map(|(k, _)| k.clone());
                key.is_some_and(|k| self.macros.remove(&k).is_some())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apple_force_touch_probe() -> LibinputDeviceProbe {
        LibinputDeviceProbe {
            name: "Apple Inc. Apple Internal Keyboard / Trackpad".to_owned(),
            vendor_id: 0x05ac,
            product_id: 0x027c,
            is_pointer: true,
            tap_finger_count: 5,
            supports_clickfinger: true,
            has_physical_buttons: false,
            default_click_method: Some(ClickMethodSpec::ButtonAreas),
            dwt_available: true,
            dwtp_available: false,
            supports_disabled_on_external_mouse: true,
            is_trackball: false,
            is_trackpoint: false,
        }
    }

    #[test]
    fn input_config_spec_parses_xkb_repeat_pointer_and_touchpad_fields() {
        let json = serde_json::json!({
            "mod_key": "Super",
            "inherit_default_binds": true,
            "keyboard": {
                "layout": "us,de",
                "variant": ",nodeadkeys",
                "model": "applealu_ansi",
                "options": "altwin:swap_alt_win,caps:escape",
                "rules": "evdev",
                "repeat_delay": 180,
                "repeat_rate": 45,
                "numlock": true
            },
            "pointer": {
                "accel_speed": -0.25,
                "accel_profile": "flat",
                "natural_scroll": false,
                "left_handed": true,
                "scroll_method": "on_button_down",
                "middle_emulation": true,
                "scroll_factor": 1.5
            },
            "touchpad": {
                "tap": true,
                "tap_and_drag": true,
                "tap_drag_lock": false,
                "dwt": true,
                "dwtp": true,
                "natural_scroll": true,
                "accel_speed": 0.35,
                "accel_profile": "adaptive",
                "scroll_method": "two_finger",
                "click_method": "clickfinger",
                "left_handed": false,
                "middle_emulation": false,
                "disabled_on_external_mouse": true,
                "scroll_factor": 0.8,
                "pinch_sensitivity": 1.2
            }
        });

        let cfg: InputConfigSpec = serde_json::from_value(json).expect("valid InputConfigSpec");
        assert_eq!(cfg.keyboard.layout.as_deref(), Some("us,de"));
        assert_eq!(cfg.keyboard.variant.as_deref(), Some(",nodeadkeys"));
        assert_eq!(cfg.keyboard.model.as_deref(), Some("applealu_ansi"));
        assert_eq!(
            cfg.keyboard.options.as_deref(),
            Some("altwin:swap_alt_win,caps:escape")
        );
        assert_eq!(cfg.keyboard.rules.as_deref(), Some("evdev"));
        assert_eq!(cfg.keyboard.repeat_delay, Some(180));
        assert_eq!(cfg.keyboard.repeat_rate, Some(45));
        assert_eq!(cfg.keyboard.numlock, Some(true));

        assert_eq!(cfg.pointer.accel_speed, Some(-0.25));
        assert_eq!(cfg.pointer.accel_profile, Some(AccelProfileSpec::Flat));
        assert_eq!(cfg.pointer.natural_scroll, Some(false));
        assert_eq!(cfg.pointer.left_handed, Some(true));
        assert_eq!(
            cfg.pointer.scroll_method,
            Some(ScrollMethodSpec::OnButtonDown)
        );
        assert_eq!(cfg.pointer.middle_emulation, Some(true));
        assert_eq!(cfg.mouse_scroll_factor(), 1.5);

        assert_eq!(cfg.touchpad.tap, Some(true));
        assert_eq!(cfg.touchpad.tap_and_drag, Some(true));
        assert_eq!(cfg.touchpad.tap_drag_lock, Some(false));
        assert_eq!(cfg.touchpad.dwt, Some(true));
        assert_eq!(cfg.touchpad.dwtp, Some(true));
        assert_eq!(cfg.touchpad_natural_scroll(), Some(true));
        assert_eq!(cfg.touchpad.accel_speed, Some(0.35));
        assert_eq!(cfg.touchpad.accel_profile, Some(AccelProfileSpec::Adaptive));
        assert_eq!(
            cfg.touchpad.scroll_method,
            Some(ScrollMethodSpec::TwoFinger)
        );
        assert_eq!(
            cfg.touchpad.click_method,
            Some(ClickMethodSpec::Clickfinger)
        );
        assert_eq!(cfg.touchpad.disabled_on_external_mouse, Some(true));
        assert_eq!(cfg.touchpad_scroll_factor(), 0.8);
        assert_eq!(cfg.touchpad_pinch_sensitivity(), 1.2);
    }

    #[test]
    fn input_config_spec_supports_lua_aliases_and_xkb_option_list() {
        let json = serde_json::json!({
            "keyboard": {
                "options": ["altwin:swap_alt_win", "lv3:ralt_switch"],
                "repeat_delay_ms": 200,
                "repeat_rate_hz": 40,
                "numlock_on_startup": true
            },
            "touchpad": {
                "tap_to_click": true,
                "disable_while_typing": false,
                "click_method": "button_areas"
            }
        });
        let cfg: InputConfigSpec = serde_json::from_value(json).expect("alias InputConfigSpec");
        assert_eq!(
            cfg.keyboard.options.as_deref(),
            Some("altwin:swap_alt_win,lv3:ralt_switch")
        );
        assert_eq!(cfg.keyboard.effective_repeat_delay(), 200);
        assert_eq!(cfg.keyboard.effective_repeat_rate(), 40);
        assert_eq!(cfg.keyboard.numlock, Some(true));
        assert_eq!(cfg.touchpad.tap, Some(true));
        assert_eq!(cfg.touchpad.dwt, Some(false));
        assert_eq!(
            cfg.touchpad.click_method,
            Some(ClickMethodSpec::ButtonAreas)
        );
    }

    #[test]
    fn force_touch_buttonpad_defaults_and_lua_overrides() {
        let probe = apple_force_touch_probe();
        assert!(probe.is_force_touch_buttonpad());

        // 1. Unset Lua config -> Clickfinger + DWT enabled by default on Force Touch buttonpads
        let default_cfg = InputConfigSpec::default();
        let resolved_default = resolve_effective_libinput_settings(&default_cfg, &[], &probe);
        assert_eq!(
            resolved_default.click_method,
            Some(ClickMethodSpec::Clickfinger)
        );
        assert_eq!(resolved_default.dwt, Some(true));

        // 2. Explicit Lua overrides (`button_areas`, `none`, `dwt = false`) always win
        let mut override_areas = InputConfigSpec::default();
        override_areas.touchpad.click_method = Some(ClickMethodSpec::ButtonAreas);
        override_areas.touchpad.dwt = Some(false);
        let res_areas = resolve_effective_libinput_settings(&override_areas, &[], &probe);
        assert_eq!(res_areas.click_method, Some(ClickMethodSpec::ButtonAreas));
        assert_eq!(res_areas.dwt, Some(false));

        let mut override_none = InputConfigSpec::default();
        override_none.touchpad.click_method = Some(ClickMethodSpec::None);
        let res_none = resolve_effective_libinput_settings(&override_none, &[], &probe);
        assert_eq!(res_none.click_method, Some(ClickMethodSpec::None));

        // 3. Per-device override beats global touchpad config
        let dev_override = vec![DeviceOverrideSpec {
            name: probe.name.clone(),
            click_method: Some(ClickMethodSpec::ButtonAreas),
            dwt: Some(false),
            natural_scroll: Some(false),
            ..DeviceOverrideSpec::default()
        }];
        let res_dev = resolve_effective_libinput_settings(&default_cfg, &dev_override, &probe);
        assert_eq!(res_dev.click_method, Some(ClickMethodSpec::ButtonAreas));
        assert_eq!(res_dev.dwt, Some(false));
        assert_eq!(res_dev.natural_scroll, Some(false));

        // 4. Non-buttonpad touchpad preserves physical-button defaults (does not force Clickfinger/DWT)
        let non_buttonpad = LibinputDeviceProbe {
            name: "SynPS/2 Synaptics TouchPad".to_owned(),
            vendor_id: 0x0002,
            product_id: 0x0007,
            is_pointer: true,
            tap_finger_count: 3,
            supports_clickfinger: false,
            has_physical_buttons: true,
            default_click_method: None,
            dwt_available: true,
            ..LibinputDeviceProbe::default()
        };
        assert!(!non_buttonpad.is_force_touch_buttonpad());
        let res_non_bp = resolve_effective_libinput_settings(&default_cfg, &[], &non_buttonpad);
        assert_eq!(res_non_bp.click_method, None);
        assert_eq!(res_non_bp.dwt, None);

        // 5. External mouse uses pointer config, never touchpad Force Touch defaults
        let mouse_probe = LibinputDeviceProbe {
            name: "Logitech MX Master 3".to_owned(),
            vendor_id: 0x046d,
            product_id: 0xb023,
            is_pointer: true,
            tap_finger_count: 0,
            ..LibinputDeviceProbe::default()
        };
        let mut mouse_cfg = InputConfigSpec::default();
        mouse_cfg.pointer.accel_profile = Some(AccelProfileSpec::Flat);
        mouse_cfg.pointer.accel_speed = Some(0.4);
        let res_mouse = resolve_effective_libinput_settings(&mouse_cfg, &[], &mouse_probe);
        assert!(!res_mouse.is_touchpad);
        assert_eq!(res_mouse.accel_profile, Some(AccelProfileSpec::Flat));
        assert_eq!(res_mouse.accel_speed, Some(0.4));
        assert_eq!(res_mouse.click_method, None);
        assert_eq!(res_mouse.dwt, None);
    }

    #[test]
    fn monitor_spec_parses_auto_scale_position_safe_area_and_non_desktop_aliases() {
        let json = serde_json::json!({
            "output": "eDP-1",
            "mode": "3024x1964@120",
            "scale": "auto",
            "transform": "flipped-90",
            "position": [1920, 0],
            "non_desktop": true,
            "safe_area": { "top": 38 }
        });
        let spec: MonitorSpec = serde_json::from_value(json).expect("valid MonitorSpec");
        assert_eq!(spec.scale, MonitorScaleSpec::Auto);
        assert_eq!(spec.scale.resolve(3024, 1964, 302, 196), 2.0);
        assert_eq!(spec.scale.resolve(1920, 1080, 0, 0), 1.0);
        assert_eq!(spec.parsed_transform().unwrap(), OutputTransform::Flipped90);
        assert_eq!(spec.resolved_position(), Some((1920, 0)));
        assert!(spec.allow_non_desktop);
        assert_eq!(
            spec.safe_area,
            SafeAreaInsets {
                top: 38,
                bottom: 0,
                left: 0,
                right: 0,
            }
        );

        let fixed_json = serde_json::json!({
            "output": "DP-1",
            "scale": 1.5,
            "position": { "x": -1920, "y": 120 },
            "allow_non_desktop": false
        });
        let fixed_spec: MonitorSpec =
            serde_json::from_value(fixed_json).expect("valid fixed MonitorSpec");
        assert_eq!(fixed_spec.scale, MonitorScaleSpec::Fixed(1.5));
        assert_eq!(fixed_spec.scale.resolve(1920, 1080, 0, 0), 1.5);
        assert_eq!(fixed_spec.resolved_position(), Some((-1920, 120)));
        assert!(!fixed_spec.allow_non_desktop);
    }
}
