//! `~/.config/koma/init.lua` & `lua/*.lua` filesystem watcher and Shadow-VM lossless hot-reload
//! (modeled directly on `niri/src/utils/watcher.rs` & `niri/src/niri.rs` `State::reload_config`).
//!
//! ### Design Guarantees
//! 1. **Neovim Atomic Save, GNU Stow Symlink & Nix Store Safe**:
//!    - Combines Linux `inotify` directory watches (`IN_CLOSE_WRITE | IN_MOVED_TO | IN_CREATE |
//!      IN_DELETE | IN_ATTRIB | IN_DELETE_SELF | IN_MOVE_SELF`) with Niri's canonical-path + `mtime`
//!      + `byte_len` `FileProps` comparison and a `35 ms` settle debounce so atomic `rename(2)` saves
//!        and symlink target swaps (even with frozen epoch `mtime = 0` on NixOS/Stow) trigger cleanly
//!        exactly once without partial-write races (`niri` issue `#2226`).
//!    - Automatically tracks all `*.lua` modules inside `<config_dir>/lua/` so editing `lua/keys.lua`
//!      reloads `init.lua` without touching the root file.
//! 2. **Shadow-VM Transactional Hot-Reload (`Zero State Corruption`)**:
//!    - Because `PhodopusSandbox` (`gc_arena::Lua`) is `!Send`, the watcher thread reads and delivers
//!      the file change signal via `calloop::channel::sync_channel`, and the compositor main thread
//!      evaluates `init.lua` inside a brand-new isolated **Shadow `PhodopusSandbox`** (`200,000` fuel,
//!      `8 MiB` heap ceiling) + compiles all `WindowRuleEngine` regexes **before** mutating `KomaState`.
//!    - On any syntax, `require`, fuel, memory, or regex error, the Shadow VM is discarded; the
//!      previous valid configuration, window stacks, and Agent `WorkspaceLease` `Epoch`s stay 100%
//!      intact while `config_error_banner` lights up the top `#f85149` alert bar.
//! 3. **In-Flight Macro Coroutine Protection**:
//!    - If a `MacroContext` coroutine (`ctx:drag` / `ctx:sleep`) is suspended on `calloop` when a
//!      reload succeeds, declarative rules/monitors/workspaces update immediately while the Shadow VM
//!      is staged in `deferred_lua_sandbox` and promoted automatically the instant `active_txs == 0`.

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime},
};

use calloop::channel::{Event as ChannelEvent, SyncSender, sync_channel};
use koma_lua::{
    DEFAULT_HEAP_LIMIT_BYTES, KomaConfigRegistry, LuaWindowRuleSpec,
    phodopus_bridge::PhodopusSandbox,
};
use koma_model::{
    DynamicWindowRules, InitialWindowRules, LayoutMode, PresentationTarget, WindowMatchSpec,
    WindowRuleEngine, WindowRuleEntry, WorkspaceState,
};
use koma_proto::{OutputId, SeatId, WorkspaceId};
use koma_render::offscreen_fbo::LazyOffscreenSurface;
use koma_seat::VirtualSeatLedger;
use regex::Regex;
use tracing::{debug, info, trace, warn};

#[cfg(feature = "smithay-tty")]
use crate::state::ActiveBackend;
use crate::{gesture_dispatcher::GestureTable, keybind_dispatcher::KeybindTable, state::KomaState};

const POLL_FALLBACK_MS: i32 = 350;
#[cfg(target_os = "linux")]
const WRITE_SETTLE_DEBOUNCE: Duration = Duration::from_millis(35);

/// Resolved configuration path strategy (`niri/src/main.rs` `ConfigPath`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KomaConfigPath {
    /// Explicit path passed via `--config <path>` or `$KOMA_CONFIG`.
    Explicit(PathBuf),
    /// Standard XDG hierarchy: `$XDG_CONFIG_HOME/koma/init.lua` -> `/etc/xdg/koma/init.lua`.
    Regular {
        user_init: PathBuf,
        system_init: PathBuf,
    },
}

impl KomaConfigPath {
    /// Resolves CLI override -> `$KOMA_CONFIG` -> XDG user/system default paths.
    #[must_use]
    pub fn resolve(cli_override: Option<PathBuf>) -> Self {
        if let Some(explicit) = cli_override.or_else(|| {
            env::var_os("KOMA_CONFIG")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        }) {
            return Self::Explicit(explicit);
        }

        let user_config_base = env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));

        Self::Regular {
            user_init: user_config_base.join("koma").join("init.lua"),
            system_init: PathBuf::from("/etc/xdg/koma/init.lua"),
        }
    }

    /// Returns the currently active `init.lua` path (preferring `user_init` when it exists).
    #[must_use]
    pub fn active_init_path(&self) -> PathBuf {
        match self {
            Self::Explicit(p) => p.clone(),
            Self::Regular {
                user_init,
                system_init,
            } => {
                if user_init.exists() || !system_init.exists() {
                    user_init.clone()
                } else {
                    system_init.clone()
                }
            }
        }
    }

    /// Directories that may contain `init.lua` or `lua/*.lua` modules.
    #[must_use]
    pub fn candidate_watch_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        let mut push_parent = |p: &Path| {
            if let Some(parent) = p.parent() {
                let dir = parent.to_path_buf();
                if !dirs.contains(&dir) {
                    dirs.push(dir.clone());
                }
                let lua_sub = dir.join("lua");
                if !dirs.contains(&lua_sub) {
                    dirs.push(lua_sub);
                }
            }
        };
        match self {
            Self::Explicit(p) => push_parent(p),
            Self::Regular {
                user_init,
                system_init,
            } => {
                push_parent(user_init);
                push_parent(system_init);
            }
        }
        dirs
    }
}

/// Identity & modification fingerprint of a watched config file (`niri/src/utils/watcher.rs` `Props`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileProps {
    canonical: PathBuf,
    mtime: Option<SystemTime>,
    byte_len: u64,
}

impl FileProps {
    fn from_path(path: &Path) -> io::Result<Self> {
        let canonical = path.canonicalize()?;
        let meta = canonical.metadata()?;
        Ok(Self {
            canonical,
            mtime: meta.modified().ok(),
            byte_len: meta.len(),
        })
    }
}

/// Pure, testable snapshot differ mirroring `niri/src/utils/watcher.rs` `WatcherInner`.
struct ConfigTreeSnapshotter {
    config_path: KomaConfigPath,
    last_init_props: Option<FileProps>,
    last_module_props: BTreeMap<PathBuf, FileProps>,
}

impl ConfigTreeSnapshotter {
    fn new(config_path: KomaConfigPath) -> Self {
        let mut this = Self {
            config_path,
            last_init_props: None,
            last_module_props: BTreeMap::new(),
        };
        let _ = this.poll_changed();
        this
    }

    /// Returns `true` if `init.lua` or any `<config_dir>/lua/**/*.lua` file was created, modified,
    /// deleted, or relinked since the last snapshot.
    fn poll_changed(&mut self) -> bool {
        let active_init = self.config_path.active_init_path();
        let new_init_props = FileProps::from_path(&active_init).ok();

        // Mirrors Niri's `remove_file` rule: if `init.lua` temporarily disappears mid-atomic-save
        // (and no fallback exists), keep the current config without firing a spurious error reload!
        let Some(valid_init_props) = new_init_props else {
            return false;
        };

        let mut new_modules = BTreeMap::new();
        if let Some(cfg_dir) = active_init.parent() {
            collect_lua_module_props(&cfg_dir.join("lua"), &mut new_modules);
        }

        let changed = self.last_init_props.as_ref() != Some(&valid_init_props)
            || self.last_module_props != new_modules;

        self.last_init_props = Some(valid_init_props);
        self.last_module_props = new_modules;
        changed
    }
}

fn collect_lua_module_props(dir: &Path, out: &mut BTreeMap<PathBuf, FileProps>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let p = entry.path();
        if p.is_dir() {
            collect_lua_module_props(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("lua")
            && let Ok(props) = FileProps::from_path(&p)
        {
            out.insert(p, props);
        }
    }
}

/// Message sent from the watcher thread to the `calloop` compositor thread.
#[derive(Debug)]
pub struct PreloadedConfigEvent {
    pub init_path: PathBuf,
    pub config_dir: PathBuf,
    pub source_res: io::Result<String>,
}

enum WatcherCommand {
    ReloadNow(Option<PathBuf>),
}

/// Handle stored on `KomaState` to keep the watcher thread alive and trigger manual reloads.
pub struct ConfigWatcherHandle {
    cmd_tx: mpsc::Sender<WatcherCommand>,
}

impl ConfigWatcherHandle {
    pub fn request_reload(&self, override_path: Option<PathBuf>) {
        let _ = self.cmd_tx.send(WatcherCommand::ReloadNow(override_path));
    }
}

/// Summary returned to `komactl reload` / `config.reload` IPC callers and logged on reload.
#[derive(Debug, Clone)]
pub struct ConfigReloadReport {
    pub generation: u64,
    pub init_path: PathBuf,
    pub binds_count: usize,
    pub gestures_count: usize,
    pub window_rules_count: usize,
    pub macros_count: usize,
    pub monitors_count: usize,
    pub deferred_vm_swap: bool,
}

/// Loads initial `~/.config/koma/init.lua` (if present on disk) and spawns the non-blocking
/// `inotify` + canonical-`FileProps` watcher wired into `calloop` (`niri/src/utils/watcher.rs` `setup`).
pub fn setup_config_watcher(state: &mut KomaState, config_path: KomaConfigPath) {
    state.config_path = config_path.clone();

    // 1. Perform initial synchronous load if `init.lua` exists on disk
    let initial_init = config_path.active_init_path();
    if initial_init.exists() {
        match fs::read_to_string(&initial_init) {
            Ok(src) => {
                let cfg_dir = initial_init
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                if let Err(err) =
                    reload_from_lua_source(state, &initial_init, &cfg_dir, &src, false)
                {
                    warn!(path = %initial_init.display(), "initial init.lua load error: {err}");
                }
            }
            Err(err) => {
                warn!(path = %initial_init.display(), "failed reading initial init.lua: {err}");
            }
        }
    } else {
        debug!(
            path = %initial_init.display(),
            "no init.lua on disk yet; watching parent config directory for creation"
        );
    }

    // 2. Register `calloop::channel::sync_channel` receiver on the compositor event loop
    let Some(loop_handle) = state.loop_handle.clone() else {
        return;
    };
    let (event_tx, event_rx) = sync_channel::<PreloadedConfigEvent>(2);

    if let Err(err) = loop_handle.insert_source(event_rx, |event, _, state| {
        if let ChannelEvent::Msg(payload) = event {
            match payload.source_res {
                Ok(src) => {
                    let _ = reload_from_lua_source(
                        state,
                        &payload.init_path,
                        &payload.config_dir,
                        &src,
                        true,
                    );
                }
                Err(io_err) => {
                    let msg = format!("failed reading {}: {io_err}", payload.init_path.display());
                    record_reload_failure(state, &payload.init_path, msg, true);
                }
            }
        }
    }) {
        warn!("failed registering config watcher channel on calloop: {err}");
        return;
    }

    // 3. Spawn background watcher thread (`inotify` + canonical `FileProps` symlink verification)
    let (cmd_tx, cmd_rx) = mpsc::channel::<WatcherCommand>();
    let spawn_res = thread::Builder::new()
        .name("koma-config-watcher".to_owned())
        .spawn(move || run_watcher_thread(config_path, cmd_rx, event_tx));

    match spawn_res {
        Ok(_) => {
            state.config_watcher = Some(ConfigWatcherHandle { cmd_tx });
        }
        Err(err) => warn!("failed spawning koma-config-watcher thread: {err}"),
    }
}

fn run_watcher_thread(
    initial_path: KomaConfigPath,
    cmd_rx: mpsc::Receiver<WatcherCommand>,
    out_tx: SyncSender<PreloadedConfigEvent>,
) {
    let mut snapshotter = ConfigTreeSnapshotter::new(initial_path);
    let mut fs_monitor = ConfigFsMonitor::new();
    fs_monitor.sync_watches(&snapshotter.config_path);

    loop {
        let mut force_reload = false;

        // Drain explicit reload commands (`komactl reload`) without blocking
        loop {
            match cmd_rx.try_recv() {
                Ok(WatcherCommand::ReloadNow(Some(override_p))) => {
                    snapshotter = ConfigTreeSnapshotter::new(KomaConfigPath::Explicit(override_p));
                    fs_monitor.sync_watches(&snapshotter.config_path);
                    force_reload = true;
                }
                Ok(WatcherCommand::ReloadNow(None)) => {
                    force_reload = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }

        if !force_reload {
            fs_monitor.wait_for_poll_tick(&snapshotter.config_path);
        }

        if force_reload || snapshotter.poll_changed() {
            let init_path = snapshotter.config_path.active_init_path();
            let config_dir = init_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            trace!(path = %init_path.display(), "detected init.lua / lua module modification");
            let source_res = fs::read_to_string(&init_path);
            if out_tx
                .send(PreloadedConfigEvent {
                    init_path,
                    config_dir,
                    source_res,
                })
                .is_err()
            {
                break;
            }
        }
    }
}

/// Platform filesystem event monitor (`inotify` on Linux, periodic snapshot polling elsewhere).
struct ConfigFsMonitor {
    #[cfg(target_os = "linux")]
    inotify: Option<LinuxInotifyMonitor>,
}

impl ConfigFsMonitor {
    fn new() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            inotify: LinuxInotifyMonitor::new().ok(),
        }
    }

    fn sync_watches(&mut self, cfg_path: &KomaConfigPath) {
        #[cfg(target_os = "linux")]
        if let Some(ino) = self.inotify.as_mut() {
            ino.sync_watches(cfg_path);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = cfg_path;
    }

    fn wait_for_poll_tick(&mut self, cfg_path: &KomaConfigPath) {
        #[cfg(target_os = "linux")]
        if let Some(ino) = self.inotify.as_mut() {
            if ino.wait_for_activity(POLL_FALLBACK_MS) {
                // Debounce multi-step editor writes (`truncate` + `write` + `rename`)
                thread::sleep(WRITE_SETTLE_DEBOUNCE);
                ino.drain_events();
            }
            ino.sync_watches(cfg_path);
            return;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = cfg_path;
        thread::sleep(Duration::from_millis(POLL_FALLBACK_MS as u64));
    }
}

/// Minimal Linux `inotify` wrapper using safe `rustix` abstractions watching config & parent
/// directories so atomic `rename(2)` saves in Neovim wake immediately (< 1 ms) without polling.
#[cfg(target_os = "linux")]
struct LinuxInotifyMonitor {
    fd: OwnedFd,
    watched_dirs: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
impl LinuxInotifyMonitor {
    fn new() -> io::Result<Self> {
        use rustix::fs::inotify::{CreateFlags, init as inotify_init};

        let fd = inotify_init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC)
            .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
        Ok(Self {
            fd,
            watched_dirs: Vec::new(),
        })
    }

    fn sync_watches(&mut self, cfg_path: &KomaConfigPath) {
        use rustix::fs::inotify::{WatchFlags, add_watch as inotify_add_watch};

        let mask = WatchFlags::CLOSE_WRITE
            | WatchFlags::MOVED_TO
            | WatchFlags::CREATE
            | WatchFlags::DELETE
            | WatchFlags::ATTRIB
            | WatchFlags::DELETE_SELF
            | WatchFlags::MOVE_SELF;

        for dir in cfg_path.candidate_watch_dirs() {
            for candidate in [Some(dir.as_path()), dir.parent()].into_iter().flatten() {
                if candidate.is_dir()
                    && !self.watched_dirs.iter().any(|d| d == candidate)
                    && inotify_add_watch(&self.fd, candidate, mask).is_ok()
                {
                    self.watched_dirs.push(candidate.to_path_buf());
                }
            }
        }
    }

    fn wait_for_activity(&self, timeout_ms: i32) -> bool {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};

        let mut pfds = [PollFd::new(&self.fd, PollFlags::IN)];
        let timeout = (timeout_ms >= 0).then(|| Timespec {
            tv_sec: i64::from(timeout_ms / 1000),
            tv_nsec: (i64::from(timeout_ms % 1000) * 1_000_000) as _,
        });
        if let Ok(n) = poll(&mut pfds, timeout.as_ref())
            && n > 0
            && pfds[0].revents().contains(PollFlags::IN)
        {
            self.drain_events();
            return true;
        }
        false
    }

    fn drain_events(&self) {
        let mut buf = [0_u8; 4096];
        loop {
            match rustix::io::read(&self.fd, &mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => break,
            }
        }
    }
}

/// Evaluates `init_source` in a brand-new Shadow `PhodopusSandbox` and compiles all window-rule
/// regexes **without** touching `KomaState`. Used by both live hot-reload and `komactl validate`!
pub fn build_validated_shadow_sandbox(
    config_dir: &Path,
    init_source: &str,
    now_ms: u64,
) -> Result<
    (
        PhodopusSandbox,
        KomaConfigRegistry,
        Vec<WindowRuleEntry>,
        KeybindTable,
        GestureTable,
    ),
    String,
> {
    let mut shadow_vm = PhodopusSandbox::new(DEFAULT_HEAP_LIMIT_BYTES, Some(config_dir))?;
    shadow_vm.set_clock_ms(now_ms);
    let registry = shadow_vm.eval_init_lua(init_source)?;
    let compiled_rules = compile_window_rule_specs(&registry.window_rules)?;
    let compiled_binds = KeybindTable::compile_with_options(
        &registry.binds,
        &registry.unbinds,
        registry.input.inherit_default_binds,
        registry.input.mod_key.as_deref(),
    )?;
    let compiled_gestures = GestureTable::compile(&registry.gestures)?;
    Ok((
        shadow_vm,
        registry,
        compiled_rules,
        compiled_binds,
        compiled_gestures,
    ))
}

fn compile_window_rule_specs(specs: &[LuaWindowRuleSpec]) -> Result<Vec<WindowRuleEntry>, String> {
    let mut entries = Vec::with_capacity(specs.len());
    for (idx, spec) in specs.iter().enumerate() {
        let mut matches = Vec::with_capacity(spec.matches.len());
        for m in &spec.matches {
            let app_id = match &m.app_id {
                Some(pat) => Some(Regex::new(pat).map_err(|e| {
                    format!("window rule #{idx} invalid `matches.app_id` regex `{pat}`: {e}")
                })?),
                None => None,
            };
            let title = match &m.title {
                Some(pat) => Some(Regex::new(pat).map_err(|e| {
                    format!("window rule #{idx} invalid `matches.title` regex `{pat}`: {e}")
                })?),
                None => None,
            };
            matches.push(WindowMatchSpec {
                app_id,
                title,
                is_floating: m.is_floating,
                is_focused: m.is_focused,
                is_xwayland: m.is_xwayland,
                is_modal_dialog: m.is_modal_dialog,
                is_screencast_target: m.is_screencast_target,
                is_agent_workspace: m.is_agent_workspace,
            });
        }

        let mut excludes = Vec::with_capacity(spec.excludes.len());
        for ex in &spec.excludes {
            let app_id = match &ex.app_id {
                Some(pat) => Some(Regex::new(pat).map_err(|e| {
                    format!("window rule #{idx} invalid `excludes.app_id` regex `{pat}`: {e}")
                })?),
                None => None,
            };
            let title = match &ex.title {
                Some(pat) => Some(Regex::new(pat).map_err(|e| {
                    format!("window rule #{idx} invalid `excludes.title` regex `{pat}`: {e}")
                })?),
                None => None,
            };
            excludes.push(WindowMatchSpec {
                app_id,
                title,
                is_floating: ex.is_floating,
                is_focused: ex.is_focused,
                is_xwayland: ex.is_xwayland,
                is_modal_dialog: ex.is_modal_dialog,
                is_screencast_target: ex.is_screencast_target,
                is_agent_workspace: ex.is_agent_workspace,
            });
        }

        entries.push(WindowRuleEntry {
            id: spec.id.map_or((idx + 1) as u64, |r| r.0),
            name: spec.name.clone(),
            enabled: spec.enabled,
            matches,
            excludes,
            initial: InitialWindowRules {
                open_on_workspace: spec.initial.open_on_workspace.clone(),
                open_mode: spec.initial.open_mode,
                open_focused: spec.initial.open_focused,
                default_floating_pos: spec
                    .initial
                    .default_floating_pos
                    .as_ref()
                    .map(|p| (p.anchor, p.x, p.y)),
            },
            dynamic: DynamicWindowRules {
                opacity: spec.opacity,
                inactive_opacity: spec.inactive_opacity,
                animation: spec.animation.clone(),
                no_blur: spec.no_blur,
                no_shadow: spec.no_shadow,
                border_color: spec.border_color.clone(),
                corner_radius: spec.corner_radius,
                clip_to_geometry: spec.clip_to_geometry,
                focusable: spec.focusable,
                suppress_maximize: None,
                block_out_from: spec.block_out_from,
            },
        });
    }
    Ok(entries)
}

/// Synchronous entry point called on `calloop` (from either `ConfigWatcher` or `config.reload` IPC)
/// to transactionally validate and apply `init_source`.
pub fn reload_from_lua_source(
    state: &mut KomaState,
    init_path: &Path,
    config_dir: &Path,
    init_source: &str,
    send_desktop_notification: bool,
) -> Result<ConfigReloadReport, String> {
    let now_ms = state.elapsed_ms();
    let (shadow_vm, registry, compiled_rules, mut compiled_binds, compiled_gestures) =
        match build_validated_shadow_sandbox(config_dir, init_source, now_ms) {
            Ok(ok) => ok,
            Err(err_msg) => {
                record_reload_failure(state, init_path, err_msg.clone(), send_desktop_notification);
                return Err(err_msg);
            }
        };

    let had_previous_error = state.config_error_banner.take().is_some();
    state.config_generation = state.config_generation.saturating_add(1);

    // 1. Atomically swap compiled `WindowRuleEngine` entries (preserving ProtectedSurfaceClassifier)
    let window_rules_count = compiled_rules.len();
    state.rules.replace_rules(compiled_rules);

    // Preserve active submap mode across hot-reload if still defined in the new bind table
    if let Some(cur_mode) = &state.keybinds.active_mode
        && compiled_binds
            .binds
            .iter()
            .any(|b| b.mode.as_ref() == Some(cur_mode))
    {
        compiled_binds.active_mode = Some(cur_mode.clone());
    }
    // A pending repeat belongs to the old bind table: cancel it before the atomic swap.
    state.cancel_keybind_repeat("config_reload");
    state.keybinds = compiled_binds;
    state.gestures = compiled_gestures;

    // 2. Reconcile declarative `koma.workspace(...)` specs losslessly (never dropping existing
    //    mapped windows, Deck stacks, or active Agent `WorkspaceLease` `Epoch`s!)
    reconcile_declarative_workspaces(state, &registry, now_ms);

    // 3. Reconcile live KMS monitor scales, transforms, 2D positions & notch safe areas
    #[cfg(feature = "smithay-tty")]
    {
        let updated = if let Some(ActiveBackend::Tty(tty)) = state.backend.as_mut() {
            tty.apply_hot_reloaded_monitors(&registry)
        } else {
            Vec::new()
        };
        for upd in updated {
            state.set_output_safe_area(upd.output.name(), upd.safe_area);
            if upd.explicit_pos.is_some() {
                state
                    .output_layout
                    .set_explicit_position(&upd.output.name(), upd.explicit_pos);
                state.apply_output_layout_positions();
            }
            state.handle_output_resized(&upd.output, upd.logical_w, upd.logical_h, upd.scale);
        }
    }

    let binds_count = registry.binds.len();
    let gestures_count = registry.gestures.len();
    let macros_count = registry.macros.len();
    let monitors_count = registry.monitors.len();

    // 4. Swap `PhodopusSandbox` immediately if no macro coroutines are parked, else stage in
    //    `deferred_lua_sandbox` until in-flight coroutines complete (`Zero Dropped Coroutines`).
    let deferred_vm_swap = if state.lua_sandbox.has_active_coroutines() {
        info!(
            active_coroutines = state.lua_sandbox.active_coroutine_count(),
            generation = state.config_generation,
            "in-flight macro coroutines active; staging validated Shadow PhodopusSandbox for lossless swap"
        );
        state.deferred_lua_sandbox = Some(shadow_vm);
        true
    } else {
        state.deferred_lua_sandbox = None;
        state.lua_sandbox = shadow_vm;
        false
    };

    state.apply_runtime_input_config(&registry);
    state.queue_redraw();
    if !deferred_vm_swap {
        state.emit_lua_event(
            "config.reloaded",
            serde_json::json!({
                "generation": state.config_generation,
                "init_path": init_path.display().to_string(),
            }),
        );
    }

    if had_previous_error && send_desktop_notification {
        spawn_nonblocking_notify(
            state,
            "normal",
            "Koma Config Reloaded",
            &format!("Recovered valid config from {}", init_path.display()),
        );
    }

    info!(
        generation = state.config_generation,
        path = %init_path.display(),
        binds = binds_count,
        gestures = gestures_count,
        window_rules = window_rules_count,
        macros = macros_count,
        deferred_vm_swap,
        "applied lossless transactional init.lua reload"
    );

    Ok(ConfigReloadReport {
        generation: state.config_generation,
        init_path: init_path.to_path_buf(),
        binds_count,
        gestures_count,
        window_rules_count,
        macros_count,
        monitors_count,
        deferred_vm_swap,
    })
}

/// Called from `macro_runtime::drive_macro_tick` once `active_txs` drains to 0 to complete any
/// staged Shadow-VM swap.
pub fn promote_deferred_sandbox_if_idle(state: &mut KomaState) {
    if state.deferred_lua_sandbox.is_some()
        && !state.lua_sandbox.has_active_coroutines()
        && let Some(ready_vm) = state.deferred_lua_sandbox.take()
    {
        state.lua_sandbox = ready_vm;
        let registry = state.lua_sandbox.config_snapshot();
        state.apply_runtime_input_config(&registry);
        state.emit_lua_event(
            "config.reloaded",
            serde_json::json!({ "generation": state.config_generation }),
        );
        info!(
            generation = state.config_generation,
            "promoted staged Shadow PhodopusSandbox after in-flight coroutines finished"
        );
    }
}

fn reconcile_declarative_workspaces(
    state: &mut KomaState,
    registry: &KomaConfigRegistry,
    now_ms: u64,
) {
    // Apply global `koma.layout(...)`, `koma.dwindle({...})` & `koma.scrolling({...})` settings across workspaces first
    for ws in state.workspaces.values_mut() {
        let mut d_cfg = registry.dwindle;
        let mut s_cfg = registry.scrolling.clone();
        if ws.is_special {
            d_cfg.outer_gap = d_cfg.outer_gap.max(48);
            s_cfg.outer_gap = s_cfg.outer_gap.max(48);
        }
        ws.dwindle.config = d_cfg;
        let vp = ws.viewport;
        ws.scrolling.update_config(s_cfg, vp, now_ms);
        if matches!(ws.presentation, PresentationTarget::Physical(_))
            && !ws.is_special
            && !registry.workspaces.iter().any(|s| s.name == ws.name)
        {
            ws.set_layout_mode(registry.default_layout, now_ms);
        } else {
            ws.recalculate(now_ms);
        }
    }

    let mut sync_ids: Vec<WorkspaceId> = state.workspaces.keys().cloned().collect();

    for spec in &registry.workspaces {
        let is_headless = spec.presentation.eq_ignore_ascii_case("headless");
        let effective_layout = if is_headless {
            LayoutMode::Deck
        } else if spec.has_explicit_layout {
            spec.layout
        } else {
            registry.default_layout
        };
        let use_spec_gaps = spec.has_explicit_gaps
            || spec.gaps_in.is_some()
            || spec.gaps_out.is_some()
            || spec.gaps.inner != 6
            || spec.gaps.outer != 10;

        if let Some((existing_id, existing)) = state
            .workspaces
            .iter_mut()
            .find(|(_, w)| w.name == spec.name)
        {
            let mut d_cfg = registry.dwindle;
            let mut s_cfg = registry.scrolling.clone();
            if use_spec_gaps {
                let inner = spec.effective_inner_gap();
                let outer = spec.effective_outer_gap();
                d_cfg.inner_gap = inner;
                d_cfg.outer_gap = if existing.is_special {
                    outer.max(48)
                } else {
                    outer
                };
                s_cfg.inner_gap = inner;
                s_cfg.outer_gap = if existing.is_special {
                    outer.max(48)
                } else {
                    outer
                };
            } else if existing.is_special {
                d_cfg.outer_gap = d_cfg.outer_gap.max(48);
                s_cfg.outer_gap = s_cfg.outer_gap.max(48);
            }
            existing.dwindle.config = d_cfg;
            let vp = existing.viewport;
            existing.scrolling.update_config(s_cfg, vp, now_ms);
            existing.set_layout_mode(effective_layout, now_ms);
            if !sync_ids.contains(existing_id) {
                sync_ids.push(existing_id.clone());
            }
            continue;
        }

        let next_idx = state.workspaces.len() + 1;
        let ws_id = WorkspaceId::new(format!("ws_{next_idx:02}"));
        let vseat_id = SeatId::new(format!("vseat_{next_idx:02}"));

        let presentation = if is_headless {
            let slot = state.fbo_pool.acquire_or_borrow(&ws_id);
            state.offscreen_surfaces.insert(
                ws_id.clone(),
                LazyOffscreenSurface::new(
                    spec.viewport.width as i32,
                    spec.viewport.height as i32,
                    spec.viewport.scale,
                ),
            );
            PresentationTarget::Offscreen {
                lazy_fbo_slot: slot,
            }
        } else {
            let out_id = spec
                .preferred_output
                .clone()
                .unwrap_or_else(|| OutputId::from("primary"));
            PresentationTarget::Physical(out_id)
        };

        let mut ws = WorkspaceState::new(
            ws_id.clone(),
            spec.name.clone(),
            vseat_id.clone(),
            spec.viewport,
            presentation,
            effective_layout,
            now_ms,
        );
        let mut d_cfg = registry.dwindle;
        let mut s_cfg = registry.scrolling.clone();
        if use_spec_gaps {
            let inner = spec.effective_inner_gap();
            let outer = spec.effective_outer_gap();
            d_cfg.inner_gap = inner;
            d_cfg.outer_gap = if ws.is_special { outer.max(48) } else { outer };
            s_cfg.inner_gap = inner;
            s_cfg.outer_gap = if ws.is_special { outer.max(48) } else { outer };
        } else if ws.is_special {
            d_cfg.outer_gap = d_cfg.outer_gap.max(48);
            s_cfg.outer_gap = s_cfg.outer_gap.max(48);
        }
        ws.dwindle.config = d_cfg;
        ws.scrolling.update_config(s_cfg, spec.viewport, now_ms);

        state
            .vseats
            .insert(vseat_id.clone(), VirtualSeatLedger::new(vseat_id));
        state.workspaces.insert(ws_id.clone(), ws);
        sync_ids.push(ws_id);
    }

    for id in sync_ids {
        state.sync_workspace_geometries(&id);
    }
}

fn record_reload_failure(
    state: &mut KomaState,
    init_path: &Path,
    err_msg: String,
    send_desktop_notification: bool,
) {
    state.config_reload_failures = state.config_reload_failures.saturating_add(1);
    warn!(
        failures = state.config_reload_failures,
        path = %init_path.display(),
        error = %err_msg,
        "init.lua hot-reload rejected; keeping previous valid configuration active"
    );
    state.config_error_banner = Some(err_msg.clone());
    state.queue_redraw();

    if send_desktop_notification {
        spawn_nonblocking_notify(state, "critical", "Koma init.lua Reload Failed", &err_msg);
    }
}

fn spawn_nonblocking_notify(state: &KomaState, urgency: &str, summary: &str, body: &str) {
    let mut cmd = Command::new("notify-send");
    cmd.args(["-a", "koma", "-u", urgency, summary, body]);
    state.configure_child_env(&mut cmd, None);
    let _ = crate::child_reaper::spawn_detached(&mut cmd, "config-reload.notify");
}

/// Triggers an immediate synchronous reload from disk (`config.reload` IPC handler).
pub fn reload_active_config_now(
    state: &mut KomaState,
    explicit_path: Option<PathBuf>,
) -> Result<ConfigReloadReport, String> {
    if let Some(p) = &explicit_path {
        state.config_path = KomaConfigPath::Explicit(p.clone());
        if let Some(w) = &state.config_watcher {
            w.request_reload(Some(p.clone()));
        }
    }
    let target = explicit_path.unwrap_or_else(|| state.config_path.active_init_path());
    let src = fs::read_to_string(&target)
        .map_err(|e| format!("failed reading `{}`: {e}", target.display()))?;
    let cfg_dir = target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    reload_from_lua_source(state, &target, &cfg_dir, &src, true)
}

#[allow(dead_code)]
fn _keep_rule_engine_type_check(_: &WindowRuleEngine) {}
