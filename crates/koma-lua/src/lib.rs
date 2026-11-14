//! Sandboxed Lua 5.4 Configuration & Timed Macro Engine (`Tier 1`).
//!
//! Bridges `Phodopus` (`SequencePoll::Suspend(HostOpHandle)`, `Fuel`, and `MemoryLimit`)
//! to `Koma`'s non-blocking `calloop` event loop without holding native Rust stack frames.

pub mod host_abi;
pub mod recorder;
pub mod schema;
pub mod typegen;

#[cfg(feature = "phodopus-vm")]
pub mod phodopus_bridge;

pub use crate::{
    host_abi::{
        CONFIG_FUEL_BUDGET, DEFAULT_HEAP_LIMIT_BYTES, HostBridge, LAYOUT_HOOK_FUEL_BUDGET,
        MACRO_SLICE_FUEL_BUDGET, ParkedMacroTask, PendingHostOp, TrampolineScheduler,
    },
    recorder::{RecordedStep, compile_recorded_macro},
    schema::{
        ActionDescriptor, BindHandleId, BindOptions, BindSpec, GestureSpec, KomaConfigRegistry,
        KomaHandleRef, LuaLayerRuleSpec, LuaWindowRuleSpec, MacroDefinition, MacroHandleId,
        MacroLimitsSpec, MacroParamSpec, MonitorSpec, RuleHandleId, WorkspaceSpec,
        XwaylandConfigSpec, validate_gesture_spec,
    },
    typegen::generate_luals_stubs,
};
pub use koma_model::{AnimationConfig, BlurConfig, DwindleConfig, ScrollingConfig};
