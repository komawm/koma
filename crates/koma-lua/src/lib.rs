#![forbid(unsafe_code)]

//! Sandboxed Lua 5.4 Configuration & Timed Macro Engine (`Tier 1`).
//!
//! Bridges `Phodopus` (`SequencePoll::Suspend(HostOpHandle)`, `Fuel`, and `MemoryLimit`)
//! to `Koma`'s non-blocking `calloop` event loop without holding native Rust stack frames.

pub mod effective_keymap;
pub mod host_abi;
pub mod keybind_catalog;
pub mod path_resolve;
pub mod recorder;
pub mod scaffold;
pub mod schema;
pub mod typegen;

#[cfg(feature = "phodopus-vm")]
pub mod phodopus_bridge;

pub use crate::{
    effective_keymap::{
        ChordKind, EffectiveBind, chord_identity, chord_kind, describe_action, format_chord,
        group_for_action, lua_quote, merge_binds, short_action,
    },
    host_abi::{
        CONFIG_FUEL_BUDGET, DEFAULT_HEAP_LIMIT_BYTES, HostBridge, LAYOUT_HOOK_FUEL_BUDGET,
        MACRO_SLICE_FUEL_BUDGET, ParkedMacroTask, PendingHostOp, TrampolineScheduler,
    },
    keybind_catalog::{BindRepeat, KEYBIND_CATALOG, KEYBIND_GROUP_ORDER, KeybindCatalogEntry},
    recorder::{RecordedStep, compile_recorded_macro},
    scaffold::{
        ConfigTemplateFile, ScaffoldError, TEMPLATE_MARKER, TEMPLATE_VERSION,
        installed_template_version, render_config_template, scaffold_config_tree,
        template_version_in,
    },
    schema::{
        AccelProfileSpec, ActionDescriptor, AppearanceSpec, BindHandleId, BindOptions, BindSpec,
        ClickMethodSpec, DEFAULT_KEYBOARD_REPEAT_DELAY_MS, DEFAULT_KEYBOARD_REPEAT_RATE_HZ,
        DesktopQuerySnapshot, DeviceOverrideSpec, EffectiveLibinputSettings, FollowMouseMode,
        GestureSpec, InputConfigSpec, KeyboardInputSpec, KomaConfigRegistry, KomaHandleRef,
        LibinputDeviceProbe, LuaLayerRuleSpec, LuaMovePathOptsInput, LuaOutputInfo,
        LuaPathSpecInput, LuaWindowInfo, LuaWindowRuleSpec, LuaWorkspaceInfo, MacroDefinition,
        MacroHandleId, MacroLimitsSpec, MacroParamSpec, MonitorPositionSpec, MonitorScaleSpec,
        MonitorSpec, PointerInputSpec, RuleHandleId, ScrollMethodSpec, TabletInputSpec,
        TouchpadInputSpec, WorkspaceSpec, XwaylandConfigSpec, resolve_effective_libinput_settings,
        validate_gesture_spec,
    },
    typegen::generate_luals_stubs,
};
pub use koma_model::{
    AgentCursorConfig, AnimationConfig, BlurConfig, BorderConfig, DimConfig, DwindleConfig,
    FocusRingConfig, OutputTransform, PathSplineKind, SafeAreaInsets, ScrollingConfig,
    ShadowConfig,
};
