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
use koma_model::{AnchorPoint, DwindleConfig, ScrollingConfig};
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
        ActionDescriptor, AppearanceSpec, BindHandleId, BindOptions, BindSpec, DeviceOverrideSpec,
        GestureSpec, InputConfigSpec, KomaConfigRegistry, KomaHandleRef, LuaAnchorPointInput,
        LuaClickOptsInput, LuaDragSpecInput, LuaKeyOptsInput, LuaKinematicsInput, LuaLayerRuleSpec,
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
}

/// Shared interior state accessible to `Phodopus` callbacks on the single-threaded `calloop` thread.
#[derive(Default)]
struct SandboxInner {
    next_id: u64,
    now_ms: u64,
    in_declarative_load: bool,
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
        if mode == ExecutorMode::HostSuspended {
            if let Some(raw_handle) = pending_raw {
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
        if mode == ExecutorMode::HostSuspended {
            if let Some(raw_handle) = pending_raw {
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
        self.lua
            .try_enter(|ctx| {
                let exec = ctx.fetch(stashed);
                exec.resume_host_op(ctx, handle, result)?;
                let mut fuel = Fuel::with(MACRO_SLICE_FUEL_BUDGET as i32);
                let _ = exec.step(ctx, &mut fuel);
                Ok(exec.mode())
            })
            .unwrap_or(Ok(ExecutorMode::Stopped))
    }

    /// Cancels a parked `HostOpHandle` coroutine with a catchable error string (e.g., on `Mod + G`).
    pub fn cancel_parked(
        &mut self,
        stashed: &StashedExecutor,
        raw_handle: u64,
        reason: &str,
    ) -> Result<(), HostOpError> {
        let handle = HostOpHandle::from_raw(raw_handle);
        self.lua
            .try_enter(|ctx| {
                let exec = ctx.fetch(stashed);
                exec.cancel_host_op(ctx, handle, reason)?;
                let mut fuel = Fuel::with(4_096);
                let _ = exec.step(ctx, &mut fuel);
                Ok(())
            })
            .unwrap_or(Ok(()))
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
            if let Some(tx_id) = self.handle_to_tx.remove(&stale.raw_host_op_handle) {
                if let Some(inst) = self.active_txs.remove(&tx_id) {
                    let _ = self.cancel_parked(
                        &inst.stashed_exec,
                        stale.raw_host_op_handle,
                        "ERR_STALE_EPOCH: workspace epoch changed while macro was suspended",
                    );
                    report.cancelled_stale += 1;
                }
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
            if let Some(tx_id) = self.handle_to_tx.remove(&raw_h) {
                if let Some(inst) = self.active_txs.remove(&tx_id) {
                    let _ = self.cancel_parked(&inst.stashed_exec, raw_h, reason);
                    cancelled += 1;
                }
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

    #[must_use]
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

    // 2. koma.monitor, koma.workspace, koma.input, koma.device, koma.appearance
    {
        let st = state.clone();
        koma.set_field(
            ctx,
            "monitor",
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
                let val: Value = stack.consume(ctx)?;
                let spec: MonitorSpec = from_value(val).map_err(RuntimeError::new)?;
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
                let spec: WorkspaceSpec = from_value(val).map_err(RuntimeError::new)?;
                st.borrow_mut().config.workspaces.push(spec);
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
                let spec: AppearanceSpec = from_value(val).map_err(RuntimeError::new)?;
                st.borrow_mut().config.appearance = spec;
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
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
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
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
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
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
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
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
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
            Callback::from_fn(&ctx, move |ctx, _, mut stack| {
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
    if edge == TriggerEdge::Both {
        if let Value::Table(tbl) = target {
            let press_act = extract_action_descriptor(tbl.get_value(ctx, "press"))?;
            let rel_act = extract_action_descriptor(tbl.get_value(ctx, "release"))?;
            return Ok((press_act, Some(rel_act)));
        }
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

fn build_act_namespace<'gc>(ctx: Context<'gc>) -> Result<Table<'gc>, LuaError<'gc>> {
    let act = Table::new(&ctx);

    let unit_act = |ctx: Context<'gc>, desc: ActionDescriptor| {
        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            stack.replace(ctx, UserData::new_static(&ctx, desc.clone()));
            Ok(CallbackReturn::Return)
        })
    };

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
            let ws: std::string::String = stack.consume(ctx)?;
            stack.replace(
                ctx,
                UserData::new_static(&ctx, ActionDescriptor::WindowMoveToWorkspace(ws)),
            );
            Ok(CallbackReturn::Return)
        }),
    );
    act.set_field(ctx, "window", win);

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

    let populate_scrolling_ns = |tbl: &Table<'_>| {
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
    };
    let strip = Table::new(&ctx);
    populate_scrolling_ns(&strip);
    act.set_field(ctx, "scroll_strip", strip);
    let scrolling_ns = Table::new(&ctx);
    populate_scrolling_ns(&scrolling_ns);
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
                    let dur = if matches!(
                        kinematics.profile,
                        koma_proto::KinematicProfileKind::Instant
                    ) {
                        0
                    } else {
                        kinematics.duration_ms
                    };
                    inner.host_actions.push_back(QueuedHostAction::MovePointer {
                        ws_id: sc.ws_id.clone(),
                        expected_epoch: sc.expected_epoch,
                        target,
                        kinematics,
                    });
                    if dur > 0 {
                        let finish_at_ms = inner.now_ms.saturating_add(dur);
                        let ws_id = sc.ws_id.clone();
                        Ok::<_, LuaError<'_>>(Some(inner.park_host_op(
                            &sc,
                            PendingHostOp::PointerTrajectory {
                                ws_id,
                                finish_at_ms,
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
                        kinematics: LuaKinematicsInput {
                            profile: koma_proto::KinematicProfileKind::Instant,
                            duration_ms: 0,
                            arc_curvature: 0.0,
                        },
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
                    inner.host_actions.push_back(QueuedHostAction::MovePointer {
                        ws_id: sc.ws_id.clone(),
                        expected_epoch: sc.expected_epoch,
                        target: to_pt,
                        kinematics: kin,
                    });
                    if kin.duration_ms > 0 {
                        let finish_at_ms = inner.now_ms.saturating_add(kin.duration_ms);
                        let ws_id = sc.ws_id.clone();
                        Ok::<_, LuaError<'_>>(Some(inner.park_host_op(
                            &sc,
                            PendingHostOp::PointerTrajectory {
                                ws_id,
                                finish_at_ms,
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
}
