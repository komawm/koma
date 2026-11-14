use std::collections::BTreeMap;

use koma_model::{
    Anchor9, AnchorPoint, AnimationConfig, BlockOutPolicy, BlurConfig, DwindleConfig,
    FrameClockConfig, LayoutMode, LogicalViewport, OpenMode, ScrollingConfig,
};
use koma_proto::{KinematicProfileKind, MacroId, OutputId, TextInjectStrategy};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorSpec {
    pub output: Option<OutputId>,
    #[serde(rename = "match")]
    pub edid_match: Option<EdidMatchSpec>,
    pub mode: String,
    pub scale: f64,
    pub transform: String,
    pub vrr: serde_json::Value,
    pub max_bpc: Option<u8>,
    pub drm_lease: bool,
    pub off: bool,
    pub focus_at_startup: bool,
    pub backdrop_color: Option<String>,
}

impl Default for MonitorSpec {
    fn default() -> Self {
        Self {
            output: None,
            edid_match: None,
            mode: "preferred".to_owned(),
            scale: 1.0,
            transform: "normal".to_owned(),
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
            frame_clock: FrameClockConfig::default(),
            isolate_vseat: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfigSpec {
    pub mod_key: Option<String>,
    pub keyboard: serde_json::Value,
    pub pointer: serde_json::Value,
    pub touchpad: serde_json::Value,
    pub tablet: serde_json::Value,
}

impl InputConfigSpec {
    #[must_use]
    pub fn touchpad_natural_scroll(&self) -> Option<bool> {
        self.touchpad
            .get("natural_scroll")
            .and_then(serde_json::Value::as_bool)
    }

    #[must_use]
    pub fn mouse_scroll_factor(&self) -> f64 {
        self.pointer
            .get("scroll_factor")
            .and_then(serde_json::Value::as_f64)
            .filter(|&f| f.is_finite() && f != 0.0)
            .unwrap_or(1.0)
    }

    #[must_use]
    pub fn touchpad_scroll_factor(&self) -> f64 {
        self.touchpad
            .get("scroll_factor")
            .and_then(serde_json::Value::as_f64)
            .filter(|&f| f.is_finite() && f != 0.0)
            .unwrap_or(1.0)
    }

    #[must_use]
    pub fn touchpad_pinch_sensitivity(&self) -> f64 {
        self.touchpad
            .get("pinch_sensitivity")
            .and_then(serde_json::Value::as_f64)
            .filter(|&f| f.is_finite() && f > 0.0)
            .unwrap_or(1.0)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceOverrideSpec {
    pub name: String,
    pub sensitivity: Option<f64>,
    pub map_to_output: Option<OutputId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSpec {
    pub border: serde_json::Value,
    pub focus_ring: serde_json::Value,
    pub corner_radius: Option<u16>,
    pub shadow: serde_json::Value,
    pub blur: BlurConfig,
    pub animations: AnimationConfig,
}

impl Default for AppearanceSpec {
    fn default() -> Self {
        Self {
            border: serde_json::Value::Null,
            focus_ring: serde_json::Value::Null,
            corner_radius: Some(10),
            shadow: serde_json::Value::Null,
            blur: BlurConfig::default(),
            animations: AnimationConfig::default(),
        }
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
    pub corner_radius: Option<u16>,
    pub clip_to_geometry: Option<bool>,
    pub focusable: Option<bool>,
    pub pin_on_output: Option<bool>,
    pub suppress_events: Vec<String>,
    pub block_out_from: Option<BlockOutPolicy>,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaKinematicsInput {
    pub profile: KinematicProfileKind,
    pub duration_ms: u64,
    pub arc_curvature: f64,
}

impl Default for LuaKinematicsInput {
    fn default() -> Self {
        Self {
            profile: KinematicProfileKind::MinimumJerk,
            duration_ms: 110,
            arc_curvature: 0.08,
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
#[derive(Debug, Clone, Default)]
pub struct KomaConfigRegistry {
    pub env_vars: BTreeMap<String, String>,
    pub notifications: Vec<String>,
    pub monitors: Vec<MonitorSpec>,
    pub workspaces: Vec<WorkspaceSpec>,
    pub dwindle: DwindleConfig,
    pub scrolling: ScrollingConfig,
    pub input: InputConfigSpec,
    pub devices: Vec<DeviceOverrideSpec>,
    pub appearance: AppearanceSpec,
    pub xwayland: XwaylandConfigSpec,
    pub binds: Vec<BindSpec>,
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
