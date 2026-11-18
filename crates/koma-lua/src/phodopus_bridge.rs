//! Complete `Phodopus` (`v0.2.0-alpha.1`) global `koma.*` configuration & `async_sequence` macro ABI.
//!
//! Implements every declarative API and coroutine method specified in `docs/LUA_API_SPEC.md`:
//! - Hard sandbox heap ceiling via `Lua::builder().memory_limit(...)` without loading `io` or `os`.
//! - Symlink-jailed `~/.config/koma/lua/` module registration via `LuaBuilder::add_embedded_module`.
//! - Handle-returning `koma.bind`, `koma.rule.window`, `koma.rule.layer`, and `koma.macro.define`
//!   wrapping `UserData::new_static` with a shared `:set_enabled(bool)` / `:remove()` metatable.
//! - Pre-compiled `koma.act.*` `ActionDescriptor` constructors for zero-VM-wake keybind execution.
//! - Stackless `MacroContext` (`ctx`) methods built with `phodopus::async_sequence` and
//!   `AsyncSequence::suspend(HostOpHandle)`, parking coroutines across `calloop` with zero fuel burn.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    fs,
    path::Path,
    rc::Rc,
};

use gc_arena::Rootable;
use koma_model::{
    AnchorPoint, AnimationConfig, CubicBezier, CurveSpec, DwindleConfig, HookeSpringParams,
    LayoutMode, PathSplineKind, PointerTrajectorySpec, ScrollingConfig,
};
use koma_proto::{Epoch, ErrorSymbol, IpcError, MacroId, TextInjectStrategy, WorkspaceId};
use phodopus::{
    Callback, CallbackReturn, Closure, Context, Error as LuaError, Executor, ExecutorMode, Fuel,
    Function, HostOpError, HostOpHandle, HostOpRegistry, HostOpResult, Lua, MetaMethod,
    RuntimeError, SequenceReturn, StashedExecutor, StashedFunction, Table, UserData, Value,
    async_sequence,
};
use phodopus_util::serde::{LuaSerdeExt as _, from_value, to_value};
use regex::Regex;

use crate::{
    host_abi::{
        CONFIG_FUEL_BUDGET, DEFAULT_HEAP_LIMIT_BYTES, MACRO_SLICE_FUEL_BUDGET, ParkedMacroTask,
        PendingHostOp, TrampolineScheduler,
    },
    schema::{
        ActionDescriptor, AppearanceSpec, BindHandleId, BindOptions, BindSpec,
        DesktopQuerySnapshot, DeviceOverrideSpec, GestureSpec, InputConfigSpec, KomaConfigRegistry,
        KomaHandleRef, LuaAnchorPointInput, LuaClickOptsInput, LuaDragSpecInput, LuaKeyOptsInput,
        LuaKinematicsInput, LuaLayerRuleSpec, LuaMovePathOptsInput, LuaPathSpecInput,
        LuaScrollSpecInput, LuaSettleOptsInput, LuaTextOptsInput, LuaWindowRuleSpec,
        MacroDefinition, MacroHandleId, MacroLimitsSpec, MacroParamSpec, MonitorSpec, RuleHandleId,
        TriggerEdge, WorkspaceSpec, XwaylandConfigSpec, validate_gesture_spec,
    },
};

/// Ordered Ring 0 host side-effects emitted by synchronous and `async_sequence` `MacroContext` calls.
#[derive(Debug, Clone)]
pub enum QueuedHostAction {
    AssertFocusedApp {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        app_regex: String,
    },
    TapKeyChord {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        chord: String,
        hold_ms: u64,
    },
    KeyDown {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        key: String,
    },
    KeyUp {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        key: String,
    },
    MovePointer {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        target: AnchorPoint,
        kinematics: LuaKinematicsInput,
    },
    MovePointerPath {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        waypoints: Vec<AnchorPoint>,
        spline: PathSplineKind,
        kinematics: LuaKinematicsInput,
    },
    SettleAgentCursor {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        immediate: bool,
    },
    PointerButton {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        button: String,
        pressed: bool,
    },
    ClickPointer {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        target: AnchorPoint,
        opts: LuaClickOptsInput,
    },
    Scroll {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        dx: f64,
        dy: f64,
    },
    InjectText {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        utf8: String,
        strategy: TextInjectStrategy,
    },
    PasteMime {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        mime_type: String,
        payload: Vec<u8>,
    },
    SpawnProcess {
        argv: Vec<String>,
    },
    DispatchAction {
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        action: ActionDescriptor,
    },
}

/// Shared interior state accessible to `Phodopus` callbacks on the single-threaded `calloop` thread.
#[derive(Default)]
struct SandboxInner {
    next_id: u64,
    now_ms: u64,
    in_declarative_load: bool,
    event_dispatch_depth: u8,
    pub desktop_snapshot: DesktopQuerySnapshot,
    pub config: KomaConfigRegistry,
    pub bind_callbacks: BTreeMap<BindHandleId, StashedFunction>,
    pub mode_enter_hooks: BTreeMap<String, StashedFunction>,
    pub event_hooks: BTreeMap<String, Vec<StashedFunction>>,
    pub macro_fns: BTreeMap<MacroId, StashedFunction>,
    pub scheduler: TrampolineScheduler,
    pub host_actions: VecDeque<QueuedHostAction>,
}

impl SandboxInner {
    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn park_host_op(&mut self, scope: &MacroExecScope, op: PendingHostOp) -> HostOpHandle {
        let handle = HostOpHandle::new();
        self.scheduler.register_suspended(ParkedMacroTask {
            raw_host_op_handle: handle.raw(),
            macro_id: scope.macro_id.clone(),
            ws_id: scope.ws_id.clone(),
            expected_epoch: scope.expected_epoch,
            op,
        });
        handle
    }
}

type SharedState = Rc<RefCell<SandboxInner>>;

/// Execution scope carried inside every `MacroContext` (`ctx`) `UserData` instance.
#[derive(Clone)]
struct MacroExecScope {
    ws_id: WorkspaceId,
    expected_epoch: Epoch,
    macro_id: MacroId,
    state: SharedState,
}

#[derive(Debug)]
struct LuaBridgeError(String);

impl std::fmt::Display for LuaBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LuaBridgeError {}

fn runtime_err<'gc>(msg: impl Into<String>) -> LuaError<'gc> {
    RuntimeError::new(LuaBridgeError(msg.into())).into()
}

/// Tracks a live `StashedExecutor` parked on a `HostOpHandle` inside `PhodopusSandbox`.
struct ActiveMacroInstance {
    ws_id: WorkspaceId,
    #[allow(dead_code)]
    expected_epoch: Epoch,
    #[allow(dead_code)]
    macro_id: MacroId,
    stashed_exec: StashedExecutor,
    parked_raw_handle: u64,
}

/// Summary returned by `PhodopusSandbox::drive_ready_coroutines` on each `calloop` wakeup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SchedulerStepReport {
    pub resumed_slices: usize,
    pub completed_macros: usize,
    pub cancelled_stale: usize,
}

/// Production `Phodopus` sandbox managing `~/.config/koma/init.lua` and `HostOpHandle` macro coroutines.
pub struct PhodopusSandbox {
    next_tx_id: u64,
    active_txs: BTreeMap<u64, ActiveMacroInstance>,
    handle_to_tx: BTreeMap<u64, u64>,
    lua: Lua,
    state: SharedState,
    clock_ms: Rc<Cell<u64>>,
}

impl Default for PhodopusSandbox {
    fn default() -> Self {
        Self::new(DEFAULT_HEAP_LIMIT_BYTES, None).expect("default PhodopusSandbox init")
    }
}

impl PhodopusSandbox {
    /// Builds a fresh `Phodopus` VM with a hard `gc-arena` heap ceiling (`memory_limit`),
    /// optionally pre-loading jailed `.lua` modules under `<config_dir>/lua/`.
    ///
    /// Note: `.with_io()` is intentionally NEVER called so `io.*` and `os.execute` do not exist.
    pub fn new(heap_limit_bytes: usize, config_dir: Option<&Path>) -> Result<Self, String> {
        let mut builder = Lua::builder();
        builder.memory_limit(heap_limit_bytes);

        if let Some(dir) = config_dir {
            let lua_subdir = dir.join("lua");
            if lua_subdir.is_dir() {
                preload_jailed_lua_tree(&mut builder, &lua_subdir)?;
            }
        }

        let mut lua = builder.build();
        lua.load_serde();

        let state: SharedState = Rc::new(RefCell::new(SandboxInner::default()));
        let clock_ms = Rc::new(Cell::new(0_u64));

        {
            let state_ref = state.clone();
            lua.try_enter(|ctx| {
                install_koma_globals(ctx, state_ref)?;
                Ok(())
            })
            .map_err(|e| format!("failed installing koma.* globals: {e}"))?;
        }

        Ok(Self {
            next_tx_id: 1,
            active_txs: BTreeMap::new(),
            handle_to_tx: BTreeMap::new(),
            lua,
            state,
            clock_ms,
        })
    }

    /// Updates the monotonic millisecond clock read when scheduling `SleepTimer` / `WaitDamageSettle`.
    pub fn set_clock_ms(&self, now_ms: u64) {
        self.clock_ms.set(now_ms);
        self.state.borrow_mut().now_ms = now_ms;
    }

    /// Updates the read-only desktop snapshot exposed to `ctx:windows()`, `ctx:focused_window()`,
    /// `ctx:outputs()`, and `ctx:workspaces()`.
    pub fn set_desktop_snapshot(&self, snap: DesktopQuerySnapshot) {
        self.state.borrow_mut().desktop_snapshot = snap;
    }

    /// Returns `true` when at least one `koma.on(event_name, ...)` handler is registered.
    #[must_use]
    pub fn has_event_hooks(&self, event_name: &str) -> bool {
        self.state
            .borrow()
            .event_hooks
            .get(event_name)
            .is_some_and(|hooks| !hooks.is_empty())
    }

    /// Invokes all `koma.on(event_name, fn(ctx, ev))` listeners with recursion depth capped at `4`.
    pub fn emit_event_hooks(
        &mut self,
        event_name: &str,
        payload: &serde_json::Value,
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        now_ms: u64,
    ) -> Result<usize, String> {
        const MAX_EVENT_DEPTH: u8 = 4;
        let hooks = {
            let mut inner = self.state.borrow_mut();
            if inner.event_dispatch_depth >= MAX_EVENT_DEPTH {
                return Err(format!(
                    "event hook recursion limit ({MAX_EVENT_DEPTH}) exceeded on `{event_name}`"
                ));
            }
            let list = inner
                .event_hooks
                .get(event_name)
                .cloned()
                .unwrap_or_default();
            if list.is_empty() {
                return Ok(0);
            }
            inner.event_dispatch_depth += 1;
            list
        };

        self.set_clock_ms(now_ms);
        let mut launched = 0_usize;
        let mut last_err: Option<String> = None;

        for (idx, stashed_fn) in hooks.into_iter().enumerate() {
            let scope_ws_id = ws_id.clone();
            let macro_id = MacroId::new(format!("event_{event_name}_{idx}"));
            let scope = MacroExecScope {
                ws_id: scope_ws_id.clone(),
                expected_epoch,
                macro_id: macro_id.clone(),
                state: self.state.clone(),
            };

            let step_res = self.lua.try_enter(|ctx| {
                let cb_fn = ctx.fetch(&stashed_fn);
                let ctx_ud = create_macro_context_userdata(ctx, scope)?;
                let ev_val = to_value(ctx, payload).map_err(RuntimeError::new)?;
                let exec = Executor::start(ctx, cb_fn, (ctx_ud, ev_val));
                let mut fuel = Fuel::with(MACRO_SLICE_FUEL_BUDGET as i32);
                let _ = exec.step(ctx, &mut fuel).map_err(RuntimeError::new)?;
                Ok(ctx.stash(exec))
            });

            match step_res {
                Ok(stashed_exec) => {
                    launched += 1;
                    let (mode, pending_raw) = self.lua.enter(|ctx| {
                        let ex = ctx.fetch(&stashed_exec);
                        (ex.mode(), ex.pending_host_op(ctx).map(HostOpHandle::raw))
                    });
                    if mode == ExecutorMode::HostSuspended
                        && let Some(raw_handle) = pending_raw
                    {
                        let tx_id = self.next_tx_id;
                        self.next_tx_id += 1;
                        self.handle_to_tx.insert(raw_handle, tx_id);
                        self.active_txs.insert(
                            tx_id,
                            ActiveMacroInstance {
                                ws_id: scope_ws_id,
                                expected_epoch,
                                macro_id,
                                stashed_exec,
                                parked_raw_handle: raw_handle,
                            },
                        );
                    }
                }
                Err(err) => {
                    last_err = Some(format!("Lua event `{event_name}` hook error: {err}"));
                }
            }
        }

        self.state.borrow_mut().event_dispatch_depth -= 1;
        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(launched)
    }

    /// Evaluates `~/.config/koma/init.lua` synchronously within `CONFIG_FUEL_BUDGET` (`200,000` fuel).
    /// Any attempt to yield/suspend during declarative config load is rejected (`LUA_API_SPEC.md` §1.1).
    pub fn eval_init_lua(&mut self, source: &str) -> Result<KomaConfigRegistry, String> {
        self.state.borrow_mut().in_declarative_load = true;
        let compile_res = self.lua.try_enter(|ctx| {
            let closure = Closure::load(ctx, Some("=init.lua"), source.as_bytes())?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        });
        let stashed = match compile_res {
            Ok(ex) => ex,
            Err(err) => {
                self.state.borrow_mut().in_declarative_load = false;
                return Err(format!("init.lua syntax error: {err}"));
            }
        };

        let mut fuel = Fuel::with(CONFIG_FUEL_BUDGET as i32);
        let step_res = self.lua.try_enter(|ctx| {
            let exec = ctx.fetch(&stashed);
            let done = exec.step(ctx, &mut fuel).map_err(RuntimeError::new)?;
            if !done && !fuel.should_continue() {
                return Err(runtime_err(format!(
                    "init.lua exceeded CONFIG_FUEL_BUDGET ({CONFIG_FUEL_BUDGET} instructions)"
                )));
            }
            if exec.mode() == ExecutorMode::HostSuspended {
                return Err(runtime_err(
                    "suspending macro operations are forbidden at top-level during init.lua load",
                ));
            }
            exec.take_result::<()>(ctx).map_err(RuntimeError::new)??;
            Ok(())
        });
        self.state.borrow_mut().in_declarative_load = false;

        step_res.map_err(|e| format!("init.lua evaluation error: {e}"))?;
        Ok(self.state.borrow().config.clone())
    }

    /// Returns a clone of the current declarative animation & curve registry (`AnimationConfig`).
    #[must_use]
    pub fn animations_snapshot(&self) -> AnimationConfig {
        self.state.borrow().config.appearance.animations.clone()
    }

    /// Launches a registered `koma.macro.define` coroutine (`lua.exec_tx` or `koma.act.macro.run`)
    /// under `expected_epoch`, stepping its first `Fuel`-bounded slice immediately.
    pub fn start_macro_tx(
        &mut self,
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        macro_id: &MacroId,
        json_args: &serde_json::Value,
        now_ms: u64,
    ) -> Result<(StashedExecutor, ExecutorMode), IpcError> {
        self.set_clock_ms(now_ms);

        let (stashed_fn, fuel_budget) = {
            let inner = self.state.borrow();
            let def = inner.config.macros.get(macro_id).ok_or_else(|| {
                IpcError::new(
                    ErrorSymbol::ErrNotFound,
                    format!("unknown macro `{macro_id}`"),
                )
            })?;
            if !def.enabled {
                return Err(IpcError::new(
                    ErrorSymbol::ErrInvalidParams,
                    format!("macro `{macro_id}` is currently disabled"),
                ));
            }
            let func = inner.macro_fns.get(macro_id).cloned().ok_or_else(|| {
                IpcError::new(
                    ErrorSymbol::ErrNotFound,
                    format!("missing stashed closure for macro `{macro_id}`"),
                )
            })?;
            (func, def.limits.fuel)
        };

        let scope_ws_id = ws_id.clone();
        let scope = MacroExecScope {
            ws_id,
            expected_epoch,
            macro_id: macro_id.clone(),
            state: self.state.clone(),
        };

        let stashed_exec = self
            .lua
            .try_enter(|ctx| {
                let run_fn = ctx.fetch(&stashed_fn);
                let ctx_ud = create_macro_context_userdata(ctx, scope)?;
                let args_val = to_value(ctx, json_args).map_err(RuntimeError::new)?;
                let exec = Executor::start(ctx, run_fn, (ctx_ud, args_val));
                let mut fuel = Fuel::with(fuel_budget.min(i32::MAX as u64) as i32);
                let _ = exec.step(ctx, &mut fuel).map_err(RuntimeError::new)?;
                Ok(ctx.stash(exec))
            })
            .map_err(|e| {
                IpcError::new(
                    ErrorSymbol::ErrInvalidParams,
                    format!("Lua macro error: {e}"),
                )
            })?;

        let (mode, pending_raw) = self.lua.enter(|ctx| {
            let ex = ctx.fetch(&stashed_exec);
            (ex.mode(), ex.pending_host_op(ctx).map(HostOpHandle::raw))
        });
        if mode == ExecutorMode::HostSuspended
            && let Some(raw_handle) = pending_raw
        {
            let tx_id = self.next_tx_id;
            self.next_tx_id += 1;
            self.handle_to_tx.insert(raw_handle, tx_id);
            self.active_txs.insert(
                tx_id,
                ActiveMacroInstance {
                    ws_id: scope_ws_id,
                    expected_epoch,
                    macro_id: macro_id.clone(),
                    stashed_exec: stashed_exec.clone(),
                    parked_raw_handle: raw_handle,
                },
            );
        }
        Ok((stashed_exec, mode))
    }

    /// Launches a stashed `koma.bind(chord, function(ctx) ... end)` coroutine callback on `ws_id`,
    /// executing its initial `MACRO_SLICE_FUEL_BUDGET` slice immediately and parking it inside
    /// `active_txs` if it yields on `ctx:sleep`, `ctx:drag`, or `ctx:wait_settle`.
    pub fn start_bind_callback_tx(
        &mut self,
        bind_id: BindHandleId,
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        now_ms: u64,
    ) -> Result<ExecutorMode, String> {
        let stashed_fn = {
            let inner = self.state.borrow();
            inner.bind_callbacks.get(&bind_id).cloned().ok_or_else(|| {
                format!(
                    "missing stashed Lua callback for BindHandleId({})",
                    bind_id.0
                )
            })?
        };
        let label = MacroId::new(format!("bind_cb_{}", bind_id.0));
        self.launch_context_closure(stashed_fn, label, ws_id, expected_epoch, now_ms)
    }

    /// Invokes an optional `koma.mode(name, { on_enter = function(ctx) ... end })` hook coroutine.
    pub fn start_mode_enter_hook_tx(
        &mut self,
        mode_name: &str,
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        now_ms: u64,
    ) -> Result<Option<ExecutorMode>, String> {
        let maybe_fn = self.state.borrow().mode_enter_hooks.get(mode_name).cloned();
        let Some(stashed_fn) = maybe_fn else {
            return Ok(None);
        };
        let label = MacroId::new(format!("mode_enter_{mode_name}"));
        self.launch_context_closure(stashed_fn, label, ws_id, expected_epoch, now_ms)
            .map(Some)
    }

    fn launch_context_closure(
        &mut self,
        stashed_fn: StashedFunction,
        macro_id: MacroId,
        ws_id: WorkspaceId,
        expected_epoch: Epoch,
        now_ms: u64,
    ) -> Result<ExecutorMode, String> {
        self.set_clock_ms(now_ms);
        let scope_ws_id = ws_id.clone();
        let scope = MacroExecScope {
            ws_id,
            expected_epoch,
            macro_id: macro_id.clone(),
            state: self.state.clone(),
        };

        let stashed_exec = self
            .lua
            .try_enter(|ctx| {
                let cb_fn = ctx.fetch(&stashed_fn);
                let ctx_ud = create_macro_context_userdata(ctx, scope)?;
                let exec = Executor::start(ctx, cb_fn, (ctx_ud,));
                let mut fuel = Fuel::with(MACRO_SLICE_FUEL_BUDGET as i32);
                let _ = exec.step(ctx, &mut fuel).map_err(RuntimeError::new)?;
                Ok(ctx.stash(exec))
            })
            .map_err(|e| format!("Lua keybind callback error: {e}"))?;

        let (mode, pending_raw) = self.lua.enter(|ctx| {
            let ex = ctx.fetch(&stashed_exec);
            (ex.mode(), ex.pending_host_op(ctx).map(HostOpHandle::raw))
        });
        if mode == ExecutorMode::HostSuspended
            && let Some(raw_handle) = pending_raw
        {
            let tx_id = self.next_tx_id;
            self.next_tx_id += 1;
            self.handle_to_tx.insert(raw_handle, tx_id);
            self.active_txs.insert(
                tx_id,
                ActiveMacroInstance {
                    ws_id: scope_ws_id,
                    expected_epoch,
                    macro_id,
                    stashed_exec,
                    parked_raw_handle: raw_handle,
                },
            );
        }
        Ok(mode)
    }

    /// Resumes a parked `ExecutorMode::HostSuspended` coroutine when its timer or damage-settle fires.
    pub fn resume_and_step(
        &mut self,
        stashed: &StashedExecutor,
        raw_handle: u64,
        result: HostOpResult,
        now_ms: u64,
    ) -> Result<ExecutorMode, HostOpError> {
        self.set_clock_ms(now_ms);
        let handle = HostOpHandle::from_raw(raw_handle);
        self.lua.enter(|ctx| -> Result<ExecutorMode, HostOpError> {
            let exec = ctx.fetch(stashed);
            exec.resume_host_op(ctx, handle, result)?;
            let mut fuel = Fuel::with(MACRO_SLICE_FUEL_BUDGET as i32);
            let _ = exec.step(ctx, &mut fuel);
            Ok(exec.mode())
        })
    }

    /// Cancels a parked `HostOpHandle` coroutine with a catchable error string (e.g., on `Mod + G`).
    pub fn cancel_parked(
        &mut self,
        stashed: &StashedExecutor,
        raw_handle: u64,
        reason: &str,
    ) -> Result<(), HostOpError> {
        let handle = HostOpHandle::from_raw(raw_handle);
        self.lua.enter(|ctx| -> Result<(), HostOpError> {
            let exec = ctx.fetch(stashed);
            exec.cancel_host_op(ctx, handle, reason)?;
            let mut fuel = Fuel::with(4_096);
            let _ = exec.step(ctx, &mut fuel);
            Ok(())
        })
    }

    /// Runs incremental GC and sweeps any `HostOpRegistry` entries whose Lua thread was collected.
    pub fn sweep_abandoned_host_ops(&mut self) -> Vec<u64> {
        self.lua.gc_collect();
        self.lua.enter(|ctx| {
            ctx.singleton::<Rootable![HostOpRegistry<'_>]>()
                .abandoned_handles(&ctx)
                .into_iter()
                .map(HostOpHandle::raw)
                .collect()
        })
    }

    /// Drains queued Ring 0 side-effect commands emitted since the previous tick.
    pub fn drain_queued_actions(&mut self) -> Vec<QueuedHostAction> {
        self.state.borrow_mut().host_actions.drain(..).collect()
    }

    /// Borrows the mutable `TrampolineScheduler` for `calloop` readiness polling.
    pub fn with_scheduler_mut<R>(&mut self, f: impl FnOnce(&mut TrampolineScheduler) -> R) -> R {
        let mut guard = self.state.borrow_mut();
        f(&mut guard.scheduler)
    }

    /// Polls `TrampolineScheduler` against `now_ms` and `settled_workspaces`, cancels any coroutine
    /// whose workspace `Epoch` changed while parked, resumes ready `HostOpHandle`s, and re-tracks
    /// multi-stage coroutines (such as 5-stage `ctx:drag` or `ctx:sequence`) if they park again!
    pub fn drive_ready_coroutines(
        &mut self,
        now_ms: u64,
        settled_workspaces: &[WorkspaceId],
        bridge: &dyn crate::host_abi::HostBridge,
    ) -> SchedulerStepReport {
        self.set_clock_ms(now_ms);
        let (valid_tasks, stale_tasks) =
            self.state
                .borrow_mut()
                .scheduler
                .poll_ready(now_ms, settled_workspaces, bridge);

        let mut report = SchedulerStepReport::default();

        // 1. Deterministically abort stale-epoch coroutines (`IPC_SPEC.md` §4.2)
        for stale in stale_tasks {
            if let Some(tx_id) = self.handle_to_tx.remove(&stale.raw_host_op_handle)
                && let Some(inst) = self.active_txs.remove(&tx_id)
            {
                let _ = self.cancel_parked(
                    &inst.stashed_exec,
                    stale.raw_host_op_handle,
                    "ERR_STALE_EPOCH: workspace epoch changed while macro was suspended",
                );
                report.cancelled_stale += 1;
            }
        }

        // 2. Resume ready `HostOpHandle` coroutines and re-park if next stage suspends
        for ready in valid_tasks {
            let Some(tx_id) = self.handle_to_tx.remove(&ready.raw_host_op_handle) else {
                continue;
            };
            let Some(mut inst) = self.active_txs.remove(&tx_id) else {
                continue;
            };

            report.resumed_slices += 1;
            let stashed_clone = inst.stashed_exec.clone();
            let _ = self.resume_and_step(
                &stashed_clone,
                ready.raw_host_op_handle,
                HostOpResult::empty(),
                now_ms,
            );

            let (next_mode, next_pending_raw) = self.lua.enter(|ctx| {
                let ex = ctx.fetch(&stashed_clone);
                (ex.mode(), ex.pending_host_op(ctx).map(HostOpHandle::raw))
            });

            match next_mode {
                ExecutorMode::HostSuspended => {
                    if let Some(next_raw) = next_pending_raw {
                        inst.parked_raw_handle = next_raw;
                        self.handle_to_tx.insert(next_raw, tx_id);
                        self.active_txs.insert(tx_id, inst);
                    }
                }
                _ => {
                    report.completed_macros += 1;
                }
            }
        }

        report
    }

    /// Cancels all live `Phodopus` macro coroutines targeting `ws_id` (invoked on `Mod + G` preemption
    /// or `AssertFocusedApp` / `ProtectedSurface` violation) in O(k).
    pub fn cancel_workspace_coroutines(&mut self, ws_id: &WorkspaceId, reason: &str) -> usize {
        let handles = self
            .state
            .borrow_mut()
            .scheduler
            .cancel_workspace_tasks(ws_id);
        let mut cancelled = 0;
        for raw_h in handles {
            if let Some(tx_id) = self.handle_to_tx.remove(&raw_h)
                && let Some(inst) = self.active_txs.remove(&tx_id)
            {
                let _ = self.cancel_parked(&inst.stashed_exec, raw_h, reason);
                cancelled += 1;
            }
        }
        // Also sweep any orphaned active_txs matching ws_id
        let orphan_ids: Vec<u64> = self
            .active_txs
            .iter()
            .filter(|(_, inst)| &inst.ws_id == ws_id)
            .map(|(&id, _)| id)
            .collect();
        for tx_id in orphan_ids {
            if let Some(inst) = self.active_txs.remove(&tx_id) {
                self.handle_to_tx.remove(&inst.parked_raw_handle);
                let _ = self.cancel_parked(&inst.stashed_exec, inst.parked_raw_handle, reason);
                cancelled += 1;
            }
        }
        cancelled
    }

    #[must_use]
    pub fn next_wakeup_deadline_ms(&self) -> Option<u64> {
        self.state.borrow().scheduler.earliest_deadline_ms()
    }

    #[must_use]
    pub fn pending_settle_quiet_ms(&self, ws_id: &WorkspaceId) -> Option<u64> {
        self.state.borrow().scheduler.pending_settle_quiet_ms(ws_id)
    }

    /// Returns `true` if one or more `MacroContext` coroutines are currently suspended on `calloop`.
    #[must_use]
    pub fn has_active_coroutines(&self) -> bool {
        !self.active_txs.is_empty()
    }

    #[must_use]
    pub fn active_coroutine_count(&self) -> usize {
        self.active_txs.len()
    }

    pub fn config_snapshot(&self) -> KomaConfigRegistry {
        self.state.borrow().config.clone()
    }
}

// ==============================================================================
// Jailed `~/.config/koma/lua/` VFS Preloader (Zero `io` Stdlib, Symlink-Safe)
// ==============================================================================

fn preload_jailed_lua_tree(
    builder: &mut phodopus::LuaBuilder,
    lua_root: &Path,
) -> Result<(), String> {
    let canonical_root = lua_root
        .canonicalize()
        .map_err(|e| format!("failed canonicalizing {}: {e}", lua_root.display()))?;

    fn walk_dir(
        builder: &mut phodopus::LuaBuilder,
        canonical_root: &Path,
        current: &Path,
    ) -> Result<(), String> {
        let entries = fs::read_dir(current)
            .map_err(|e| format!("failed reading {}: {e}", current.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let canonical = path
                .canonicalize()
                .map_err(|e| format!("failed canonicalizing {}: {e}", path.display()))?;

            // Hard sandbox boundary: reject symlinks escaping `~/.config/koma/lua/`
            if !canonical.starts_with(canonical_root) {
                return Err(format!(
                    "security violation: Lua module {} escapes jailed root {}",
                    path.display(),
                    canonical_root.display()
                ));
            }

            if canonical.is_dir() {
                walk_dir(builder, canonical_root, &canonical)?;
            } else if canonical.extension().is_some_and(|ext| ext == "lua") {
                let rel = canonical
                    .strip_prefix(canonical_root)
                    .map_err(|e| e.to_string())?;
                let mut parts: Vec<String> = rel
                    .iter()
                    .map(|os| os.to_string_lossy().into_owned())
                    .collect();
                if let Some(last) = parts.last_mut() {
                    if last == "init.lua" {
                        parts.pop();
                    } else if let Some(stripped) = last.strip_suffix(".lua") {
                        *last = stripped.to_owned();
                    }
                }
                if !parts.is_empty() {
                    let module_name = parts.join(".");
                    let bytes = fs::read(&canonical)
                        .map_err(|e| format!("failed reading {}: {e}", canonical.display()))?;
                    builder.add_embedded_module(module_name, bytes);
                }
            }
        }
        Ok(())
    }

    walk_dir(builder, &canonical_root, &canonical_root)
}

// ==============================================================================
// Global `koma.*` Table, Handle Metatable & `koma.act.*` Builders
// ==============================================================================

fn build_handle_metatable<'gc>(ctx: Context<'gc>, state: SharedState) -> Table<'gc> {
    let index = Table::new(&ctx);
    let mt = Table::new(&ctx);

    let st_enable = state.clone();
    index.set_field(
        ctx,
        "set_enabled",
        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let enabled: bool = stack.consume(ctx)?;
            let handle = *ud
                .downcast_static::<KomaHandleRef>()
                .map_err(RuntimeError::new)?;
            let ok = st_enable
                .borrow_mut()
                .config
                .apply_handle_enabled(handle, enabled);
            stack.replace(ctx, ok);
            Ok(CallbackReturn::Return)
        }),
    );

    let st_remove = state;
    index.set_field(
        ctx,
        "remove",
        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let handle = *ud
                .downcast_static::<KomaHandleRef>()
                .map_err(RuntimeError::new)?;
            let ok = st_remove.borrow_mut().config.remove_handle(handle);
            stack.replace(ctx, ok);
            Ok(CallbackReturn::Return)
        }),
    );

    mt.set(ctx, MetaMethod::Index, index).unwrap();
    mt
}

fn wrap_handle<'gc>(ctx: Context<'gc>, mt: Table<'gc>, handle: KomaHandleRef) -> UserData<'gc> {
    let ud = UserData::new_static(&ctx, handle);
    ud.set_metatable(&ctx, Some(mt));
    ud
}

fn install_koma_globals<'gc>(ctx: Context<'gc>, state: SharedState) -> Result<(), LuaError<'gc>> {
    let koma = Table::new(&ctx);
    let handle_mt = build_handle_metatable(ctx, state.clone());

    // 1. koma.env(key, value) & koma.notify(message)
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "env",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let (k, v): (std::string::String, std::string::String) = stack.consume(ctx)?;
                st.borrow_mut().config.env_vars.insert(k, v);
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "notify",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let msg: std::string::String = stack.consume(ctx)?;
                st.borrow_mut().config.notifications.push(msg);
                Ok(CallbackReturn::Return)
            }),
        );
    }

    // 2. koma.layout, koma.monitor, koma.workspace, koma.input, koma.device, koma.appearance
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "layout",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let mode_str: std::string::String = stack.consume(ctx)?;
                let parsed_mode = match mode_str.trim().to_ascii_lowercase().as_str() {
                    "dwindle" | "bsp" => LayoutMode::Dwindle,
                    "scrolling" => LayoutMode::Scrolling,
                    "deck" => LayoutMode::Deck,
                    other => {
                        return Err(runtime_err(format!(
                            "invalid layout mode `{other}` (expected \"dwindle\", \"scrolling\", \"deck\", or \"bsp\")"
                        )));
                    }
                };
                let mut inner = st.borrow_mut();
                inner.config.default_layout = parsed_mode;
                for ws in &mut inner.config.workspaces {
                    if !ws.has_explicit_layout && !ws.presentation.eq_ignore_ascii_case("headless")
                    {
                        ws.layout = parsed_mode;
                    }
                }
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "monitor",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let has_explicit_transform =
                    matches!(val, Value::Table(t) if !t.get_value(ctx, "transform").is_nil());
                let mut spec: MonitorSpec = from_value(val).map_err(RuntimeError::new)?;
                spec.has_explicit_transform = has_explicit_transform;
                spec.parsed_transform().map_err(runtime_err)?;
                if let crate::schema::MonitorScaleSpec::Fixed(s) = spec.scale
                    && (!s.is_finite() || !(0.5..=4.0).contains(&s))
                {
                    return Err(runtime_err(format!(
                        "invalid monitor scale `{s}` (expected \"auto\" or 0.5..=4.0)"
                    )));
                }
                st.borrow_mut().config.monitors.push(spec);
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "workspace",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let (has_explicit_layout, has_explicit_gaps) = match val {
                    Value::Table(t) => (
                        !t.get_value(ctx, "layout").is_nil(),
                        !t.get_value(ctx, "gaps").is_nil()
                            || !t.get_value(ctx, "gaps_in").is_nil()
                            || !t.get_value(ctx, "gaps_out").is_nil(),
                    ),
                    _ => (false, false),
                };
                let mut spec: WorkspaceSpec = from_value(val).map_err(RuntimeError::new)?;
                let mut inner = st.borrow_mut();
                spec.has_explicit_layout = has_explicit_layout;
                spec.has_explicit_gaps = has_explicit_gaps;
                if !has_explicit_layout && !spec.presentation.eq_ignore_ascii_case("headless") {
                    spec.layout = inner.config.default_layout;
                }
                if let Some(gi) = spec.gaps_in {
                    spec.gaps.inner = gi;
                }
                if let Some(go) = spec.gaps_out {
                    spec.gaps.outer = go;
                }
                inner.config.workspaces.push(spec);
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "dwindle",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let spec: DwindleConfig = from_value(val).map_err(RuntimeError::new)?;
                st.borrow_mut().config.dwindle = spec;
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "scrolling",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let spec: ScrollingConfig = from_value(val).map_err(RuntimeError::new)?;
                st.borrow_mut().config.scrolling = spec.sanitized();
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "input",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let spec: InputConfigSpec = from_value(val).map_err(RuntimeError::new)?;
                st.borrow_mut().config.input = spec;
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "device",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let spec: DeviceOverrideSpec = from_value(val).map_err(RuntimeError::new)?;
                st.borrow_mut().config.devices.push(spec);
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "appearance",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let mut spec: AppearanceSpec = from_value(val).map_err(RuntimeError::new)?;
                let mut inner = st.borrow_mut();
                let mut merged_curves =
                    std::mem::take(&mut inner.config.appearance.animations.curves);
                merged_curves.extend(spec.animations.curves);
                spec.animations.curves = merged_curves;
                inner.config.appearance = spec;
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "bezier",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let (name, arg2, y1_opt, x2_opt, y2_opt, dur_opt): (
                    std::string::String,
                    Value,
                    Option<f64>,
                    Option<f64>,
                    Option<f64>,
                    Option<u32>,
                ) = stack.consume(ctx)?;
                let (bezier, duration_ms) = match arg2 {
                    Value::Table(tbl) => {
                        let mut pts: Option<[f64; 4]> = tbl.get(ctx, "points").ok();
                        if pts.is_none() {
                            let p1: Result<f64, _> = tbl.get(ctx, 1);
                            let p2: Result<f64, _> = tbl.get(ctx, 2);
                            let p3: Result<f64, _> = tbl.get(ctx, 3);
                            let p4: Result<f64, _> = tbl.get(ctx, 4);
                            if let (Ok(a), Ok(b), Ok(c), Ok(d)) = (p1, p2, p3, p4) {
                                pts = Some([a, b, c, d]);
                            }
                        }
                        let [x1, y1, x2, y2] = if let Some(p) = pts {
                            p
                        } else {
                            let x1: f64 = tbl
                                .get(ctx, "x1")
                                .map_err(|_| runtime_err("koma.bezier table requires `points` or `x1,y1,x2,y2`"))?;
                            let y1: f64 = tbl.get(ctx, "y1").map_err(RuntimeError::new)?;
                            let x2: f64 = tbl.get(ctx, "x2").map_err(RuntimeError::new)?;
                            let y2: f64 = tbl.get(ctx, "y2").map_err(RuntimeError::new)?;
                            [x1, y1, x2, y2]
                        };
                        let dur: Option<u32> = tbl.get(ctx, "duration_ms").ok();
                        (CubicBezier::new(x1, y1, x2, y2), dur.unwrap_or(250).clamp(1, 5000))
                    }
                    Value::Number(x1) => {
                        let y1 = y1_opt.ok_or_else(|| runtime_err("koma.bezier requires y1"))?;
                        let x2 = x2_opt.ok_or_else(|| runtime_err("koma.bezier requires x2"))?;
                        let y2 = y2_opt.ok_or_else(|| runtime_err("koma.bezier requires y2"))?;
                        (
                            CubicBezier::new(x1, y1, x2, y2),
                            dur_opt.unwrap_or(250).clamp(1, 5000),
                        )
                    }
                    Value::Integer(x1) => {
                        let y1 = y1_opt.ok_or_else(|| runtime_err("koma.bezier requires y1"))?;
                        let x2 = x2_opt.ok_or_else(|| runtime_err("koma.bezier requires x2"))?;
                        let y2 = y2_opt.ok_or_else(|| runtime_err("koma.bezier requires y2"))?;
                        (
                            CubicBezier::new(x1 as f64, y1, x2, y2),
                            dur_opt.unwrap_or(250).clamp(1, 5000),
                        )
                    }
                    _ => {
                        return Err(runtime_err(
                            "koma.bezier(name, x1, y1, x2, y2, duration_ms?) or koma.bezier(name, { points = {...}, duration_ms = ... })",
                        ));
                    }
                };
                st.borrow_mut().config.appearance.animations.curves.insert(
                    name,
                    CurveSpec::Bezier {
                        bezier,
                        duration_ms,
                    },
                );
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "spring",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let (name, tbl): (std::string::String, Table) = stack.consume(ctx)?;
                let mass: f64 = tbl
                    .get::<_, Option<f64>>(ctx, "mass")
                    .ok()
                    .flatten()
                    .unwrap_or(1.0);
                let stiffness: f64 = tbl
                    .get::<_, Option<f64>>(ctx, "stiffness")
                    .ok()
                    .flatten()
                    .unwrap_or(800.0);
                let epsilon: f64 = tbl
                    .get::<_, Option<f64>>(ctx, "epsilon")
                    .ok()
                    .flatten()
                    .unwrap_or(0.0005);
                let damping: Option<f64> = tbl.get::<_, Option<f64>>(ctx, "damping").ok().flatten();
                let damping_ratio: Option<f64> = tbl
                    .get::<_, Option<f64>>(ctx, "damping_ratio")
                    .ok()
                    .flatten();
                let params = if let Some(c) = damping {
                    HookeSpringParams::from_physical(mass, c, stiffness, epsilon)
                } else {
                    HookeSpringParams::from_ratio(
                        damping_ratio.unwrap_or(1.0),
                        stiffness,
                        mass,
                        epsilon,
                    )
                };
                st.borrow_mut()
                    .config
                    .appearance
                    .animations
                    .curves
                    .insert(name, CurveSpec::Spring(params));
                Ok(CallbackReturn::Return)
            }),
        );
    }

    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "xwayland",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let spec: XwaylandConfigSpec = from_value(val).map_err(RuntimeError::new)?;
                st.borrow_mut().config.xwayland = spec;
                Ok(CallbackReturn::Return)
            }),
        );
    }

    // 3. koma.bind(chord, action_or_fn, opts?) -> KomaHandle
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "bind",
            Callback::from_fn_with(&ctx, handle_mt, move |&handle_mt, ctx, _, mut stack| {
                let (chord, target, opts_val): (std::string::String, Value, Option<Value>) =
                    stack.consume(ctx)?;
                let options: BindOptions = match opts_val {
                    Some(v) if !v.is_nil() => from_value(v).map_err(RuntimeError::new)?,
                    _ => BindOptions::default(),
                };
                let mut inner = st.borrow_mut();
                let id = BindHandleId(inner.alloc_id());

                let (action, release_action) =
                    parse_bind_target(ctx, &mut inner, id, target, options.edge)?;
                inner.config.binds.push(BindSpec {
                    id,
                    mode: None,
                    enabled: true,
                    chord,
                    action,
                    release_action,
                    options,
                });
                drop(inner);

                let handle_ud = wrap_handle(ctx, handle_mt, KomaHandleRef::Bind(id));
                stack.replace(ctx, handle_ud);
                Ok(CallbackReturn::Return)
            }),
        );
    }

    // 3b. koma.unbind(chord)
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "unbind",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let chord: std::string::String = stack.consume(ctx)?;
                if chord.trim().is_empty() {
                    return Err(runtime_err("koma.unbind requires a non-empty chord"));
                }
                st.borrow_mut().config.unbinds.push(chord);
                Ok(CallbackReturn::Return)
            }),
        );
    }

    // 4. koma.mode(name, { on_enter = fn?, binds = { { chord, action, opts? }, ... } })
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "mode",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let (mode_name, spec_tbl): (std::string::String, Table) = stack.consume(ctx)?;
                let mut inner = st.borrow_mut();

                let on_enter_val = spec_tbl.get_value(ctx, "on_enter");
                if let Value::Function(func) = on_enter_val {
                    inner
                        .mode_enter_hooks
                        .insert(mode_name.clone(), ctx.stash(func));
                }

                if let Value::Table(binds_tbl) = spec_tbl.get_value(ctx, "binds") {
                    let len = binds_tbl.length();
                    for idx in 1..=len {
                        if let Value::Table(entry) = binds_tbl.get_value(ctx, idx) {
                            let chord: std::string::String = entry.get(ctx, 1)?;
                            let target: Value = entry.get_value(ctx, 2);
                            let opts_val: Value = entry.get_value(ctx, 3);
                            let options: BindOptions = if opts_val.is_nil() {
                                BindOptions::default()
                            } else {
                                from_value(opts_val).map_err(RuntimeError::new)?
                            };
                            let id = BindHandleId(inner.alloc_id());
                            let (action, release_action) =
                                parse_bind_target(ctx, &mut inner, id, target, options.edge)?;
                            inner.config.binds.push(BindSpec {
                                id,
                                mode: Some(mode_name.clone()),
                                enabled: true,
                                chord,
                                action,
                                release_action,
                                options,
                            });
                        }
                    }
                }
                Ok(CallbackReturn::Return)
            }),
        );
    }

    // 5. koma.gesture({ fingers = 3, direction = "left"|"horizontal"|"pinch_in", action = koma.act.* | fn }) -> KomaHandle
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "gesture",
            Callback::from_fn_with(&ctx, handle_mt, move |&handle_mt, ctx, _, mut stack| {
                let tbl: Table = stack.consume(ctx)?;
                let raw_fingers: i64 = tbl.get(ctx, "fingers")?;
                let raw_direction: std::string::String = tbl.get(ctx, "direction")?;
                let (fingers, direction) =
                    validate_gesture_spec(raw_fingers, &raw_direction).map_err(runtime_err)?;

                let cooldown_ms: Option<u64> = match tbl.get_value(ctx, "cooldown_ms") {
                    Value::Nil => None,
                    v => Some(from_value(v).map_err(RuntimeError::new)?),
                };
                let allow_when_locked: bool = match tbl.get_value(ctx, "allow_when_locked") {
                    Value::Nil => false,
                    v => from_value(v).map_err(RuntimeError::new)?,
                };
                let when_preempted: std::string::String = match tbl.get_value(ctx, "when_preempted")
                {
                    Value::Nil => "allow".to_owned(),
                    v => from_value(v).map_err(RuntimeError::new)?,
                };
                let desc: Option<std::string::String> = match tbl.get_value(ctx, "desc") {
                    Value::Nil => None,
                    v => Some(from_value(v).map_err(RuntimeError::new)?),
                };

                let act_val = tbl.get_value(ctx, "action");
                let mut inner = st.borrow_mut();
                let id = BindHandleId(inner.alloc_id());
                let (action, _) =
                    parse_bind_target(ctx, &mut inner, id, act_val, TriggerEdge::Press)?;

                inner.config.gestures.push(GestureSpec {
                    id: Some(id),
                    enabled: true,
                    fingers,
                    direction,
                    action,
                    cooldown_ms,
                    allow_when_locked,
                    when_preempted,
                    desc,
                });
                drop(inner);

                let handle_ud = wrap_handle(ctx, handle_mt, KomaHandleRef::Bind(id));
                stack.replace(ctx, handle_ud);
                Ok(CallbackReturn::Return)
            }),
        );
    }

    // 6. koma.on(event_name, function(ctx) ... end)
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "on",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let (event_name, func): (std::string::String, Function) = stack.consume(ctx)?;
                st.borrow_mut()
                    .event_hooks
                    .entry(event_name)
                    .or_default()
                    .push(ctx.stash(func));
                Ok(CallbackReturn::Return)
            }),
        );
    }

    // 7. koma.act.* pre-compiled fast-path action descriptor constructors
    koma.set_field(ctx, "act", build_act_namespace(ctx)?);

    // 8. koma.rule.{ window, layer }
    let rule_ns = Table::new(&ctx);
    {
        let st = state.clone();
        rule_ns.set_field(
            ctx,
            "window",
            Callback::from_fn_with(&ctx, handle_mt, move |&handle_mt, ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let mut spec: LuaWindowRuleSpec = from_value(val).map_err(RuntimeError::new)?;
                // Validate regex patterns upfront so invalid rules fail atomically during reload!
                for m in spec.matches.iter().chain(spec.excludes.iter()) {
                    if let Some(re) = &m.app_id {
                        Regex::new(re).map_err(RuntimeError::new)?;
                    }
                    if let Some(re) = &m.title {
                        Regex::new(re).map_err(RuntimeError::new)?;
                    }
                }
                let mut inner = st.borrow_mut();
                let id = RuleHandleId(inner.alloc_id());
                spec.id = Some(id);
                spec.enabled = true;
                inner.config.window_rules.push(spec);
                drop(inner);

                stack.replace(
                    ctx,
                    wrap_handle(ctx, handle_mt, KomaHandleRef::WindowRule(id)),
                );
                Ok(CallbackReturn::Return)
            }),
        );
    }
    {
        let st = state.clone();
        rule_ns.set_field(
            ctx,
            "layer",
            Callback::from_fn_with(&ctx, handle_mt, move |&handle_mt, ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let mut spec: LuaLayerRuleSpec = from_value(val).map_err(RuntimeError::new)?;
                for m in spec.matches.iter().chain(spec.excludes.iter()) {
                    if let Some(re) = &m.namespace {
                        Regex::new(re).map_err(RuntimeError::new)?;
                    }
                }
                let mut inner = st.borrow_mut();
                let id = RuleHandleId(inner.alloc_id());
                spec.id = Some(id);
                spec.enabled = true;
                inner.config.layer_rules.push(spec);
                drop(inner);

                stack.replace(
                    ctx,
                    wrap_handle(ctx, handle_mt, KomaHandleRef::LayerRule(id)),
                );
                Ok(CallbackReturn::Return)
            }),
        );
    }
    koma.set_field(ctx, "rule", rule_ns);

    // 9. koma.macro.{ define }
    let macro_ns = Table::new(&ctx);
    {
        let st = state;
        macro_ns.set_field(
            ctx,
            "define",
            Callback::from_fn_with(&ctx, handle_mt, move |&handle_mt, ctx, _, mut stack| {
                let tbl: Table = stack.consume(ctx)?;
                let id_str: std::string::String = tbl.get(ctx, "id")?;
                let app_id_regex: std::string::String = tbl.get(ctx, "app_id")?;
                Regex::new(&app_id_regex).map_err(RuntimeError::new)?;

                let description = match tbl.get_value(ctx, "description") {
                    Value::Nil => std::string::String::new(),
                    v => from_value(v).map_err(RuntimeError::new)?,
                };
                let params: BTreeMap<std::string::String, MacroParamSpec> =
                    match tbl.get_value(ctx, "params") {
                        Value::Nil => BTreeMap::new(),
                        v => from_value(v).map_err(RuntimeError::new)?,
                    };
                let limits: MacroLimitsSpec = match tbl.get_value(ctx, "limits") {
                    Value::Nil => MacroLimitsSpec::default(),
                    v => from_value(v).map_err(RuntimeError::new)?,
                };

                let Value::Function(run_fn) = tbl.get_value(ctx, "run") else {
                    return Err(runtime_err(format!(
                        "koma.macro.define(`{id_str}`) requires `run = function(ctx, args) ... end`"
                    )));
                };

                let mut inner = st.borrow_mut();
                let handle_id = MacroHandleId(inner.alloc_id());
                let macro_id = MacroId::new(id_str);
                inner.macro_fns.insert(macro_id.clone(), ctx.stash(run_fn));
                inner.config.macros.insert(
                    macro_id,
                    MacroDefinition {
                        handle_id,
                        id: MacroId::new(tbl.get::<_, std::string::String>(ctx, "id")?),
                        enabled: true,
                        app_id_regex,
                        description,
                        params,
                        limits,
                    },
                );
                drop(inner);

                stack.replace(
                    ctx,
                    wrap_handle(ctx, handle_mt, KomaHandleRef::Macro(handle_id)),
                );
                Ok(CallbackReturn::Return)
            }),
        );
    }
    koma.set_field(ctx, "macro", macro_ns);

    ctx.set_global("koma", koma);
    Ok(())
}

fn parse_bind_target<'gc>(
    ctx: Context<'gc>,
    inner: &mut SandboxInner,
    id: BindHandleId,
    target: Value<'gc>,
    edge: TriggerEdge,
) -> Result<(ActionDescriptor, Option<ActionDescriptor>), LuaError<'gc>> {
    if edge == TriggerEdge::Both
        && let Value::Table(tbl) = target
    {
        let press_act = extract_action_descriptor(tbl.get_value(ctx, "press"))?;
        let rel_act = extract_action_descriptor(tbl.get_value(ctx, "release"))?;
        return Ok((press_act, Some(rel_act)));
    }
    match target {
        Value::Function(func) => {
            inner.bind_callbacks.insert(id, ctx.stash(func));
            Ok((ActionDescriptor::LuaCallback(id), None))
        }
        other => Ok((extract_action_descriptor(other)?, None)),
    }
}

fn extract_action_descriptor<'gc>(val: Value<'gc>) -> Result<ActionDescriptor, LuaError<'gc>> {
    if let Value::UserData(ud) = val {
        let act = ud
            .downcast_static::<ActionDescriptor>()
            .map_err(RuntimeError::new)?;
        Ok(act.clone())
    } else {
        Err(runtime_err(
            "expected a `koma.act.*` descriptor or Lua callback `function(ctx) ... end`",
        ))
    }
}

fn unit_act<'gc>(ctx: Context<'gc>, desc: ActionDescriptor) -> Callback<'gc> {
    Callback::from_fn(&ctx, move |ctx, _, mut stack| {
        stack.replace(ctx, UserData::new_static(&ctx, desc.clone()));
        Ok(CallbackReturn::Return)
    })
}

fn populate_scrolling_ns<'gc>(ctx: Context<'gc>, tbl: &Table<'gc>) {
    tbl.set_field(
        ctx,
        "swipe",
        unit_act(ctx, ActionDescriptor::ScrollStripSwipe),
    );
    tbl.set_field(
        ctx,
        "switch_preset_column_width",
        unit_act(
            ctx,
            ActionDescriptor::ScrollingSwitchPresetWidth { forward: true },
        ),
    );
    tbl.set_field(
        ctx,
        "switch_preset_column_width_back",
        unit_act(
            ctx,
            ActionDescriptor::ScrollingSwitchPresetWidth { forward: false },
        ),
    );
    tbl.set_field(
        ctx,
        "maximize_column",
        unit_act(ctx, ActionDescriptor::ScrollingMaximizeColumn),
    );
    tbl.set_field(
        ctx,
        "center_column",
        unit_act(ctx, ActionDescriptor::ScrollingCenterColumn),
    );
    tbl.set_field(
        ctx,
        "move_column_left",
        unit_act(ctx, ActionDescriptor::ScrollingMoveColumnLeft),
    );
    tbl.set_field(
        ctx,
        "move_column_right",
        unit_act(ctx, ActionDescriptor::ScrollingMoveColumnRight),
    );
    tbl.set_field(
        ctx,
        "switch_preset_window_height",
        unit_act(
            ctx,
            ActionDescriptor::ScrollingSwitchPresetHeight { forward: true },
        ),
    );
    tbl.set_field(
        ctx,
        "switch_preset_window_height_back",
        unit_act(
            ctx,
            ActionDescriptor::ScrollingSwitchPresetHeight { forward: false },
        ),
    );
    tbl.set_field(
        ctx,
        "reset_window_height",
        unit_act(ctx, ActionDescriptor::ScrollingResetWindowHeight),
    );
    tbl.set_field(
        ctx,
        "toggle_column_tabbed_display",
        unit_act(ctx, ActionDescriptor::ScrollingToggleColumnTabbed),
    );
    tbl.set_field(
        ctx,
        "consume_window_into_column",
        unit_act(ctx, ActionDescriptor::ScrollingConsumeIntoColumn),
    );
    tbl.set_field(
        ctx,
        "expel_window_from_column",
        unit_act(ctx, ActionDescriptor::ScrollingExpelFromColumn),
    );
    tbl.set_field(
        ctx,
        "consume_or_expel_window_left",
        unit_act(ctx, ActionDescriptor::ScrollingConsumeOrExpelLeft),
    );
    tbl.set_field(
        ctx,
        "consume_or_expel_window_right",
        unit_act(ctx, ActionDescriptor::ScrollingConsumeOrExpelRight),
    );
}

fn build_act_namespace<'gc>(ctx: Context<'gc>) -> Result<Table<'gc>, LuaError<'gc>> {
    let act = Table::new(&ctx);

    // koma.act.spawn({ "ghostty", ... })
    act.set_field(
        ctx,
        "spawn",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let val: Value = stack.consume(ctx)?;
            let argv: Vec<std::string::String> = from_value(val).map_err(RuntimeError::new)?;
            if argv.is_empty() {
                return Err(runtime_err("koma.act.spawn requires non-empty argv array"));
            }
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::Spawn { argv }),
            );
            Ok(CallbackReturn::Return)
        }),
    );

    // koma.act.window.*
    let win = Table::new(&ctx);
    win.set_field(ctx, "close", unit_act(ctx, ActionDescriptor::WindowClose));
    win.set_field(
        ctx,
        "fullscreen_toggle",
        unit_act(ctx, ActionDescriptor::WindowFullscreenToggle),
    );
    win.set_field(
        ctx,
        "float_toggle",
        unit_act(ctx, ActionDescriptor::WindowFloatToggle),
    );
    win.set_field(
        ctx,
        "pseudotile_toggle",
        unit_act(ctx, ActionDescriptor::WindowPseudotileToggle),
    );
    win.set_field(
        ctx,
        "swap",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let dir: std::string::String = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::WindowSwapDirection(dir)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    win.set_field(
        ctx,
        "move_direction",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let dir: std::string::String = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::WindowMoveDirection(dir)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    win.set_field(
        ctx,
        "drag_move",
        unit_act(ctx, ActionDescriptor::WindowDragMove),
    );
    win.set_field(
        ctx,
        "drag_resize",
        unit_act(ctx, ActionDescriptor::WindowDragResize),
    );
    win.set_field(
        ctx,
        "resize",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let tbl: Table = stack.consume(ctx)?;
            let width: i32 = tbl.get(ctx, "width").unwrap_or(0);
            let height: i32 = tbl.get(ctx, "height").unwrap_or(0);
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::WindowResizeDelta { width, height }),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    win.set_field(
        ctx,
        "move_to_workspace",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (workspace, opts): (std::string::String, Option<Table>) = stack.consume(ctx)?;
            // `follow = false` is the `Hyprland` `movetoworkspacesilent` equivalent:
            // the window moves but the source workspace keeps the keyboard focus.
            let follow = match opts {
                Some(tbl) => tbl
                    .get::<_, Option<bool>>(ctx, "follow")
                    .ok()
                    .flatten()
                    .unwrap_or(true),
                None => true,
            };
            stack.replace(
                ctx,
                UserData::new_static(
                    &ctx,
                    ActionDescriptor::WindowMoveToWorkspace { workspace, follow },
                ),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    win.set_field(
        ctx,
        "focus",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let win_id: u64 = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::WindowFocus(win_id)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    win.set_field(
        ctx,
        "move_to_monitor",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let mon: std::string::String = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::WindowMoveToMonitor(mon)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    win.set_field(
        ctx,
        "move_to_monitor_direction",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let dir: std::string::String = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::WindowMoveToMonitorDirection(dir)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    for (field, dir) in [
        ("move_to_monitor_left", "left"),
        ("move_to_monitor_right", "right"),
        ("move_to_monitor_up", "up"),
        ("move_to_monitor_down", "down"),
    ] {
        win.set_field(
            ctx,
            field,
            unit_act(
                ctx,
                ActionDescriptor::WindowMoveToMonitorDirection(dir.to_owned()),
            ),
        );
    }
    act.set_field(ctx, "window", win);

    // koma.act.monitor.*
    let mon_ns = Table::new(&ctx);
    mon_ns.set_field(
        ctx,
        "focus",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let mon: std::string::String = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::MonitorFocus(mon)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    mon_ns.set_field(
        ctx,
        "focus_direction",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let dir: std::string::String = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::MonitorFocusDirection(dir)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    for (field, dir) in [
        ("focus_left", "left"),
        ("focus_right", "right"),
        ("focus_up", "up"),
        ("focus_down", "down"),
    ] {
        mon_ns.set_field(
            ctx,
            field,
            unit_act(ctx, ActionDescriptor::MonitorFocusDirection(dir.to_owned())),
        );
    }
    act.set_field(ctx, "monitor", mon_ns);

    // koma.act.focus.*
    let focus = Table::new(&ctx);
    for dir in ["left", "right", "up", "down"] {
        focus.set_field(
            ctx,
            dir,
            unit_act(ctx, ActionDescriptor::FocusDirection(dir.to_owned())),
        );
    }
    act.set_field(ctx, "focus", focus);

    // koma.act.dwindle.*
    let dwindle_ns = Table::new(&ctx);
    dwindle_ns.set_field(
        ctx,
        "toggle_split",
        unit_act(ctx, ActionDescriptor::DwindleToggleSplit),
    );
    dwindle_ns.set_field(
        ctx,
        "swap_split",
        unit_act(ctx, ActionDescriptor::DwindleSwapSplit),
    );
    dwindle_ns.set_field(
        ctx,
        "split_ratio",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let delta: f64 = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::DwindleSplitRatio(delta)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    act.set_field(ctx, "dwindle", dwindle_ns);

    // koma.act.scratchpad.* (`special:<name>`, default `"magic"`)
    let scratchpad_ns = Table::new(&ctx);
    scratchpad_ns.set_field(
        ctx,
        "toggle",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let name: Option<std::string::String> = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::ScratchpadToggle(name)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    scratchpad_ns.set_field(
        ctx,
        "move_window",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (name, silent_opt): (Option<std::string::String>, Option<bool>) =
                stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(
                    &ctx,
                    ActionDescriptor::ScratchpadMoveWindow {
                        name,
                        silent: silent_opt.unwrap_or(false),
                    },
                ),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    act.set_field(ctx, "scratchpad", scratchpad_ns);

    // koma.act.deck.*
    let deck = Table::new(&ctx);
    deck.set_field(
        ctx,
        "cycle_prev",
        unit_act(ctx, ActionDescriptor::DeckCyclePrev),
    );
    deck.set_field(
        ctx,
        "cycle_next",
        unit_act(ctx, ActionDescriptor::DeckCycleNext),
    );
    act.set_field(ctx, "deck", deck);

    // koma.act.overview.* & koma.act.scroll_strip.*
    let overview = Table::new(&ctx);
    overview.set_field(
        ctx,
        "toggle",
        unit_act(ctx, ActionDescriptor::OverviewToggle),
    );
    overview.set_field(ctx, "open", unit_act(ctx, ActionDescriptor::OverviewOpen));
    act.set_field(ctx, "overview", overview);

    let strip = Table::new(&ctx);
    populate_scrolling_ns(ctx, &strip);
    act.set_field(ctx, "scroll_strip", strip);
    let scrolling_ns = Table::new(&ctx);
    populate_scrolling_ns(ctx, &scrolling_ns);
    act.set_field(ctx, "scrolling", scrolling_ns);

    // koma.act.workspace.*
    let ws_ns = Table::new(&ctx);
    ws_ns.set_field(
        ctx,
        "focus",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ws: std::string::String = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::WorkspaceFocus(ws)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    ws_ns.set_field(
        ctx,
        "focus_next",
        unit_act(ctx, ActionDescriptor::WorkspaceFocusNext),
    );
    ws_ns.set_field(
        ctx,
        "focus_prev",
        unit_act(ctx, ActionDescriptor::WorkspaceFocusPrev),
    );
    ws_ns.set_field(
        ctx,
        "peek",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ws: std::string::String = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::WorkspacePeek(ws)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    ws_ns.set_field(
        ctx,
        "set_layout",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (mode_val, workspace): (Value, Option<std::string::String>) = stack.consume(ctx)?;
            let layout: LayoutMode = from_value(mode_val).map_err(RuntimeError::new)?;
            stack.replace(
                ctx,
                UserData::new_static(
                    &ctx,
                    ActionDescriptor::WorkspaceSetLayout { workspace, layout },
                ),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    act.set_field(ctx, "workspace", ws_ns);

    // koma.act.mode.*
    let mode_ns = Table::new(&ctx);
    mode_ns.set_field(
        ctx,
        "enter",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let name: std::string::String = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::ModeEnter(name)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    mode_ns.set_field(ctx, "exit", unit_act(ctx, ActionDescriptor::ModeExit));
    act.set_field(ctx, "mode", mode_ns);

    // koma.act.reload_config() / koma.act.quit()
    act.set_field(
        ctx,
        "reload_config",
        unit_act(ctx, ActionDescriptor::ReloadConfig),
    );
    act.set_field(ctx, "quit", unit_act(ctx, ActionDescriptor::CompositorQuit));

    // koma.act.macro.run(macro_id, args?)
    let macro_act = Table::new(&ctx);
    macro_act.set_field(
        ctx,
        "run",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (id_str, args_val): (std::string::String, Option<Value>) = stack.consume(ctx)?;
            let args: serde_json::Value = match args_val {
                Some(v) if !v.is_nil() => from_value(v).map_err(RuntimeError::new)?,
                _ => serde_json::json!({}),
            };
            stack.replace(
                ctx,
                UserData::new_static(
                    &ctx,
                    ActionDescriptor::RunMacro {
                        macro_id: MacroId::new(id_str),
                        args,
                    },
                ),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    act.set_field(ctx, "macro", macro_act);

    Ok(act)
}

// ==============================================================================
// `MacroContext` (`ctx`) UserData + `async_sequence` Coroutine Suspension Methods
// ==============================================================================

fn create_macro_context_userdata<'gc>(
    ctx: Context<'gc>,
    scope: MacroExecScope,
) -> Result<UserData<'gc>, LuaError<'gc>> {
    let methods = Table::new(&ctx);
    let mt = Table::new(&ctx);

    // 1. ctx:assert_app(regex)
    methods.try_set_field(
        ctx,
        "assert_app",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let app_regex: std::string::String = stack.consume(ctx)?;
            Regex::new(&app_regex).map_err(RuntimeError::new)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?;
            sc.state
                .borrow_mut()
                .host_actions
                .push_back(QueuedHostAction::AssertFocusedApp {
                    ws_id: sc.ws_id.clone(),
                    expected_epoch: sc.expected_epoch,
                    app_regex,
                });
            Ok(CallbackReturn::Return)
        }),
    )?;

    // 2. ctx:key_down(key) & ctx:key_up(key) (tracked in vSeat shadow ledger)
    methods.try_set_field(
        ctx,
        "key_down",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let key: std::string::String = stack.consume(ctx)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?;
            sc.state
                .borrow_mut()
                .host_actions
                .push_back(QueuedHostAction::KeyDown {
                    ws_id: sc.ws_id.clone(),
                    expected_epoch: sc.expected_epoch,
                    key,
                });
            Ok(CallbackReturn::Return)
        }),
    )?;

    methods.try_set_field(
        ctx,
        "key_up",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let key: std::string::String = stack.consume(ctx)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?;
            sc.state
                .borrow_mut()
                .host_actions
                .push_back(QueuedHostAction::KeyUp {
                    ws_id: sc.ws_id.clone(),
                    expected_epoch: sc.expected_epoch,
                    key,
                });
            Ok(CallbackReturn::Return)
        }),
    )?;

    // 3. ctx:spawn(argv)
    methods.try_set_field(
        ctx,
        "spawn",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let val: Value = stack.consume(ctx)?;
            let argv: Vec<std::string::String> = from_value(val).map_err(RuntimeError::new)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?;
            sc.state
                .borrow_mut()
                .host_actions
                .push_back(QueuedHostAction::SpawnProcess { argv });
            Ok(CallbackReturn::Return)
        }),
    )?;

    // 4. ctx:sleep_ms(duration_ms) -> suspends via `AsyncSequence::suspend(HostOpHandle)`
    methods.try_set_field(
        ctx,
        "sleep_ms",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let duration_ms: i64 = stack.consume(ctx)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();
            let dur = duration_ms.max(0) as u64;

            let seq = async_sequence(&ctx, |_, mut seq| async move {
                if dur > 0 {
                    let handle = seq.try_enter(|_, _, _, _| {
                        let mut inner = sc.state.borrow_mut();
                        let deadline_ms = inner.now_ms.saturating_add(dur);
                        Ok::<_, LuaError<'_>>(
                            inner.park_host_op(&sc, PendingHostOp::SleepTimer { deadline_ms }),
                        )
                    })?;
                    seq.suspend(handle).await?;
                }
                Ok(SequenceReturn::Return)
            });
            Ok(CallbackReturn::Sequence(seq))
        }),
    )?;

    // 5. ctx:wait_settle({ quiet_ms = 40, timeout_ms = 500 }) -> `PendingHostOp::WaitDamageSettle`
    methods.try_set_field(
        ctx,
        "wait_settle",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let opts_val: Option<Value> = stack.consume(ctx)?;
            let opts: LuaSettleOptsInput = match opts_val {
                Some(v) if !v.is_nil() => from_value(v).map_err(RuntimeError::new)?,
                _ => LuaSettleOptsInput::default(),
            };
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();

            let seq = async_sequence(&ctx, |_, mut seq| async move {
                let handle = seq.try_enter(|_, _, _, _| {
                    let mut inner = sc.state.borrow_mut();
                    let timeout_at_ms = inner.now_ms.saturating_add(opts.timeout_ms);
                    let ws_id = sc.ws_id.clone();
                    Ok::<_, LuaError<'_>>(inner.park_host_op(
                        &sc,
                        PendingHostOp::WaitDamageSettle {
                            ws_id,
                            quiet_ms: opts.quiet_ms,
                            timeout_at_ms,
                        },
                    ))
                })?;
                seq.suspend(handle).await?;
                Ok(SequenceReturn::Return)
            });
            Ok(CallbackReturn::Sequence(seq))
        }),
    )?;

    // 6. ctx:key(chord, opts?) & ctx:sequence(chords_array, gap_ms)
    methods.try_set_field(
        ctx,
        "key",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let (chord, opts_val): (std::string::String, Option<Value>) = stack.consume(ctx)?;
            let opts: LuaKeyOptsInput = match opts_val {
                Some(v) if !v.is_nil() => from_value(v).map_err(RuntimeError::new)?,
                _ => LuaKeyOptsInput::default(),
            };
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();

            let seq = async_sequence(&ctx, |_, mut seq| async move {
                let maybe_handle = seq.try_enter(|_, _, _, _| {
                    let mut inner = sc.state.borrow_mut();
                    inner.host_actions.push_back(QueuedHostAction::TapKeyChord {
                        ws_id: sc.ws_id.clone(),
                        expected_epoch: sc.expected_epoch,
                        chord,
                        hold_ms: opts.hold_ms,
                    });
                    if opts.after_ms > 0 {
                        let deadline_ms = inner.now_ms.saturating_add(opts.after_ms);
                        Ok::<_, LuaError<'_>>(Some(
                            inner.park_host_op(&sc, PendingHostOp::SleepTimer { deadline_ms }),
                        ))
                    } else {
                        Ok(None)
                    }
                })?;
                if let Some(handle) = maybe_handle {
                    seq.suspend(handle).await?;
                }
                Ok(SequenceReturn::Return)
            });
            Ok(CallbackReturn::Sequence(seq))
        }),
    )?;

    methods.try_set_field(
        ctx,
        "sequence",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let (chords_val, gap_ms): (Value, i64) = stack.consume(ctx)?;
            let chords: Vec<std::string::String> =
                from_value(chords_val).map_err(RuntimeError::new)?;
            let gap = gap_ms.max(0) as u64;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();

            let seq = async_sequence(&ctx, |_, mut seq| async move {
                let total = chords.len();
                for (idx, chord) in chords.into_iter().enumerate() {
                    let maybe_handle = seq.try_enter(|_, _, _, _| {
                        let mut inner = sc.state.borrow_mut();
                        inner.host_actions.push_back(QueuedHostAction::TapKeyChord {
                            ws_id: sc.ws_id.clone(),
                            expected_epoch: sc.expected_epoch,
                            chord,
                            hold_ms: 15,
                        });
                        if gap > 0 && idx + 1 < total {
                            let deadline_ms = inner.now_ms.saturating_add(gap);
                            Ok::<_, LuaError<'_>>(Some(
                                inner.park_host_op(&sc, PendingHostOp::SleepTimer { deadline_ms }),
                            ))
                        } else {
                            Ok(None)
                        }
                    })?;
                    if let Some(handle) = maybe_handle {
                        seq.suspend(handle).await?;
                    }
                }
                Ok(SequenceReturn::Return)
            });
            Ok(CallbackReturn::Sequence(seq))
        }),
    )?;

    // 7. ctx:move_to(pos, kinematics?), ctx:click(pos, opts?), and 5-stage ctx:drag(spec)
    methods.try_set_field(
        ctx,
        "move_to",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let (pos_val, kin_val): (Value, Option<Value>) = stack.consume(ctx)?;
            let pos_in: LuaAnchorPointInput = from_value(pos_val).map_err(RuntimeError::new)?;
            let target = pos_in.into_anchor_point().map_err(runtime_err)?;
            let kinematics: LuaKinematicsInput = match kin_val {
                Some(v) if !v.is_nil() => from_value(v).map_err(RuntimeError::new)?,
                _ => LuaKinematicsInput::default(),
            };
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();

            let seq = async_sequence(&ctx, |_, mut seq| async move {
                let maybe_handle = seq.try_enter(|_, _, _, _| {
                    let mut inner = sc.state.borrow_mut();
                    let spec = kinematics.resolve_spec(&inner.config.appearance.animations);
                    let dur = spec.estimated_park_ms(512.0);
                    let settle_driven = matches!(
                        spec,
                        PointerTrajectorySpec::HookeSpring {
                            duration_cap_ms: None,
                            ..
                        }
                    );
                    inner.host_actions.push_back(QueuedHostAction::MovePointer {
                        ws_id: sc.ws_id.clone(),
                        expected_epoch: sc.expected_epoch,
                        target,
                        kinematics,
                    });
                    if dur > 0 {
                        let started_at_ms = inner.now_ms;
                        let finish_at_ms = started_at_ms.saturating_add(dur);
                        let ws_id = sc.ws_id.clone();
                        Ok::<_, LuaError<'_>>(Some(inner.park_host_op(
                            &sc,
                            PendingHostOp::PointerTrajectory {
                                ws_id,
                                started_at_ms,
                                finish_at_ms,
                                settle_driven,
                            },
                        )))
                    } else {
                        Ok(None)
                    }
                })?;
                if let Some(handle) = maybe_handle {
                    seq.suspend(handle).await?;
                }
                Ok(SequenceReturn::Return)
            });
            Ok(CallbackReturn::Sequence(seq))
        }),
    )?;

    methods.try_set_field(
        ctx,
        "click",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let (pos_val, opts_val): (Value, Option<Value>) = stack.consume(ctx)?;
            let pos_in: LuaAnchorPointInput = from_value(pos_val).map_err(RuntimeError::new)?;
            let target = pos_in.into_anchor_point().map_err(runtime_err)?;
            let opts: LuaClickOptsInput = match opts_val {
                Some(v) if !v.is_nil() => from_value(v).map_err(RuntimeError::new)?,
                _ => LuaClickOptsInput::default(),
            };
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();

            let seq = async_sequence(&ctx, |_, mut seq| async move {
                let maybe_handle = seq.try_enter(|_, _, _, _| {
                    let mut inner = sc.state.borrow_mut();
                    let hold = opts.hold_ms;
                    inner
                        .host_actions
                        .push_back(QueuedHostAction::ClickPointer {
                            ws_id: sc.ws_id.clone(),
                            expected_epoch: sc.expected_epoch,
                            target,
                            opts,
                        });
                    if hold > 0 {
                        let deadline_ms = inner.now_ms.saturating_add(hold);
                        Ok::<_, LuaError<'_>>(Some(
                            inner.park_host_op(&sc, PendingHostOp::SleepTimer { deadline_ms }),
                        ))
                    } else {
                        Ok(None)
                    }
                })?;
                if let Some(handle) = maybe_handle {
                    seq.suspend(handle).await?;
                }
                Ok(SequenceReturn::Return)
            });
            Ok(CallbackReturn::Sequence(seq))
        }),
    )?;

    methods.try_set_field(
        ctx,
        "drag",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let spec_val: Value = stack.consume(ctx)?;
            let spec: LuaDragSpecInput = from_value(spec_val).map_err(RuntimeError::new)?;
            let from_pt = spec.from.into_anchor_point().map_err(runtime_err)?;
            let to_pt = spec.to.into_anchor_point().map_err(runtime_err)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();

            // Multi-stage long-press drag coroutine (`LUA_API_SPEC.md` §5.3):
            // 1. Warp to `from` & press button -> 2. hold_before_move_ms ->
            // 3. smooth trajectory to `to` -> 4. hold_before_release_ms -> 5. release button
            let seq = async_sequence(&ctx, |_, mut seq| async move {
                let button = spec.button;
                let hold_pre = spec.hold_before_move_ms;
                let hold_post = spec.hold_before_release_ms;
                let kin = spec.kinematics;

                // Stage 1 & 2: Position at start, press button, hold before drag motion
                let pre_handle = seq.try_enter(|_, _, _, _| {
                    let mut inner = sc.state.borrow_mut();
                    inner.host_actions.push_back(QueuedHostAction::MovePointer {
                        ws_id: sc.ws_id.clone(),
                        expected_epoch: sc.expected_epoch,
                        target: from_pt,
                        kinematics: LuaKinematicsInput::instant(),
                    });
                    inner
                        .host_actions
                        .push_back(QueuedHostAction::PointerButton {
                            ws_id: sc.ws_id.clone(),
                            expected_epoch: sc.expected_epoch,
                            button: button.clone(),
                            pressed: true,
                        });
                    if hold_pre > 0 {
                        let deadline_ms = inner.now_ms.saturating_add(hold_pre);
                        Ok::<_, LuaError<'_>>(Some(
                            inner.park_host_op(&sc, PendingHostOp::SleepTimer { deadline_ms }),
                        ))
                    } else {
                        Ok(None)
                    }
                })?;
                if let Some(h) = pre_handle {
                    seq.suspend(h).await?;
                }

                // Stage 3: Smooth kinematic trajectory to `to`
                let traj_handle = seq.try_enter(|_, _, _, _| {
                    let mut inner = sc.state.borrow_mut();
                    let spec = kin.resolve_spec(&inner.config.appearance.animations);
                    let dur = spec.estimated_park_ms(512.0);
                    let settle_driven = matches!(
                        spec,
                        PointerTrajectorySpec::HookeSpring {
                            duration_cap_ms: None,
                            ..
                        }
                    );
                    inner.host_actions.push_back(QueuedHostAction::MovePointer {
                        ws_id: sc.ws_id.clone(),
                        expected_epoch: sc.expected_epoch,
                        target: to_pt,
                        kinematics: kin,
                    });
                    if dur > 0 {
                        let started_at_ms = inner.now_ms;
                        let finish_at_ms = started_at_ms.saturating_add(dur);
                        let ws_id = sc.ws_id.clone();
                        Ok::<_, LuaError<'_>>(Some(inner.park_host_op(
                            &sc,
                            PendingHostOp::PointerTrajectory {
                                ws_id,
                                started_at_ms,
                                finish_at_ms,
                                settle_driven,
                            },
                        )))
                    } else {
                        Ok(None)
                    }
                })?;
                if let Some(h) = traj_handle {
                    seq.suspend(h).await?;
                }

                // Stage 4: Hold at destination before releasing button
                if hold_post > 0 {
                    let post_handle = seq.try_enter(|_, _, _, _| {
                        let mut inner = sc.state.borrow_mut();
                        let deadline_ms = inner.now_ms.saturating_add(hold_post);
                        Ok::<_, LuaError<'_>>(
                            inner.park_host_op(&sc, PendingHostOp::SleepTimer { deadline_ms }),
                        )
                    })?;
                    seq.suspend(post_handle).await?;
                }

                // Stage 5: Release mouse button cleanly
                seq.try_enter(|_, _, _, _| {
                    sc.state
                        .borrow_mut()
                        .host_actions
                        .push_back(QueuedHostAction::PointerButton {
                            ws_id: sc.ws_id.clone(),
                            expected_epoch: sc.expected_epoch,
                            button,
                            pressed: false,
                        });
                    Ok::<_, LuaError<'_>>(())
                })?;

                Ok(SequenceReturn::Return)
            });
            Ok(CallbackReturn::Sequence(seq))
        }),
    )?;

    fn resolve_path_waypoints<'gc>(
        inputs: Vec<LuaAnchorPointInput>,
    ) -> Result<Vec<AnchorPoint>, LuaError<'gc>> {
        if !(2..=128).contains(&inputs.len()) {
            return Err(runtime_err(
                "pointer path requires between 2 and 128 waypoints",
            ));
        }
        inputs
            .into_iter()
            .map(|w| w.into_anchor_point().map_err(runtime_err))
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn build_path_callback<'gc>(
        ctx: &Context<'gc>,
        sc: MacroExecScope,
        waypoints: Vec<AnchorPoint>,
        spline: PathSplineKind,
        button: Option<String>,
        hold_before_move_ms: u64,
        hold_before_release_ms: u64,
        kinematics: LuaKinematicsInput,
    ) -> CallbackReturn<'gc> {
        let seq = async_sequence(ctx, |_, mut seq| async move {
            if let Some(ref btn) = button {
                let first = waypoints[0].clone();
                let pre_handle = seq.try_enter(|_, _, _, _| {
                    let mut inner = sc.state.borrow_mut();
                    inner.host_actions.push_back(QueuedHostAction::MovePointer {
                        ws_id: sc.ws_id.clone(),
                        expected_epoch: sc.expected_epoch,
                        target: first,
                        kinematics: LuaKinematicsInput::instant(),
                    });
                    inner
                        .host_actions
                        .push_back(QueuedHostAction::PointerButton {
                            ws_id: sc.ws_id.clone(),
                            expected_epoch: sc.expected_epoch,
                            button: btn.clone(),
                            pressed: true,
                        });
                    if hold_before_move_ms > 0 {
                        let deadline_ms = inner.now_ms.saturating_add(hold_before_move_ms);
                        Ok::<_, LuaError<'_>>(Some(
                            inner.park_host_op(&sc, PendingHostOp::SleepTimer { deadline_ms }),
                        ))
                    } else {
                        Ok(None)
                    }
                })?;
                if let Some(h) = pre_handle {
                    seq.suspend(h).await?;
                }
            }

            let traj_handle = seq.try_enter(|_, _, _, _| {
                let mut inner = sc.state.borrow_mut();
                let spec = kinematics.resolve_spec(&inner.config.appearance.animations);
                let dur = spec.estimated_park_ms(512.0);
                let settle_driven = matches!(
                    spec,
                    PointerTrajectorySpec::HookeSpring {
                        duration_cap_ms: None,
                        ..
                    }
                );
                inner
                    .host_actions
                    .push_back(QueuedHostAction::MovePointerPath {
                        ws_id: sc.ws_id.clone(),
                        expected_epoch: sc.expected_epoch,
                        waypoints,
                        spline,
                        kinematics,
                    });
                if dur > 0 {
                    let started_at_ms = inner.now_ms;
                    let finish_at_ms = started_at_ms.saturating_add(dur);
                    let ws_id = sc.ws_id.clone();
                    Ok::<_, LuaError<'_>>(Some(inner.park_host_op(
                        &sc,
                        PendingHostOp::PointerTrajectory {
                            ws_id,
                            started_at_ms,
                            finish_at_ms,
                            settle_driven,
                        },
                    )))
                } else {
                    Ok(None)
                }
            })?;
            if let Some(h) = traj_handle {
                seq.suspend(h).await?;
            }

            if let Some(btn) = button {
                if hold_before_release_ms > 0 {
                    let post_handle = seq.try_enter(|_, _, _, _| {
                        let mut inner = sc.state.borrow_mut();
                        let deadline_ms = inner.now_ms.saturating_add(hold_before_release_ms);
                        Ok::<_, LuaError<'_>>(
                            inner.park_host_op(&sc, PendingHostOp::SleepTimer { deadline_ms }),
                        )
                    })?;
                    seq.suspend(post_handle).await?;
                }
                seq.try_enter(|_, _, _, _| {
                    sc.state
                        .borrow_mut()
                        .host_actions
                        .push_back(QueuedHostAction::PointerButton {
                            ws_id: sc.ws_id.clone(),
                            expected_epoch: sc.expected_epoch,
                            button: btn,
                            pressed: false,
                        });
                    Ok::<_, LuaError<'_>>(())
                })?;
            }

            Ok(SequenceReturn::Return)
        });
        CallbackReturn::Sequence(seq)
    }

    methods.try_set_field(
        ctx,
        "move_path",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let (waypoints_val, opts_val): (Value, Option<Value>) = stack.consume(ctx)?;
            let waypoints_in: Vec<LuaAnchorPointInput> =
                from_value(waypoints_val).map_err(RuntimeError::new)?;
            let waypoints = resolve_path_waypoints(waypoints_in)?;
            let opts: LuaMovePathOptsInput = match opts_val {
                Some(v) if !v.is_nil() => from_value(v).map_err(RuntimeError::new)?,
                _ => LuaMovePathOptsInput::default(),
            };
            let (spline, kinematics) = opts.into_parts();
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();
            Ok(build_path_callback(
                &ctx, sc, waypoints, spline, None, 0, 0, kinematics,
            ))
        }),
    )?;

    methods.try_set_field(
        ctx,
        "stroke_path",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let spec_val: Value = stack.consume(ctx)?;
            let spec: LuaPathSpecInput = from_value(spec_val).map_err(RuntimeError::new)?;
            let waypoints = resolve_path_waypoints(spec.waypoints)?;
            let button = Some(spec.button.unwrap_or_else(|| "left".to_owned()));
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();
            Ok(build_path_callback(
                &ctx,
                sc,
                waypoints,
                spec.spline,
                button,
                spec.hold_before_move_ms,
                spec.hold_before_release_ms,
                spec.kinematics,
            ))
        }),
    )?;

    methods.try_set_field(
        ctx,
        "path",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let spec_val: Value = stack.consume(ctx)?;
            let spec: LuaPathSpecInput = from_value(spec_val).map_err(RuntimeError::new)?;
            let waypoints = resolve_path_waypoints(spec.waypoints)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();
            Ok(build_path_callback(
                &ctx,
                sc,
                waypoints,
                spec.spline,
                spec.button,
                spec.hold_before_move_ms,
                spec.hold_before_release_ms,
                spec.kinematics,
            ))
        }),
    )?;

    methods.try_set_field(
        ctx,
        "settle_cursor",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let immediate: Option<bool> = stack.consume(ctx)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();
            sc.state
                .borrow_mut()
                .host_actions
                .push_back(QueuedHostAction::SettleAgentCursor {
                    ws_id: sc.ws_id.clone(),
                    expected_epoch: sc.expected_epoch,
                    immediate: immediate.unwrap_or(false),
                });
            Ok(CallbackReturn::Return)
        }),
    )?;

    // 8. ctx:scroll({ dx, dy, steps, step_ms })
    methods.try_set_field(
        ctx,
        "scroll",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let spec_val: Value = stack.consume(ctx)?;
            let spec: LuaScrollSpecInput = from_value(spec_val).map_err(RuntimeError::new)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();

            let seq = async_sequence(&ctx, |_, mut seq| async move {
                let count = spec.steps.max(1);
                let per_dx = spec.dx / f64::from(count);
                let per_dy = spec.dy / f64::from(count);
                for i in 0..count {
                    let maybe_h = seq.try_enter(|_, _, _, _| {
                        let mut inner = sc.state.borrow_mut();
                        inner.host_actions.push_back(QueuedHostAction::Scroll {
                            ws_id: sc.ws_id.clone(),
                            expected_epoch: sc.expected_epoch,
                            dx: per_dx,
                            dy: per_dy,
                        });
                        if spec.step_ms > 0 && i + 1 < count {
                            let deadline_ms = inner.now_ms.saturating_add(spec.step_ms);
                            Ok::<_, LuaError<'_>>(Some(
                                inner.park_host_op(&sc, PendingHostOp::SleepTimer { deadline_ms }),
                            ))
                        } else {
                            Ok(None)
                        }
                    })?;
                    if let Some(h) = maybe_h {
                        seq.suspend(h).await?;
                    }
                }
                Ok(SequenceReturn::Return)
            });
            Ok(CallbackReturn::Sequence(seq))
        }),
    )?;

    // 9. ctx:text(utf8_str, { strategy = "auto" }) -> 3-Tier Zero-IME Injection
    methods.try_set_field(
        ctx,
        "text",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let (utf8, opts_val): (std::string::String, Option<Value>) = stack.consume(ctx)?;
            let opts: LuaTextOptsInput = match opts_val {
                Some(v) if !v.is_nil() => from_value(v).map_err(RuntimeError::new)?,
                _ => LuaTextOptsInput::default(),
            };
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?;
            sc.state
                .borrow_mut()
                .host_actions
                .push_back(QueuedHostAction::InjectText {
                    ws_id: sc.ws_id.clone(),
                    expected_epoch: sc.expected_epoch,
                    utf8,
                    strategy: opts.strategy,
                });
            Ok(CallbackReturn::Return)
        }),
    )?;

    // 10. ctx:paste_mime(mime_type, payload) -> isolated vSeat clipboard + settle sync
    methods.try_set_field(
        ctx,
        "paste_mime",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.from_front(ctx)?;
            let (mime_type, payload_str): (std::string::String, std::string::String) =
                stack.consume(ctx)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?
                .clone();

            let seq = async_sequence(&ctx, |_, mut seq| async move {
                let handle = seq.try_enter(|_, _, _, _| {
                    let mut inner = sc.state.borrow_mut();
                    inner.host_actions.push_back(QueuedHostAction::PasteMime {
                        ws_id: sc.ws_id.clone(),
                        expected_epoch: sc.expected_epoch,
                        mime_type,
                        payload: payload_str.into_bytes(),
                    });
                    let timeout_at_ms = inner.now_ms.saturating_add(400);
                    let ws_id = sc.ws_id.clone();
                    Ok::<_, LuaError<'_>>(inner.park_host_op(
                        &sc,
                        PendingHostOp::WaitDamageSettle {
                            ws_id,
                            quiet_ms: 35,
                            timeout_at_ms,
                        },
                    ))
                })?;
                seq.suspend(handle).await?;
                Ok(SequenceReturn::Return)
            });
            Ok(CallbackReturn::Sequence(seq))
        }),
    )?;

    methods.try_set_field(
        ctx,
        "focused_window",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.consume(ctx)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?;
            let focused = sc.state.borrow().desktop_snapshot.focused_window.clone();
            let val = match focused {
                Some(ref info) => to_value(ctx, info).map_err(RuntimeError::new)?,
                None => Value::Nil,
            };
            stack.replace(ctx, val);
            Ok(CallbackReturn::Return)
        }),
    )?;

    methods.try_set_field(
        ctx,
        "windows",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.consume(ctx)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?;
            let windows = sc.state.borrow().desktop_snapshot.windows.clone();
            let val = to_value(ctx, &windows).map_err(RuntimeError::new)?;
            stack.replace(ctx, val);
            Ok(CallbackReturn::Return)
        }),
    )?;

    methods.try_set_field(
        ctx,
        "outputs",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.consume(ctx)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?;
            let outputs = sc.state.borrow().desktop_snapshot.outputs.clone();
            let val = to_value(ctx, &outputs).map_err(RuntimeError::new)?;
            stack.replace(ctx, val);
            Ok(CallbackReturn::Return)
        }),
    )?;

    methods.try_set_field(
        ctx,
        "workspaces",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let ud: UserData = stack.consume(ctx)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?;
            let workspaces = sc.state.borrow().desktop_snapshot.workspaces.clone();
            let val = to_value(ctx, &workspaces).map_err(RuntimeError::new)?;
            stack.replace(ctx, val);
            Ok(CallbackReturn::Return)
        }),
    )?;

    methods.try_set_field(
        ctx,
        "dispatch",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let (ud, action_val): (UserData, Value) = stack.consume(ctx)?;
            let sc = ud
                .downcast_static::<MacroExecScope>()
                .map_err(RuntimeError::new)?;
            let action = extract_action_descriptor(action_val)?;
            if matches!(action, ActionDescriptor::LuaCallback(_)) {
                return Err(runtime_err(
                    "ctx:dispatch cannot invoke nested LuaCallback actions",
                ));
            }
            sc.state
                .borrow_mut()
                .host_actions
                .push_back(QueuedHostAction::DispatchAction {
                    ws_id: sc.ws_id.clone(),
                    expected_epoch: sc.expected_epoch,
                    action,
                });
            Ok(CallbackReturn::Return)
        }),
    )?;

    mt.set(ctx, MetaMethod::Index, methods)?;
    let ud = UserData::new_static(&ctx, scope);
    ud.set_metatable(&ctx, Some(mt));
    Ok(ud)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_init_lua_and_drives_async_sequence_macro_lifecycle() {
        let mut sandbox = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();

        let init_src = r#"
            koma.env("XCURSOR_SIZE", "24")
            koma.monitor({ output = "eDP-1", mode = "2560x1600@240.000", scale = 1.6, vrr = "on_demand" })
            koma.workspace({ name = "agent:work", presentation = "headless", layout = "deck" })

            local term_bind = koma.bind("Mod + Return", koma.act.spawn({ "ghostty" }), { desc = "Terminal" })
            term_bind:set_enabled(false)

            koma.rule.window({
                name = "vault-blackout",
                matches = { { app_id = [[^org\.keepassxc\.KeePassXC$]] } },
                block_out_from = "all",
            })

            koma.macro.define({
                id = "wps.drag_indent",
                app_id = "^wps$",
                description = "Drags first-line indent ruler",
                params = { delta_px = { type = "int", default = 64 } },
                run = function(ctx, args)
                    ctx:assert_app("^wps$")
                    ctx:drag({
                        button = "left",
                        from = { anchor = "top-left", x = 240, y = 148 },
                        to   = { anchor = "top-left", x = 240 + args.delta_px, y = 148 },
                        hold_before_move_ms = 60,
                        kinematics = { profile = "minimum_jerk", duration_ms = 120, arc_curvature = 0.0 },
                        hold_before_release_ms = 30,
                    })
                    ctx:text("Heading 1", { strategy = "auto" })
                end,
            })
        "#;

        let cfg = sandbox
            .eval_init_lua(init_src)
            .expect("init.lua must evaluate cleanly");
        assert_eq!(cfg.monitors.len(), 1);
        assert_eq!(cfg.workspaces.len(), 1);
        assert!(
            !cfg.binds[0].enabled,
            "handle:set_enabled(false) must mutate registry"
        );
        assert_eq!(cfg.window_rules.len(), 1);
        assert!(cfg.macros.contains_key(&MacroId::from("wps.drag_indent")));

        // Start macro coroutine under Epoch(4); Stage 1+2 of ctx:drag must park on HostSuspended
        let (exec, mode) = sandbox
            .start_macro_tx(
                WorkspaceId::from("ws_01"),
                Epoch(4),
                &MacroId::from("wps.drag_indent"),
                &serde_json::json!({ "delta_px": 64 }),
                1_000,
            )
            .expect("macro should start and park on hold_before_move_ms");
        assert_eq!(mode, ExecutorMode::HostSuspended);

        let initial_actions = sandbox.drain_queued_actions();
        assert_eq!(
            initial_actions.len(),
            3,
            "AssertFocusedApp + MovePointer(from) + PointerButton(down)"
        );

        struct TestEpochBridge(Epoch);
        impl crate::host_abi::HostBridge for TestEpochBridge {
            fn current_epoch(&self, _ws_id: &WorkspaceId) -> Option<Epoch> {
                Some(self.0)
            }
            fn focused_app_id(&self, _ws_id: &WorkspaceId) -> Option<std::string::String> {
                Some("wps".into())
            }
            fn tap_key_chord(
                &mut self,
                _ws_id: &WorkspaceId,
                _chord: &str,
            ) -> Result<(), IpcError> {
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
        let bridge = TestEpochBridge(Epoch(4));

        // Wake 1 at t = 1060 ms (hold_before_move_ms = 60 elapsed -> starts 120 ms trajectory)
        let rep1 = sandbox.drive_ready_coroutines(1_060, &[], &bridge);
        assert_eq!(rep1.resumed_slices, 1);
        assert_eq!(rep1.completed_macros, 0);
        assert_eq!(
            sandbox.drain_queued_actions().len(),
            1,
            "MovePointer(to) trajectory started"
        );

        // Wake 2 at t = 1180 ms (120 ms trajectory finished -> enters 30 ms hold_before_release)
        let rep2 = sandbox.drive_ready_coroutines(1_180, &[], &bridge);
        assert_eq!(rep2.resumed_slices, 1);
        assert_eq!(rep2.completed_macros, 0);

        // Wake 3 at t = 1210 ms (hold_before_release_ms = 30 elapsed -> releases button + ctx:text -> completes!)
        let rep3 = sandbox.drive_ready_coroutines(1_210, &[], &bridge);
        assert_eq!(rep3.resumed_slices, 1);
        assert_eq!(rep3.completed_macros, 1);
        let final_actions = sandbox.drain_queued_actions();
        assert_eq!(final_actions.len(), 2, "PointerButton(up) + InjectText");
        let _ = exec;
    }

    #[test]
    fn evaluates_configurable_layout_appearance_input_and_unbind_apis() {
        let mut sandbox = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();

        let init_src = r##"
            koma.layout("scrolling")
            koma.workspace({ name = "1", gaps_in = 8, gaps_out = 16 })
            koma.workspace({ name = "2", layout = "dwindle" })

            koma.input({
                mod_key = "Alt",
                inherit_default_binds = false,
            })

            koma.appearance({
                corner_radius = 14,
                border = {
                    enabled = true,
                    width = 3.0,
                    active_color = "#ff5555",
                    inactive_color = "#282a36",
                    agent_leased_color = "#bd93f9",
                    human_preempt_color = "#ffb86c",
                },
                shadow = {
                    enabled = true,
                    range = 24,
                    render_power = 4,
                    sharp = false,
                    color = "#00000099",
                    offset_x = 0.0,
                    offset_y = 6.0,
                    scale = 1.0,
                },
                dim = {
                    dim_inactive = true,
                    dim_strength = 0.20,
                    brightness = 0.95,
                    contrast = 1.05,
                },
                brightness = 0.90,
                blur = {
                    enabled = true,
                    size = 8,
                    passes = 4,
                },
                animations = {
                    enabled = true,
                    damping_ratio = 0.85,
                    stiffness = 750.0,
                },
            })

            koma.unbind("Mod + Q")
            koma.bind("Mod + Return", koma.act.spawn({ "foot" }))
        "##;

        let cfg = sandbox
            .eval_init_lua(init_src)
            .expect("configurable init.lua should evaluate cleanly");
        assert_eq!(cfg.default_layout, LayoutMode::Scrolling);
        assert_eq!(cfg.workspaces.len(), 2);
        assert_eq!(cfg.workspaces[0].layout, LayoutMode::Scrolling);
        assert_eq!(cfg.workspaces[0].effective_inner_gap(), 8);
        assert_eq!(cfg.workspaces[0].effective_outer_gap(), 16);
        assert_eq!(cfg.workspaces[1].layout, LayoutMode::Dwindle);

        assert_eq!(cfg.input.mod_key.as_deref(), Some("Alt"));
        assert!(!cfg.input.inherit_default_binds);
        assert_eq!(cfg.unbinds, vec!["Mod + Q".to_owned()]);

        assert_eq!(cfg.appearance.corner_radius, Some(14));
        assert_eq!(cfg.appearance.border.width, 3.0);
        assert_eq!(cfg.appearance.border.active_color, "#ff5555");
        assert_eq!(cfg.appearance.shadow.range, 24);
        assert_eq!(cfg.appearance.shadow.render_power, 4);

        let eff_dim = cfg.appearance.effective_dim();
        assert!(eff_dim.dim_inactive);
        assert!((eff_dim.dim_strength - 0.20).abs() < 1e-6);
        assert!((eff_dim.brightness - 0.90).abs() < 1e-6);

        let stubs = crate::generate_luals_stubs();
        assert!(stubs.contains("---@class KomaBorderSpec"));
        assert!(stubs.contains("---@class KomaShadowSpec"));
        assert!(stubs.contains("---@class KomaDimSpec"));
        assert!(stubs.contains("---@field layout fun(mode: KomaLayoutMode)"));
        assert!(stubs.contains("---@field unbind fun(chord: string)"));

        let mut example_sandbox = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();
        let example_cfg = example_sandbox
            .eval_init_lua(include_str!("../../../examples/init.lua"))
            .expect("examples/init.lua should evaluate cleanly");
        assert_eq!(example_cfg.default_layout, LayoutMode::Dwindle);
        assert!(example_cfg.input.inherit_default_binds);
        assert!(example_cfg.appearance.border.enabled);
        assert!(example_cfg.appearance.shadow.enabled);
    }

    #[test]
    fn evaluates_monitor_macbook_fields_transforms_and_monitor_actions() {
        let mut sandbox = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();
        let lua = r##"
            koma.monitor({
                output = "eDP-1",
                mode = "3024x1964@120",
                scale = "auto",
                position = { x = 0, y = 0 },
                safe_area = { top = 38 },
            })
            koma.monitor({
                output = "DP-1",
                mode = "3840x2160@60",
                scale = 1.5,
                transform = "normal",
                position = { 1512, 0 },
                allow_non_desktop = false,
            })
            koma.monitor({
                output = "DP-2",
                non_desktop = true,
                transform = "flipped-270",
            })
            koma.bind("Mod + Left", koma.act.monitor.focus_left())
            koma.bind("Mod + Shift + Right", koma.act.window.move_to_monitor_right())
            koma.bind("Mod + 0", koma.act.monitor.focus("DP-1"))
            koma.bind("Mod + Shift + 0", koma.act.window.move_to_monitor("eDP-1"))
        "##;
        let cfg = sandbox
            .eval_init_lua(lua)
            .expect("multi-monitor init.lua must evaluate cleanly");
        assert_eq!(cfg.monitors.len(), 3);
        assert!(!cfg.monitors[0].has_explicit_transform);
        assert!(cfg.monitors[1].has_explicit_transform);
        assert_eq!(cfg.monitors[1].resolved_position(), Some((1512, 0)));
        assert!(cfg.monitors[2].allow_non_desktop);
        assert_eq!(
            cfg.monitors[2].parsed_transform().unwrap(),
            koma_model::OutputTransform::Flipped270
        );
        assert_eq!(cfg.monitors[0].safe_area.top, 38);
        assert_eq!(cfg.monitors[0].scale, crate::MonitorScaleSpec::Auto);
        assert_eq!(cfg.binds.len(), 4);
        assert_eq!(
            cfg.binds[0].action,
            ActionDescriptor::MonitorFocusDirection("left".to_owned())
        );
        assert_eq!(
            cfg.binds[1].action,
            ActionDescriptor::WindowMoveToMonitorDirection("right".to_owned())
        );
        assert_eq!(
            cfg.binds[2].action,
            ActionDescriptor::MonitorFocus("DP-1".to_owned())
        );
        assert_eq!(
            cfg.binds[3].action,
            ActionDescriptor::WindowMoveToMonitor("eDP-1".to_owned())
        );
    }

    #[test]
    fn window_move_to_workspace_defaults_to_follow_and_accepts_silent_option() {
        let mut sandbox = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();
        let lua = r#"
            koma.bind("Mod + Shift + 2", koma.act.window.move_to_workspace("2"))
            koma.bind("Mod + Shift + 3", koma.act.window.move_to_workspace("3", { follow = false }))
        "#;
        let cfg = sandbox
            .eval_init_lua(lua)
            .expect("window.move_to_workspace options must evaluate cleanly");
        assert_eq!(cfg.binds.len(), 2);
        assert_eq!(
            cfg.binds[0].action,
            ActionDescriptor::WindowMoveToWorkspace {
                workspace: "2".to_owned(),
                follow: true,
            }
        );
        assert_eq!(
            cfg.binds[1].action,
            ActionDescriptor::WindowMoveToWorkspace {
                workspace: "3".to_owned(),
                follow: false,
            }
        );

        let stubs = crate::generate_luals_stubs();
        assert!(stubs.contains(
            "move_to_workspace: fun(ws: string, opts?: { follow?: boolean }): KomaAction"
        ));
    }

    #[test]
    fn evaluates_dotfiles_showcase_and_preserves_registered_curves() {
        let mut sandbox = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();
        let cfg = sandbox
            .eval_init_lua(include_str!("../../../examples/dotfiles_showcase.lua"))
            .expect("examples/dotfiles_showcase.lua should evaluate cleanly");

        let anims = &cfg.appearance.animations;
        assert!(anims.curves.contains_key("overshoot_pop"));
        assert!(anims.curves.contains_key("silk_glide"));
        assert!(anims.curves.contains_key("bouncy_spring"));
        assert_eq!(anims.windows_open.curve.as_deref(), Some("overshoot_pop"));
        assert_eq!(anims.windows_open.style.as_deref(), Some("popin"));

        let border = &cfg.appearance.border;
        assert_eq!(border.active_color_end.as_deref(), Some("#bb9af7"));
        assert_eq!(border.inactive_color_end.as_deref(), Some("#1f2335"));
        assert!((border.gradient_angle - 45.0).abs() < 1e-4);
        assert!((border.gradient_rotate_rpm - 12.0).abs() < 1e-4);

        assert_eq!(cfg.window_rules[0].inactive_opacity, Some(0.82));
        assert_eq!(cfg.window_rules[0].animation.as_deref(), Some("popin"));
        assert_eq!(cfg.window_rules[0].border_color.as_deref(), Some("#7dcfff"));
        assert_eq!(cfg.window_rules[1].no_blur, Some(true));
        assert_eq!(cfg.window_rules[1].no_shadow, Some(true));

        assert!(sandbox.has_event_hooks("output.changed"));
        assert!(!sandbox.has_event_hooks("window.closed"));

        // Partial `koma.appearance(...)` override must preserve already registered curves.
        let mut partial_sb = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();
        let partial_cfg = partial_sb
            .eval_init_lua(
                r##"
                koma.bezier("custom_pop", 0.05, 0.9, 0.1, 1.08, 220)
                koma.spring("custom_sp", { stiffness = 500, damping = 20, mass = 1.0 })
                koma.appearance({ corner_radius = 10 })
                "##,
            )
            .expect("partial appearance after curve registration must succeed");
        assert!(
            partial_cfg
                .appearance
                .animations
                .curves
                .contains_key("custom_pop")
        );
        assert!(
            partial_cfg
                .appearance
                .animations
                .curves
                .contains_key("custom_sp")
        );
        assert_eq!(partial_cfg.appearance.corner_radius, Some(10));
    }

    #[test]
    fn macro_context_desktop_queries_dispatch_redaction_and_event_hooks() {
        use crate::{LuaOutputInfo, LuaWindowInfo, LuaWorkspaceInfo};

        let mut sandbox = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();
        let cfg = sandbox
            .eval_init_lua(
                r##"
                koma.bind("Mod + Return", function(ctx)
                    local focused = ctx:focused_window()
                    assert(focused ~= nil and focused.id == 42)
                    assert(focused.app_id == "com.mitchellh.ghostty")

                    local wins = ctx:windows()
                    assert(#wins == 2)
                    assert(wins[2].app_id == "[protected]")
                    assert(wins[2].title == "[protected]")

                    local outs = ctx:outputs()
                    assert(#outs == 1 and outs[1].name == "DP-1" and outs[1].focused == true)

                    local wss = ctx:workspaces()
                    assert(#wss == 1 and wss[1].name == "1" and wss[1].active == true)

                    ctx:dispatch(koma.act.window.focus(42))
                    ctx:dispatch(koma.act.workspace.set_layout("scrolling", "2"))

                    local nested_ok = pcall(function()
                        ctx:dispatch({ LuaCallback = 1 })
                    end)
                    assert(not nested_ok)
                end, { desc = "Run-or-Raise test" })

                koma.on("output.changed", function(ctx, ev)
                    if ev.width and ev.width >= 3440 then
                        ctx:dispatch(koma.act.workspace.set_layout("scrolling"))
                    end
                end)
                "##,
            )
            .expect("closure + event hook script must parse cleanly");

        let ActionDescriptor::LuaCallback(bind_id) = cfg.binds[0].action else {
            panic!("expected LuaCallback action for function bind");
        };

        let focused_win = LuaWindowInfo {
            id: 42,
            app_id: "com.mitchellh.ghostty".to_owned(),
            title: "fish".to_owned(),
            workspace: "1".to_owned(),
            floating: false,
            fullscreen: false,
            focused: true,
        };
        sandbox.set_desktop_snapshot(DesktopQuerySnapshot {
            focused_window: Some(focused_win.clone()),
            windows: vec![
                focused_win,
                LuaWindowInfo {
                    id: 99,
                    app_id: "[protected]".to_owned(),
                    title: "[protected]".to_owned(),
                    workspace: "1".to_owned(),
                    floating: true,
                    fullscreen: false,
                    focused: false,
                },
            ],
            outputs: vec![LuaOutputInfo {
                name: "DP-1".to_owned(),
                width: 3440,
                height: 1440,
                scale: 1.0,
                refresh_mhz: 165_000,
                active_workspace: "1".to_owned(),
                focused: true,
            }],
            workspaces: vec![LuaWorkspaceInfo {
                id: "1".to_owned(),
                name: "1".to_owned(),
                layout: "dwindle".to_owned(),
                presentation: "headed".to_owned(),
                focused_window_id: Some(42),
                window_count: 2,
                active: true,
            }],
        });

        let ws_id = WorkspaceId::new("1");
        sandbox
            .start_bind_callback_tx(bind_id, ws_id.clone(), Epoch(1), 100)
            .expect("bind closure must execute cleanly");
        let actions = sandbox.drain_queued_actions();
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            actions[0],
            QueuedHostAction::DispatchAction {
                action: ActionDescriptor::WindowFocus(42),
                ..
            }
        ));
        assert!(matches!(
            &actions[1],
            QueuedHostAction::DispatchAction {
                action: ActionDescriptor::WorkspaceSetLayout {
                    workspace: Some(ws),
                    layout: LayoutMode::Scrolling,
                },
                ..
            } if ws == "2"
        ));

        sandbox
            .emit_event_hooks(
                "output.changed",
                &serde_json::json!({ "output": "DP-1", "width": 3440, "height": 1440 }),
                ws_id.clone(),
                Epoch(1),
                200,
            )
            .expect("event hook must execute cleanly");
        let hook_actions = sandbox.drain_queued_actions();
        assert_eq!(hook_actions.len(), 1);
        assert!(matches!(
            hook_actions[0],
            QueuedHostAction::DispatchAction {
                action: ActionDescriptor::WorkspaceSetLayout {
                    workspace: None,
                    layout: LayoutMode::Scrolling,
                },
                ..
            }
        ));

        // Verify recursion depth cap (`MAX_EVENT_DEPTH = 4`).
        let mut depth_sb = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();
        depth_sb
            .eval_init_lua(
                r##"
                koma.on("workspace.focused", function(ctx, ev)
                    ctx:dispatch(koma.act.workspace.focus("2"))
                end)
                "##,
            )
            .unwrap();
        for _ in 0..4 {
            assert!(
                depth_sb
                    .emit_event_hooks(
                        "workspace.focused",
                        &serde_json::json!({}),
                        ws_id.clone(),
                        Epoch(1),
                        300,
                    )
                    .is_ok()
            );
        }
        // Reset check isn't stuck on sequential calls: each top-level `emit_event_hooks`
        // decrements depth back to 0 on exit, so sequential dispatches stay within cap!
        let depth_err = {
            depth_sb.state.borrow_mut().event_dispatch_depth = 4;
            depth_sb.emit_event_hooks(
                "workspace.focused",
                &serde_json::json!({}),
                ws_id,
                Epoch(1),
                301,
            )
        };
        assert!(depth_err.is_err());

        // Verify LuaLS stubs include new desktop query/event types.
        let stubs = crate::generate_luals_stubs();
        assert!(stubs.contains("---@class KomaWindowInfo"));
        assert!(stubs.contains("---@class KomaCurveSpec"));
        assert!(stubs.contains("---@class KomaKinematicsSpec"));
        assert!(stubs.contains("---@field on fun(event: KomaEventName"));
        assert!(stubs.contains("function MacroContext:dispatch(action) end"));
    }

    #[test]
    fn macro_pointer_kinematics_support_hooke_spring_named_curve_and_bezier() {
        let mut sandbox = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();
        sandbox
            .eval_init_lua(
                r##"
                koma.spring("mouse_glide", { mass = 1.0, damping = 22.0, stiffness = 360.0, epsilon = 0.01 })

                koma.macro.define({
                    id = "pointer.hooke_and_bezier_demo",
                    app_id = ".*",
                    description = "Exercises 2D Hooke spring, named spring curve, and cubic Bezier pointer kinematics",
                    run = function(ctx)
                        ctx:move_to(
                            { anchor = "center", x = 80, y = 0 },
                            { profile = "spring", mass = 1.0, damping = 22.0, stiffness = 360.0 }
                        )
                        ctx:drag({
                            button = "left",
                            from = { u = 0.25, v = 0.5 },
                            to = { u = 0.75, v = 0.5 },
                            hold_before_move_ms = 20,
                            kinematics = { curve = "mouse_glide" },
                            hold_before_release_ms = 10,
                        })
                        ctx:move_to(
                            { u = 0.5, v = 0.5 },
                            { profile = "bezier", bezier = { 0.05, 0.9, 0.1, 1.1 }, duration_ms = 140 }
                        )
                    end,
                })
                "##,
            )
            .expect("hooke and bezier macro script must load cleanly");

        let anim_cfg = sandbox.animations_snapshot();
        let ws_id = WorkspaceId::from("ws_01");
        let (_exec, mode) = sandbox
            .start_macro_tx(
                ws_id.clone(),
                Epoch(1),
                &MacroId::from("pointer.hooke_and_bezier_demo"),
                &serde_json::json!({}),
                1_000,
            )
            .expect("macro should start cleanly");
        assert_eq!(mode, ExecutorMode::HostSuspended);

        let a1 = sandbox.drain_queued_actions();
        assert_eq!(a1.len(), 1);
        match &a1[0] {
            QueuedHostAction::MovePointer { kinematics, .. } => {
                match kinematics.resolve_spec(&anim_cfg) {
                    PointerTrajectorySpec::HookeSpring {
                        params,
                        duration_cap_ms: None,
                        ..
                    } => {
                        assert!((params.mass - 1.0).abs() < 1e-6);
                        assert!((params.damping - 22.0).abs() < 1e-6);
                        assert!((params.stiffness - 360.0).abs() < 1e-6);
                    }
                    other => panic!("expected HookeSpring, got {other:?}"),
                }
            }
            other => panic!("expected MovePointer, got {other:?}"),
        }

        struct SettleBridge(Epoch, bool);
        impl crate::host_abi::HostBridge for SettleBridge {
            fn current_epoch(&self, _ws_id: &WorkspaceId) -> Option<Epoch> {
                Some(self.0)
            }
            fn pointer_trajectory_settled(&self, _ws_id: &WorkspaceId) -> Option<bool> {
                Some(self.1)
            }
            fn focused_app_id(&self, _ws_id: &WorkspaceId) -> Option<String> {
                None
            }
            fn tap_key_chord(&mut self, _: &WorkspaceId, _: &str) -> Result<(), IpcError> {
                Ok(())
            }
            fn move_pointer(&mut self, _: &WorkspaceId, _: &AnchorPoint) -> Result<u64, IpcError> {
                Ok(0)
            }
            fn inject_text(
                &mut self,
                _: &WorkspaceId,
                _: &str,
                _: TextInjectStrategy,
            ) -> Result<(), IpcError> {
                Ok(())
            }
            fn paste_mime(&mut self, _: &WorkspaceId, _: &str, _: &[u8]) -> Result<(), IpcError> {
                Ok(())
            }
        }

        // Still moving at 1_040 ms -> must stay suspended!
        let r0 = sandbox.drive_ready_coroutines(1_040, &[], &SettleBridge(Epoch(1), false));
        assert_eq!(r0.resumed_slices, 0);

        // Settle signal at 1_240 ms wakes ctx:move_to before the 2500 ms safety cap and starts drag!
        let r1 = sandbox.drive_ready_coroutines(1_240, &[], &SettleBridge(Epoch(1), true));
        assert_eq!(r1.resumed_slices, 1);
        let a2 = sandbox.drain_queued_actions();
        assert_eq!(
            a2.len(),
            2,
            "drag Stage 1+2 MovePointer(instant) + Button(down)"
        );

        // hold_before_move_ms (20 ms) -> Stage 3 drag trajectory using named `mouse_glide` spring!
        let r2 = sandbox.drive_ready_coroutines(1_260, &[], &SettleBridge(Epoch(1), false));
        assert_eq!(r2.resumed_slices, 1);
        let a3 = sandbox.drain_queued_actions();
        assert_eq!(a3.len(), 1);
        match &a3[0] {
            QueuedHostAction::MovePointer { kinematics, .. } => {
                assert!(matches!(
                    kinematics.resolve_spec(&anim_cfg),
                    PointerTrajectorySpec::HookeSpring { .. }
                ));
            }
            other => panic!("expected named-curve HookeSpring MovePointer, got {other:?}"),
        }

        // Spring settles -> Stage 4 hold (10 ms) -> Stage 5 button up + Bezier move_to
        let r3 = sandbox.drive_ready_coroutines(1_500, &[], &SettleBridge(Epoch(1), true));
        assert_eq!(r3.resumed_slices, 1);
        let r4 = sandbox.drive_ready_coroutines(1_510, &[], &SettleBridge(Epoch(1), true));
        assert_eq!(r4.resumed_slices, 1);
        let a4 = sandbox.drain_queued_actions();
        assert!(matches!(
            a4[0],
            QueuedHostAction::PointerButton { pressed: false, .. }
        ));
        match &a4[1] {
            QueuedHostAction::MovePointer { kinematics, .. } => {
                match kinematics.resolve_spec(&anim_cfg) {
                    PointerTrajectorySpec::Bezier {
                        bezier,
                        duration_ms,
                        ..
                    } => {
                        assert_eq!(duration_ms, 140);
                        assert!((bezier.y2 - 1.1).abs() < 1e-6);
                    }
                    other => panic!("expected Bezier spec, got {other:?}"),
                }
            }
            other => panic!("expected MovePointer(Bezier), got {other:?}"),
        }

        let r5 = sandbox.drive_ready_coroutines(1_650, &[], &SettleBridge(Epoch(1), false));
        assert_eq!(r5.completed_macros, 1);
    }

    #[test]
    fn agent_cursor_config_and_path_macro_methods_work_in_sandbox() {
        let mut sandbox = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, None).unwrap();
        let script = r##"
            koma.appearance({
                agent_cursor = {
                    enabled = true,
                    size = 32,
                    accent_color = "#7ee787",
                    trail_enabled = true,
                    fog_enabled = false,
                    ripple_enabled = true,
                    label_enabled = false,
                    idle_timeout_ms = 5000,
                    idle_wobble = false,
                },
            })
            koma.macro.define({
                id = "cursor.path_demo",
                app_id = ".*",
                run = function(ctx)
                    ctx:move_path(
                        { { x = 10, y = 20 }, { x = 120, y = 80 }, { x = 240, y = 40 } },
                        { spline = "catmull_rom", kinematics = { profile = "bezier", duration_ms = 120 } }
                    )
                    ctx:stroke_path({
                        waypoints = { { grid_225 = "B2" }, { grid_225 = "G8" } },
                        spline = "bezier",
                        hold_before_move_ms = 20,
                        hold_before_release_ms = 10,
                        kinematics = { profile = "minimum_jerk", duration_ms = 100 },
                    })
                    ctx:path({
                        waypoints = { { x = 50, y = 50 }, { x = 150, y = 150 } },
                    })
                    ctx:settle_cursor(true)
                end,
            })
        "##;
        sandbox.eval_init_lua(script).unwrap();

        let ac = &sandbox.config_snapshot().appearance.agent_cursor;
        assert!(ac.enabled);
        assert!((ac.size - 32.0).abs() < 1e-4);
        assert_eq!(ac.accent_color, "#7ee787");
        assert!(!ac.fog_enabled);
        assert!(!ac.label_enabled);
        assert_eq!(ac.idle_timeout_ms, 5000);
        assert!(!ac.idle_wobble);

        let stubs = crate::generate_luals_stubs();
        assert!(stubs.contains("---@alias KomaPathSplineKind"));
        assert!(stubs.contains("---@class KomaAgentCursorSpec"));
        assert!(stubs.contains("---@field label_enabled? boolean"));
        assert!(stubs.contains("---@class KomaPathSpec"));
        assert!(stubs.contains("---@field agent_cursor? KomaAgentCursorSpec"));
        assert!(stubs.contains("function MacroContext:move_path(waypoints, opts) end"));
        assert!(stubs.contains("function MacroContext:stroke_path(spec) end"));
        assert!(stubs.contains("function MacroContext:path(spec) end"));
        assert!(stubs.contains("function MacroContext:settle_cursor(immediate) end"));

        let (_exec, mode) = sandbox
            .start_macro_tx(
                WorkspaceId::new("ws-1"),
                Epoch(1),
                &MacroId::new("cursor.path_demo"),
                &serde_json::json!({}),
                0,
            )
            .unwrap();
        assert_eq!(mode, ExecutorMode::HostSuspended);

        struct DummyBridge;
        impl crate::host_abi::HostBridge for DummyBridge {
            fn current_epoch(&self, _ws_id: &WorkspaceId) -> Option<Epoch> {
                Some(Epoch(1))
            }
            fn focused_app_id(&self, _ws_id: &WorkspaceId) -> Option<String> {
                None
            }
            fn tap_key_chord(&mut self, _: &WorkspaceId, _: &str) -> Result<(), IpcError> {
                Ok(())
            }
            fn move_pointer(&mut self, _: &WorkspaceId, _: &AnchorPoint) -> Result<u64, IpcError> {
                Ok(0)
            }
            fn inject_text(
                &mut self,
                _: &WorkspaceId,
                _: &str,
                _: TextInjectStrategy,
            ) -> Result<(), IpcError> {
                Ok(())
            }
            fn paste_mime(&mut self, _: &WorkspaceId, _: &str, _: &[u8]) -> Result<(), IpcError> {
                Ok(())
            }
        }

        sandbox.drive_ready_coroutines(0, &[], &DummyBridge);
        let a0 = sandbox.drain_queued_actions();
        assert!(matches!(
            &a0[0],
            QueuedHostAction::MovePointerPath {
                spline: PathSplineKind::CatmullRom,
                waypoints,
                ..
            } if waypoints.len() == 3
        ));

        sandbox.drive_ready_coroutines(120, &[], &DummyBridge);
        let a1 = sandbox.drain_queued_actions();
        assert!(matches!(a1[0], QueuedHostAction::MovePointer { .. }));
        assert!(matches!(
            &a1[1],
            QueuedHostAction::PointerButton {
                button,
                pressed: true,
                ..
            } if button == "left"
        ));

        sandbox.drive_ready_coroutines(140, &[], &DummyBridge);
        let a2 = sandbox.drain_queued_actions();
        assert!(matches!(
            &a2[0],
            QueuedHostAction::MovePointerPath {
                spline: PathSplineKind::Bezier,
                waypoints,
                ..
            } if waypoints.len() == 2
        ));

        sandbox.drive_ready_coroutines(240, &[], &DummyBridge);
        sandbox.drive_ready_coroutines(250, &[], &DummyBridge);
        let a3 = sandbox.drain_queued_actions();
        assert!(matches!(
            a3[0],
            QueuedHostAction::PointerButton { pressed: false, .. }
        ));
        assert!(matches!(a3[1], QueuedHostAction::MovePointerPath { .. }));

        let r_end = sandbox.drive_ready_coroutines(450, &[], &DummyBridge);
        assert_eq!(r_end.completed_macros, 1);
        let a4 = sandbox.drain_queued_actions();
        assert!(matches!(
            a4[0],
            QueuedHostAction::SettleAgentCursor {
                immediate: true,
                ..
            }
        ));
    }
}
