//! Compositor runtime state (`KomaState`), unified `ActiveBackend` (`Winit` | `Tty`),
//! Hyprland `CSpace` dual-layer (`Dwindle` / `Scrolling` / `Deck` + `FloatingStack`) &
//! `special:<name>` Scratchpad orchestration, `security-context-v1` isolation, and
//! two-phase `Unmapped -> Mapped` window lifecycle.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::OsString,
    sync::{Arc, atomic::AtomicU64},
    time::{Duration, Instant},
};

use calloop::{LoopHandle, LoopSignal, RegistrationToken};
use koma_lua::{
    ActionDescriptor, DesktopQuerySnapshot, FollowMouseMode, LuaOutputInfo, LuaWindowInfo,
    LuaWorkspaceInfo, TrampolineScheduler, phodopus_bridge::PhodopusSandbox,
};
use koma_model::{
    AgentCursorConfig, AgentCursorFrameSnapshot, AgentCursorVisualDynamics, CurveSpec, LayoutMode,
    LogicalPoint, LogicalRect, LogicalViewport, OpenMode, OutputLayout, OutputTransform,
    PresentationTarget, ResizeCorner, SafeAreaInsets, SavedOutputConfig, SizeHints,
    WindowAnimationTable, WindowMatchContext, WindowOpenStyle, WindowRuleEngine,
    WorkspaceLeaseState, WorkspaceState, WorkspaceTransitionTable, is_waydroid_app_id,
};
use koma_proto::{
    CapabilityToken, Epoch, EventTopic, OutputId, SeatId, TokenClaims, WindowId, WorkspaceEvent,
    WorkspaceId,
};
use koma_render::{
    LazyFboPool,
    blur_pipeline::CachedBackdropBlur,
    frame_composer::{
        ClosingWindowSceneItem, WindowSceneItem, capture_window_snapshot, surface_origin,
        window_snapshot_elements,
    },
    offscreen_fbo::{LazyOffscreenSurface, WindowFrameCache},
};
use koma_seat::VirtualSeatLedger;
use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        renderer::{
            ImportDma as _, element::memory::MemoryRenderBuffer, gles::GlesRenderer,
            utils::with_renderer_surface_state,
        },
    },
    desktop::{
        LayerMap, PopupKind, PopupManager, Window, WindowSurfaceType, find_popup_root_surface,
        get_popup_toplevel_coords, layer_map_for_output, utils::under_from_surface_tree,
    },
    input::{Seat, SeatState, keyboard::XkbConfig, pointer::CursorImageStatus},
    output::{Output, Scale},
    reexports::{
        wayland_protocols::{
            ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
            xdg::{
                decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
                shell::server::xdg_toplevel,
            },
        },
        wayland_server::{
            DisplayHandle, Resource as _,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_shm, wl_surface::WlSurface},
        },
    },
    utils::{
        Clock, IsAlive as _, Logical, Monotonic, Point, Rectangle, SERIAL_COUNTER, Size, Transform,
    },
    wayland::{
        compositor::{
            BufferAssignment, CompositorClientState, CompositorState, HookId, RectangleKind,
            SurfaceAttributes, TraversalAction, add_pre_commit_hook, get_parent,
            remove_pre_commit_hook, with_states, with_surface_tree_downward,
        },
        cursor_shape::CursorShapeManagerState,
        dmabuf::DmabufState,
        foreign_toplevel_list::ForeignToplevelListState,
        fractional_scale::{FractionalScaleManagerState, with_fractional_scale},
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        keyboard_shortcuts_inhibit::{KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor},
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        pointer_gestures::PointerGesturesState,
        presentation::PresentationState,
        relative_pointer::RelativePointerManagerState,
        security_context::{SecurityContext, SecurityContextState},
        selection::{
            data_device::DataDeviceState,
            ext_data_control::DataControlState as ExtDataControlState,
            primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState as WlrDataControlState,
        },
        session_lock::{LockSurface, SessionLockManagerState},
        shell::{
            wlr_layer::{KeyboardInteractivity, Layer, WlrLayerShellState},
            xdg::{
                PopupSurface, SurfaceCachedState, ToplevelSurface, XdgShellState,
                XdgToplevelSurfaceData, decoration::XdgDecorationState,
            },
        },
        shm::ShmState,
        single_pixel_buffer::SinglePixelBufferState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
        virtual_pointer::VirtualPointerManagerState,
        xdg_activation::{XdgActivationState, XdgActivationToken},
        xdg_toplevel_icon::XdgToplevelIconManager,
    },
};
use tracing::{debug, info, warn};

const TILED_TOPLEVEL_STATES: [xdg_toplevel::State; 4] = [
    xdg_toplevel::State::TiledLeft,
    xdg_toplevel::State::TiledRight,
    xdg_toplevel::State::TiledTop,
    xdg_toplevel::State::TiledBottom,
];

#[cfg(feature = "smithay-tty")]
use crate::backend_tty::TtyBackendState;
use crate::{
    backend_winit::{BACKGROUND_LAYER_TIERS, FOREGROUND_LAYER_TIERS, WinitBackendState},
    config_watcher::{ConfigWatcherHandle, KomaConfigPath},
    cursor::CursorManager,
    desktop_protocols::{ActivationTokenPolicy, DeferredXdgActivation, StoredToplevelIcon},
    dnd::{ActiveDnd, DndIcon},
    ext_workspace::{ExtWorkspaceState, sync_ext_workspaces},
    foreign_toplevel::WlrForeignToplevelState,
    gamma_control::{GammaControlState, GammaLut},
    gesture_dispatcher::GestureTable,
    ipc_server::IpcClientConn,
    keybind_dispatcher::KeybindTable,
    keybind_repeat::RepeatTiming,
    macro_runtime::{ActivePointerAnimation, WorkspaceDamageTracker},
    output_management::WlrOutputManagerState,
    output_power::{OutputPowerMode, OutputPowerState},
    screencopy::ScreencopyState,
    seat_guard::{SeatHandle, SeatHandleGuard},
    security_context::{
        JailedWaylandSocket, NewClient, PeerSocketCredentials, client_can_view_primary_selection,
        client_is_unrestricted, is_surface_from_restricted_client, query_peer_credentials,
        resolve_surface_jailed_workspace,
    },
    xwayland_satellite::SatelliteSlot,
};
use koma_lua::{
    BindHandleId,
    schema::{DeviceOverrideSpec, InputConfigSpec, KeyboardInputSpec, KomaConfigRegistry},
};
use smithay::{
    backend::input::{Event as _, InputBackend, TouchEvent as _},
    input::touch::{
        DownEvent as TouchDownPayload, MotionEvent as TouchMotionPayload, UpEvent as TouchUpPayload,
    },
    wayland::{
        input_method::{
            InputMethodManagerState, InputMethodSeat as _, PopupSurface as ImePopupSurface,
        },
        text_input::{TextInputManagerState, TextInputSeat as _},
    },
};

/// Canonical signature of the active `KeyboardInputSpec` XKB settings for runtime reload diffing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyboardXkbSignature {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: Option<String>,
}

impl From<&KeyboardInputSpec> for KeyboardXkbSignature {
    fn from(spec: &KeyboardInputSpec) -> Self {
        Self {
            rules: spec.rules.clone().unwrap_or_default(),
            model: spec.model.clone().unwrap_or_default(),
            layout: spec.layout.clone().unwrap_or_default(),
            variant: spec.variant.clone().unwrap_or_default(),
            options: spec.options.clone(),
        }
    }
}

/// Fail-closed state machine for `ext-session-lock-v1`.
///
/// Once locked, client disconnect or locker destruction without `unlock_and_destroy` moves
/// the session into [`Self::Abandoned`] rather than unlocking, keeping blackout rendering and
/// input/IPC denials enforced until a new lock client locks and cleanly unlocks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionLockPhase {
    #[default]
    Unlocked,
    Locked(Option<ExtSessionLockV1>),
    Abandoned,
}

/// Coordinates fail-closed session lock state and per-output `LockSurface` handles.
#[derive(Debug, Default)]
pub struct SessionLockCoordinator {
    pub phase: SessionLockPhase,
    pub surfaces: HashMap<Output, LockSurface>,
}

impl SessionLockCoordinator {
    /// Resolves the effective lock phase, detecting a dead `ExtSessionLockV1` even before mutation.
    #[must_use]
    pub fn effective_phase(&self) -> SessionLockPhase {
        match &self.phase {
            SessionLockPhase::Unlocked => SessionLockPhase::Unlocked,
            SessionLockPhase::Abandoned => SessionLockPhase::Abandoned,
            SessionLockPhase::Locked(Some(locker)) if !locker.is_alive() => {
                SessionLockPhase::Abandoned
            }
            SessionLockPhase::Locked(opt) => SessionLockPhase::Locked(opt.clone()),
        }
    }

    #[must_use]
    pub fn is_locked_or_abandoned(&self) -> bool {
        !matches!(self.effective_phase(), SessionLockPhase::Unlocked)
    }

    #[must_use]
    pub fn phase_label(&self) -> &'static str {
        match self.effective_phase() {
            SessionLockPhase::Unlocked => "unlocked",
            SessionLockPhase::Locked(_) => "locked",
            SessionLockPhase::Abandoned => "abandoned",
        }
    }

    /// Transitions `Locked` -> `Abandoned` when the active locker resource/client dies and purges dead surfaces.
    pub fn reconcile_liveness(&mut self) -> bool {
        let before = self.surfaces.len();
        self.surfaces.retain(|_, surf| surf.alive());
        let pruned_surface = self.surfaces.len() != before;

        let became_abandoned = matches!(
            &self.phase,
            SessionLockPhase::Locked(Some(locker)) if !locker.is_alive()
        );
        if became_abandoned {
            self.phase = SessionLockPhase::Abandoned;
            self.surfaces.clear();
        }
        pruned_surface || became_abandoned
    }

    #[must_use]
    pub fn can_accept_new_lock(&mut self) -> bool {
        self.reconcile_liveness();
        matches!(
            self.phase,
            SessionLockPhase::Unlocked | SessionLockPhase::Abandoned
        )
    }

    /// Acquires the lock if currently `Unlocked` or `Abandoned`, rejecting racing second lockers.
    pub fn try_lock(&mut self, locker: Option<ExtSessionLockV1>) -> bool {
        if !self.can_accept_new_lock() {
            return false;
        }
        self.surfaces.clear();
        self.phase = SessionLockPhase::Locked(locker);
        true
    }

    /// Unlocks ONLY when actively `Locked`; refuses unlocking an `Abandoned` session directly.
    pub fn unlock(&mut self) -> bool {
        self.reconcile_liveness();
        if !matches!(self.phase, SessionLockPhase::Locked(_)) {
            return false;
        }
        self.phase = SessionLockPhase::Unlocked;
        self.surfaces.clear();
        true
    }

    /// Explicitly marks an active `Locked` session as `Abandoned` (e.g., on locker drop/crash).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn abandon_active_lock(&mut self) -> bool {
        if matches!(self.phase, SessionLockPhase::Locked(_)) {
            self.phase = SessionLockPhase::Abandoned;
            self.surfaces.clear();
            true
        } else {
            false
        }
    }

    #[must_use]
    pub fn lock_matches_active(&self, locker: &ExtSessionLockV1) -> bool {
        matches!(
            &self.phase,
            SessionLockPhase::Locked(Some(active)) if active == locker && active.is_alive()
        )
    }

    pub fn insert_surface(&mut self, output: Output, surface: LockSurface) {
        self.surfaces.insert(output, surface);
    }

    pub fn remove_wl_surface(&mut self, wl_surface: &WlSurface) -> bool {
        let before = self.surfaces.len();
        self.surfaces
            .retain(|_, surf| surf.wl_surface() != wl_surface);
        self.surfaces.len() != before
    }

    #[must_use]
    pub fn active_lock_surface_for_output(&self, output: &Output) -> Option<&LockSurface> {
        match self.effective_phase() {
            SessionLockPhase::Locked(_) => self.surfaces.get(output).filter(|s| s.alive()),
            SessionLockPhase::Unlocked | SessionLockPhase::Abandoned => None,
        }
    }

    #[must_use]
    pub fn first_active_lock_surface(&self) -> Option<&LockSurface> {
        match self.effective_phase() {
            SessionLockPhase::Locked(_) => self.surfaces.values().find(|s| s.alive()),
            SessionLockPhase::Unlocked | SessionLockPhase::Abandoned => None,
        }
    }
}

/// Canonical front-to-back hit-testing result across lock, layer-shell, and workspace windows.
#[derive(Debug, Clone)]
pub enum PointerHitTarget {
    LockSurface {
        surface: WlSurface,
        origin: Point<f64, Logical>,
    },
    Layer {
        surface: WlSurface,
        origin: Point<f64, Logical>,
        layer_root: WlSurface,
        interactivity: KeyboardInteractivity,
    },
    Window {
        surface: WlSurface,
        origin: Point<f64, Logical>,
        /// Mapped window the surface belongs to, resolved by the same hit test that found it.
        win_id: WindowId,
        /// Workspace the hit window lives on (`special:<name>` when the overlay owns the hit).
        ws_id: WorkspaceId,
    },
}

impl PointerHitTarget {
    #[must_use]
    pub fn surface(&self) -> &WlSurface {
        match self {
            Self::LockSurface { surface, .. }
            | Self::Layer { surface, .. }
            | Self::Window { surface, .. } => surface,
        }
    }

    #[must_use]
    pub fn origin(&self) -> Point<f64, Logical> {
        match self {
            Self::LockSurface { origin, .. }
            | Self::Layer { origin, .. }
            | Self::Window { origin, .. } => *origin,
        }
    }
}

/// Front-to-back scene tiers ranked cleanly from lock surface down to background layers.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ZOrderTier {
    LockSurface = 0,
    OverlayLayer = 1,
    TopLayer = 2,
    FloatingOrSpecialWindow = 3,
    TiledWindow = 4,
    BottomLayer = 5,
    BackgroundLayer = 6,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZOrderCandidate<T> {
    pub tier: ZOrderTier,
    pub rect: LogicalRect,
    pub payload: T,
}

/// Pure front-to-back hit-test resolver honoring Z-order tiers and session lock preemption.
#[cfg_attr(not(test), allow(dead_code))]
#[must_use]
pub fn resolve_z_order_hit<T: Copy>(
    is_locked_or_abandoned: bool,
    pt: LogicalPoint,
    candidates: &[ZOrderCandidate<T>],
) -> Option<(ZOrderTier, T)> {
    candidates
        .iter()
        .filter(|c| {
            if is_locked_or_abandoned {
                c.tier == ZOrderTier::LockSurface && c.rect.contains_point(pt)
            } else {
                c.tier != ZOrderTier::LockSurface && c.rect.contains_point(pt)
            }
        })
        .min_by_key(|c| c.tier)
        .map(|c| (c.tier, c.payload))
}

/// Effective keyboard focus owner ranked cleanly by security and `KeyboardInteractivity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardFocusSource {
    LockSurface,
    LockBlackoutNone,
    ExclusiveLayer,
    OnDemandLayer,
    ActiveWindow,
}

/// Pure keyboard focus priority resolver: SessionLock > Exclusive > OnDemand > ActiveWindow.
#[must_use]
pub const fn resolve_keyboard_focus_source(
    is_locked_or_abandoned: bool,
    has_active_lock_surface: bool,
    has_exclusive_layer: bool,
    has_ondemand_layer: bool,
) -> KeyboardFocusSource {
    if is_locked_or_abandoned {
        if has_active_lock_surface {
            KeyboardFocusSource::LockSurface
        } else {
            KeyboardFocusSource::LockBlackoutNone
        }
    } else if has_exclusive_layer {
        KeyboardFocusSource::ExclusiveLayer
    } else if has_ondemand_layer {
        KeyboardFocusSource::OnDemandLayer
    } else {
        KeyboardFocusSource::ActiveWindow
    }
}

/// Converts Koma's pure domain `OutputTransform` into Smithay's `Transform`.
#[must_use]
pub const fn koma_transform_to_smithay(t: OutputTransform) -> Transform {
    match t {
        OutputTransform::Normal => Transform::Normal,
        OutputTransform::Rotate90 => Transform::_90,
        OutputTransform::Rotate180 => Transform::_180,
        OutputTransform::Rotate270 => Transform::_270,
        OutputTransform::Flipped => Transform::Flipped,
        OutputTransform::Flipped90 => Transform::Flipped90,
        OutputTransform::Flipped180 => Transform::Flipped180,
        OutputTransform::Flipped270 => Transform::Flipped270,
    }
}

/// Converts Smithay's `Transform` into Koma's pure domain `OutputTransform`.
#[must_use]
pub const fn smithay_transform_to_koma(t: Transform) -> OutputTransform {
    match t {
        Transform::Normal => OutputTransform::Normal,
        Transform::_90 => OutputTransform::Rotate90,
        Transform::_180 => OutputTransform::Rotate180,
        Transform::_270 => OutputTransform::Rotate270,
        Transform::Flipped => OutputTransform::Flipped,
        Transform::Flipped90 => OutputTransform::Flipped90,
        Transform::Flipped180 => OutputTransform::Flipped180,
        Transform::Flipped270 => OutputTransform::Flipped270,
    }
}

/// Computes logical output width, height, and scale from a Smithay `Output`.
#[must_use]
pub fn output_logical_viewport(output: &Output) -> (u32, u32, f64) {
    let scale = output.current_scale().fractional_scale().max(0.01);
    let (w, h) = output
        .current_mode()
        .map(|mode| {
            let phys_transformed = output.current_transform().transform_size(mode.size);
            let logical = phys_transformed
                .to_f64()
                .to_logical(scale)
                .to_i32_round::<i32>();
            (logical.w.max(64) as u32, logical.h.max(64) as u32)
        })
        .unwrap_or((1920, 1080));
    (w, h, scale)
}

/// Checks whether a surface currently holds a committed buffer in Smithay's renderer state.
///
/// Must be queried after `on_commit_buffer_handler`, which consumes `SurfaceAttributes::buffer`.
#[must_use]
pub fn is_surface_mapped(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

/// Returns `true` when `surface` has an explicitly committed empty `wl_region` input mask
/// (`wl_surface.set_input_region`), allowing pointer clicks to pass through (`Waydroid` container).
#[must_use]
pub fn surface_has_empty_input_region(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceAttributes>()
            .current()
            .input_region
            .as_ref()
            .is_some_and(|region| {
                !region.rects.iter().any(|(kind, rect)| {
                    matches!(kind, RectangleKind::Add) && rect.size.w > 0 && rect.size.h > 0
                })
            })
    })
}

fn surfaces_share_root(a: &WlSurface, b: &WlSurface) -> bool {
    if a == b {
        return true;
    }
    let mut ra = a.clone();
    while let Some(parent) = get_parent(&ra) {
        ra = parent;
    }
    let mut rb = b.clone();
    while let Some(parent) = get_parent(&rb) {
        rb = parent;
    }
    ra == rb
}

/// Active Ring 0 display & input backend (`niri/src/backend/mod.rs` `Backend` enum).
#[allow(clippy::large_enum_variant)] // Stored once on `KomaState`; avoids boxing churn across backend paths.
pub enum ActiveBackend {
    Winit(WinitBackendState),
    #[cfg(feature = "smithay-tty")]
    Tty(TtyBackendState),
}

impl ActiveBackend {
    /// Returns the primary physical/nested `smithay::output::Output` for layer-shell & scale queries.
    #[must_use]
    pub fn primary_output(&self) -> Option<Output> {
        match self {
            Self::Winit(w) => Some(w.output.clone()),
            #[cfg(feature = "smithay-tty")]
            Self::Tty(tty) => tty.primary_output(),
        }
    }

    /// Returns all active physical/nested `Output`s.
    #[must_use]
    pub fn all_outputs(&self) -> Vec<Output> {
        match self {
            Self::Winit(w) => vec![w.output.clone()],
            #[cfg(feature = "smithay-tty")]
            Self::Tty(tty) => tty.all_outputs(),
        }
    }

    /// Borrows the primary GPU's `GlesRenderer` for offscreen `LazyFbo` `SCM_RIGHTS` capture.
    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer) -> T,
    ) -> Option<T> {
        match self {
            Self::Winit(w) => Some(f(w.backend.renderer())),
            #[cfg(feature = "smithay-tty")]
            Self::Tty(tty) => tty.with_primary_renderer(f),
        }
    }

    /// Imports a client `zwp_linux_dmabuf_v1` buffer into the active GPU renderer.
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        match self {
            Self::Winit(w) => w.backend.renderer().import_dmabuf(dmabuf, None).is_ok(),
            #[cfg(feature = "smithay-tty")]
            Self::Tty(tty) => tty.import_dmabuf(dmabuf),
        }
    }

    /// Schedules a frame redraw on the active display backend.
    pub fn request_redraw(&mut self) {
        match self {
            Self::Winit(w) => w.request_redraw(),
            #[cfg(feature = "smithay-tty")]
            Self::Tty(tty) => tty.queue_redraw_all(),
        }
    }
}

/// Per-Wayland-client security & compositor state stored inside `wayland-server`.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub restricted: bool,
    // Security/audit client metadata populated at Wayland accept & security-context attach.
    #[allow(dead_code)]
    pub credentials_unknown: bool,
    pub primary_selection_disabled: bool,
    #[allow(dead_code)]
    pub peer_creds: Option<PeerSocketCredentials>,
    #[allow(dead_code)]
    pub security_context: Option<SecurityContext>,
    pub jailed_workspace: Option<WorkspaceId>,
    #[allow(dead_code)]
    pub sandbox_label: Option<String>,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Newly created `xdg_toplevel` awaiting initial rule resolution & first buffer commit.
pub struct UnmappedToplevel {
    pub win_id: WindowId,
    pub window: Window,
    pub initial_configure_sent: bool,
    pub decoration_mode: DecorationMode,
    pub icon: Option<StoredToplevelIcon>,
}

/// Active mapped Wayland window participating in `DwindleTree`, `ScrollingStrip`, `DeckStashStack`,
/// or `FloatingStack`.
pub struct MappedWindow {
    pub win_id: WindowId,
    pub ws_id: WorkspaceId,
    pub window: Window,
    pub app_id: String,
    pub title: String,
    pub is_floating: bool,
    pub is_minimized: bool,
    pub is_sandboxed: bool,
    pub preferred_size: (u32, u32),
    pub rect: LogicalRect,
    pub decoration_mode: DecorationMode,
    pub icon: Option<StoredToplevelIcon>,
}

/// A toplevel that left the layout while its `windows_close` spring is still settling: only its
/// captured pixels linger, never its surface, layout slot, focus or IPC presence.
pub struct ClosingWindow {
    pub win_id: WindowId,
    pub ws_id: WorkspaceId,
    /// Last captured frame of the destroyed surface, uploaded through `ImportMem` at draw time.
    pub snapshot: MemoryRenderBuffer,
    /// Pixel dimensions of `snapshot` (its buffer scale is `1`).
    pub snapshot_size: (i32, i32),
}

/// Layer-shell tiers composited for one output frame, split around the window scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLayerTiers {
    /// Tiers drawn in front of every toplevel (empty while a window is fullscreen).
    pub above_toplevels: &'static [Layer],
    /// Tiers drawn behind every toplevel (always composited).
    pub below_toplevels: &'static [Layer],
}

/// Per-workspace virtual/agent cursor runtime decoupled from physical human `pointer_location`.
#[derive(Debug, Clone)]
pub struct AgentCursorRuntime {
    pub ws_local_pos: LogicalPoint,
    /// Resting normalized window anchor `(win_id, u, v)` so scroll/re-tile keeps the virtual
    /// pointer attached to its target surface.
    pub anchored_window: Option<(WindowId, f64, f64)>,
    pub visual: AgentCursorVisualDynamics,
}

impl Default for AgentCursorRuntime {
    fn default() -> Self {
        Self::new(LogicalPoint { x: 0.0, y: 0.0 })
    }
}

impl AgentCursorRuntime {
    #[must_use]
    pub fn new(initial_pos: LogicalPoint) -> Self {
        Self {
            ws_local_pos: initial_pos,
            anchored_window: None,
            visual: AgentCursorVisualDynamics::new(),
        }
    }
}

pub struct KomaState {
    pub start_time: Instant,
    pub clock: Clock<Monotonic>,
    pub presentation_seq: AtomicU64,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub loop_handle: Option<LoopHandle<'static, Self>>,
    pub trampoline_timer_token: Option<RegistrationToken>,
    /// One-shot timer driving repeat-while-held keybinds (`komawm/koma#76`).
    pub keybind_repeat_timer_token: Option<RegistrationToken>,
    pub socket_name: OsString,
    pub instance_signature: Option<String>,
    pub primary_satellite: Option<SatelliteSlot>,
    pub workspace_satellites: HashMap<WorkspaceId, SatelliteSlot>,
    pub jailed_wayland_sockets: HashMap<WorkspaceId, JailedWaylandSocket>,

    // Smithay Wayland protocol managers (privileged globals gated by `client_is_unrestricted`)
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub session_lock_state: SessionLockManagerState,
    pub session_lock: SessionLockCoordinator,
    pub focused_ondemand_layer: Option<WlSurface>,
    // Preview capture subscription ledger retained for IPC/overview lifecycle.
    #[allow(dead_code)]
    pub preview_refcounts: HashMap<WorkspaceId, u32>,
    // RAII Wayland global/protocol state handles kept alive on `KomaState`.
    #[allow(dead_code)]
    pub shm_state: ShmState,
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub wlr_data_control_state: WlrDataControlState,
    pub ext_data_control_state: ExtDataControlState,
    #[allow(dead_code)]
    pub security_context_state: SecurityContextState,
    #[allow(dead_code)]
    pub virtual_keyboard_state: VirtualKeyboardManagerState,
    #[allow(dead_code)]
    pub pointer_gestures_state: PointerGesturesState,
    #[allow(dead_code)]
    pub cursor_shape_manager_state: CursorShapeManagerState,
    #[allow(dead_code)]
    pub dmabuf_state: DmabufState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,
    #[allow(dead_code)]
    pub text_input_state: TextInputManagerState,
    #[allow(dead_code)]
    pub input_method_state: InputMethodManagerState,
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    #[allow(dead_code)]
    pub xdg_toplevel_icon_manager: XdgToplevelIconManager,
    pub xdg_activation_state: XdgActivationState,
    pub xdg_activation_tokens: HashMap<XdgActivationToken, ActivationTokenPolicy>,
    pub deferred_xdg_activations: Vec<DeferredXdgActivation>,
    pub foreign_toplevel_list_state: ForeignToplevelListState,
    pub wlr_foreign_toplevel_state: WlrForeignToplevelState,
    pub ext_workspace_state: ExtWorkspaceState,
    pub idle_notifier_state: IdleNotifierState<Self>,
    #[allow(dead_code)]
    pub idle_inhibit_state: IdleInhibitManagerState,
    pub idle_inhibitors: HashSet<WlSurface>,
    #[allow(dead_code)]
    pub pointer_constraints_state: PointerConstraintsState,
    #[allow(dead_code)]
    pub relative_pointer_manager_state: RelativePointerManagerState,
    #[allow(dead_code)]
    pub single_pixel_buffer_state: SinglePixelBufferState,
    #[allow(dead_code)]
    pub presentation_state: PresentationState,
    #[allow(dead_code)]
    pub virtual_pointer_state: VirtualPointerManagerState,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    pub active_shortcuts_inhibitors: Vec<KeyboardShortcutsInhibitor>,
    pub screencopy_state: ScreencopyState,
    pub output_power_state: OutputPowerState,
    pub wlr_output_manager_state: WlrOutputManagerState,
    pub gamma_control_state: GammaControlState,
    pub popups: PopupManager,
    pub ime_popups: Vec<ImePopupSurface>,
    pub applied_keyboard_xkb: Option<KeyboardXkbSignature>,
    pub applied_keyboard_repeat: (i32, i32),
    pub applied_startup_numlock: Option<bool>,
    pub virtual_keyboard_keymap_active: bool,
    /// Resolved `koma.input({ pointer = { follow_mouse = ... } })` policy, refreshed on config apply.
    pub follow_mouse_mode: FollowMouseMode,
    /// Window that owns pointer-transferred keyboard focus in [`FollowMouseMode::FocusOnly`].
    pub pointer_focus_window: Option<WindowId>,

    // Physical/nested host seat (`seat0` / `libseat`)
    pub seat: Seat<Self>,
    pub pointer_location: Point<f64, Logical>,
    pub cursor_manager: CursorManager,
    pub cursor_anim_timer_token: Option<RegistrationToken>,
    /// Active client `wl_data_device` drag-and-drop grab, including its optional icon surface.
    pub dnd_drag: Option<ActiveDnd>,

    // Koma pure domain model (`koma-model` + `koma-lua` + `koma-seat`)
    pub output_layout: OutputLayout,
    pub output_workspaces: HashMap<String, WorkspaceId>,
    pub focused_output_name: Option<String>,
    pub output_safe_areas: HashMap<String, SafeAreaInsets>,
    pub active_ws_id: WorkspaceId,
    /// Optional `special:<name>` Scratchpad overlay workspace currently visible above `active_ws_id`!
    pub active_special_ws_id: Option<WorkspaceId>,
    pub workspaces: HashMap<WorkspaceId, WorkspaceState>,
    pub vseats: HashMap<SeatId, VirtualSeatLedger>,
    pub rules: WindowRuleEngine,
    pub keybinds: KeybindTable,
    pub gestures: GestureTable,
    pub pending_passthrough_actions: Vec<ActionDescriptor>,
    pub lua_scheduler: TrampolineScheduler,
    pub lua_sandbox: PhodopusSandbox,
    pub deferred_lua_sandbox: Option<PhodopusSandbox>,
    pub config_path: KomaConfigPath,
    pub config_watcher: Option<ConfigWatcherHandle>,
    pub config_generation: u64,
    pub config_reload_failures: u64,
    pub config_error_banner: Option<String>,
    pub damage_trackers: HashMap<WorkspaceId, WorkspaceDamageTracker>,
    pub pointer_animations: Vec<ActivePointerAnimation>,
    pub agent_cursors: HashMap<WorkspaceId, AgentCursorRuntime>,
    pub window_animations: RefCell<WindowAnimationTable>,
    /// Windows whose surface is gone but whose captured frame is still fading out.
    pub closing_windows: Vec<ClosingWindow>,
    /// On-demand unmap/destroy frame snapshots keyed by `WindowId` for `windows_close`.
    /// Active mapped windows keep no entry here; snapshots are populated only right before a
    /// closing surface loses its buffers and are consumed in [`Self::begin_close_animation`].
    pub window_frames: HashMap<WindowId, WindowFrameCache>,
    /// Per-toplevel root `wl_surface` pre-commit hooks for capturing closing snapshots right
    /// before an explicit unmap (`attach(null) + commit`) discards the surface's buffer.
    pub toplevel_pre_commit_hooks: HashMap<WlSurface, HookId>,
    /// Last known root `wl_surface` for child `wl_subsurface`s, retained across
    /// `wl_subsurface.destroy` until `wl_surface.destroyed` so subsurface teardown can still
    /// locate its parent toplevel.
    pub subsurface_roots: HashMap<WlSurface, WlSurface>,
    #[cfg(test)]
    pub(crate) unmap_snapshot_attempts: u64,
    #[cfg(test)]
    pub(crate) last_snapshot_had_live_buffer: bool,
    pub workspace_transitions: RefCell<WorkspaceTransitionTable>,
    pub last_notified_focus_win_id: Option<WindowId>,
    /// Live 4-quadrant `smart_split` / `Scrolling` column drop zone `(ws_id, preview_rect)` during drag.
    pub active_drop_preview: Option<(WorkspaceId, LogicalRect)>,
    /// Active `MoveWindowGrab` cursor state `(ws_id, cursor_x)` driving continuous VBlank edge auto-scroll.
    pub active_drag_edge_scroll: Option<(WorkspaceId, f64)>,
    pub edge_scroll_keep_alive: bool,
    /// Frame edge currently hovered for interactive resize: `(win_id, corner)` plus the cursor
    /// status to restore once the pointer leaves the edge (`Hyprland` border-drag parity).
    pub frame_edge_hover: Option<(WindowId, ResizeCorner)>,
    /// Cursor status captured when `frame_edge_hover` was entered, restored on exit.
    pub cursor_before_frame_edge: Option<CursorImageStatus>,
    pub backdrop_blur_cache: RefCell<CachedBackdropBlur>,
    pub fbo_pool: LazyFboPool,
    pub offscreen_surfaces: HashMap<WorkspaceId, LazyOffscreenSurface>,
    pub token_store: HashMap<CapabilityToken, TokenClaims>,
    next_ipc_conn_id: u64,
    pub ipc_clients: HashMap<u64, IpcClientConn>,

    // Two-phase window lifecycle tables
    next_win_slot: u64,
    pub unmapped_windows: HashMap<WlSurface, UnmappedToplevel>,
    pub mapped_windows: Vec<MappedWindow>,

    // Unified display & input backend (`Winit` nested or `Tty` bare-metal KMS/DRM)
    pub backend: Option<ActiveBackend>,
    #[cfg(test)]
    pub test_outputs: Vec<Output>,
    pub redraw_queued: bool,
}

impl KomaState {
    pub fn new(
        display_handle: DisplayHandle,
        loop_signal: LoopSignal,
        loop_handle: LoopHandle<'static, Self>,
        socket_name: OsString,
        seat_name: &str,
        initial_viewport: LogicalViewport,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);

        let layer_shell_state =
            WlrLayerShellState::new_with_filter::<Self, _>(&display_handle, client_is_unrestricted);
        let session_lock_state =
            SessionLockManagerState::new::<Self, _>(&display_handle, client_is_unrestricted);

        let shm_state = ShmState::new::<Self>(
            &display_handle,
            vec![wl_shm::Format::Abgr8888, wl_shm::Format::Xbgr8888],
        );
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);

        let primary_selection_state = PrimarySelectionState::new_with_filter::<Self, _>(
            &display_handle,
            client_can_view_primary_selection,
        );
        let wlr_data_control_state = WlrDataControlState::new::<Self, _>(
            &display_handle,
            Some(&primary_selection_state),
            client_is_unrestricted,
        );
        let ext_data_control_state = ExtDataControlState::new::<Self, _>(
            &display_handle,
            Some(&primary_selection_state),
            client_is_unrestricted,
        );
        let security_context_state =
            SecurityContextState::new::<Self, _>(&display_handle, client_is_unrestricted);
        let virtual_keyboard_state =
            VirtualKeyboardManagerState::new::<Self, _>(&display_handle, client_is_unrestricted);
        let text_input_state = TextInputManagerState::new::<Self>(&display_handle);
        let input_method_state =
            InputMethodManagerState::new::<Self, _>(&display_handle, client_is_unrestricted);

        let pointer_gestures_state = PointerGesturesState::new::<Self>(&display_handle);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&display_handle);
        let dmabuf_state = DmabufState::new();
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let mut xdg_toplevel_icon_manager = XdgToplevelIconManager::new::<Self>(&display_handle);
        xdg_toplevel_icon_manager.replace_icon_sizes([16, 24, 32, 48, 64]);
        let xdg_activation_state = XdgActivationState::new::<Self>(&display_handle);
        let foreign_toplevel_list_state = ForeignToplevelListState::new_with_filter::<Self>(
            &display_handle,
            client_is_unrestricted,
        );
        let wlr_foreign_toplevel_state = WlrForeignToplevelState::new(&display_handle);
        let ext_workspace_state = ExtWorkspaceState::new(&display_handle);
        let idle_notifier_state = IdleNotifierState::new(&display_handle, loop_handle.clone());
        let idle_inhibit_state = IdleInhibitManagerState::new::<Self>(&display_handle);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);
        let relative_pointer_manager_state =
            RelativePointerManagerState::new::<Self>(&display_handle);
        let clock = Clock::<Monotonic>::new();
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&display_handle);
        let presentation_state = PresentationState::new::<Self>(&display_handle, clock.id() as u32);
        let virtual_pointer_state =
            VirtualPointerManagerState::new::<Self, _>(&display_handle, client_is_unrestricted);
        let keyboard_shortcuts_inhibit_state =
            KeyboardShortcutsInhibitState::new::<Self>(&display_handle);
        let screencopy_state = ScreencopyState::new(&display_handle);
        let output_power_state = OutputPowerState::new(&display_handle);
        let wlr_output_manager_state = WlrOutputManagerState::new(&display_handle);
        let gamma_control_state = GammaControlState::new(&display_handle);

        let mut seat = seat_state.new_wl_seat(&display_handle, seat_name);
        seat.add_keyboard(
            XkbConfig::default(),
            koma_lua::schema::DEFAULT_KEYBOARD_REPEAT_DELAY_MS,
            koma_lua::schema::DEFAULT_KEYBOARD_REPEAT_RATE_HZ,
        )
        .expect("failed to initialize XKB keyboard on host seat");
        seat.add_pointer();
        seat.add_touch();

        // Human `Headed` workspaces default to `registry.default_layout` (`LayoutMode::Dwindle`)!
        let lua_sandbox = PhodopusSandbox::default();
        let initial_reg = lua_sandbox.config_snapshot();
        let active_ws_id = WorkspaceId::from("ws_01");
        let vseat_id = SeatId::from("vseat_01");
        let ws = WorkspaceState::new(
            active_ws_id.clone(),
            "1",
            vseat_id.clone(),
            initial_viewport,
            PresentationTarget::Physical(OutputId::from("primary")),
            initial_reg.default_layout,
            0,
        );

        let mut workspaces = HashMap::new();
        workspaces.insert(active_ws_id.clone(), ws);

        let mut vseats = HashMap::new();
        vseats.insert(vseat_id.clone(), VirtualSeatLedger::new(vseat_id));

        Self {
            start_time: Instant::now(),
            clock,
            presentation_seq: AtomicU64::new(0),
            display_handle,
            loop_signal,
            loop_handle: Some(loop_handle),
            trampoline_timer_token: None,
            keybind_repeat_timer_token: None,
            socket_name,
            instance_signature: None,
            primary_satellite: None,
            workspace_satellites: HashMap::new(),
            jailed_wayland_sockets: HashMap::new(),
            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            session_lock_state,
            session_lock: SessionLockCoordinator::default(),
            focused_ondemand_layer: None,
            preview_refcounts: HashMap::new(),
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            wlr_data_control_state,
            ext_data_control_state,
            security_context_state,
            virtual_keyboard_state,
            pointer_gestures_state,
            cursor_shape_manager_state,
            dmabuf_state,
            viewporter_state,
            fractional_scale_state,
            text_input_state,
            input_method_state,
            xdg_decoration_state,
            xdg_toplevel_icon_manager,
            xdg_activation_state,
            xdg_activation_tokens: HashMap::new(),
            deferred_xdg_activations: Vec::new(),
            foreign_toplevel_list_state,
            wlr_foreign_toplevel_state,
            ext_workspace_state,
            idle_notifier_state,
            idle_inhibit_state,
            idle_inhibitors: HashSet::new(),
            pointer_constraints_state,
            relative_pointer_manager_state,
            single_pixel_buffer_state,
            presentation_state,
            virtual_pointer_state,
            keyboard_shortcuts_inhibit_state,
            active_shortcuts_inhibitors: Vec::new(),
            screencopy_state,
            output_power_state,
            wlr_output_manager_state,
            gamma_control_state,
            popups: PopupManager::default(),
            ime_popups: Vec::new(),
            applied_keyboard_xkb: Some(KeyboardXkbSignature::default()),
            applied_keyboard_repeat: (
                koma_lua::schema::DEFAULT_KEYBOARD_REPEAT_DELAY_MS,
                koma_lua::schema::DEFAULT_KEYBOARD_REPEAT_RATE_HZ,
            ),
            applied_startup_numlock: None,
            virtual_keyboard_keymap_active: false,
            follow_mouse_mode: FollowMouseMode::default(),
            pointer_focus_window: None,
            seat,
            pointer_location: Point::from((
                f64::from(initial_viewport.width) * 0.5,
                f64::from(initial_viewport.height) * 0.5,
            )),
            cursor_manager: CursorManager::new(),
            cursor_anim_timer_token: None,
            dnd_drag: None,
            output_layout: OutputLayout::new(),
            output_workspaces: HashMap::new(),
            focused_output_name: None,
            output_safe_areas: HashMap::new(),
            active_ws_id,
            active_special_ws_id: None,
            workspaces,
            vseats,
            rules: WindowRuleEngine::new(),
            keybinds: KeybindTable::default(),
            gestures: GestureTable::default(),
            pending_passthrough_actions: Vec::new(),
            lua_scheduler: TrampolineScheduler::default(),
            lua_sandbox,
            deferred_lua_sandbox: None,
            config_path: KomaConfigPath::resolve(None),
            config_watcher: None,
            config_generation: 0,
            config_reload_failures: 0,
            config_error_banner: None,
            damage_trackers: HashMap::new(),
            pointer_animations: Vec::new(),
            agent_cursors: HashMap::new(),
            window_animations: RefCell::new(WindowAnimationTable::default()),
            closing_windows: Vec::new(),
            window_frames: HashMap::new(),
            toplevel_pre_commit_hooks: HashMap::new(),
            subsurface_roots: HashMap::new(),
            #[cfg(test)]
            unmap_snapshot_attempts: 0,
            #[cfg(test)]
            last_snapshot_had_live_buffer: false,
            workspace_transitions: RefCell::new(WorkspaceTransitionTable::default()),
            last_notified_focus_win_id: None,
            active_drop_preview: None,
            active_drag_edge_scroll: None,
            edge_scroll_keep_alive: false,
            frame_edge_hover: None,
            cursor_before_frame_edge: None,
            backdrop_blur_cache: RefCell::new(CachedBackdropBlur::default()),
            fbo_pool: LazyFboPool::default(),
            offscreen_surfaces: HashMap::new(),
            token_store: HashMap::new(),
            next_ipc_conn_id: 1,
            ipc_clients: HashMap::new(),
            next_win_slot: 1,
            unmapped_windows: HashMap::new(),
            mapped_windows: Vec::new(),
            backend: None,
            #[cfg(test)]
            test_outputs: Vec::new(),
            redraw_queued: false,
        }
    }

    /// Returns the workspace currently receiving human keyboard & spawn actions (`special:<name>`
    /// Scratchpad when open on top, otherwise `active_ws_id`).
    #[must_use]
    pub fn effective_interactive_ws_id(&self) -> WorkspaceId {
        self.active_special_ws_id
            .clone()
            .unwrap_or_else(|| self.active_ws_id.clone())
    }

    /// Finds or lazily creates a `special:<name>` Scratchpad overlay workspace matching the
    /// current physical output viewport (`Hyprland` `togglespecialworkspace` / `special:magic`).
    pub fn ensure_special_workspace(&mut self, raw_name: Option<&str>) -> WorkspaceId {
        let canonical_name = WorkspaceState::normalize_special_name(raw_name);
        if let Some((existing_id, _)) = self
            .workspaces
            .iter()
            .find(|(_, ws)| ws.name == canonical_name)
        {
            return existing_id.clone();
        }

        let next_idx = self.workspaces.len() + 1;
        let ws_id = WorkspaceId::new(format!("ws_{next_idx:02}"));
        let vseat_id = SeatId::new(format!("vseat_{next_idx:02}"));
        let viewport = self
            .workspaces
            .get(&self.active_ws_id)
            .map(|ws| ws.viewport)
            .unwrap_or_default();
        let now_ms = self.elapsed_ms();
        let registry = self.lua_sandbox.config_snapshot();
        let matching_spec = registry
            .workspaces
            .iter()
            .find(|s| s.name == canonical_name);
        let layout_mode = matching_spec
            .map(|s| s.layout)
            .unwrap_or(registry.default_layout);

        let mut ws = WorkspaceState::new(
            ws_id.clone(),
            canonical_name,
            vseat_id.clone(),
            viewport,
            PresentationTarget::Physical(OutputId::from("primary")),
            layout_mode,
            now_ms,
        );
        let mut d_cfg = registry.dwindle;
        let mut s_cfg = registry.scrolling.clone();
        if let Some(spec) = matching_spec {
            let inner = spec.effective_inner_gap();
            let outer = spec.effective_outer_gap().max(48);
            d_cfg.inner_gap = inner;
            d_cfg.outer_gap = outer;
            s_cfg.inner_gap = inner;
            s_cfg.outer_gap = outer;
        } else {
            d_cfg.inner_gap = 10;
            d_cfg.outer_gap = d_cfg.outer_gap.max(48);
            s_cfg.inner_gap = 10;
            s_cfg.outer_gap = s_cfg.outer_gap.max(48);
        }
        ws.dwindle.config = d_cfg;
        ws.scrolling.update_config(s_cfg, viewport, now_ms);
        // An empty `special:` overlay is hidden from bars unless it opts in, like any other
        // workspace; scratchpads are created on first use and stay listed while they hold windows.
        ws.persistent = matching_spec.is_some_and(|s| s.persistent);

        self.vseats
            .insert(vseat_id.clone(), VirtualSeatLedger::new(vseat_id));
        self.workspaces.insert(ws_id.clone(), ws);
        sync_ext_workspaces(self);
        ws_id
    }

    /// Resolves `query` to an existing workspace, matching exact workspace ids before names.
    ///
    /// A workspace may be *named* `ws_01` while the real workspace with id `ws_01` is a different
    /// workspace, so id precedence must be explicit: a single `id == query || name == query` scan
    /// over a `HashMap` would pick whichever entry iterated first.
    #[must_use]
    pub fn find_workspace_by_id_or_name(&self, query: &str) -> Option<WorkspaceId> {
        if let Some((id, _)) = self.workspaces.iter().find(|(id, _)| id.as_str() == query) {
            return Some(id.clone());
        }
        self.workspaces
            .iter()
            .find(|(_, ws)| ws.name == query)
            .map(|(id, _)| id.clone())
    }

    /// Resolves an existing workspace by `WorkspaceId` or `name`, or lazily creates a new workspace
    /// applying `registry.default_layout` and any matching `WorkspaceSpec` (`layout`, `gaps`).
    pub fn ensure_workspace_by_name_or_id(&mut self, query: &str) -> WorkspaceId {
        if query.starts_with(koma_model::SPECIAL_WORKSPACE_PREFIX) {
            return self.ensure_special_workspace(Some(query));
        }
        if let Some(existing_id) = self.find_workspace_by_id_or_name(query) {
            return existing_id;
        }

        let next_idx = self.workspaces.len() + 1;
        let ws_id = WorkspaceId::new(format!("ws_{next_idx:02}"));
        let vseat_id = SeatId::new(format!("vseat_{next_idx:02}"));
        let fallback_viewport = self
            .workspaces
            .get(&self.active_ws_id)
            .map(|ws| ws.viewport)
            .unwrap_or_default();
        let now_ms = self.elapsed_ms();
        let registry = self.lua_sandbox.config_snapshot();
        let matching_spec = registry.workspaces.iter().find(|s| s.name == query);

        let is_headless =
            matching_spec.is_some_and(|s| s.presentation.eq_ignore_ascii_case("headless"));
        let viewport = matching_spec
            .filter(|_s| is_headless)
            .map(|s| s.viewport)
            .unwrap_or(fallback_viewport);
        let layout_mode = if is_headless {
            LayoutMode::Deck
        } else {
            matching_spec
                .map(|s| s.layout)
                .unwrap_or(registry.default_layout)
        };
        let presentation = if is_headless {
            let slot = self.fbo_pool.acquire_or_borrow(&ws_id);
            self.offscreen_surfaces.insert(
                ws_id.clone(),
                LazyOffscreenSurface::new(
                    viewport.width as i32,
                    viewport.height as i32,
                    viewport.scale,
                ),
            );
            PresentationTarget::Offscreen {
                lazy_fbo_slot: slot,
            }
        } else {
            let out_id = matching_spec
                .and_then(|s| s.preferred_output.clone())
                .unwrap_or_else(|| OutputId::from("primary"));
            PresentationTarget::Physical(out_id)
        };

        let mut ws = WorkspaceState::new(
            ws_id.clone(),
            query,
            vseat_id.clone(),
            viewport,
            presentation,
            layout_mode,
            now_ms,
        );
        let mut d_cfg = registry.dwindle;
        let mut s_cfg = registry.scrolling.clone();
        if let Some(spec) = matching_spec
            && (spec.has_explicit_gaps || spec.gaps_in.is_some() || spec.gaps_out.is_some())
        {
            d_cfg.inner_gap = spec.effective_inner_gap();
            d_cfg.outer_gap = spec.effective_outer_gap();
            s_cfg.inner_gap = spec.effective_inner_gap();
            s_cfg.outer_gap = spec.effective_outer_gap();
        }
        ws.dwindle.config = d_cfg;
        ws.scrolling.update_config(s_cfg, viewport, now_ms);
        // `persistent = true` is the opt-out from hiding empty workspaces in `ext-workspace-v1`
        // bars; it must be applied at creation so a never-used workspace stays listed.
        ws.persistent = matching_spec.is_some_and(|s| s.persistent);

        self.vseats
            .insert(vseat_id.clone(), VirtualSeatLedger::new(vseat_id));
        self.workspaces.insert(ws_id.clone(), ws);
        sync_ext_workspaces(self);
        ws_id
    }

    /// Inserts a newly accepted Wayland `UnixStream` into `wayland-server` with its resolved
    /// `ClientState` security policy.
    pub fn insert_wayland_client(&mut self, new_client: NewClient) {
        let NewClient {
            client,
            restricted,
            credentials_unknown,
            security_context,
            jailed_workspace,
            sandbox_label,
        } = new_client;

        let peer_creds = if credentials_unknown {
            None
        } else {
            query_peer_credentials(&client)
        };

        debug!(
            restricted,
            ?peer_creds,
            ?security_context,
            ?jailed_workspace,
            ?sandbox_label,
            "inserting Wayland client into display_handle"
        );

        let data = Arc::new(ClientState {
            compositor_state: CompositorClientState::default(),
            restricted,
            credentials_unknown,
            primary_selection_disabled: restricted,
            peer_creds,
            security_context,
            jailed_workspace,
            sandbox_label,
        });

        if let Err(err) = self.display_handle.insert_client(client, data) {
            warn!("failed inserting Wayland client: {err}");
        }
    }

    #[must_use]
    pub fn primary_output(&self) -> Option<Output> {
        #[cfg(test)]
        if self.backend.is_none() && !self.test_outputs.is_empty() {
            return self.test_outputs.first().cloned();
        }
        self.backend
            .as_ref()
            .and_then(ActiveBackend::primary_output)
    }

    #[must_use]
    pub fn all_outputs(&self) -> Vec<Output> {
        #[cfg(test)]
        if self.backend.is_none() && !self.test_outputs.is_empty() {
            return self.test_outputs.clone();
        }
        self.backend
            .as_ref()
            .map(ActiveBackend::all_outputs)
            .unwrap_or_default()
    }

    /// Returns the currently focused physical `Output`, falling back to primary or first output.
    #[must_use]
    pub fn focused_output(&self) -> Option<Output> {
        let outputs = self.all_outputs();
        if let Some(ref focused_name) = self.focused_output_name
            && let Some(out) = outputs.iter().find(|o| o.name() == *focused_name)
        {
            return Some(out.clone());
        }
        self.primary_output().or_else(|| outputs.into_iter().next())
    }

    #[must_use]
    pub fn focused_output_name(&self) -> Option<String> {
        self.focused_output().map(|o| o.name())
    }

    #[must_use]
    pub fn output_safe_area(&self, output_name: &str) -> SafeAreaInsets {
        self.output_safe_areas
            .get(output_name)
            .copied()
            .unwrap_or_default()
            .clamped_non_negative()
    }

    pub fn set_output_safe_area(
        &mut self,
        output_name: impl Into<String>,
        safe_area: SafeAreaInsets,
    ) {
        self.output_safe_areas
            .insert(output_name.into(), safe_area.clamped_non_negative());
    }

    #[must_use]
    pub fn active_workspace_for_output(&self, output_name: &str) -> WorkspaceId {
        self.output_workspaces
            .get(output_name)
            .cloned()
            .unwrap_or_else(|| self.active_ws_id.clone())
    }

    #[must_use]
    pub fn output_for_workspace(&self, ws_id: &WorkspaceId) -> Option<Output> {
        let outputs = self.all_outputs();
        if let Some((out_name, _)) = self
            .output_workspaces
            .iter()
            .find(|(_, assigned)| *assigned == ws_id)
            && let Some(out) = outputs.iter().find(|o| o.name() == *out_name)
        {
            return Some(out.clone());
        }
        if let Some(ws) = self.workspaces.get(ws_id) {
            if ws.is_special {
                return self.focused_output();
            }
            if let PresentationTarget::Physical(ref out_id) = ws.presentation {
                if out_id.as_str() == "primary" {
                    return self.primary_output().or_else(|| outputs.into_iter().next());
                }
                if let Some(out) = outputs.iter().find(|o| o.name() == out_id.as_str()) {
                    return Some(out.clone());
                }
            }
        }
        if *ws_id == self.active_ws_id {
            self.focused_output()
        } else {
            None
        }
    }

    #[must_use]
    pub fn workspace_output_origin(&self, ws_id: &WorkspaceId) -> Point<f64, Logical> {
        self.output_for_workspace(ws_id)
            .map(|o| o.current_location().to_f64())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn viewport_for_output(&self, output: &Output) -> LogicalViewport {
        let (logical_w, logical_h, scale) = output_logical_viewport(output);
        let non_exclusive = {
            let mut map = layer_map_for_output(output);
            map.arrange();
            map.non_exclusive_zone()
        };
        let safe_area = self.output_safe_area(&output.name());
        LogicalViewport::new(logical_w, logical_h, scale).with_work_area_zone_and_safe_area(
            non_exclusive.loc.x,
            non_exclusive.loc.y,
            non_exclusive.size.w,
            non_exclusive.size.h,
            safe_area,
        )
    }

    #[must_use]
    pub fn preferred_scale_for_surface(&self, surface: &WlSurface) -> f64 {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        if let Some(mapped) = self
            .mapped_windows
            .iter()
            .find(|m| m.window.toplevel().is_some_and(|t| *t.wl_surface() == root))
            && let Some(out) = self.output_for_workspace(&mapped.ws_id)
        {
            return out.current_scale().fractional_scale().max(0.01);
        }
        for out in self.all_outputs() {
            let map = layer_map_for_output(&out);
            if map.layers().any(|l| l.wl_surface() == &root) {
                return out.current_scale().fractional_scale().max(0.01);
            }
        }
        self.focused_output()
            .map(|o| o.current_scale().fractional_scale().max(0.01))
            .unwrap_or(1.0)
    }

    /// Updates `wl_surface.enter/leave` and `wp_fractional_scale_v1` preferred scale across a surface tree
    /// without nesting `with_states` locks.
    pub fn sync_surface_tree_output_and_scale(
        &self,
        root_surface: &WlSurface,
        target_output: Option<&Output>,
        scale: f64,
    ) {
        let clamped_scale = scale.max(0.01);
        let mut tree_surfaces: Vec<WlSurface> = Vec::new();
        with_surface_tree_downward(
            root_surface,
            (),
            |_, _, ()| TraversalAction::DoChildren(()),
            |surf, states, ()| {
                with_fractional_scale(states, |fractional| {
                    fractional.set_preferred_scale(clamped_scale);
                });
                tree_surfaces.push(surf.clone());
            },
            |_, _, ()| true,
        );
        let all_outputs = self.all_outputs();
        for surf in &tree_surfaces {
            for out in &all_outputs {
                if Some(out) == target_output {
                    out.enter(surf);
                } else {
                    out.leave(surf);
                }
            }
        }
    }

    pub fn sync_window_output_membership(&self, ws_id: &WorkspaceId) {
        let target_output = self.output_for_workspace(ws_id);
        let scale = target_output
            .as_ref()
            .map(|o| o.current_scale().fractional_scale())
            .or_else(|| self.workspaces.get(ws_id).map(|ws| ws.viewport.scale))
            .unwrap_or(1.0);
        for mapped in &self.mapped_windows {
            if mapped.ws_id == *ws_id
                && let Some(toplevel) = mapped.window.toplevel()
            {
                self.sync_surface_tree_output_and_scale(
                    toplevel.wl_surface(),
                    target_output.as_ref(),
                    scale,
                );
            }
        }
    }

    pub fn sync_output_layer_scales(&self, output: &Output) {
        let scale = output.current_scale().fractional_scale().max(0.01);
        let surfaces: Vec<WlSurface> = {
            let map = layer_map_for_output(output);
            map.layers().map(|l| l.wl_surface().clone()).collect()
        };
        for surf in surfaces {
            with_surface_tree_downward(
                &surf,
                (),
                |_, _, ()| TraversalAction::DoChildren(()),
                |_, states, ()| {
                    with_fractional_scale(states, |fractional| {
                        fractional.set_preferred_scale(scale);
                    });
                },
                |_, _, ()| true,
            );
        }
    }

    pub fn apply_output_layout_positions(&self) {
        let outputs = self.all_outputs();
        for entry in self.output_layout.entries() {
            if let Some(out) = outputs.iter().find(|o| o.name() == entry.name) {
                let target_loc: Point<i32, Logical> = entry.resolved_pos.into();
                if out.current_location() != target_loc {
                    out.change_current_state(None, None, None, Some(target_loc));
                }
            }
        }
    }

    pub fn ensure_initial_workspace_for_output(&mut self, output: &Output) -> WorkspaceId {
        let out_name = output.name();
        if let Some(existing) = self.output_workspaces.get(&out_name).cloned()
            && self.workspaces.contains_key(&existing)
        {
            return existing;
        }

        // 1. Restore saved workspace from hotplug memory if still present and not displayed elsewhere.
        if let Some(saved_ws) = self
            .output_layout
            .saved_hotplug_state(&out_name)
            .and_then(|s| s.active_ws_id.clone())
            && self.workspaces.contains_key(&saved_ws)
            && !self
                .output_workspaces
                .iter()
                .any(|(other_out, ws)| other_out != &out_name && ws == &saved_ws)
        {
            self.output_workspaces
                .insert(out_name.clone(), saved_ws.clone());
            if let Some(ws) = self.workspaces.get_mut(&saved_ws)
                && !ws.is_special
            {
                ws.presentation = PresentationTarget::Physical(OutputId::new(&out_name));
            }
            return saved_ws;
        }

        // 2. Claim an existing unassigned physical workspace with matching `preferred_output`.
        let matching_unassigned = self
            .workspaces
            .iter()
            .find(|(id, ws)| {
                !ws.is_special
                    && matches!(
                        &ws.presentation,
                        PresentationTarget::Physical(out_id) if out_id.as_str() == out_name
                    )
                    && !self
                        .output_workspaces
                        .values()
                        .any(|assigned| assigned == *id)
            })
            .map(|(id, _)| id.clone());
        if let Some(ws_id) = matching_unassigned {
            self.output_workspaces.insert(out_name, ws_id.clone());
            return ws_id;
        }

        // 3. First connected monitor inherits the startup `active_ws_id` (`ws_01`) if unclaimed.
        if !self
            .output_workspaces
            .values()
            .any(|assigned| assigned == &self.active_ws_id)
            && self.workspaces.contains_key(&self.active_ws_id)
        {
            let ws_id = self.active_ws_id.clone();
            if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                ws.presentation = PresentationTarget::Physical(OutputId::new(&out_name));
            }
            self.output_workspaces.insert(out_name, ws_id.clone());
            return ws_id;
        }

        // 4. Create a fresh numbered physical workspace (`2`, `3`, ...) bound to this monitor.
        let mut num = 1usize;
        let query = loop {
            let candidate = num.to_string();
            let used = self
                .workspaces
                .values()
                .any(|ws| !ws.is_special && ws.name == candidate);
            if !used {
                break candidate;
            }
            num += 1;
        };
        let ws_id = self.ensure_workspace_by_name_or_id(&query);
        if let Some(ws) = self.workspaces.get_mut(&ws_id) {
            ws.presentation = PresentationTarget::Physical(OutputId::new(&out_name));
        }
        self.output_workspaces.insert(out_name, ws_id.clone());
        ws_id
    }

    #[cfg_attr(not(feature = "smithay-tty"), allow(dead_code))]
    #[must_use]
    pub fn is_output_enabled(&self, output_name: &str) -> bool {
        self.output_layout
            .entry(output_name)
            .is_none_or(|entry| entry.enabled)
    }

    pub fn register_or_update_output(
        &mut self,
        output: &Output,
        explicit_pos: Option<(i32, i32)>,
        safe_area: SafeAreaInsets,
        enabled: bool,
    ) {
        let out_name = output.name();
        let was_registered = self.output_layout.entry(&out_name).is_some();
        if !was_registered
            && let Some(saved) = self.output_layout.saved_hotplug_state(&out_name).cloned()
        {
            let saved_mode = saved.mode.and_then(|(w, h, refresh)| {
                output.modes().into_iter().find(|m| {
                    m.size.w == w
                        && m.size.h == h
                        && (refresh == 0 || m.refresh.max(0) as u32 == refresh)
                })
            });
            let saved_transform = saved.transform.map(koma_transform_to_smithay);
            let saved_scale = saved.scale.map(Scale::Fractional);
            if saved_mode.is_some() || saved_transform.is_some() || saved_scale.is_some() {
                output.change_current_state(saved_mode, saved_transform, saved_scale, None);
            }
        }

        self.set_output_safe_area(&out_name, safe_area);
        let (logical_w, logical_h, scale) = output_logical_viewport(output);
        let transform = smithay_transform_to_koma(output.current_transform());
        let saved_pos = self
            .output_layout
            .saved_hotplug_state(&out_name)
            .and_then(|s| s.position);
        let pos_to_use = explicit_pos.or(saved_pos);

        self.output_layout.upsert_output(
            out_name.clone(),
            koma_model::OutputLayoutSpec {
                logical_w,
                logical_h,
                scale,
                transform,
                explicit_pos: pos_to_use,
                enabled,
            },
        );
        self.apply_output_layout_positions();

        let assigned_ws = if enabled {
            Some(self.ensure_initial_workspace_for_output(output))
        } else {
            self.output_workspaces.remove(&out_name);
            if self.focused_output_name.as_deref() == Some(out_name.as_str()) {
                self.focused_output_name = self
                    .output_layout
                    .enabled_entries()
                    .next()
                    .map(|e| e.name.clone());
                if let Some(ref next_name) = self.focused_output_name {
                    self.active_ws_id = self.active_workspace_for_output(next_name);
                }
            }
            None
        };
        if let Some(ref ws_id) = assigned_ws {
            if self.focused_output_name.is_none() {
                self.focused_output_name = Some(out_name.clone());
                self.active_ws_id = ws_id.clone();
            }
            let resolved_pos = self
                .output_layout
                .entry(&out_name)
                .map(|e| e.explicit_pos.or(Some(e.resolved_pos)))
                .unwrap_or(pos_to_use);
            let cur_mode = output
                .current_mode()
                .map(|m| (m.size.w, m.size.h, m.refresh.max(0) as u32));
            self.output_layout.record_hotplug_state(
                &out_name,
                SavedOutputConfig {
                    active_ws_id: Some(ws_id.clone()),
                    position: resolved_pos,
                    scale: Some(scale),
                    transform: Some(transform),
                    mode: cur_mode,
                    enabled,
                },
            );
        }

        self.sync_output_workarea(output);
        if let Some(ref ws_id) = assigned_ws {
            self.sync_window_output_membership(ws_id);
        }
        self.sync_output_layer_scales(output);
        self.clamp_and_update_pointer_bounds();
        self.reconcile_keyboard_focus();
        self.sync_foreign_toplevels();
        crate::output_management::sync_output_manager(self);
        self.queue_redraw();
        sync_ext_workspaces(self);
        self.emit_lua_event(
            "output.changed",
            serde_json::json!({
                "name": out_name,
                "event": if was_registered { "updated" } else { "added" },
                "width": logical_w,
                "height": logical_h,
                "scale": scale,
            }),
        );
    }

    #[cfg(any(feature = "smithay-tty", test))]
    pub fn unregister_output(&mut self, output: &Output) {
        let out_name = output.name();
        let cur_ws = self.output_workspaces.remove(&out_name);
        let cur_mode = output
            .current_mode()
            .map(|m| (m.size.w, m.size.h, m.refresh.max(0) as u32));
        let cur_scale = output.current_scale().fractional_scale();
        let cur_transform = smithay_transform_to_koma(output.current_transform());
        let saved_entry = self.output_layout.entry(&out_name).cloned();
        self.output_layout.record_hotplug_state(
            &out_name,
            SavedOutputConfig {
                active_ws_id: cur_ws.or_else(|| {
                    self.output_layout
                        .saved_hotplug_state(&out_name)
                        .and_then(|s| s.active_ws_id.clone())
                }),
                position: saved_entry.as_ref().and_then(|e| e.explicit_pos),
                scale: Some(cur_scale),
                transform: Some(cur_transform),
                mode: cur_mode,
                enabled: saved_entry.as_ref().is_none_or(|e| e.enabled),
            },
        );
        self.output_layout.remove_output(&out_name);
        self.apply_output_layout_positions();

        if self.focused_output_name.as_deref() == Some(out_name.as_str()) {
            let next_output = self
                .all_outputs()
                .into_iter()
                .find(|o| o.name() != out_name)
                .map(|o| o.name());
            self.focused_output_name = next_output.clone();
            if let Some(ref next_name) = next_output {
                self.active_ws_id = self.active_workspace_for_output(next_name);
            }
        }
        let ws_ids: Vec<WorkspaceId> = self.workspaces.keys().cloned().collect();
        for ws_id in ws_ids {
            self.sync_window_output_membership(&ws_id);
        }
        self.clamp_and_update_pointer_bounds();
        self.focus_active_deck_window();
        self.reconcile_idle_inhibitors();
        self.sync_foreign_toplevels();
        crate::output_management::sync_output_manager(self);
        self.queue_redraw();
        sync_ext_workspaces(self);
        self.emit_lua_event(
            "output.changed",
            serde_json::json!({
                "name": out_name,
                "event": "removed",
                "width": 0_u32,
                "height": 0_u32,
                "scale": 1.0_f64,
            }),
        );
    }

    pub fn clamp_and_update_pointer_bounds(&mut self) {
        if self.output_layout.enabled_entries().next().is_some() {
            let clamped = self.output_layout.clamp_pointer(LogicalPoint {
                x: self.pointer_location.x,
                y: self.pointer_location.y,
            });
            self.pointer_location = Point::from((clamped.x, clamped.y));
        }
    }

    pub fn focus_output(&mut self, target_name: &str) -> bool {
        let Some(target_out) = self
            .all_outputs()
            .into_iter()
            .find(|o| o.name() == target_name)
        else {
            return false;
        };
        if self
            .output_layout
            .entry(target_name)
            .is_some_and(|e| !e.enabled)
        {
            return false;
        }
        self.break_active_pointer_constraints();
        // Compare the *effective interactive* workspace, so dismissing a `special:` overlay over
        // the target workspace still emits its activation event.
        let prev_effective = self.effective_interactive_ws_id();
        self.active_special_ws_id = None;
        self.focused_output_name = Some(target_name.to_owned());
        let ws_id = self.ensure_initial_workspace_for_output(&target_out);
        self.active_ws_id = ws_id.clone();
        self.sync_output_workarea(&target_out);

        let rect = self.output_logical_rect(&target_out);
        let inside_target = self.pointer_location.x >= f64::from(rect.loc.x)
            && self.pointer_location.x < f64::from(rect.loc.x + rect.size.w)
            && self.pointer_location.y >= f64::from(rect.loc.y)
            && self.pointer_location.y < f64::from(rect.loc.y + rect.size.h);
        if !inside_target && !self.is_locked_or_abandoned() {
            let center = Point::from((
                f64::from(rect.loc.x) + f64::from(rect.size.w) * 0.5,
                f64::from(rect.loc.y) + f64::from(rect.size.h) * 0.5,
            ));
            self.pointer_location = center;
            if let Some(pointer) = self.ptr()
                && !pointer.is_grabbed()
            {
                let under = self.surface_under(center);
                pointer.motion(
                    self,
                    under,
                    &smithay::input::pointer::MotionEvent {
                        location: center,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: smithay::backend::input::InputTime::now(),
                    },
                );
                pointer.frame(self);
            }
        }

        self.focus_active_deck_window();
        self.reconcile_idle_inhibitors();
        self.sync_foreign_toplevels();
        self.queue_redraw();
        sync_ext_workspaces(self);
        if self.effective_interactive_ws_id() != prev_effective {
            let presentation = match self.workspaces.get(&ws_id).map(|ws| &ws.presentation) {
                Some(PresentationTarget::Offscreen { .. }) => "headless",
                _ => "physical",
            };
            self.emit_workspace_activated(&ws_id, Some(target_name), presentation);
        }
        true
    }

    /// Makes `ws_id` the focused workspace, switching the output that owns it when needed.
    ///
    /// This is the single workspace-switch path shared by `workspace.focus`, `ws.focus`,
    /// `WorkspaceFocusNext`/`Prev`, `ext-workspace-v1.activate` and window moves that follow the
    /// window (`Hyprland` `movetoworkspace`), so every one of them lands in exactly the same state
    /// and publishes the same activation event.
    pub fn focus_workspace(&mut self, ws_id: &WorkspaceId) -> bool {
        self.focus_workspace_with_direction(ws_id, 0)
    }

    /// [`Self::focus_workspace`] with an explicit slide direction for the workspace transition.
    ///
    /// `direction_hint` is `0` for an implicit direction derived from the workspace names and
    /// `1` / `-1` for `WorkspaceFocusNext` / `WorkspaceFocusPrev`.
    pub fn focus_workspace_with_direction(
        &mut self,
        ws_id: &WorkspaceId,
        direction_hint: i32,
    ) -> bool {
        let Some(is_special) = self.workspaces.get(ws_id).map(|ws| ws.is_special) else {
            return false;
        };
        // Re-focusing the workspace that is already interactive (including re-opening the same
        // `special:` overlay) is not a transition: emit nothing, so hooks do not reset state.
        let prev_effective = self.effective_interactive_ws_id();
        // An external workspace switch stops a pending repeat; a switch performed by the
        // repeated action itself is exempt inside `cancel_keybind_repeat_for_external_change`.
        self.cancel_keybind_repeat_for_external_change("workspace_change");
        self.break_active_pointer_constraints();
        if is_special {
            self.active_special_ws_id = Some(ws_id.clone());
            self.focus_active_deck_window();
            self.reconcile_idle_inhibitors();
            self.sync_foreign_toplevels();
            self.queue_redraw();
            sync_ext_workspaces(self);
            if self.effective_interactive_ws_id() != prev_effective {
                let out_name = self.focused_output_name.clone();
                self.emit_workspace_activated(ws_id, out_name.as_deref(), "special");
            }
            return true;
        }

        if let Some(hosting_out) = self
            .output_workspaces
            .iter()
            .find(|(_, id)| *id == ws_id)
            .map(|(name, _)| name.clone())
        {
            return self.focus_output(&hosting_out);
        }

        self.active_special_ws_id = None;
        let prev_ws_id = self.active_ws_id.clone();
        self.active_ws_id = ws_id.clone();
        let mut switched_out: Option<String> = None;
        if let Some(focused_out) = self.focused_output() {
            let out_name = focused_out.name();
            self.output_workspaces
                .insert(out_name.clone(), ws_id.clone());
            let vp = self.viewport_for_output(&focused_out);
            let now_ms = self.elapsed_ms();
            if let Some(ws) = self.workspaces.get_mut(ws_id) {
                ws.presentation = PresentationTarget::Physical(OutputId::new(&out_name));
                if ws.viewport != vp {
                    ws.viewport = vp;
                    ws.recalculate(now_ms);
                }
            }
            self.sync_workspace_geometries(ws_id);
            self.sync_window_output_membership(ws_id);
            switched_out = Some(out_name);
        }
        self.focus_active_deck_window();
        if let Some(out_name) = switched_out.as_deref() {
            self.trigger_workspace_transition(out_name, &prev_ws_id, ws_id, direction_hint);
        }
        self.reconcile_idle_inhibitors();
        self.sync_foreign_toplevels();
        self.queue_redraw();
        sync_ext_workspaces(self);
        if self.effective_interactive_ws_id() != prev_effective {
            let presentation = match self.workspaces.get(ws_id).map(|ws| &ws.presentation) {
                Some(PresentationTarget::Offscreen { .. }) => "headless",
                _ => "physical",
            };
            self.emit_workspace_activated(ws_id, switched_out.as_deref(), presentation);
        }
        true
    }

    /// Moves the active window of `src_ws_id` into `dst_ws_id` and follows it, like
    /// `Hyprland` `movetoworkspace`.
    pub fn move_active_window_between_workspaces(
        &mut self,
        src_ws_id: &WorkspaceId,
        dst_ws_id: &WorkspaceId,
    ) -> bool {
        self.move_active_window_between_workspaces_with_follow(src_ws_id, dst_ws_id, true)
    }

    /// Moves the active window of `src_ws_id` into `dst_ws_id`.
    ///
    /// With `follow` the destination workspace is focused and the moved window takes
    /// keyboard focus. Without it the source workspace keeps focus, matching `Hyprland`
    /// `movetoworkspacesilent`.
    pub fn move_active_window_between_workspaces_with_follow(
        &mut self,
        src_ws_id: &WorkspaceId,
        dst_ws_id: &WorkspaceId,
        follow: bool,
    ) -> bool {
        if src_ws_id == dst_ws_id {
            return false;
        }
        let now_ms = self.elapsed_ms();
        let active_win = self.workspaces.get_mut(src_ws_id).and_then(|src| {
            let win_id = src.active_window()?;
            let was_float = src.floating.contains(win_id);
            let _ = src.remove_window(win_id, now_ms);
            Some((win_id, was_float))
        });

        let Some((win_id, was_float)) = active_win else {
            return false;
        };
        let pref = self
            .mapped_windows
            .iter()
            .find(|m| m.win_id == win_id)
            .map(|m| m.preferred_size);
        let dst_origin = self.workspace_output_origin(dst_ws_id);
        let cursor = LogicalPoint {
            x: self.pointer_location.x - dst_origin.x,
            y: self.pointer_location.y - dst_origin.y,
        };

        let now_float = if let Some(dst) = self.workspaces.get_mut(dst_ws_id) {
            dst.insert_window(
                win_id,
                was_float,
                None,
                None,
                pref,
                Some(cursor),
                false,
                true,
                now_ms,
            )
        } else {
            was_float
        };

        if let Some(m) = self.mapped_windows.iter_mut().find(|m| m.win_id == win_id) {
            m.ws_id = dst_ws_id.clone();
            m.is_floating = now_float;
        }
        self.sync_workspace_geometries(src_ws_id);
        self.sync_workspace_geometries(dst_ws_id);
        self.sync_window_output_membership(dst_ws_id);
        if follow {
            // `focus_workspace` republishes the workspace state at the end of the switch.
            self.focus_workspace(dst_ws_id);
        } else {
            // A silent move never reaches `focus_workspace`, so the occupancy change on both
            // workspaces has to be pushed here or the bar keeps a stale entry.
            self.focus_active_deck_window();
            sync_ext_workspaces(self);
        }
        self.queue_redraw();
        true
    }

    pub fn move_active_window_to_output(&mut self, target_name: &str) -> bool {
        let Some(target_out) = self
            .all_outputs()
            .into_iter()
            .find(|o| o.name() == target_name)
        else {
            return false;
        };
        if self
            .output_layout
            .entry(target_name)
            .is_some_and(|e| !e.enabled)
        {
            return false;
        }
        let src_ws_id = self.effective_interactive_ws_id();
        let dst_ws_id = self.ensure_initial_workspace_for_output(&target_out);
        if src_ws_id == dst_ws_id {
            return false;
        }
        if !self.move_active_window_between_workspaces(&src_ws_id, &dst_ws_id) {
            return false;
        }
        self.focus_output(target_name)
    }

    #[must_use]
    pub fn is_locked_or_abandoned(&self) -> bool {
        self.session_lock.is_locked_or_abandoned()
    }

    /// Polls active locker/lock-surface liveness, transitioning dead lockers to `Abandoned`
    /// and enforcing blackout/focus clearing immediately without ever auto-unlocking.
    pub fn reconcile_session_lock(&mut self) -> bool {
        let changed = self.session_lock.reconcile_liveness();
        if changed {
            if self.is_locked_or_abandoned() {
                self.focused_ondemand_layer = None;
                self.clear_ime_state_for_lock();
            }
            self.reconcile_keyboard_focus();
            self.queue_redraw();
        }
        changed
    }

    /// Revokes client focus, grabs, and transient drags when entering `Locked` state.
    pub fn clear_inputs_for_lock(&mut self) {
        self.focused_ondemand_layer = None;
        self.active_drop_preview = None;
        self.active_drag_edge_scroll = None;
        self.edge_scroll_keep_alive = false;
        self.keybinds.pending_releases.clear();
        self.keybinds.pending_mouse_releases.clear();
        self.keybinds.suppressed_buttons.clear();
        self.keybinds.active_mode = None;
        self.keybinds.reset_scroll_trackers();
        self.cancel_keybind_repeat("session_lock");
        self.gestures.reset_active_sessions();

        let now_input = smithay::backend::input::InputTime::now();
        self.break_active_pointer_constraints();
        let _ = crate::pointer_grabs::cancel_active_pointer_grab(
            self,
            SERIAL_COUNTER.next_serial(),
            now_input,
        );
        if let Some(pointer) = self.ptr() {
            let serial = SERIAL_COUNTER.next_serial();
            pointer.motion(
                self,
                None,
                &smithay::input::pointer::MotionEvent {
                    location: self.pointer_location,
                    serial,
                    time: now_input,
                },
            );
            pointer.frame(self);
        }
        self.clear_ime_state_for_lock();
        if let Some(touch) = self.touch() {
            touch.cancel(self);
        }
        if let Some(keyboard) = self.kb() {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, None, serial);
        }
    }

    pub fn import_client_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        self.backend
            .as_mut()
            .is_some_and(|b| b.import_dmabuf(dmabuf))
    }

    pub fn change_vt(&mut self, vt: i32) {
        #[cfg(feature = "smithay-tty")]
        if let Some(ActiveBackend::Tty(tty)) = self.backend.as_mut() {
            tty.change_vt(vt);
        }
        #[cfg(not(feature = "smithay-tty"))]
        let _ = vt;
    }

    pub fn allocate_ipc_conn_id(&mut self) -> u64 {
        let id = self.next_ipc_conn_id;
        self.next_ipc_conn_id += 1;
        id
    }

    pub fn broadcast_workspace_event(
        &mut self,
        topic: EventTopic,
        ws_id: &WorkspaceId,
        epoch: Epoch,
        event: WorkspaceEvent,
    ) {
        if self.is_locked_or_abandoned() {
            return;
        }
        let ts_ns = self.start_time.elapsed().as_nanos() as u64;
        for conn in self.ipc_clients.values_mut() {
            let topic_ok =
                conn.subscribed_topics.is_empty() || conn.subscribed_topics.contains(&topic);
            let ws_ok =
                conn.subscribed_workspaces.is_empty() || conn.subscribed_workspaces.contains(ws_id);
            if topic_ok && ws_ok {
                conn.event_ring
                    .push(ts_ns, topic, ws_id.clone(), epoch, event.clone());
                if conn.drain_event_ring_into_outbox().is_ok() {
                    let _ = conn.outbox.try_flush(&conn.stream);
                }
            }
        }
    }

    pub fn allocate_window_id(&mut self) -> WindowId {
        let slot = self.next_win_slot;
        self.next_win_slot += 1;
        WindowId::new(slot, 1)
    }

    #[must_use]
    pub fn elapsed_duration(&self) -> Duration {
        self.start_time.elapsed()
    }

    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    /// Sends the initial `xdg_toplevel.configure(w, h)` scoped to the client's target workspace.
    pub fn send_initial_configure(&mut self, toplevel: &ToplevelSurface) {
        let target_ws_id = resolve_surface_jailed_workspace(self, toplevel.wl_surface())
            .filter(|ws_id| self.workspaces.contains_key(ws_id))
            .unwrap_or_else(|| self.effective_interactive_ws_id());

        let viewport = self
            .workspaces
            .get(&target_ws_id)
            .map(|ws| ws.viewport)
            .unwrap_or_default();

        // If `xdg_toplevel` already has a parent or explicit fixed `max_size`, don't force full-screen size
        let (min_sz, max_sz) = read_toplevel_min_max_size(toplevel.wl_surface());
        let hints = SizeHints::from_min_max(min_sz, max_sz);
        let has_parent = toplevel.parent().is_some();
        let initial_size = if has_parent || (max_sz.0 > 0 && max_sz.1 > 0) {
            None
        } else {
            let wa = viewport.work_area_rect();
            let inner_w = wa.w.saturating_sub(24);
            let inner_h = wa.h.saturating_sub(24);
            let clamped =
                hints.clamp_rect(LogicalRect::new(0, 0, inner_w.max(64), inner_h.max(64)));
            Some(Size::from((clamped.w as i32, clamped.h as i32)))
        };

        toplevel.with_pending_state(|state| {
            state.size = initial_size;
            state.states.set(xdg_toplevel::State::Activated);
            if initial_size.is_some() {
                for tiled_state in TILED_TOPLEVEL_STATES {
                    state.states.set(tiled_state);
                }
            }
        });
        toplevel.send_configure();
    }

    /// Synchronizes every `MappedWindow` in `ws_id` with its computed `Dwindle` / `Scrolling` /
    /// `Deck` / `FloatingStack` rectangle and dispatches `xdg_toplevel.configure` on size changes!
    pub fn sync_workspace_geometries(&mut self, ws_id: &WorkspaceId) {
        self.sync_workspace_geometries_internal(ws_id, None, false);
    }

    /// Synchronizes `ws_id` geometries while bypassing spring interpolation (`0 ms` cursor lock)
    /// for `instant_win` (e.g. a floating drag card attached 1:1 to the mouse cursor) while
    /// allowing surviving `Dwindle` tiles to smoothly spring into vacated space!
    pub fn sync_workspace_geometries_with_instant_window(
        &mut self,
        ws_id: &WorkspaceId,
        instant_win: Option<WindowId>,
    ) {
        self.sync_workspace_geometries_internal(ws_id, instant_win, false);
    }

    /// Synchronizes `ws_id` geometries with `instant_bypass = true` across all windows so manual
    /// `Mod + MouseRight` split-ratio resizing tracks the pointer with zero phase lag
    /// (`Hyprland` `DwindleAlgorithm.cpp` lines 369-378).
    pub fn sync_workspace_geometries_all_instant(&mut self, ws_id: &WorkspaceId) {
        self.sync_workspace_geometries_internal(ws_id, None, true);
    }

    fn sync_workspace_geometries_internal(
        &mut self,
        ws_id: &WorkspaceId,
        instant_win: Option<WindowId>,
        all_instant: bool,
    ) {
        let Some(ws) = self.workspaces.get(ws_id) else {
            return;
        };
        let is_offscreen = matches!(ws.presentation, PresentationTarget::Offscreen { .. });
        let visible = ws.compute_animation_target_geometries();
        let now_ms = self.elapsed_ms();
        let appearance = self.lua_sandbox.config_snapshot().appearance;
        let anim_cfg = appearance.animations;
        let border_inset = if appearance.border.enabled {
            (appearance.border.width.max(0.0) * 2.0).round() as u32
        } else {
            0
        };

        for (win_id, target_rect, is_float) in visible {
            if let Some(mapped) = self.mapped_windows.iter_mut().find(|m| m.win_id == win_id) {
                let rect_changed = mapped.rect != target_rect;
                let size_changed = mapped.rect.w != target_rect.w || mapped.rect.h != target_rect.h;
                let float_changed = mapped.is_floating != is_float;
                mapped.rect = target_rect;
                mapped.is_floating = is_float;
                let bypass = is_offscreen || all_instant || instant_win == Some(win_id);
                if rect_changed || bypass {
                    self.window_animations.borrow_mut().retarget_geometry(
                        win_id,
                        target_rect,
                        now_ms,
                        &anim_cfg,
                        bypass,
                    );
                }
                let is_fs = ws.is_fullscreen(win_id);
                let is_max = ws.is_maximized(win_id);
                let dec_mode = mapped.decoration_mode;
                let state_needs_configure = mapped.window.toplevel().is_some_and(|t| {
                    t.with_pending_state(|st| {
                        st.states.contains(xdg_toplevel::State::Fullscreen) != is_fs
                            || st.states.contains(xdg_toplevel::State::Maximized) != is_max
                            || st.decoration_mode != Some(dec_mode)
                    })
                });
                if size_changed || float_changed || state_needs_configure {
                    let inner_w = target_rect.w.saturating_sub(border_inset).max(48) as i32;
                    let inner_h = target_rect.h.saturating_sub(border_inset).max(48) as i32;
                    if let Some(toplevel) = mapped.window.toplevel() {
                        toplevel.with_pending_state(|st| {
                            st.size = Some(Size::from((inner_w, inner_h)));
                            for tiled_state in TILED_TOPLEVEL_STATES {
                                if is_float || is_fs || is_max {
                                    st.states.unset(tiled_state);
                                } else {
                                    st.states.set(tiled_state);
                                }
                            }
                            if is_max {
                                st.states.set(xdg_toplevel::State::Maximized);
                            } else {
                                st.states.unset(xdg_toplevel::State::Maximized);
                            }
                            if is_fs {
                                st.states.set(xdg_toplevel::State::Fullscreen);
                            } else {
                                st.states.unset(xdg_toplevel::State::Fullscreen);
                            }
                            st.decoration_mode = Some(dec_mode);
                        });
                        toplevel.send_pending_configure();
                    }
                }
            }
        }
    }

    /// Promotes an `UnmappedToplevel` into `mapped_windows` + `WorkspaceState` (`DwindleTree` /
    /// `ScrollingStrip` / `DeckStashStack` / `FloatingStack`), enforcing:
    /// 1. Jailed container pinning to `Headless` workspaces,
    /// 2. Initial window rules (`open_on_workspace`, `open_mode = float | stash_background`), and
    /// 3. Hyprland `DwindleAlgorithm.cpp` `max_size` & `xdg_toplevel.parent()` auto-float downgrade!
    pub fn promote_unmapped_to_mapped(&mut self, unmapped: UnmappedToplevel) {
        let Some(toplevel) = unmapped.window.toplevel() else {
            return;
        };
        let wl_surface = toplevel.wl_surface().clone();
        let has_parent = toplevel.parent().is_some();
        let (app_id, title) = read_toplevel_metadata(&wl_surface);
        let (min_sz, max_sz) = read_toplevel_min_max_size(&wl_surface);
        let is_sandboxed = is_surface_from_restricted_client(self, &wl_surface);

        let jailed_ws = resolve_surface_jailed_workspace(self, &wl_surface)
            .filter(|id| self.workspaces.contains_key(id));

        let provisional_ws_id = jailed_ws
            .clone()
            .unwrap_or_else(|| self.effective_interactive_ws_id());
        let is_agent_ws = self
            .workspaces
            .get(&provisional_ws_id)
            .is_some_and(|ws| matches!(ws.presentation, PresentationTarget::Offscreen { .. }));

        let match_ctx = WindowMatchContext {
            app_id: &app_id,
            title: &title,
            is_floating: has_parent,
            is_focused: true,
            is_xwayland: false,
            is_modal_dialog: has_parent,
            is_screencast_target: false,
            is_agent_workspace: is_agent_ws,
        };
        let initial_rules = self.rules.resolve_initial(&match_ctx);

        // Resolve target workspace (`jail` strictly overrides user rules!)
        let ws_id = if let Some(j_id) = jailed_ws {
            j_id
        } else if let Some(rule_ws_name) = initial_rules.open_on_workspace.as_deref() {
            if rule_ws_name.starts_with(koma_model::SPECIAL_WORKSPACE_PREFIX) {
                self.ensure_special_workspace(Some(rule_ws_name))
            } else {
                self.workspaces
                    .iter()
                    .find(|(id, ws)| id.as_str() == rule_ws_name || ws.name == rule_ws_name)
                    .map(|(id, _)| id.clone())
                    .unwrap_or(provisional_ws_id)
            }
        } else {
            provisional_ws_id
        };

        let viewport = self
            .workspaces
            .get(&ws_id)
            .map(|ws| ws.viewport)
            .unwrap_or_default();

        let fixed_min_eq_max = max_sz.0 > 0 && max_sz.1 > 0 && min_sz == max_sz;
        let want_floating = has_parent
            || fixed_min_eq_max
            || matches!(initial_rules.open_mode, Some(OpenMode::Float));
        let open_in_stash = matches!(initial_rules.open_mode, Some(OpenMode::StashBackground));
        let focus_new = initial_rules.open_focused.unwrap_or(true);

        let geom_bbox = unmapped.window.geometry();
        let wa = viewport.work_area_rect();
        let preferred_size = if geom_bbox.size.w > 0 && geom_bbox.size.h > 0 {
            (geom_bbox.size.w as u32, geom_bbox.size.h as u32)
        } else if max_sz.0 > 0 && max_sz.1 > 0 {
            max_sz
        } else {
            ((wa.w * 3 / 5).max(360), (wa.h * 3 / 5).max(260))
        };

        let explicit_float_rect =
            initial_rules
                .default_floating_pos
                .and_then(|(anchor, dx, dy)| {
                    let pt = koma_model::AnchorPoint::AnchorOffset {
                        anchor,
                        dx: f64::from(dx),
                        dy: f64::from(dy),
                    }
                    .resolve_in(wa)
                    .ok()?;
                    Some(LogicalRect::new(
                        pt.x.round() as i32,
                        pt.y.round() as i32,
                        preferred_size.0,
                        preferred_size.1,
                    ))
                });

        let opt_max_size = (max_sz.0 > 0 || max_sz.1 > 0).then_some(max_sz);
        let ws_origin = self.workspace_output_origin(&ws_id);
        let cursor_logical = LogicalPoint {
            x: self.pointer_location.x - ws_origin.x,
            y: self.pointer_location.y - ws_origin.y,
        };

        let now_ms = self.elapsed_ms();
        let is_floating = if let Some(ws) = self.workspaces.get_mut(&ws_id) {
            ws.frame_clock.record_activity(now_ms);
            let _ = ws.set_size_hints(unmapped.win_id, SizeHints::from_min_max(min_sz, max_sz));
            let floated = ws.insert_window(
                unmapped.win_id,
                want_floating,
                explicit_float_rect,
                opt_max_size,
                Some(preferred_size),
                Some(cursor_logical),
                open_in_stash,
                focus_new,
                now_ms,
            );
            if matches!(initial_rules.open_mode, Some(OpenMode::Fullscreen)) {
                ws.toggle_fullscreen(unmapped.win_id);
            }
            floated
        } else {
            want_floating
        };

        let mapped = MappedWindow {
            win_id: unmapped.win_id,
            ws_id: ws_id.clone(),
            window: unmapped.window,
            app_id: app_id.clone(),
            title: title.clone(),
            is_floating,
            is_minimized: false,
            is_sandboxed,
            preferred_size,
            rect: wa,
            decoration_mode: unmapped.decoration_mode,
            icon: unmapped.icon,
        };
        info!(
            win_id = %mapped.win_id,
            ws_id = %ws_id,
            app_id = %app_id,
            title = %title,
            is_floating,
            is_sandboxed,
            "promoted Wayland toplevel into workspace Space"
        );
        let win_id_created = mapped.win_id;
        let mapped_is_protected = self
            .rules
            .resolve_dynamic(&WindowMatchContext {
                app_id: &app_id,
                title: &title,
                is_floating,
                is_focused: true,
                is_xwayland: false,
                is_modal_dialog: false,
                is_screencast_target: false,
                is_agent_workspace: is_agent_ws,
            })
            .block_out_from
            .is_some_and(|p| p.blocks_agent_capture());
        let anim_style_override = self
            .rules
            .resolve_dynamic(&WindowMatchContext {
                app_id: &app_id,
                title: &title,
                is_floating,
                is_focused: true,
                is_xwayland: false,
                is_modal_dialog: false,
                is_screencast_target: false,
                is_agent_workspace: is_agent_ws,
            })
            .animation;
        let ws_viewport = self
            .workspaces
            .get(&ws_id)
            .map_or(LogicalViewport::new(1920, 1080, 1.0), |w| w.viewport);
        self.mapped_windows.push(mapped);
        self.sync_workspace_geometries(&ws_id);
        self.sync_window_output_membership(&ws_id);
        // A newly occupied workspace must leave the `hidden` set before the bar redraws.
        sync_ext_workspaces(self);
        if let Some(final_rect) = self
            .mapped_windows
            .iter()
            .find(|m| m.win_id == win_id_created)
            .map(|m| m.rect)
        {
            let anim_cfg = self.lua_sandbox.config_snapshot().appearance.animations;
            // A recycled window slot must never inherit the previous occupant's fading frame.
            self.closing_windows.retain(|c| c.win_id != win_id_created);
            self.window_frames.remove(&win_id_created);
            self.window_animations.borrow_mut().on_window_opened(
                win_id_created,
                final_rect,
                ws_viewport,
                now_ms,
                &anim_cfg,
                anim_style_override.as_deref(),
                is_agent_ws,
            );
        }

        let visible_anywhere = ws_id == self.active_ws_id
            || Some(&ws_id) == self.active_special_ws_id.as_ref()
            || self.output_workspaces.values().any(|id| id == &ws_id);
        if ws_id == self.active_ws_id || Some(&ws_id) == self.active_special_ws_id.as_ref() {
            self.focus_active_deck_window();
            self.queue_redraw();
        } else if visible_anywhere {
            self.queue_redraw();
        }
        if !is_agent_ws {
            let title_redacted = if mapped_is_protected {
                "[protected]"
            } else {
                title.as_str()
            };
            self.emit_lua_event(
                "window.mapped",
                serde_json::json!({
                    "id": win_id_created.slot,
                    "app_id": app_id,
                    "title_redacted": title_redacted,
                    "workspace": ws_id.to_string(),
                    "floating": is_floating,
                }),
            );
        }
    }

    /// Arranges `wlr-layer-shell` surfaces for `output` and syncs the non-exclusive workarea
    /// composed with `SafeAreaInsets` (camera notch) into every matching physical workspace.
    pub fn sync_output_workarea(&mut self, output: &Output) {
        let (logical_w, logical_h, scale) = output_logical_viewport(output);
        let non_exclusive = {
            let mut map = layer_map_for_output(output);
            map.arrange();
            map.non_exclusive_zone()
        };
        let safe_area = self.output_safe_area(&output.name());
        let viewport = LogicalViewport::new(logical_w, logical_h, scale)
            .with_work_area_zone_and_safe_area(
                non_exclusive.loc.x,
                non_exclusive.loc.y,
                non_exclusive.size.w,
                non_exclusive.size.h,
                safe_area,
            );
        let out_name = output.name();
        let single_output = self.all_outputs().len() <= 1;
        let is_primary = self.primary_output().as_ref() == Some(output);
        let assigned_ws = self.output_workspaces.get(&out_name).cloned();
        let now_ms = self.elapsed_ms();
        let mut affected_ws = Vec::new();
        for (ws_id, ws) in &mut self.workspaces {
            let matches = assigned_ws.as_ref() == Some(ws_id)
                || match &ws.presentation {
                    PresentationTarget::Physical(out_id) => {
                        out_id.as_str() == out_name
                            || (single_output
                                && !ws.is_special
                                && self.output_workspaces.is_empty())
                            || (is_primary && out_id.as_str() == "primary")
                    }
                    PresentationTarget::Offscreen { .. } => false,
                };
            if matches && ws.viewport != viewport {
                ws.viewport = viewport;
                ws.recalculate(now_ms);
                affected_ws.push(ws_id.clone());
            }
        }
        for ws_id in affected_ws {
            self.sync_workspace_geometries(&ws_id);
        }
    }

    /// Updates physical workspace & Scratchpad viewports on `WinitEvent::Resized` or KMS modeset.
    pub fn handle_output_resized(
        &mut self,
        output: &Output,
        logical_w: u32,
        logical_h: u32,
        scale: f64,
    ) {
        let out_name = output.name();
        let transform = smithay_transform_to_koma(output.current_transform());
        if self.output_layout.entry(&out_name).is_some() {
            let existing = self.output_layout.entry(&out_name).cloned();
            self.output_layout.upsert_output(
                out_name.clone(),
                koma_model::OutputLayoutSpec {
                    logical_w: logical_w.max(64),
                    logical_h: logical_h.max(64),
                    scale,
                    transform,
                    explicit_pos: existing.as_ref().and_then(|e| e.explicit_pos),
                    enabled: existing.as_ref().is_none_or(|e| e.enabled),
                },
            );
            self.apply_output_layout_positions();
        } else {
            self.register_or_update_output(output, None, self.output_safe_area(&out_name), true);
        }

        let non_exclusive = {
            let mut map = layer_map_for_output(output);
            map.arrange();
            map.non_exclusive_zone()
        };
        let safe_area = self.output_safe_area(&out_name);
        let viewport = LogicalViewport::new(logical_w.max(64), logical_h.max(64), scale)
            .with_work_area_zone_and_safe_area(
                non_exclusive.loc.x,
                non_exclusive.loc.y,
                non_exclusive.size.w,
                non_exclusive.size.h,
                safe_area,
            );
        if let Some(lock_surface) = self.session_lock.active_lock_surface_for_output(output) {
            let needs_configure = lock_surface.with_pending_state(|st| {
                let target = Some((viewport.width, viewport.height).into());
                if st.size != target {
                    st.size = target;
                    true
                } else {
                    false
                }
            });
            if needs_configure {
                lock_surface.send_configure();
            }
        }

        let single_output = self.all_outputs().len() <= 1;
        let is_primary = self.primary_output().as_ref() == Some(output);
        let assigned_ws = self.output_workspaces.get(&out_name).cloned();
        let now_ms = self.elapsed_ms();
        let mut affected_ws: Vec<WorkspaceId> = Vec::new();
        for (ws_id, ws) in &mut self.workspaces {
            let matches = assigned_ws.as_ref() == Some(ws_id)
                || match &ws.presentation {
                    PresentationTarget::Physical(out_id) => {
                        out_id.as_str() == out_name
                            || (single_output
                                && !ws.is_special
                                && self.output_workspaces.is_empty())
                            || (is_primary && out_id.as_str() == "primary")
                    }
                    PresentationTarget::Offscreen { .. } => false,
                };
            if matches {
                ws.viewport = viewport;
                if !ws.is_special && (single_output || assigned_ws.as_ref() == Some(ws_id)) {
                    ws.presentation = PresentationTarget::Physical(OutputId::new(&out_name));
                }
                ws.recalculate(now_ms);
                affected_ws.push(ws_id.clone());
            }
        }

        for ws_id in &affected_ws {
            self.sync_workspace_geometries(ws_id);
            self.sync_window_output_membership(ws_id);
        }
        self.sync_output_layer_scales(output);
        self.clamp_and_update_pointer_bounds();
        self.reconcile_keyboard_focus();
        crate::output_management::sync_output_manager(self);
        self.queue_redraw();
    }

    /// Window that currently owns keyboard focus inside the interactive workspace.
    ///
    /// In [`FollowMouseMode::FocusOnly`] this is the pointer-focused window recorded by the
    /// follow-mouse path; otherwise it is the workspace's active window. Keeping this a single
    /// source of truth makes seat keyboard focus, `xdg_toplevel` `Activated`, foreign-toplevel
    /// state and Lua/IPC snapshots agree without changing `WorkspaceState::active_window`.
    #[must_use]
    pub fn focused_window_id(&self) -> Option<WindowId> {
        let ws_id = self.effective_interactive_ws_id();
        if let Some(win_id) = self.pointer_focus_window
            && self
                .mapped_windows
                .iter()
                .any(|m| m.win_id == win_id && !m.is_minimized && m.ws_id == ws_id)
        {
            return Some(win_id);
        }
        self.workspaces
            .get(&ws_id)
            .and_then(WorkspaceState::active_window)
            .filter(|id| {
                self.mapped_windows
                    .iter()
                    .any(|m| m.win_id == *id && !m.is_minimized)
            })
    }

    #[must_use]
    fn active_deck_wl_surface(&self) -> Option<WlSurface> {
        let active_id = self.focused_window_id()?;
        self.mapped_windows
            .iter()
            .find(|m| m.win_id == active_id && !m.is_minimized)
            .and_then(|m| m.window.toplevel().map(|t| t.wl_surface().clone()))
    }

    #[must_use]
    pub fn output_at_point_strict(&self, global_pos: Point<f64, Logical>) -> Option<Output> {
        for output in self.all_outputs() {
            let origin = output.current_location().to_f64();
            let (w, h, _) = output_logical_viewport(&output);
            let local = global_pos - origin;
            if local.x >= 0.0 && local.y >= 0.0 && local.x < f64::from(w) && local.y < f64::from(h)
            {
                return Some(output);
            }
        }
        None
    }

    #[must_use]
    pub fn output_at_point(&self, global_pos: Point<f64, Logical>) -> Option<Output> {
        self.output_at_point_strict(global_pos)
            .or_else(|| self.focused_output())
    }

    #[must_use]
    fn find_top_exclusive_layer_surface(&self) -> Option<WlSurface> {
        const TIER_ORDER: [Layer; 4] =
            [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background];
        let mut best: Option<(usize, WlSurface)> = None;
        for output in self.all_outputs() {
            let map = layer_map_for_output(&output);
            for (rank, tier) in TIER_ORDER.iter().enumerate() {
                if let Some(layer) = map.layers_on(*tier).rev().find(|l| {
                    l.alive()
                        && is_surface_mapped(l.wl_surface())
                        && l.cached_state().keyboard_interactivity
                            == KeyboardInteractivity::Exclusive
                }) {
                    if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
                        best = Some((rank, layer.wl_surface().clone()));
                    }
                    break;
                }
            }
        }
        best.map(|(_, surf)| surf)
    }

    #[must_use]
    fn active_ondemand_layer_surface(&self) -> Option<WlSurface> {
        let target = self.focused_ondemand_layer.as_ref()?;
        for output in self.all_outputs() {
            let map = layer_map_for_output(&output);
            if let Some(layer) = map.layers().find(|l| l.wl_surface() == target)
                && layer.alive()
                && is_surface_mapped(layer.wl_surface())
                && layer.cached_state().keyboard_interactivity == KeyboardInteractivity::OnDemand
            {
                return Some(target.clone());
            }
        }
        None
    }

    /// Reconciles seat keyboard focus respecting fail-closed priority:
    /// `LockSurface` / `LockBlackoutNone` > `Exclusive` layer > `OnDemand` layer > Active window.
    pub fn reconcile_keyboard_focus(&mut self) {
        let is_locked = self.is_locked_or_abandoned();
        let lock_target = if matches!(
            self.session_lock.effective_phase(),
            SessionLockPhase::Locked(_)
        ) {
            let pointer_output = self.output_at_point(self.pointer_location);
            pointer_output
                .as_ref()
                .and_then(|out| self.session_lock.active_lock_surface_for_output(out))
                .or_else(|| self.session_lock.first_active_lock_surface())
                .map(|s| s.wl_surface().clone())
        } else {
            None
        };

        let exclusive_layer = if is_locked {
            None
        } else {
            self.find_top_exclusive_layer_surface()
        };
        let ondemand_layer = if is_locked {
            None
        } else {
            let candidate = self.active_ondemand_layer_surface();
            if candidate.is_none() {
                self.focused_ondemand_layer = None;
            }
            candidate
        };

        let target_surface = match resolve_keyboard_focus_source(
            is_locked,
            lock_target.is_some(),
            exclusive_layer.is_some(),
            ondemand_layer.is_some(),
        ) {
            KeyboardFocusSource::LockSurface => lock_target,
            KeyboardFocusSource::LockBlackoutNone => None,
            KeyboardFocusSource::ExclusiveLayer => exclusive_layer,
            KeyboardFocusSource::OnDemandLayer => ondemand_layer,
            KeyboardFocusSource::ActiveWindow => self.active_deck_wl_surface(),
        };

        if let Some(keyboard) = self.kb()
            && keyboard.current_focus() != target_surface
        {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, target_surface, serial);
        }
    }

    /// Updates `Activated` toplevel state for `effective_interactive_ws_id` and reconciles
    /// seat keyboard focus without bypassing lock-screen or layer-shell priority.
    pub fn focus_active_deck_window(&mut self) {
        let target_ws_id = self.effective_interactive_ws_id();
        let active_id = self.focused_window_id();
        for mapped in &self.mapped_windows {
            if let Some(toplevel) = mapped.window.toplevel() {
                let is_active = mapped.ws_id == target_ws_id
                    && Some(mapped.win_id) == active_id
                    && !mapped.is_minimized;
                let changed = toplevel.with_pending_state(|st| {
                    let was = st.states.contains(xdg_toplevel::State::Activated);
                    if was == is_active {
                        return false;
                    }
                    if is_active {
                        st.states.set(xdg_toplevel::State::Activated);
                    } else {
                        st.states.unset(xdg_toplevel::State::Activated);
                    }
                    true
                });
                if changed {
                    toplevel.send_pending_configure();
                }
            }
        }
        self.reconcile_keyboard_focus();
        self.sync_foreign_toplevels();

        let now_ms = self.elapsed_ms();
        let anim_cfg = self.lua_sandbox.config_snapshot().appearance.animations;
        let is_offscreen = self
            .workspaces
            .get(&target_ws_id)
            .is_some_and(|w| matches!(w.presentation, PresentationTarget::Offscreen { .. }));
        {
            let mut tbl = self.window_animations.borrow_mut();
            for mapped in &self.mapped_windows {
                if mapped.ws_id == target_ws_id && !mapped.is_minimized {
                    tbl.retarget_focus(
                        mapped.win_id,
                        Some(mapped.win_id) == active_id,
                        now_ms,
                        &anim_cfg,
                        is_offscreen,
                    );
                }
            }
        }

        if self.last_notified_focus_win_id != active_id {
            self.last_notified_focus_win_id = active_id;
            if !is_offscreen
                && let Some(focused_id) = active_id
                && let Some(m) = self.mapped_windows.iter().find(|m| m.win_id == focused_id)
            {
                let payload = serde_json::json!({
                    "id": focused_id.slot,
                    "app_id": m.app_id,
                    "workspace": m.ws_id.to_string(),
                });
                self.emit_lua_event("window.focused", payload);
            }
        }
    }

    #[must_use]
    fn layer_surface_under_tiers(
        &self,
        global_pos: Point<f64, Logical>,
        tiers: [Layer; 2],
    ) -> Option<PointerHitTarget> {
        let hit_layer = |layer: &smithay::desktop::LayerSurface,
                         map: &LayerMap,
                         output_origin: Point<f64, Logical>,
                         output_local: Point<f64, Logical>|
         -> Option<PointerHitTarget> {
            let layer_geo = map.layer_geometry(layer)?;
            let layer_loc = layer_geo.loc.to_f64();
            let (surf, offset) =
                layer.surface_under(output_local - layer_loc, WindowSurfaceType::ALL)?;
            Some(PointerHitTarget::Layer {
                surface: surf,
                origin: offset.to_f64() + layer_loc + output_origin,
                layer_root: layer.wl_surface().clone(),
                interactivity: layer.cached_state().keyboard_interactivity,
            })
        };

        for output in self.all_outputs() {
            let output_origin = output.current_location().to_f64();
            let output_local = global_pos - output_origin;
            let map = layer_map_for_output(&output);
            for tier in tiers {
                if let Some(top_layer) = map.layer_under(tier, output_local)
                    && let Some(hit) = hit_layer(top_layer, &map, output_origin, output_local)
                {
                    return Some(hit);
                }
                for layer in map.layers_on(tier).rev() {
                    if let Some(hit) = hit_layer(layer, &map, output_origin, output_local) {
                        return Some(hit);
                    }
                }
            }
        }
        None
    }

    /// Unified Z-order hit-testing across `LockSurface` -> `Overlay` -> `Top` -> windows -> `Bottom` -> `Background`.
    #[must_use]
    pub fn hit_test_target(&self, global_pos: Point<f64, Logical>) -> Option<PointerHitTarget> {
        if self.is_locked_or_abandoned() {
            for output in self.all_outputs() {
                let output_origin = output.current_location().to_f64();
                let output_local = global_pos - output_origin;
                if let Some(lock_surf) = self.session_lock.active_lock_surface_for_output(&output) {
                    let (w, h, _) = output_logical_viewport(&output);
                    if output_local.x >= 0.0
                        && output_local.y >= 0.0
                        && output_local.x < f64::from(w)
                        && output_local.y < f64::from(h)
                    {
                        if let Some((surf, offset)) = under_from_surface_tree(
                            lock_surf.wl_surface(),
                            output_local,
                            (0, 0),
                            WindowSurfaceType::ALL,
                        ) {
                            return Some(PointerHitTarget::LockSurface {
                                surface: surf,
                                origin: offset.to_f64() + output_origin,
                            });
                        }
                        return Some(PointerHitTarget::LockSurface {
                            surface: lock_surf.wl_surface().clone(),
                            origin: output_origin,
                        });
                    }
                }
            }
            return None;
        }

        if let Some(hit) = self.layer_surface_under_tiers(global_pos, [Layer::Overlay, Layer::Top])
        {
            return Some(hit);
        }

        if let Some((win_id, ws_id, surface, origin)) = self.hit_test_visible_surface(global_pos) {
            return Some(PointerHitTarget::Window {
                surface,
                origin,
                win_id,
                ws_id,
            });
        }

        self.layer_surface_under_tiers(global_pos, [Layer::Bottom, Layer::Background])
    }

    /// Unified surface hit-test returning the focused Wayland surface and logical origin.
    #[must_use]
    pub fn surface_under(
        &self,
        global_pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.hit_test_target(global_pos)
            .map(|target| (target.surface().clone(), target.origin()))
    }

    #[must_use]
    pub fn is_workspace_visible_anywhere(&self, ws_id: &WorkspaceId) -> bool {
        *ws_id == self.active_ws_id
            || Some(ws_id) == self.active_special_ws_id.as_ref()
            || self.output_workspaces.values().any(|id| id == ws_id)
    }

    #[must_use]
    pub fn agent_cursor_config(&self) -> AgentCursorConfig {
        self.lua_sandbox
            .config_snapshot()
            .appearance
            .agent_cursor
            .clone()
    }

    /// Returns `true` if `ws_id` currently has an in-flight scrolling camera glide or
    /// window geometry/closing animation that could make point hit-testing transient.
    #[must_use]
    pub fn ws_has_active_layout_animation(&self, ws_id: &WorkspaceId) -> bool {
        let now_ms = self.elapsed_ms();
        let camera_animating = self
            .workspaces
            .get(ws_id)
            .is_some_and(|ws| ws.scrolling.is_camera_animating(now_ms));
        if camera_animating {
            return true;
        }
        let ws_win_ids: Vec<WindowId> = self
            .mapped_windows
            .iter()
            .filter(|m| &m.ws_id == ws_id)
            .map(|m| m.win_id)
            .collect();
        self.window_animations
            .borrow_mut()
            .has_active_for_windows(now_ms, ws_win_ids.iter())
    }

    /// Advances the visual cursor spring/trail/ripple state for `ws_id` if visible/enabled.
    #[must_use]
    pub fn step_agent_cursor_for_workspace(
        &mut self,
        ws_id: &WorkspaceId,
    ) -> Option<AgentCursorFrameSnapshot> {
        let now_ms = self.elapsed_ms();
        let cfg = self.agent_cursor_config();
        if !cfg.enabled {
            return None;
        }
        let vp_rect = self.workspaces.get(ws_id).map(|ws| ws.viewport.full_rect());
        self.agent_cursors
            .get_mut(ws_id)?
            .visual
            .step_frame_in_viewport(now_ms, &cfg, vp_rect)
    }

    /// Advances the visual agent cursor snapshot for output `out_name`, preferring a focused
    /// special scratchpad workspace if active on that output, otherwise `out_ws_id`.
    #[must_use]
    pub fn step_agent_cursor_for_output(
        &mut self,
        out_name: &str,
        out_ws_id: &WorkspaceId,
    ) -> Option<AgentCursorFrameSnapshot> {
        let sp_snap = if self.focused_output_name().as_deref() == Some(out_name) {
            self.active_special_ws_id
                .clone()
                .and_then(|sp| self.step_agent_cursor_for_workspace(&sp))
        } else {
            None
        };
        sp_snap.or_else(|| self.step_agent_cursor_for_workspace(out_ws_id))
    }

    fn hit_test_window_in_workspace(
        &self,
        ws_id: &WorkspaceId,
        output_local: Point<f64, Logical>,
        out_origin: Point<f64, Logical>,
        b_off: f64,
    ) -> Option<(WindowId, WlSurface, Point<f64, Logical>)> {
        let ws = self.workspaces.get(ws_id)?;
        for (win_id, rect, _) in ws
            .compute_visible_geometries_at(self.elapsed_ms())
            .into_iter()
            .rev()
        {
            let Some(mapped) = self.mapped_windows.iter().find(|m| m.win_id == win_id) else {
                continue;
            };
            if mapped.is_minimized {
                continue;
            }
            // Frame space: the inner rect is the window geometry (border/border-fallback checks).
            let inner_origin = Point::from((f64::from(rect.x) + b_off, f64::from(rect.y) + b_off));
            let local_pos = output_local - inner_origin;
            // Surface space: CSD surfaces extend above/left of the geometry rect, so hit-test the
            // surface tree at its real `(0, 0)` and report that same origin for pointer focus.
            let win_surface_origin = surface_origin(inner_origin, mapped.window.geometry().loc);
            let surface_pos = output_local - win_surface_origin;
            let global_origin = win_surface_origin + out_origin;
            if let Some((surf, surf_offset)) = mapped
                .window
                .surface_under(surface_pos, WindowSurfaceType::ALL)
            {
                return Some((win_id, surf, surf_offset.to_f64() + global_origin));
            }
            let in_frame = output_local.x >= f64::from(rect.x)
                && output_local.x <= f64::from(rect.x + rect.w as i32)
                && output_local.y >= f64::from(rect.y)
                && output_local.y <= f64::from(rect.y + rect.h as i32);
            let allow_frame_fallback = mapped.window.toplevel().is_some_and(|t| {
                let root = t.wl_surface();
                if surface_has_empty_input_region(root) {
                    return false;
                }
                !is_surface_mapped(root)
                    || local_pos.x < 0.0
                    || local_pos.y < 0.0
                    || local_pos.x >= f64::from(rect.w) - 2.0 * b_off
                    || local_pos.y >= f64::from(rect.h) - 2.0 * b_off
            });
            if in_frame
                && allow_frame_fallback
                && let Some(toplevel) = mapped.window.toplevel()
            {
                return Some((win_id, toplevel.wl_surface().clone(), global_origin));
            }
        }
        None
    }

    /// Resolves the front-to-back window surface under `ws_local_pos` inside `ws_id` without
    /// reading or mutating global human `pointer_location`.
    #[must_use]
    pub fn surface_under_in_workspace(
        &self,
        ws_id: &WorkspaceId,
        ws_local_pos: Point<f64, Logical>,
    ) -> Option<(WindowId, WlSurface, Point<f64, Logical>)> {
        let border_cfg = self.lua_sandbox.config_snapshot().appearance.border;
        let b_off = if border_cfg.enabled {
            f64::from(border_cfg.width.max(0.0))
        } else {
            0.0
        };
        let out_origin = self.workspace_output_origin(ws_id);
        self.hit_test_window_in_workspace(ws_id, ws_local_pos, out_origin, b_off)
    }

    /// Resolves the interactive-resize grab edge under `global_pos`, front-to-back, so a frame
    /// border or corner starts a resize without a modifier (`Hyprland` `DragController` parity).
    ///
    /// The grab zone is the outer frame ring: `appearance.border.width` plus a small slop, which is
    /// exactly the strip Koma paints and the client surface does not cover. Fullscreen windows are
    /// never resize targets, and a window whose frame is thinner than the slop on both axes still
    /// resolves to its nearer edge.
    #[must_use]
    pub fn frame_edge_under_pointer(
        &self,
        global_pos: Point<f64, Logical>,
    ) -> Option<(WindowId, WorkspaceId, ResizeCorner)> {
        let border_cfg = self.lua_sandbox.config_snapshot().appearance.border;
        let border_w = if border_cfg.enabled {
            f64::from(border_cfg.width.max(0.0))
        } else {
            0.0
        };
        let threshold = (border_w + 4.0).max(6.0);

        let has_outputs = !self.all_outputs().is_empty();
        let output_name = match (has_outputs, self.output_at_point_strict(global_pos)) {
            (true, None) => return None,
            (_, Some(out)) => out.name(),
            (false, None) => self.focused_output_name.clone().unwrap_or_default(),
        };
        let ws_id = self.active_workspace_for_output(&output_name);
        let origin = self.workspace_output_origin(&ws_id);

        // A `special:<name>` Scratchpad sits above the workspace and owns the pointer while open.
        let mut candidates: Vec<&MappedWindow> = self
            .mapped_windows
            .iter()
            .filter(|m| m.ws_id == ws_id && !m.is_minimized)
            .collect();
        let focused = self
            .workspaces
            .get(&ws_id)
            .and_then(|ws| ws.active_window());
        // The presentation model owns fullscreen/maximize, so a presenting window is never a
        // frame-edge resize target (komawm/koma#30).
        let fullscreen = self
            .workspaces
            .get(&ws_id)
            .and_then(|ws| ws.fullscreen_window());
        // Focused window first, then the rest: shared tiled borders resize the focused tile.
        candidates.sort_by_key(|m| Some(m.win_id) != focused);

        for mapped in candidates {
            if fullscreen == Some(mapped.win_id) {
                continue;
            }
            let frame = LogicalRect::new(
                mapped.rect.x + origin.x as i32,
                mapped.rect.y + origin.y as i32,
                mapped.rect.w,
                mapped.rect.h,
            );
            let point = LogicalPoint {
                x: global_pos.x,
                y: global_pos.y,
            };
            if let Some(corner) = ResizeCorner::frame_edge_at(frame, point, threshold) {
                return Some((mapped.win_id, ws_id, corner));
            }
        }
        None
    }

    /// Refreshes the interactive-resize hover cursor for `global_pos` and returns the edge under it.
    ///
    /// Entering an edge saves the current cursor status; leaving restores it, so a client-provided
    /// cursor (I-beam, hand) is never permanently clobbered by Koma's resize arrow.
    pub fn refresh_frame_edge_hover(
        &mut self,
        global_pos: Point<f64, Logical>,
    ) -> Option<(WindowId, WorkspaceId, ResizeCorner)> {
        let edge = self.frame_edge_under_pointer(global_pos);
        match edge.clone() {
            Some((win_id, _ws_id, corner)) => {
                if self.frame_edge_hover.map(|(id, _)| id) != Some(win_id)
                    || self.frame_edge_hover.map(|(_, c)| c) != Some(corner)
                {
                    if self.frame_edge_hover.is_none() {
                        self.cursor_before_frame_edge = Some(self.cursor_manager.status().clone());
                    }
                    self.frame_edge_hover = Some((win_id, corner));
                }
                self.set_cursor_status(CursorImageStatus::Named(
                    crate::pointer_grabs::cursor_icon_for_corner(corner),
                ));
            }
            None => {
                if self.frame_edge_hover.take().is_some() {
                    let restore = self
                        .cursor_before_frame_edge
                        .take()
                        .unwrap_or_else(CursorImageStatus::default_named);
                    self.set_cursor_status(restore);
                }
            }
        }
        edge
    }

    /// Hit-tests `global_pos` across any open `special:<name>` Scratchpad first, then the output's
    /// active workspace, strictly in **front-to-back** Z-order (`FloatingStack` topmost -> bottom -> Tiled panes).
    #[must_use]
    pub fn hit_test_visible_surface(
        &self,
        global_pos: Point<f64, Logical>,
    ) -> Option<(WindowId, WorkspaceId, WlSurface, Point<f64, Logical>)> {
        let border_cfg = self.lua_sandbox.config_snapshot().appearance.border;
        let b_off = if border_cfg.enabled {
            f64::from(border_cfg.width.max(0.0))
        } else {
            0.0
        };
        let has_outputs = !self.all_outputs().is_empty();
        let target_output = self.output_at_point_strict(global_pos);
        if has_outputs && target_output.is_none() {
            return None;
        }
        let (out_origin, output_local, candidate_workspaces): (
            Point<f64, Logical>,
            Point<f64, Logical>,
            Vec<WorkspaceId>,
        ) = if let Some(ref out) = target_output {
            let origin = out.current_location().to_f64();
            let out_name = out.name();
            let include_special = self.focused_output_name().as_deref() == Some(out_name.as_str());
            let mut list = Vec::with_capacity(2);
            if include_special && let Some(sp) = &self.active_special_ws_id {
                list.push(sp.clone());
            }
            list.push(self.active_workspace_for_output(&out_name));
            (origin, global_pos - origin, list)
        } else {
            let mut list = Vec::with_capacity(2);
            if let Some(sp) = &self.active_special_ws_id {
                list.push(sp.clone());
            }
            list.push(self.active_ws_id.clone());
            (Point::default(), global_pos, list)
        };

        for ws_id in candidate_workspaces {
            if let Some((win_id, surf, origin)) =
                self.hit_test_window_in_workspace(&ws_id, output_local, out_origin, b_off)
            {
                return Some((win_id, ws_id, surf, origin));
            }
        }
        None
    }

    #[must_use]
    fn tab_indicator_window_at(
        &self,
        global_pos: Point<f64, Logical>,
    ) -> Option<(WorkspaceId, WindowId)> {
        let (target_ws_id, output_local) =
            if let Some(out) = self.output_at_point_strict(global_pos) {
                let origin = out.current_location().to_f64();
                let out_name = out.name();
                let ws_id = if self.focused_output_name().as_deref() == Some(out_name.as_str())
                    && let Some(sp) = &self.active_special_ws_id
                {
                    sp.clone()
                } else {
                    self.active_workspace_for_output(&out_name)
                };
                (ws_id, global_pos - origin)
            } else if self.all_outputs().is_empty() {
                (self.effective_interactive_ws_id(), global_pos)
            } else {
                return None;
            };
        let now_ms = self.elapsed_ms();
        let tab_win_id = self.workspaces.get(&target_ws_id).and_then(|ws| {
            if ws.layout_mode == LayoutMode::Scrolling && ws.presented_window_id().is_none() {
                ws.scrolling.hit_test_tab_indicator(
                    LogicalPoint {
                        x: output_local.x,
                        y: output_local.y,
                    },
                    ws.viewport,
                    now_ms,
                )
            } else {
                None
            }
        })?;
        Some((target_ws_id, tab_win_id))
    }

    fn apply_layer_click_interactivity(
        &mut self,
        layer_root: WlSurface,
        interactivity: KeyboardInteractivity,
    ) {
        match interactivity {
            KeyboardInteractivity::Exclusive => {
                self.focused_ondemand_layer = None;
                self.reconcile_keyboard_focus();
            }
            KeyboardInteractivity::OnDemand => {
                self.focused_ondemand_layer = Some(layer_root);
                self.reconcile_keyboard_focus();
            }
            _ => {}
        }
    }

    /// Handles pointer press keyboard focus rules across layer shells (`Exclusive` / `OnDemand` / `None`),
    /// windows, and session lock preemption.
    pub fn handle_pointer_button_focus(&mut self, global_pos: Point<f64, Logical>) {
        self.reconcile_session_lock();
        if self.is_locked_or_abandoned() {
            self.reconcile_keyboard_focus();
            return;
        }

        if let Some(PointerHitTarget::Layer {
            layer_root,
            interactivity,
            ..
        }) = self.layer_surface_under_tiers(global_pos, [Layer::Overlay, Layer::Top])
        {
            self.apply_layer_click_interactivity(layer_root, interactivity);
            return;
        }

        if self.tab_indicator_window_at(global_pos).is_some()
            || self.hit_test_visible_surface(global_pos).is_some()
        {
            self.focus_window_at_point(global_pos);
            return;
        }

        if let Some(PointerHitTarget::Layer {
            layer_root,
            interactivity,
            ..
        }) = self.layer_surface_under_tiers(global_pos, [Layer::Bottom, Layer::Background])
        {
            self.apply_layer_click_interactivity(layer_root, interactivity);
            return;
        }

        if self.focused_ondemand_layer.take().is_some() {
            self.reconcile_keyboard_focus();
        }
    }

    /// Activates & raises the window clicked at `global_pos` (and dismisses an open Scratchpad
    /// if the user clicks outside Scratchpad windows onto a background window!).
    pub fn focus_window_at_point(&mut self, global_pos: Point<f64, Logical>) {
        if self.is_locked_or_abandoned() {
            self.reconcile_keyboard_focus();
            return;
        }
        // An explicit click always wins over `focus_only` pointer-focus state.
        self.pointer_focus_window = None;

        if let Some((target_ws_id, tab_win_id)) = self.tab_indicator_window_at(global_pos) {
            let now_ms = self.elapsed_ms();
            self.focused_ondemand_layer = None;
            if let Some(hit_out) = self.output_at_point_strict(global_pos) {
                self.focused_output_name = Some(hit_out.name());
                if !self
                    .workspaces
                    .get(&target_ws_id)
                    .is_some_and(|w| w.is_special)
                {
                    self.active_ws_id = target_ws_id.clone();
                }
            }
            if let Some(ws) = self.workspaces.get_mut(&target_ws_id) {
                ws.activate_window(tab_win_id, now_ms);
            }
            self.sync_workspace_geometries(&target_ws_id);
            self.focus_active_deck_window();
            // A click can move the active workspace to another output; bars follow that here.
            sync_ext_workspaces(self);
            self.queue_redraw();
            return;
        }

        if let Some((win_id, hit_ws_id, _, _)) = self.hit_test_visible_surface(global_pos) {
            self.activate_window_hit(win_id, &hit_ws_id, global_pos);
        } else {
            self.focused_ondemand_layer = None;
            if let Some(hit_out) = self.output_at_point_strict(global_pos) {
                let hit_out_name = hit_out.name();
                self.focused_output_name = Some(hit_out_name.clone());
                self.active_ws_id = self.active_workspace_for_output(&hit_out_name);
            }
            self.focus_active_deck_window();
            sync_ext_workspaces(self);
        }
    }

    /// Activates and raises the resolved `win_id` of `hit_ws_id` (click focus and
    /// `follow_mouse = normal` share this path so both always agree).
    pub(crate) fn activate_window_hit(
        &mut self,
        win_id: WindowId,
        hit_ws_id: &WorkspaceId,
        global_pos: Point<f64, Logical>,
    ) {
        // Pointer-driven focus is a user interaction outside the repeat: stop a pending repeat.
        self.cancel_keybind_repeat_for_external_change("pointer_focus");
        let now_ms = self.elapsed_ms();
        self.focused_ondemand_layer = None;
        self.pointer_focus_window = None;
        if self.active_special_ws_id.as_ref() != Some(hit_ws_id) {
            self.active_special_ws_id = None;
        }
        if let Some(hit_out) = self.output_at_point_strict(global_pos) {
            let hit_out_name = hit_out.name();
            self.focused_output_name = Some(hit_out_name.clone());
            if !self.workspaces.get(hit_ws_id).is_some_and(|w| w.is_special) {
                self.output_workspaces
                    .insert(hit_out_name, hit_ws_id.clone());
                self.active_ws_id = hit_ws_id.clone();
            }
        }
        if let Some(ws) = self.workspaces.get_mut(hit_ws_id) {
            ws.activate_window(win_id, now_ms);
        }
        self.sync_workspace_geometries(hit_ws_id);
        self.focus_active_deck_window();
        // Click/`follow_mouse = normal` focus can switch the displayed workspace.
        sync_ext_workspaces(self);
        self.queue_redraw();
    }

    /// `follow_mouse = focus_only`: transfers seat keyboard focus to `win_id` without raising it or
    /// changing the workspace's active-window stacking, so the layout/tab order is untouched while
    /// `Activated`, foreign-toplevel and Lua focus state follow the pointer.
    pub(crate) fn focus_window_by_pointer(
        &mut self,
        win_id: WindowId,
        hit_ws_id: &WorkspaceId,
        global_pos: Point<f64, Logical>,
    ) {
        self.cancel_keybind_repeat_for_external_change("pointer_focus");
        self.focused_ondemand_layer = None;
        if let Some(hit_out) = self.output_at_point_strict(global_pos) {
            let hit_out_name = hit_out.name();
            self.focused_output_name = Some(hit_out_name.clone());
            if !self.workspaces.get(hit_ws_id).is_some_and(|w| w.is_special) {
                self.output_workspaces
                    .insert(hit_out_name, hit_ws_id.clone());
                self.active_ws_id = hit_ws_id.clone();
            }
        }
        self.pointer_focus_window = Some(win_id);
        self.focus_active_deck_window();
        // Hovering a window on another output can switch the displayed workspace.
        sync_ext_workspaces(self);
        self.queue_redraw();
    }

    /// Collects **front-to-back** `WindowSceneItem`s for a single workspace (`ws_id`).
    #[must_use]
    pub fn drop_preview_for_workspace(&self, ws_id: &WorkspaceId) -> Option<LogicalRect> {
        self.active_drop_preview
            .as_ref()
            .and_then(|(id, rect)| (id == ws_id).then_some(*rect))
    }

    /// Advances per-frame `ScrollingStrip::camera_spring` settlement, keeps resting anchored agent
    /// cursors aligned with moved/scrolled windows, and advances interactive drag edge auto-scroll
    /// (`30 px` trigger zone, `100 ms` debounce, `1500 px/s` ramp) before rendering!
    pub fn tick_frame_animations_and_edge_scroll(&mut self) {
        let now_ms = self.elapsed_ms();
        self.reap_settled_closing_windows();
        for ws in self.workspaces.values_mut() {
            let _ = ws.scrolling.sample_camera_x(now_ms);
        }

        let active_anim_ws: Vec<WorkspaceId> = self
            .pointer_animations
            .iter()
            .map(|a| a.ws_id.clone())
            .collect();
        for (ws_id, ac) in &mut self.agent_cursors {
            if active_anim_ws.contains(ws_id) {
                continue;
            }
            let Some((win_id, u, v)) = ac.anchored_window else {
                continue;
            };
            if let Some(ws) = self.workspaces.get(ws_id)
                && let Some((_, rect, _)) = ws
                    .compute_visible_geometries_at(now_ms)
                    .into_iter()
                    .find(|(id, _, _)| *id == win_id)
            {
                let tracked = LogicalPoint {
                    x: f64::from(rect.x) + u * f64::from(rect.w.max(1)),
                    y: f64::from(rect.y) + v * f64::from(rect.h.max(1)),
                };
                if (tracked.x - ac.ws_local_pos.x).abs() > 0.25
                    || (tracked.y - ac.ws_local_pos.y).abs() > 0.25
                {
                    ac.ws_local_pos = tracked;
                    ac.visual.track_anchored_position(tracked, now_ms);
                }
            }
        }

        if let Some((drag_ws_id, cursor_x)) = self.active_drag_edge_scroll.clone()
            && let Some(ws) = self.workspaces.get_mut(&drag_ws_id)
            && ws.layout_mode == LayoutMode::Scrolling
        {
            let vp = ws.viewport;
            let step = ws.scrolling.tick_edge_auto_scroll(cursor_x, vp, now_ms);
            self.edge_scroll_keep_alive = step.keeps_frame_loop_alive();
            if matches!(step, koma_model::EdgeScrollStep::Scrolled(_)) {
                let cursor = koma_model::LogicalPoint {
                    x: cursor_x,
                    y: self.pointer_location.y,
                };
                let dz = ws.scrolling.predict_drop_zone_2d(cursor, vp, now_ms);
                self.active_drop_preview = Some((drag_ws_id, dz.preview_rect));
            }
            return;
        }
        self.edge_scroll_keep_alive = false;
    }

    #[must_use]
    pub fn has_active_animations(&self) -> bool {
        let now_ms = self.elapsed_ms();
        let rotating_border = {
            let appearance = &self.lua_sandbox.config_snapshot().appearance;
            appearance.border.is_gradient_rotating()
                && self.mapped_windows.iter().any(|m| !m.is_minimized)
        };
        let ac_cfg = self.agent_cursor_config();
        let agent_cursor_anim = ac_cfg.enabled
            && self.agent_cursors.iter().any(|(ws_id, ac)| {
                self.is_workspace_visible_anywhere(ws_id)
                    && ac.visual.needs_animation_tick(now_ms, &ac_cfg)
            });
        self.edge_scroll_keep_alive
            || rotating_border
            || agent_cursor_anim
            || self
                .window_animations
                .borrow_mut()
                .has_active_animations(now_ms)
            || self
                .workspace_transitions
                .borrow_mut()
                .has_active_animations(now_ms)
            || self
                .workspaces
                .values()
                .any(|ws| ws.scrolling.is_camera_animating(now_ms))
    }

    /// Returns `true` when `appearance.animations.windows_close` can produce a non-instant close
    /// transition (so capturing an unmap/destroy snapshot is actually useful).
    #[must_use]
    pub(crate) fn close_animation_enabled(cfg: &koma_model::AnimationConfig) -> bool {
        cfg.enabled
            && cfg.windows_close.enabled != Some(false)
            && !matches!(
                cfg.channel_curve(&cfg.windows_close, None),
                CurveSpec::Instant
            )
            && !matches!(
                cfg.windows_close
                    .style
                    .as_deref()
                    .and_then(WindowOpenStyle::parse),
                Some(WindowOpenStyle::None)
            )
    }

    /// Installs the per-toplevel `wl_surface` pre-commit hook that snapshots a closing window
    /// right before an unmap commit (`attach(null) + commit`) discards its buffer, while doing
    /// zero offscreen rendering on active frame commits.
    pub fn register_toplevel_pre_commit_hook(&mut self, surface: &WlSurface) {
        #[derive(Clone, Copy)]
        enum PendingBufferOp {
            Removed,
            NewOrUnchanged,
        }

        if !surface.is_alive() {
            return;
        }
        let hook_id = add_pre_commit_hook::<Self, _>(surface, |state, _dh, surface| {
            let pending_op = with_states(surface, |states| {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                match guard.pending().buffer.as_ref() {
                    Some(BufferAssignment::Removed) => PendingBufferOp::Removed,
                    Some(BufferAssignment::NewBuffer(_)) | None => PendingBufferOp::NewOrUnchanged,
                }
            });
            let find_mapped_win_id = |state: &Self| {
                state
                    .mapped_windows
                    .iter()
                    .find(|m| {
                        m.window
                            .toplevel()
                            .is_some_and(|t| t.wl_surface() == surface)
                    })
                    .map(|m| m.win_id)
            };
            match pending_op {
                PendingBufferOp::Removed => {
                    let Some(win_id) = find_mapped_win_id(state) else {
                        return;
                    };
                    // Capture once before `on_commit_buffer_handler` discards the old buffer,
                    // keeping any snapshot already captured when a subsurface was destroyed.
                    if !state.window_frames.contains_key(&win_id) {
                        state.refresh_window_frame(win_id);
                    }
                }
                PendingBufferOp::NewOrUnchanged => {
                    // Normal commit: zero offscreen FBO work; evict any stale snapshot (for
                    // instance after a non-fatal subsurface destruction).
                    if !state.window_frames.is_empty()
                        && let Some(win_id) = find_mapped_win_id(state)
                    {
                        state.window_frames.remove(&win_id);
                    }
                }
            }
        });
        if let Some(prev) = self
            .toplevel_pre_commit_hooks
            .insert(surface.clone(), hook_id)
        {
            remove_pre_commit_hook(surface, &prev);
        }
    }

    /// Removes the per-toplevel pre-commit unmap-snapshot hook tracked for `surface`.
    pub fn remove_toplevel_pre_commit_hook(&mut self, surface: &WlSurface) {
        if let Some(hook_id) = self.toplevel_pre_commit_hooks.remove(surface) {
            remove_pre_commit_hook(surface, &hook_id);
        }
    }

    /// Captures `win_id`'s current committed surface tree into its offscreen snapshot cache.
    ///
    /// Called lazily right before an unmap commit (`BufferAssignment::Removed`) or surface /
    /// subsurface destruction drops the client's buffers, never on normal frame commits.
    pub fn refresh_window_frame(&mut self, win_id: WindowId) {
        let cfg = self.lua_sandbox.config_snapshot().appearance.animations;
        if !Self::close_animation_enabled(&cfg) {
            self.window_frames.remove(&win_id);
            return;
        }
        let Some(idx) = self.mapped_windows.iter().position(|m| m.win_id == win_id) else {
            return;
        };
        if self.mapped_windows[idx].is_minimized {
            return;
        }
        let rect = self.mapped_windows[idx].rect;
        if rect.w == 0 || rect.h == 0 {
            return;
        }
        #[cfg(test)]
        {
            self.unmap_snapshot_attempts += 1;
            self.last_snapshot_had_live_buffer = self.mapped_windows[idx]
                .window
                .toplevel()
                .is_some_and(|t| is_surface_mapped(t.wl_surface()));
        }
        let ws_id = self.mapped_windows[idx].ws_id.clone();
        let output_scale = self
            .workspaces
            .get(&ws_id)
            .map_or(1.0, |ws| ws.viewport.scale);
        let radius = f32::from(
            self.lua_sandbox
                .config_snapshot()
                .appearance
                .corner_radius
                .unwrap_or(10),
        );
        let Some(backend) = self.backend.as_mut() else {
            return;
        };
        let window = &self.mapped_windows[idx].window;
        let scale = smithay::utils::Scale::from(output_scale);
        let mut cache = self.window_frames.remove(&win_id).unwrap_or_default();
        let cached_ok = backend
            .with_primary_renderer(|renderer| {
                match window_snapshot_elements(renderer, window, rect, scale, radius) {
                    Ok(snapshot) => {
                        if let Err(err) =
                            cache.refresh(renderer, snapshot.size, scale, &snapshot.elements)
                        {
                            debug!(
                                win_id = win_id.slot,
                                %err,
                                "window frame cache refresh failed; dropping cached pixels"
                            );
                            false
                        } else {
                            true
                        }
                    }
                    Err(err) => {
                        debug!(
                            win_id = win_id.slot,
                            %err,
                            "window frame cache skipped: surface has no committed buffer"
                        );
                        false
                    }
                }
            })
            .unwrap_or(false);
        if cached_ok {
            self.window_frames.insert(win_id, cache);
        }
    }

    /// Starts the `appearance.animations.windows_close` spring for `win_id`, using an on-demand
    /// pre-commit/destruction snapshot in [`Self::window_frames`] or a live capture when the
    /// surface still holds its buffer.
    ///
    /// Returns `true` only when a captured frame is now composited. A `false` result means the
    /// caller must free the window synchronously: no snapshot was possible or animations are off.
    pub fn begin_close_animation(
        &mut self,
        win_id: WindowId,
        ws_id: &WorkspaceId,
        window: &Window,
        rect: LogicalRect,
    ) -> bool {
        let now_ms = self.elapsed_ms();
        // Take the cached frame first: whatever happens below, this window never needs it again.
        let mut cached_frame = self.window_frames.remove(&win_id);
        let appearance = self.lua_sandbox.config_snapshot().appearance;
        let cfg = appearance.animations.clone();
        if !Self::close_animation_enabled(&cfg) {
            self.window_animations
                .borrow_mut()
                .remove_immediately(win_id);
            return false;
        }
        let Some(ws_viewport) = self.workspaces.get(ws_id).map(|ws| ws.viewport) else {
            self.window_animations
                .borrow_mut()
                .remove_immediately(win_id);
            return false;
        };
        let radius = f32::from(appearance.corner_radius.unwrap_or(10));
        let output_scale = smithay::utils::Scale::from(ws_viewport.scale);
        let cached = self
            .backend
            .as_mut()
            .and_then(|backend| match cached_frame.as_mut() {
                Some(cache) => backend.with_primary_renderer(|renderer| cache.read_back(renderer)),
                None => Some(Ok(None)),
            });
        let live_capture = |state: &mut Self| {
            state.backend.as_mut().and_then(|backend| {
                backend.with_primary_renderer(|renderer| {
                    capture_window_snapshot(renderer, window, rect, output_scale, radius)
                })
            })
        };
        let snapshot = match cached {
            Some(Ok(Some(captured))) => Some(Ok(captured)),
            Some(Err(err)) => {
                debug!(
                    win_id = win_id.slot,
                    %err,
                    "close animation: cached frame read-back failed, retrying live capture"
                );
                live_capture(self)
            }
            Some(Ok(None)) | None => live_capture(self),
        };
        let (snapshot, snapshot_size) = match snapshot {
            Some(Ok(captured)) => captured,
            Some(Err(err)) => {
                debug!(
                    win_id = win_id.slot,
                    %err,
                    "close animation skipped: toplevel has no snappable frame"
                );
                self.window_animations
                    .borrow_mut()
                    .remove_immediately(win_id);
                return false;
            }
            None => {
                debug!(
                    win_id = win_id.slot,
                    "close animation skipped: no active renderer"
                );
                self.window_animations
                    .borrow_mut()
                    .remove_immediately(win_id);
                return false;
            }
        };
        if !self.window_animations.borrow_mut().start_close(
            win_id,
            rect,
            ws_viewport,
            now_ms,
            &cfg,
            None,
        ) {
            return false;
        }
        debug!(win_id = win_id.slot, "close animation started");
        self.closing_windows.retain(|c| c.win_id != win_id);
        self.closing_windows.push(ClosingWindow {
            win_id,
            ws_id: ws_id.clone(),
            snapshot,
            snapshot_size,
        });
        self.queue_redraw();
        true
    }

    /// Scene items for the captured closing frames of `ws_id`, sampled at the current time.
    ///
    /// Only pixels are returned: the windows are already gone from `mapped_windows`, so layout,
    /// hit-testing, focus and `scene.inventory` never see them again.
    #[must_use]
    pub fn closing_window_scene_items(
        &self,
        ws_id: &WorkspaceId,
    ) -> Vec<ClosingWindowSceneItem<'_>> {
        let now_ms = self.elapsed_ms();
        let mut animations = self.window_animations.borrow_mut();
        self.closing_windows
            .iter()
            .filter(|closing| &closing.ws_id == ws_id)
            .filter_map(|closing| {
                let sampled = animations.sample_close(closing.win_id, now_ms)?;
                if sampled.opacity_multiplier <= 0.002 {
                    return None;
                }
                Some(ClosingWindowSceneItem {
                    snapshot: &closing.snapshot,
                    snapshot_size: closing.snapshot_size,
                    rect: sampled.rect,
                    opacity: sampled.opacity_multiplier,
                })
            })
            .collect()
    }

    /// Releases captured closing frames once their `windows_close` spring has settled.
    pub fn reap_settled_closing_windows(&mut self) {
        let now_ms = self.elapsed_ms();
        let settled = self
            .window_animations
            .borrow_mut()
            .take_settled_closing(now_ms);
        if settled.is_empty() {
            return;
        }
        self.closing_windows
            .retain(|closing| !settled.contains(&closing.win_id));
        self.queue_redraw();
    }

    pub fn collect_workspace_scene_items<'a>(
        &'a self,
        ws_id: &WorkspaceId,
        is_screencast_or_agent_capture: bool,
    ) -> Vec<WindowSceneItem<'a>> {
        let Some(ws) = self.workspaces.get(ws_id) else {
            return Vec::new();
        };
        let focused_id = ws.active_window();
        let fullscreen_id = ws.fullscreen_window();
        let is_agent_ws = matches!(ws.presentation, PresentationTarget::Offscreen { .. });
        let now_ms = self.elapsed_ms();
        let is_scrolling_strip =
            ws.presented_window_id().is_none() && ws.layout_mode == LayoutMode::Scrolling;
        let camera_shift_px = if is_scrolling_strip {
            if is_screencast_or_agent_capture || is_agent_ws {
                ws.scrolling.camera_x.round() as i32
            } else {
                ws.scrolling.peek_camera_x(now_ms).round() as i32
            }
        } else {
            0
        };
        let ws_transform = if is_screencast_or_agent_capture || is_agent_ws {
            koma_model::SampledWorkspaceTransform::default()
        } else {
            let hosting_out = match &ws.presentation {
                PresentationTarget::Physical(out_id) => Some(out_id.0.as_str()),
                PresentationTarget::Offscreen { .. } => None,
            };
            hosting_out.map_or_else(Default::default, |out| {
                self.workspace_transitions.borrow_mut().sample(out, now_ms)
            })
        };
        let candidates_bottom_to_top = ws.compute_animation_target_geometries();
        let appearance = &self.lua_sandbox.config_snapshot().appearance;
        let default_radius = appearance.corner_radius.unwrap_or(10);
        let blur_enabled = appearance.blur.enabled;
        let border_cfg = appearance.border.clone();
        let focus_ring_cfg = appearance.focus_ring.clone();
        let shadow_cfg = appearance.shadow.clone();
        let dim_cfg = appearance.effective_dim();

        let mut items = Vec::with_capacity(candidates_bottom_to_top.len());
        for (win_id, target_rect, is_floating) in candidates_bottom_to_top.into_iter().rev() {
            if let Some(mapped) = self.mapped_windows.iter().find(|m| m.win_id == win_id) {
                if mapped.is_minimized {
                    continue;
                }
                let is_focused = Some(win_id) == focused_id;
                let dyn_rules = self.rules.resolve_dynamic(&WindowMatchContext {
                    app_id: &mapped.app_id,
                    title: &mapped.title,
                    is_floating,
                    is_focused,
                    is_xwayland: false,
                    is_modal_dialog: false,
                    is_screencast_target: is_screencast_or_agent_capture,
                    is_agent_workspace: is_agent_ws,
                });
                let is_protected = dyn_rules
                    .block_out_from
                    .is_some_and(|p| p.blocks_agent_capture());

                let base_opacity = if is_focused {
                    dyn_rules.opacity.unwrap_or(1.0)
                } else {
                    dyn_rules
                        .inactive_opacity
                        .or(dyn_rules.opacity)
                        .unwrap_or(1.0)
                };
                let is_fs = fullscreen_id == Some(win_id);
                let corner_radius = if is_fs {
                    0.0
                } else {
                    f32::from(dyn_rules.corner_radius.unwrap_or(default_radius))
                };

                let (mut render_rect, eff_opacity, focus_factor) =
                    if is_screencast_or_agent_capture || is_agent_ws {
                        (
                            target_rect,
                            base_opacity,
                            if is_focused { 1.0 } else { 0.0 },
                        )
                    } else {
                        let sampled = self.window_animations.borrow_mut().sample_visual(
                            win_id,
                            target_rect,
                            now_ms,
                        );
                        (
                            sampled.rect,
                            (base_opacity
                                * sampled.opacity_multiplier
                                * ws_transform.opacity_multiplier)
                                .clamp(0.0, 1.0),
                            sampled.focus_factor,
                        )
                    };

                render_rect.x += ws_transform.offset_x;
                render_rect.y += ws_transform.offset_y;

                if is_scrolling_strip && !is_floating {
                    render_rect.x -= camera_shift_px;
                    if render_rect.x + render_rect.w as i32 <= 0
                        || render_rect.x >= ws.viewport.width as i32
                    {
                        continue;
                    }
                }

                let is_waydroid = is_waydroid_app_id(&mapped.app_id);
                let is_click_through = is_waydroid
                    && mapped.window.toplevel().is_some_and(|t| {
                        let root = t.wl_surface();
                        is_surface_mapped(root) && surface_has_empty_input_region(root)
                    });
                let eff_no_blur = dyn_rules.no_blur.unwrap_or(is_waydroid);
                let eff_clip_to_geometry =
                    dyn_rules.clip_to_geometry.unwrap_or(true) && !is_click_through;
                let mut eff_border = border_cfg.clone();
                let mut eff_focus_ring = focus_ring_cfg.clone();
                let mut eff_shadow = shadow_cfg.clone();
                if is_click_through || is_fs {
                    // A fullscreen window is the screen: Hyprland draws no border, no focus ring and
                    // no shadow around it, and the owner reported the border ring surviving on
                    // `Mod + Shift + F` (komawm/koma#30 follow-up). The corner radius is already 0
                    // for fullscreen, and the shadow would otherwise bleed past the output edge.
                    eff_border.enabled = false;
                    eff_focus_ring.enabled = false;
                    eff_shadow.enabled = false;
                }

                items.push(WindowSceneItem {
                    window: &mapped.window,
                    rect: render_rect,
                    is_floating,
                    is_focused,
                    focus_factor,
                    is_protected_surface: is_protected,
                    opacity: eff_opacity,
                    corner_radius,
                    clip_to_geometry: eff_clip_to_geometry,
                    wants_blur: blur_enabled && !is_fs && !is_click_through && !eff_no_blur,
                    no_shadow: is_fs || is_click_through || dyn_rules.no_shadow.unwrap_or(false),
                    border_color_override: if is_click_through {
                        None
                    } else {
                        dyn_rules.border_color
                    },
                    gradient_time_ms: if is_screencast_or_agent_capture || is_agent_ws {
                        0
                    } else {
                        now_ms
                    },
                    border: eff_border,
                    focus_ring: eff_focus_ring,
                    shadow: eff_shadow,
                    dim: if is_click_through {
                        Default::default()
                    } else {
                        dim_cfg
                    },
                });
            }
        }
        items
    }

    /// Collects **front-to-back** scene lists for a specific physical output (`output_name`):
    /// `(base_ws_items, Option<(special_lease_state, special_ws_items)>)`.
    #[must_use]
    pub fn collect_output_scene_items(
        &self,
        output_name: &str,
    ) -> (
        Vec<WindowSceneItem<'_>>,
        Option<(WorkspaceLeaseState, Vec<WindowSceneItem<'_>>)>,
    ) {
        let base_ws_id = self.active_workspace_for_output(output_name);
        let base_items = self.collect_workspace_scene_items(&base_ws_id, false);
        let is_focused = self.focused_output_name().as_deref() == Some(output_name);
        let special_overlay = if is_focused {
            self.active_special_ws_id.as_ref().and_then(|sp_id| {
                let sp_ws = self.workspaces.get(sp_id)?;
                let sp_items = self.collect_workspace_scene_items(sp_id, false);
                Some((sp_ws.lease.state().clone(), sp_items))
            })
        } else {
            None
        };
        (base_items, special_overlay)
    }

    /// Layer-shell tiers composited for one workspace's output frame.
    ///
    /// Front-to-back assembly draws `above_toplevels` before the window scene and `below_toplevels`
    /// after it. While a window on `ws_id` is `Fullscreen` the `Top`/`Overlay` tiers (status bars,
    /// launchers) are suppressed for that output so the fullscreen window stays front-most; a
    /// `Maximized` window keeps them, because it only fills the non-exclusive work area.
    #[must_use]
    pub fn output_layer_tiers(&self, ws_id: &WorkspaceId) -> OutputLayerTiers {
        let fullscreen = self
            .workspaces
            .get(ws_id)
            .is_some_and(|ws| ws.fullscreen_window().is_some());
        OutputLayerTiers {
            above_toplevels: if fullscreen {
                &[]
            } else {
                &FOREGROUND_LAYER_TIERS
            },
            below_toplevels: &BACKGROUND_LAYER_TIERS,
        }
    }

    /// Returns visible toplevel `WlSurface` roots displayed on `output_name`.
    #[must_use]
    pub fn visible_physical_toplevel_surfaces_for_output(
        &self,
        output_name: &str,
    ) -> Vec<WlSurface> {
        let mut surfaces = Vec::new();
        let base_ws_id = self.active_workspace_for_output(output_name);
        let is_focused = self.focused_output_name().as_deref() == Some(output_name);
        let special_iter = if is_focused {
            self.active_special_ws_id.as_ref()
        } else {
            None
        };
        for ws_id in special_iter.into_iter().chain(std::iter::once(&base_ws_id)) {
            if let Some(ws) = self.workspaces.get(ws_id) {
                for (win_id, _, _) in ws.compute_visible_geometries() {
                    if let Some(m) = self
                        .mapped_windows
                        .iter()
                        .find(|m| m.win_id == win_id && !m.is_minimized)
                        && let Some(t) = m.window.toplevel()
                    {
                        surfaces.push(t.wl_surface().clone());
                    }
                }
            }
        }
        surfaces
    }

    pub fn configure_child_env(
        &self,
        cmd: &mut std::process::Command,
        target_ws: Option<&WorkspaceId>,
    ) {
        let wayland_display = target_ws
            .and_then(|ws_id| self.jailed_wayland_sockets.get(ws_id))
            .map_or(self.socket_name.as_os_str(), |jail| {
                jail.socket_path.as_os_str()
            });

        let x11_display = target_ws
            .and_then(|ws_id| self.workspace_satellites.get(ws_id))
            .or(self.primary_satellite.as_ref())
            .map(|s| s.display_name());

        crate::child_env::apply_base_compositor_env_to_command(
            cmd,
            crate::child_env::is_session_mode(),
            wayland_display,
            x11_display,
            self.instance_signature.as_deref(),
        );

        for (k, v) in &self.lua_sandbox.config_snapshot().env_vars {
            cmd.env(k, v);
        }
    }

    pub fn queue_redraw(&mut self) {
        self.screencopy_state.mark_damage();
        self.redraw_queued = true;
        if let Some(backend) = &mut self.backend {
            backend.request_redraw();
        }
    }

    /// Updates active cursor status, clears any pending animation timer, and requests redraw.
    pub fn set_cursor_status(&mut self, status: CursorImageStatus) {
        self.cursor_manager.set_status(status);
        if let Some(token) = self.cursor_anim_timer_token.take()
            && let Some(handle) = &self.loop_handle
        {
            handle.remove(token);
        }
        self.queue_redraw();
    }

    /// Arms a one-shot timer to advance an animated XCursor to its next frame.
    pub fn schedule_cursor_animation_redraw(&mut self, delay_ms: u64) {
        if self.cursor_anim_timer_token.is_some() {
            return;
        }
        let Some(handle) = self.loop_handle.clone() else {
            return;
        };
        let timer = calloop::timer::Timer::from_duration(Duration::from_millis(delay_ms.max(8)));
        if let Ok(token) = handle.insert_source(timer, |_, _, state| {
            state.cursor_anim_timer_token = None;
            state.queue_redraw();
            calloop::timer::TimeoutAction::Drop
        }) {
            self.cursor_anim_timer_token = Some(token);
        }
    }

    /// Repeat timing for `WhileHeld` binds, taken from the applied
    /// `koma.input.keyboard.repeat_delay` / `repeat_rate` settings (`komawm/koma#76`).
    #[must_use]
    pub fn keybind_repeat_timing(&self) -> RepeatTiming {
        RepeatTiming::from_input_settings(
            self.applied_keyboard_repeat.0,
            self.applied_keyboard_repeat.1,
        )
    }

    /// Cancels a pending repeat-while-held keybind and its timer.
    ///
    /// Called on key release, modifier change, focus/workspace change, pointer grab, session
    /// lock and configuration reload.
    pub fn cancel_keybind_repeat(&mut self, reason: &'static str) {
        if let Some(cancelled) = self.keybinds.repeat.cancel() {
            self.drop_keybind_repeat_timer(reason, cancelled.bind_id);
        }
    }

    /// Cancels a repeat because focus, workspace, grab or lock changed outside the repeated
    /// action itself; a repeat that causes the change keeps running.
    pub fn cancel_keybind_repeat_for_external_change(&mut self, reason: &'static str) {
        if let Some(cancelled) = self.keybinds.repeat.cancel_for_external_change() {
            self.drop_keybind_repeat_timer(reason, cancelled.bind_id);
        }
    }

    fn drop_keybind_repeat_timer(&mut self, reason: &'static str, bind_id: BindHandleId) {
        if let Some(token) = self.keybind_repeat_timer_token.take()
            && let Some(handle) = &self.loop_handle
        {
            handle.remove(token);
        }
        tracing::debug!(reason, ?bind_id, "cancelled keybind repeat");
    }

    /// The client drag icon currently attached to the cursor, if a drag provided one.
    #[must_use]
    pub fn dnd_icon(&self) -> Option<&DndIcon> {
        self.dnd_drag.as_ref().and_then(ActiveDnd::icon)
    }

    /// Cancels the active client drag-and-drop grab and drops its icon.
    ///
    /// Used when the drag cannot continue (icon surface destroyed, session lock acquired) so the
    /// client receives `wl_data_source.cancelled` promptly instead of being left with a stuck
    /// grab. `DndGrabHandler::cancelled` clears the tracked state, so this only has to release the
    /// seat grab and request a redraw.
    pub fn cancel_client_drag(&mut self, reason: &'static str) {
        if self.dnd_drag.take().is_none() {
            return;
        }
        tracing::debug!(reason, "cancelling client drag-and-drop grab");
        let serial = SERIAL_COUNTER.next_serial();
        let time = smithay::backend::input::InputTime::now();
        // `KomaState::ptr`/`touch` respect the seat re-entrancy guards, so a cancel triggered from
        // inside a seat callback fails soft instead of wedging the event loop.
        if let Some(pointer) = self.ptr()
            && pointer.is_grabbed()
        {
            pointer.unset_grab(self, serial, time);
        }
        if let Some(touch) = self.touch()
            && touch.is_grabbed()
        {
            touch.unset_grab(self);
        }
        self.queue_redraw();
    }
}

impl KomaState {
    /// Acquires the seat keyboard for the duration of the returned guard.
    ///
    /// Returns `None` when this thread already owns a keyboard scope: Smithay's
    /// `KeyboardHandle` mutex is not reentrant, so the nested acquisition would wedge the
    /// event loop forever (komawm/koma#37, #54, #64). Callers must skip the work instead.
    #[must_use]
    #[track_caller]
    pub fn kb(&self) -> Option<SeatHandleGuard<smithay::input::keyboard::KeyboardHandle<Self>>> {
        let scope = crate::seat_guard::acquire(SeatHandle::Keyboard)?;
        let handle = self.seat.get_keyboard()?;
        Some(SeatHandleGuard::new(handle, scope))
    }

    /// Acquires the seat pointer for the duration of the returned guard.
    ///
    /// Returns `None` on a nested acquisition; see [`Self::kb`].
    #[must_use]
    #[track_caller]
    pub fn ptr(&self) -> Option<SeatHandleGuard<smithay::input::pointer::PointerHandle<Self>>> {
        let scope = crate::seat_guard::acquire(SeatHandle::Pointer)?;
        let handle = self.seat.get_pointer()?;
        Some(SeatHandleGuard::new(handle, scope))
    }

    /// Acquires the seat touch handle for the duration of the returned guard.
    ///
    /// Returns `None` on a nested acquisition; see [`Self::kb`].
    #[must_use]
    #[track_caller]
    pub fn touch(&self) -> Option<SeatHandleGuard<smithay::input::touch::TouchHandle<Self>>> {
        let scope = crate::seat_guard::acquire(SeatHandle::Touch)?;
        let handle = self.seat.get_touch()?;
        Some(SeatHandleGuard::new(handle, scope))
    }

    /// Acquires the seat text-input handle for the duration of the returned guard.
    ///
    /// Returns `None` on a nested acquisition; see [`Self::kb`].
    #[must_use]
    #[track_caller]
    pub fn text_input(
        &self,
    ) -> Option<SeatHandleGuard<&smithay::wayland::text_input::TextInputHandle>> {
        let scope = crate::seat_guard::acquire(SeatHandle::TextInput)?;
        Some(SeatHandleGuard::new(self.seat.text_input(), scope))
    }

    /// Acquires the seat input-method handle for the duration of the returned guard.
    ///
    /// Returns `None` on a nested acquisition; see [`Self::kb`].
    #[must_use]
    #[track_caller]
    pub fn input_method(
        &self,
    ) -> Option<SeatHandleGuard<&smithay::wayland::input_method::InputMethodHandle>> {
        let scope = crate::seat_guard::acquire(SeatHandle::InputMethod)?;
        Some(SeatHandleGuard::new(self.seat.input_method(), scope))
    }

    /// Clones the seat keyboard without claiming a scope.
    ///
    /// Only for *input entry points* that keep the handle across compositor dispatch
    /// (key-binding filters, virtual-keyboard forwarding). Smithay releases its keyboard mutex
    /// between those calls, so a scope here would reject legitimate re-application of keyboard
    /// configuration. Re-entry protection on those paths comes from the callback scopes that
    /// Smithay-held callbacks mark themselves ([`crate::seat_guard::callback`]).
    #[must_use]
    pub fn keyboard_handle(&self) -> Option<smithay::input::keyboard::KeyboardHandle<Self>> {
        self.seat.get_keyboard()
    }

    /// Clones the seat pointer without claiming a scope; see [`Self::keyboard_handle`].
    #[must_use]
    pub fn pointer_handle(&self) -> Option<smithay::input::pointer::PointerHandle<Self>> {
        self.seat.get_pointer()
    }

    /// Fail-closed IME barrier enforced whenever `SessionLockPhase != Unlocked`:
    /// drops active `zwp_text_input_v3` focus/enter and discards all IME popup surfaces.
    ///
    /// Safe to call from inside a Smithay seat callback: it never re-enters the seat handles.
    /// Callers that are *not* running inside [`crate::state::KomaState::focus_changed`] should
    /// also reconcile keyboard-shortcut inhibitors ([`Self::clear_ime_state_for_lock`] does).
    pub fn clear_ime_focus_for_lock(&mut self) {
        {
            let Some(text_input) = self.text_input() else {
                // Nested acquisition (the caller already owns the text-input handle): popping the
                // popups below is still safe and is the fail-closed behaviour we want.
                self.ime_popups.clear();
                return;
            };
            if text_input.focus().is_some() {
                text_input.leave();
                text_input.set_focus(None);
            }
        }
        self.ime_popups.clear();
    }

    /// Full lock barrier: drops IME focus/popups and re-evaluates keyboard-shortcut inhibitors.
    ///
    /// Must NOT be called from a seat callback that already holds the keyboard mutex
    /// (`SeatHandler::focus_changed`), because [`Self::reconcile_keyboard_shortcuts_inhibitors`]
    /// takes `KeyboardHandle::current_focus()` on the same non-reentrant mutex
    /// (komawm/koma#37: `ext-session-lock-v1` acquire deadlocked the whole event loop).
    pub fn clear_ime_state_for_lock(&mut self) {
        self.clear_ime_focus_for_lock();
        self.reconcile_keyboard_shortcuts_inhibitors();
    }

    #[must_use]
    pub fn is_bare_metal_tty(&self) -> bool {
        match self.backend {
            Some(ActiveBackend::Winit(_)) | None => false,
            #[cfg(feature = "smithay-tty")]
            Some(ActiveBackend::Tty(_)) => true,
        }
    }

    #[must_use]
    pub fn next_presentation_seq(&self) -> u64 {
        self.presentation_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Unconstrains an `xdg_popup` relative to its root toplevel window or `wlr_layer_shell` panel.
    pub fn unconstrain_xdg_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };

        let mut target: Option<(Rectangle<i32, Logical>, Point<i32, Logical>)> = None;

        if let Some(m) = self
            .mapped_windows
            .iter()
            .find(|m| m.window.toplevel().is_some_and(|t| *t.wl_surface() == root))
        {
            let ws_origin = self.workspace_output_origin(&m.ws_id);
            let vis_rect = self
                .workspaces
                .get(&m.ws_id)
                .and_then(|ws| {
                    ws.compute_visible_geometries_at(self.elapsed_ms())
                        .into_iter()
                        .find(|(id, _, _)| *id == m.win_id)
                        .map(|(_, rect, _)| rect)
                })
                .unwrap_or(m.rect);
            let output_rect = self
                .workspaces
                .get(&m.ws_id)
                .and_then(|ws| match &ws.presentation {
                    PresentationTarget::Physical(id) => {
                        self.all_outputs().into_iter().find(|o| o.name() == id.0)
                    }
                    PresentationTarget::Offscreen { .. } => None,
                })
                .or_else(|| {
                    self.output_at_point_strict(
                        Point::from((f64::from(vis_rect.x), f64::from(vis_rect.y))) + ws_origin,
                    )
                })
                .or_else(|| self.primary_output())
                .map(|o| self.output_logical_rect(&o))
                .unwrap_or_else(|| {
                    let vp = self
                        .workspaces
                        .get(&m.ws_id)
                        .map(|ws| ws.viewport)
                        .unwrap_or_default();
                    Rectangle::from_size((vp.width as i32, vp.height as i32).into())
                });
            let is_fs = self
                .workspaces
                .get(&m.ws_id)
                .is_some_and(|ws| ws.is_fullscreen(m.win_id));
            let border_w = if is_fs {
                0
            } else {
                let border_cfg = &self.lua_sandbox.config_snapshot().appearance.border;
                if border_cfg.enabled {
                    border_cfg.width.round() as i32
                } else {
                    0
                }
            };
            let root_global_loc = Point::from((
                ws_origin.x.round() as i32 + vis_rect.x + border_w,
                ws_origin.y.round() as i32 + vis_rect.y + border_w,
            ));
            target = Some((output_rect, root_global_loc));
        }

        if target.is_none() {
            for output in self.all_outputs() {
                let map = layer_map_for_output(&output);
                if let Some(layer) = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                    && let Some(layer_geo) = map.layer_geometry(layer)
                {
                    let output_rect = self.output_logical_rect(&output);
                    let root_global_loc = output_rect.loc + layer_geo.loc;
                    target = Some((output_rect, root_global_loc));
                    break;
                }
            }
        }

        let Some((mut constraint_rect, root_global_loc)) = target else {
            return;
        };
        constraint_rect.loc -= root_global_loc;
        constraint_rect.loc -= get_popup_toplevel_coords(&kind);

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(constraint_rect);
        });
    }

    /// Reconciles active `zwp_keyboard_shortcuts_inhibitor_v1` instances against lock/preempt/focus.
    pub fn reconcile_keyboard_shortcuts_inhibitors(&mut self) {
        let focused = self.kb().and_then(|kb| kb.current_focus());
        self.reconcile_keyboard_shortcuts_inhibitors_with_focus(focused.as_ref());
    }

    /// Reconciles keyboard shortcuts inhibitors using an explicit focused surface without
    /// re-locking Smithay's `KeyboardHandle` internal mutex (required inside `focus_changed`).
    pub fn reconcile_keyboard_shortcuts_inhibitors_with_focus(
        &mut self,
        focused: Option<&WlSurface>,
    ) {
        self.active_shortcuts_inhibitors
            .retain(|inh| inh.wl_surface().alive());
        let locked = self.is_locked_or_abandoned();
        let active_ws_id = self.effective_interactive_ws_id();
        let human_preempted = self
            .workspaces
            .get(&active_ws_id)
            .is_some_and(|ws| ws.lease.is_human_preempted());

        for inhibitor in &self.active_shortcuts_inhibitors {
            let inh_surf = inhibitor.wl_surface();
            let restricted = is_surface_from_restricted_client(self, inh_surf);
            let matches_focus = focused.is_some_and(|f| surfaces_share_root(f, inh_surf));
            let should_activate = !locked && !human_preempted && !restricted && matches_focus;
            if should_activate {
                if !inhibitor.is_active() {
                    inhibitor.activate();
                }
            } else if inhibitor.is_active() {
                inhibitor.inactivate();
            }
        }
    }

    #[must_use]
    pub fn are_shortcuts_inhibited_for_focused_surface(&self) -> bool {
        if self.is_locked_or_abandoned() {
            return false;
        }
        let active_ws_id = self.effective_interactive_ws_id();
        if self
            .workspaces
            .get(&active_ws_id)
            .is_some_and(|ws| ws.lease.is_human_preempted())
        {
            return false;
        }
        let Some(focused) = self.kb().and_then(|kb| kb.current_focus()) else {
            return false;
        };
        if is_surface_from_restricted_client(self, &focused) {
            return false;
        }
        self.active_shortcuts_inhibitors.iter().any(|inh| {
            inh.wl_surface().alive()
                && inh.is_active()
                && surfaces_share_root(&focused, inh.wl_surface())
        })
    }

    /// Prunes destroyed IME popup handles alongside XDG popups during event-loop cleanup.
    pub fn cleanup_ime_popups(&mut self) {
        self.ime_popups.retain(|popup| popup.alive());
    }

    /// Computes an `Output`'s logical bounding rectangle in global compositor coordinates.
    #[must_use]
    pub fn output_logical_rect(&self, output: &Output) -> Rectangle<i32, Logical> {
        let loc = output.current_location();
        let scale = output.current_scale().fractional_scale().max(0.01);
        let size = output
            .current_mode()
            .map(|mode| {
                output
                    .current_transform()
                    .transform_size(mode.size)
                    .to_f64()
                    .to_logical(scale)
                    .to_i32_round()
            })
            .unwrap_or_else(|| {
                self.workspaces
                    .get(&self.active_ws_id)
                    .map(|ws| (ws.viewport.width as i32, ws.viewport.height as i32).into())
                    .unwrap_or_else(|| (1920, 1080).into())
            });
        Rectangle::new(loc, size)
    }

    /// Resolves the global logical geometry of a parent `wl_surface` hosting an IME popup.
    #[must_use]
    pub fn ime_parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, Logical> {
        let mut root = parent.clone();
        while let Some(ancestor) = smithay::wayland::compositor::get_parent(&root) {
            root = ancestor;
        }

        let active_ws = self.active_ws_id.clone();
        let active_special = self.active_special_ws_id.clone();

        // Check active mapped windows first, preferring the active interactive workspace.
        let win_match = self
            .mapped_windows
            .iter()
            .filter(|m| m.ws_id == active_ws || active_special.as_ref() == Some(&m.ws_id))
            .chain(self.mapped_windows.iter())
            .find(|m| m.window.toplevel().is_some_and(|t| t.wl_surface() == &root));

        if let Some(m) = win_match {
            let border = self.lua_sandbox.config_snapshot().appearance.border;
            let b_off = if border.enabled {
                f64::from(border.width.max(0.0))
            } else {
                0.0
            };
            let ws_origin = self.workspace_output_origin(&m.ws_id);
            let inner_origin =
                Point::from((f64::from(m.rect.x) + b_off, f64::from(m.rect.y) + b_off));
            let win_surface_origin = surface_origin(inner_origin, m.window.geometry().loc);
            let inner_size = Size::from((
                (f64::from(m.rect.w) - 2.0 * b_off).max(1.0).round() as i32,
                (f64::from(m.rect.h) - 2.0 * b_off).max(1.0).round() as i32,
            ));
            return Rectangle::new((ws_origin + win_surface_origin).to_i32_round(), inner_size);
        }

        for output in self.all_outputs() {
            let out_origin = output.current_location();
            let map = layer_map_for_output(&output);
            for layer in map.layers() {
                if layer.wl_surface() == &root
                    && let Some(layer_geo) = map.layer_geometry(layer)
                {
                    return Rectangle::new(out_origin + layer_geo.loc, layer_geo.size);
                }
            }
        }

        self.primary_output()
            .map(|o| self.output_logical_rect(&o))
            .unwrap_or_default()
    }

    /// Repositions `popup` near the active text-input cursor rectangle while constraining it
    /// inside the target output bounds (placing below the cursor, or flipping above if needed).
    pub fn unconstrain_ime_popup(&self, popup: &ImePopupSurface) {
        if self.is_locked_or_abandoned() {
            return;
        }
        let Some(parent) = popup.get_parent() else {
            return;
        };
        let parent_geo = self.ime_parent_geometry(&parent.surface);
        let parent_loc = if parent_geo.size.w > 0 && parent_geo.size.h > 0 {
            parent_geo.loc
        } else {
            parent.location.loc
        };

        let text_rect = popup.text_input_rectangle();
        let cursor_global = Rectangle::new(parent_loc + text_rect.loc, text_rect.size);

        let popup_bbox =
            smithay::desktop::utils::bbox_from_surface_tree(popup.wl_surface(), (0, 0));
        let popup_size: Size<i32, Logical> =
            Size::from((popup_bbox.size.w.max(1), popup_bbox.size.h.max(1)));

        let target_output = self
            .all_outputs()
            .into_iter()
            .find(|out| self.output_logical_rect(out).contains(cursor_global.loc))
            .or_else(|| self.primary_output());

        let Some(output) = target_output else {
            return;
        };
        let out_geo = self.output_logical_rect(&output);

        let max_x = out_geo.loc.x + (out_geo.size.w - popup_size.w).max(0);
        let x = cursor_global.loc.x.clamp(out_geo.loc.x, max_x);

        let below_y = cursor_global.loc.y + cursor_global.size.h.max(1);
        let above_y = cursor_global.loc.y - popup_size.h;
        let max_y = out_geo.loc.y + (out_geo.size.h - popup_size.h).max(0);

        let y = if below_y + popup_size.h <= out_geo.loc.y + out_geo.size.h {
            below_y
        } else if above_y >= out_geo.loc.y {
            above_y
        } else {
            below_y.clamp(out_geo.loc.y, max_y)
        };

        // Store location relative to `parent.location.loc` while rendering computes relative to live `parent_loc`.
        popup.set_location(Point::from((x, y)) - parent_loc);
    }

    /// Returns live mapped IME popup `(WlSurface, output_local_origin)` items for `output`,
    /// returning empty whenever the session is locked or abandoned.
    #[must_use]
    pub fn ime_popup_render_targets(
        &self,
        output: &Output,
    ) -> Vec<(WlSurface, Point<i32, Logical>)> {
        if self.is_locked_or_abandoned() || self.ime_popups.is_empty() {
            return Vec::new();
        }
        let out_geo = self.output_logical_rect(output);
        let single_output = self.all_outputs().len() <= 1;
        let mut out = Vec::with_capacity(self.ime_popups.len());

        for popup in &self.ime_popups {
            if !popup.alive() {
                continue;
            }
            self.unconstrain_ime_popup(popup);
            let Some(parent) = popup.get_parent() else {
                continue;
            };
            let parent_geo = self.ime_parent_geometry(&parent.surface);
            let parent_loc = if parent_geo.size.w > 0 && parent_geo.size.h > 0 {
                parent_geo.loc
            } else {
                parent.location.loc
            };
            let global_pos = parent_loc + popup.location();
            let bbox =
                smithay::desktop::utils::bbox_from_surface_tree(popup.wl_surface(), global_pos);
            let render_rect = if bbox.size.w > 0 && bbox.size.h > 0 {
                bbox
            } else {
                Rectangle::new(global_pos, Size::from((1, 1)))
            };
            if single_output || out_geo.overlaps(render_rect) {
                out.push((popup.wl_surface().clone(), global_pos - out_geo.loc));
            }
        }
        out
    }

    fn apply_keyboard_spec_to_seat(&mut self, kb_spec: &KeyboardInputSpec, force_xkb: bool) {
        let Some(keyboard) = self.kb() else {
            return;
        };

        let target_sig = KeyboardXkbSignature::from(kb_spec);
        if force_xkb || self.applied_keyboard_xkb.as_ref() != Some(&target_sig) {
            let xkb_cfg = XkbConfig {
                rules: &target_sig.rules,
                model: &target_sig.model,
                layout: &target_sig.layout,
                variant: &target_sig.variant,
                options: target_sig.options.clone(),
            };
            if let Err(err) = keyboard.set_xkb_config(self, xkb_cfg) {
                warn!(
                    layout = %target_sig.layout,
                    variant = %target_sig.variant,
                    options = ?target_sig.options,
                    "failed applying runtime XkbConfig: {err:?}"
                );
            } else {
                self.applied_keyboard_xkb = Some(target_sig);
                self.virtual_keyboard_keymap_active = false;
            }
        }

        // NOTE: Smithay `KeyboardHandle::change_repeat_info(rate, delay)` expects `(rate_hz, delay_ms)`.
        let target_delay = kb_spec.effective_repeat_delay();
        let target_rate = kb_spec.effective_repeat_rate();
        if self.applied_keyboard_repeat != (target_delay, target_rate) {
            keyboard.change_repeat_info(target_rate, target_delay);
            self.applied_keyboard_repeat = (target_delay, target_rate);
            // Logged because "typing repeats too fast" is otherwise unverifiable from a running
            // compositor (komawm/koma#44).
            info!(
                repeat_delay_ms = target_delay,
                repeat_rate_hz = target_rate,
                "applied keyboard repeat settings"
            );
        }

        if kb_spec.numlock.is_some() && self.applied_startup_numlock != kb_spec.numlock {
            let want_numlock = kb_spec.numlock == Some(true);
            let mut mods = keyboard.modifier_state();
            if mods.num_lock != want_numlock {
                mods.num_lock = want_numlock;
                keyboard.set_modifier_state(mods);
                keyboard.advertise_modifier_state(self);
            }
            self.applied_startup_numlock = kb_spec.numlock;
        }
    }

    /// Applies runtime XKB keyboard settings, repeat info, startup NumLock, and live `libinput`
    /// pointer/touchpad/device rules from a freshly evaluated [`KomaConfigRegistry`].
    pub fn apply_runtime_input_config(&mut self, registry: &KomaConfigRegistry) {
        self.apply_keyboard_spec_to_seat(&registry.input.keyboard, false);
        self.apply_backend_input_config(&registry.input, &registry.devices);
        self.apply_follow_mouse_config(registry.input.pointer.follow_mouse.as_deref());
    }

    /// Caches the resolved [`FollowMouseMode`] so pointer motion never re-clones the config registry,
    /// and drops any stale `focus_only` pointer-focus override when the policy stops allowing it.
    fn apply_follow_mouse_config(&mut self, raw: Option<&str>) {
        let mode = FollowMouseMode::from_config_value(raw);
        self.follow_mouse_mode = mode;
        // Only `focus_only` may keep keyboard focus on a window that is not the workspace's active
        // window, so any other policy must drop the override and re-derive focus from the stack.
        if mode != FollowMouseMode::FocusOnly && self.pointer_focus_window.take().is_some() {
            self.focus_active_deck_window();
        }
    }

    /// Restores the compositor's configured Lua XKB layout after a custom `zwp_virtual_keyboard_v1`
    /// keymap was active on the seat.
    pub fn force_reapply_configured_xkb_config(&mut self) {
        let registry = self.lua_sandbox.config_snapshot();
        self.apply_keyboard_spec_to_seat(&registry.input.keyboard, true);
    }

    /// Re-applies declarative `koma.input` + `koma.device` rules to all connected backend devices.
    pub fn apply_backend_input_config(
        &mut self,
        input: &InputConfigSpec,
        devices: &[DeviceOverrideSpec],
    ) {
        #[cfg(feature = "smithay-tty")]
        if let Some(ActiveBackend::Tty(tty)) = self.backend.as_mut() {
            tty.apply_libinput_config_to_all(input, devices);
        }
        #[cfg(not(feature = "smithay-tty"))]
        let _ = (input, devices);
    }

    fn resolve_touch_position<
        B: InputBackend,
        E: smithay::backend::input::Event<B> + smithay::backend::input::AbsolutePositionEvent<B>,
    >(
        &self,
        event: &E,
    ) -> Option<Point<f64, Logical>>
    where
        B::Device: smithay::backend::input::Device,
    {
        let dev_name = smithay::backend::input::Device::name(&event.device());
        let reg = self.lua_sandbox.config_snapshot();
        let mapped_output_id = reg
            .devices
            .iter()
            .find(|d| {
                !d.name.is_empty() && (d.name == dev_name || d.name.eq_ignore_ascii_case(&dev_name))
            })
            .and_then(|d| d.map_to_output.as_ref());

        let outputs = self.all_outputs();
        let target_output = mapped_output_id
            .and_then(|out_id| {
                outputs
                    .iter()
                    .find(|o| o.name() == out_id.as_str())
                    .cloned()
            })
            .or_else(|| self.primary_output())?;

        let out_geo = self.output_logical_rect(&target_output);
        let pos = event.position_transformed(out_geo.size) + out_geo.loc.to_f64();
        Some(pos)
    }

    pub fn handle_touch_down<B: InputBackend>(&mut self, event: B::TouchDownEvent)
    where
        B::Device: smithay::backend::input::Device,
    {
        self.reconcile_session_lock();
        self.notify_user_activity();
        let Some(pos) = self.resolve_touch_position::<B, _>(&event) else {
            return;
        };
        let Some(touch) = self.touch() else {
            return;
        };
        if !self.is_locked_or_abandoned() && !touch.is_grabbed() {
            self.handle_pointer_button_focus(pos);
        }
        let under = self.surface_under(pos);
        let serial = SERIAL_COUNTER.next_serial();
        touch.down(
            self,
            under,
            &TouchDownPayload {
                slot: event.slot(),
                location: pos,
                serial,
                time: event.time(),
            },
        );
    }

    pub fn handle_touch_motion<B: InputBackend>(&mut self, event: B::TouchMotionEvent)
    where
        B::Device: smithay::backend::input::Device,
    {
        self.notify_user_activity();
        let Some(pos) = self.resolve_touch_position::<B, _>(&event) else {
            return;
        };
        let Some(touch) = self.touch() else {
            return;
        };
        let mut under = self.surface_under(pos);
        // Same sandbox rule as pointer motion: a running client drag is never offered into a
        // jailed workspace.
        if self.dnd_drag.is_some()
            && under.as_ref().is_some_and(|(surface, _)| {
                !crate::dnd::surface_may_exchange_drag_data(self, surface)
            })
        {
            under = None;
        }
        touch.motion(
            self,
            under,
            &TouchMotionPayload {
                slot: event.slot(),
                location: pos,
                time: event.time(),
            },
        );
    }

    pub fn handle_touch_up<B: InputBackend>(&mut self, event: B::TouchUpEvent) {
        self.notify_user_activity();
        let Some(touch) = self.touch() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        touch.up(
            self,
            &TouchUpPayload {
                slot: event.slot(),
                serial,
                time: event.time(),
            },
        );
    }

    pub fn handle_touch_frame(&mut self) {
        if let Some(touch) = self.touch() {
            touch.frame(self);
        }
    }

    pub fn handle_touch_cancel(&mut self) {
        if let Some(touch) = self.touch() {
            touch.cancel(self);
        }
    }

    pub fn apply_output_power_mode(
        &mut self,
        output: &Output,
        mode: OutputPowerMode,
    ) -> Result<(), String> {
        let _ = output;
        match &mut self.backend {
            #[cfg(feature = "smithay-tty")]
            Some(ActiveBackend::Tty(tty)) => tty.set_output_dpms(output, mode),
            _ => {
                let _ = mode;
                Ok(())
            }
        }
    }

    #[must_use]
    pub fn query_output_gamma_info(&self, output: &Output) -> Option<(u32, Option<GammaLut>)> {
        match &self.backend {
            #[cfg(feature = "smithay-tty")]
            Some(ActiveBackend::Tty(tty)) => tty.query_output_gamma_info(output),
            _ => {
                let _ = output;
                None
            }
        }
    }

    pub fn apply_output_gamma(&self, output: &Output, lut: &GammaLut) -> Result<(), String> {
        match &self.backend {
            #[cfg(feature = "smithay-tty")]
            Some(ActiveBackend::Tty(tty)) => tty.apply_output_gamma(output, lut),
            _ => {
                let _ = (output, lut);
                Err("hardware gamma LUTs require bare-metal TTY DRM backend".to_owned())
            }
        }
    }

    pub fn sync_foreign_toplevels(&mut self) {
        crate::foreign_toplevel::sync_all_foreign_toplevels(self);
    }

    /// Captures a read-only desktop snapshot for `Phodopus` Lua `ctx:windows()`,
    /// `ctx:focused_window()`, `ctx:outputs()`, and `ctx:workspaces()`, redacting protected
    /// surface titles and scoping offscreen agent workspaces safely.
    #[must_use]
    pub fn capture_lua_desktop_snapshot(
        &self,
        scope_ws_id: Option<&WorkspaceId>,
    ) -> DesktopQuerySnapshot {
        let active_ws_id = self.effective_interactive_ws_id();
        let active_focused_id = self.focused_window_id();

        let mut windows = Vec::new();
        let mut focused_window = None;
        for mapped in &self.mapped_windows {
            if mapped.is_minimized {
                continue;
            }
            let Some(ws) = self.workspaces.get(&mapped.ws_id) else {
                continue;
            };
            let is_offscreen = matches!(ws.presentation, PresentationTarget::Offscreen { .. });
            if is_offscreen && scope_ws_id != Some(&mapped.ws_id) {
                continue;
            }
            let is_focused =
                mapped.ws_id == active_ws_id && Some(mapped.win_id) == active_focused_id;
            let is_protected = self
                .rules
                .resolve_dynamic(&WindowMatchContext {
                    app_id: &mapped.app_id,
                    title: &mapped.title,
                    is_floating: mapped.is_floating,
                    is_focused,
                    is_xwayland: false,
                    is_modal_dialog: false,
                    is_screencast_target: false,
                    is_agent_workspace: is_offscreen,
                })
                .block_out_from
                .is_some_and(|p| p.blocks_agent_capture());
            let info = LuaWindowInfo {
                id: mapped.win_id.slot,
                app_id: mapped.app_id.clone(),
                title: if is_protected {
                    "[protected]".to_owned()
                } else {
                    mapped.title.clone()
                },
                workspace: ws.name.clone(),
                floating: mapped.is_floating,
                fullscreen: ws.is_fullscreen(mapped.win_id),
                focused: is_focused,
            };
            if is_focused {
                focused_window = Some(info.clone());
            }
            windows.push(info);
        }

        let focused_out_name = self.focused_output_name();
        let outputs = self
            .all_outputs()
            .into_iter()
            .map(|out| {
                let name = out.name();
                let (width, height, scale) = output_logical_viewport(&out);
                let refresh_mhz = out
                    .current_mode()
                    .map_or(60_000, |m| m.refresh.max(0) as u32);
                let active_ws = self.active_workspace_for_output(&name);
                let ws_name = self
                    .workspaces
                    .get(&active_ws)
                    .map_or_else(|| active_ws.to_string(), |w| w.name.clone());
                LuaOutputInfo {
                    focused: focused_out_name.as_deref() == Some(name.as_str()),
                    name,
                    width,
                    height,
                    scale,
                    refresh_mhz,
                    active_workspace: ws_name,
                }
            })
            .collect();

        let workspaces = self
            .workspaces
            .values()
            .map(|ws| {
                let count = self
                    .mapped_windows
                    .iter()
                    .filter(|m| m.ws_id == ws.id && !m.is_minimized)
                    .count();
                let layout = match ws.layout_mode {
                    LayoutMode::Dwindle => "dwindle",
                    LayoutMode::Scrolling => "scrolling",
                    LayoutMode::Deck => "deck",
                }
                .to_owned();
                let presentation = match ws.presentation {
                    PresentationTarget::Physical(_) => "headed",
                    PresentationTarget::Offscreen { .. } => "headless",
                }
                .to_owned();
                LuaWorkspaceInfo {
                    id: ws.id.to_string(),
                    name: ws.name.clone(),
                    layout,
                    presentation,
                    focused_window_id: ws.active_window().map(|id| id.slot),
                    window_count: count,
                    active: ws.id == active_ws_id,
                }
            })
            .collect();

        DesktopQuerySnapshot {
            focused_window,
            windows,
            outputs,
            workspaces,
        }
    }

    /// Emits a `koma.on(event_name, fn(ctx, ev))` hook slice with fast no-op path when unsubscribed.
    pub fn emit_lua_event(&mut self, event_name: &str, payload: serde_json::Value) {
        if !self.lua_sandbox.has_event_hooks(event_name) {
            return;
        }
        let ws_id = self.active_ws_id.clone();
        let epoch = self
            .workspaces
            .get(&ws_id)
            .map(|w| w.lease.epoch())
            .unwrap_or_default();
        let now_ms = self.elapsed_ms();
        let snap = self.capture_lua_desktop_snapshot(None);
        self.lua_sandbox.set_desktop_snapshot(snap);
        if let Err(err) = self
            .lua_sandbox
            .emit_event_hooks(event_name, &payload, ws_id, epoch, now_ms)
        {
            warn!(event = %event_name, "{err}");
            return;
        }
        crate::macro_runtime::execute_queued_host_actions(self);
        crate::macro_runtime::rearm_trampoline_timer(self);
    }

    /// Triggers the per-output headed workspace slide/fade transition for a workspace switch.
    ///
    /// Animation only: the `"workspace.focused"` hook is emitted by the switch path itself
    /// (`focus_workspace` / `focus_output`) after the effective interactive workspace changed, so
    /// an animation retrigger can never double-emit or fire for a no-op switch.
    pub fn trigger_workspace_transition(
        &mut self,
        output_name: &str,
        old_ws: &WorkspaceId,
        new_ws: &WorkspaceId,
        direction_hint: i32,
    ) {
        if old_ws == new_ws {
            return;
        }
        let Some(ws) = self.workspaces.get(new_ws) else {
            return;
        };
        let is_offscreen = matches!(ws.presentation, PresentationTarget::Offscreen { .. });
        let viewport = ws.viewport;
        let ws_name = ws.name.clone();
        let dir = if direction_hint != 0 {
            direction_hint
        } else {
            let old_num = self
                .workspaces
                .get(old_ws)
                .and_then(|w| w.name.parse::<i32>().ok());
            let new_num = ws_name.parse::<i32>().ok();
            match (old_num, new_num) {
                (Some(a), Some(b)) if b != a => (b - a).signum(),
                _ => 1,
            }
        };
        let now_ms = self.elapsed_ms();
        let anim_cfg = self.lua_sandbox.config_snapshot().appearance.animations;
        self.workspace_transitions.borrow_mut().trigger_switch(
            output_name,
            dir,
            viewport,
            now_ms,
            &anim_cfg,
            is_offscreen,
        );
    }

    /// Emits the `workspace.focused` Lua hook for a completed workspace activation.
    ///
    /// Every switch path (`focus_workspace`, `focus_output`, keybind
    /// `WorkspaceFocus`/`WindowMoveToWorkspace`, `ws.focus`, `ext-workspace-v1.activate` and
    /// followed window moves) funnels through one of these call sites, so a hook never misses a
    /// transition and never fires twice for the same one.
    pub fn emit_workspace_activated(
        &mut self,
        ws_id: &WorkspaceId,
        output: Option<&str>,
        presentation: &str,
    ) {
        let Some(ws) = self.workspaces.get(ws_id) else {
            return;
        };
        let name = ws.name.clone();
        let layout = match ws.layout_mode {
            LayoutMode::Dwindle => "dwindle",
            LayoutMode::Scrolling => "scrolling",
            LayoutMode::Deck => "deck",
        };
        self.emit_lua_event(
            "workspace.focused",
            serde_json::json!({
                "id": ws_id.to_string(),
                "name": name,
                "output": output,
                "layout": layout,
                "presentation": presentation,
            }),
        );
    }

    /// Focuses a mapped window matching raw numeric `slot` (`LuaWindowInfo.id`).
    pub fn activate_window_by_slot(&mut self, slot: u64) -> bool {
        let Some(win_id) = self
            .mapped_windows
            .iter()
            .find(|m| m.win_id.slot == slot)
            .map(|m| m.win_id)
        else {
            return false;
        };
        self.activate_window_by_id(win_id)
    }

    /// Changes a workspace's tiling layout (`dwindle` | `scrolling` | `deck` | `bsp`) and smoothly
    /// retargets window geometries via the configured Hooke/Bezier curves.
    pub fn set_workspace_layout(&mut self, target_ws: Option<&str>, layout: LayoutMode) -> bool {
        let ws_id = match target_ws {
            Some(q) => self.ensure_workspace_by_name_or_id(q),
            None => self.effective_interactive_ws_id(),
        };
        let now_ms = self.elapsed_ms();
        let Some(ws) = self.workspaces.get_mut(&ws_id) else {
            return false;
        };
        ws.layout_mode = layout;
        ws.recalculate(now_ms);
        self.sync_workspace_geometries(&ws_id);
        self.queue_redraw();
        true
    }

    pub fn activate_window_by_id(&mut self, win_id: WindowId) -> bool {
        let Some((ws_id, was_minimized)) = self
            .mapped_windows
            .iter()
            .find(|m| m.win_id == win_id)
            .map(|m| (m.ws_id.clone(), m.is_minimized))
        else {
            return false;
        };

        if was_minimized
            && let Some(m) = self.mapped_windows.iter_mut().find(|m| m.win_id == win_id)
        {
            m.is_minimized = false;
        }

        if let Some(ws) = self.workspaces.get(&ws_id) {
            if ws.is_special {
                self.active_special_ws_id = Some(ws_id.clone());
            } else if matches!(ws.presentation, PresentationTarget::Physical(_))
                && self.active_ws_id != ws_id
            {
                self.active_special_ws_id = None;
                self.active_ws_id = ws_id.clone();
            } else if self.active_special_ws_id.is_some()
                && self.active_special_ws_id.as_ref() != Some(&ws_id)
            {
                self.active_special_ws_id = None;
            }
        }

        let now_ms = self.elapsed_ms();
        if let Some(ws) = self.workspaces.get_mut(&ws_id) {
            ws.activate_window(win_id, now_ms);
        }
        self.focused_ondemand_layer = None;
        self.pointer_focus_window = None;
        self.break_active_pointer_constraints();
        self.sync_workspace_geometries(&ws_id);
        self.focus_active_deck_window();
        self.reconcile_idle_inhibitors();
        // Restoring a minimized window makes its workspace occupied again and can switch the
        // displayed workspace (IPC `win.activate`, foreign-toplevel activation).
        sync_ext_workspaces(self);
        self.queue_redraw();
        true
    }

    pub fn activate_window_by_surface(&mut self, surface: &WlSurface) -> bool {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        let win_id = self
            .mapped_windows
            .iter()
            .find(|m| m.window.toplevel().is_some_and(|t| *t.wl_surface() == root))
            .map(|m| m.win_id);
        if let Some(win_id) = win_id {
            return self.activate_window_by_id(win_id);
        }
        false
    }

    /// Derives whether `win_id` is currently maximized from the owning workspace's
    /// `WindowPresentation`, the single source of truth for maximize/fullscreen.
    #[must_use]
    pub fn is_window_maximized(&self, win_id: WindowId) -> bool {
        self.mapped_windows
            .iter()
            .find(|m| m.win_id == win_id)
            .and_then(|m| self.workspaces.get(&m.ws_id))
            .is_some_and(|ws| ws.is_maximized(win_id))
    }

    pub fn set_window_maximized(&mut self, win_id: WindowId, maximized: bool) {
        let Some(mapped) = self.mapped_windows.iter().find(|m| m.win_id == win_id) else {
            return;
        };
        let ws_id = mapped.ws_id.clone();
        if self.is_window_maximized(win_id) == maximized {
            // `xdg_toplevel` clients still expect an answer to a redundant request.
            if let Some(toplevel) = mapped.window.toplevel() {
                toplevel.send_pending_configure();
            }
            return;
        }
        let Some(ws) = self.workspaces.get_mut(&ws_id) else {
            return;
        };
        ws.set_maximized(win_id, maximized);
        self.sync_workspace_geometries(&ws_id);
        self.focus_active_deck_window();
        self.sync_foreign_toplevels();
        self.queue_redraw();
    }

    /// Toggles maximize for `win_id` (`Mod + M`), clearing a fullscreen state on this workspace.
    pub fn toggle_window_maximized(&mut self, win_id: WindowId) {
        let maximized = !self.is_window_maximized(win_id);
        self.set_window_maximized(win_id, maximized);
    }

    pub fn set_window_minimized(&mut self, win_id: WindowId, minimized: bool) {
        let Some(mapped) = self.mapped_windows.iter_mut().find(|m| m.win_id == win_id) else {
            return;
        };
        if mapped.is_minimized == minimized {
            return;
        }
        mapped.is_minimized = minimized;
        let ws_id = mapped.ws_id.clone();
        if minimized {
            self.break_active_pointer_constraints();
            // A minimized window is not on screen; never resurrect its pixels when it closes.
            self.window_frames.remove(&win_id);
        } else {
            let now_ms = self.elapsed_ms();
            if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                ws.activate_window(win_id, now_ms);
            }
        }
        self.focus_active_deck_window();
        self.reconcile_idle_inhibitors();
        self.sync_foreign_toplevels();
        // A minimized window stops occupying its workspace, so minimize/restore changes bar
        // visibility for that workspace.
        sync_ext_workspaces(self);
        self.queue_redraw();
    }

    pub fn set_window_fullscreen(
        &mut self,
        win_id: WindowId,
        fullscreen: bool,
        _target_output: Option<Output>,
    ) {
        let Some(ws_id) = self
            .mapped_windows
            .iter()
            .find(|m| m.win_id == win_id)
            .map(|m| m.ws_id.clone())
        else {
            return;
        };
        if let Some(ws) = self.workspaces.get_mut(&ws_id) {
            ws.set_fullscreen(win_id, fullscreen);
        }
        self.sync_workspace_geometries(&ws_id);
        self.focus_active_deck_window();
        self.sync_foreign_toplevels();
        self.queue_redraw();
    }
}

pub fn read_toplevel_metadata(wl_surface: &WlSurface) -> (String, String) {
    with_states(wl_surface, |states| {
        let role = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .map(|d| d.lock().unwrap());
        let app_id = role
            .as_ref()
            .and_then(|r| r.app_id.clone())
            .unwrap_or_else(|| "unknown".to_owned());
        let title = role
            .as_ref()
            .and_then(|r| r.title.clone())
            .unwrap_or_else(String::new);
        drop(states.cached_state.get::<SurfaceCachedState>());
        (app_id, title)
    })
}

/// Reads committed `(min_size, max_size)` from Smithay's `SurfaceCachedState`
/// (`niri/src/window/mod.rs` lines 387-390).
pub fn read_toplevel_min_max_size(wl_surface: &WlSurface) -> ((u32, u32), (u32, u32)) {
    with_states(wl_surface, |states| {
        let mut guard = states.cached_state.get::<SurfaceCachedState>();
        let current = guard.current();
        let min_s = (
            current.min_size.w.max(0) as u32,
            current.min_size.h.max(0) as u32,
        );
        let max_s = (
            current.max_size.w.max(0) as u32,
            current.max_size.h.max(0) as u32,
        );
        (min_s, max_s)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use smithay::output::{Mode, PhysicalProperties, Scale, Subpixel};

    pub(crate) fn test_output(name: &str) -> Output {
        let out = Output::new(
            name.to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Koma".to_owned(),
                model: "TestOutput".to_owned(),
                serial_number: "test-0".to_owned(),
            },
        );
        out.change_current_state(
            Some(Mode {
                size: (1920, 1080).into(),
                refresh: 60_000,
            }),
            None,
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        out
    }

    #[test]
    fn session_lock_phase_transitions_and_blackout_fallback() {
        let out = test_output("DP-1");
        let mut coord = SessionLockCoordinator::default();

        assert_eq!(coord.effective_phase(), SessionLockPhase::Unlocked);
        assert_eq!(coord.phase_label(), "unlocked");
        assert!(!coord.is_locked_or_abandoned());
        assert!(!coord.unlock());
        assert!(coord.active_lock_surface_for_output(&out).is_none());

        // Unlocked -> Locked
        assert!(coord.try_lock(None));
        assert!(coord.is_locked_or_abandoned());
        assert_eq!(coord.phase_label(), "locked");
        // Racing second lock rejected while actively Locked
        assert!(!coord.try_lock(None));
        // Missing LockSurface falls back cleanly to blackout (None surface -> solid black)
        assert!(coord.active_lock_surface_for_output(&out).is_none());
        assert!(coord.first_active_lock_surface().is_none());

        // Locked -> Abandoned (e.g. locker crash / disconnect): fail-closed, NEVER auto-unlocks!
        assert!(coord.abandon_active_lock());
        assert!(coord.is_locked_or_abandoned());
        assert_eq!(coord.phase_label(), "abandoned");
        assert!(!coord.unlock(), "abandoned lock must refuse direct unlock");
        assert!(coord.is_locked_or_abandoned());
        assert!(coord.active_lock_surface_for_output(&out).is_none());

        // Abandoned -> Locked -> Unlocked via clean replacement locker
        assert!(coord.can_accept_new_lock());
        assert!(coord.try_lock(None));
        assert_eq!(coord.phase_label(), "locked");
        assert!(coord.unlock());
        assert_eq!(coord.phase_label(), "unlocked");
        assert!(!coord.is_locked_or_abandoned());
    }

    #[test]
    fn z_order_hit_testing_ranks_layers_windows_and_lock_preemption() {
        let full = LogicalRect::new(0, 0, 1920, 1080);
        let pt = LogicalPoint { x: 200.0, y: 120.0 };

        let all_layers = vec![
            ZOrderCandidate {
                tier: ZOrderTier::BackgroundLayer,
                rect: full,
                payload: "bg",
            },
            ZOrderCandidate {
                tier: ZOrderTier::BottomLayer,
                rect: full,
                payload: "bottom",
            },
            ZOrderCandidate {
                tier: ZOrderTier::TiledWindow,
                rect: full,
                payload: "tiled",
            },
            ZOrderCandidate {
                tier: ZOrderTier::FloatingOrSpecialWindow,
                rect: full,
                payload: "floating",
            },
            ZOrderCandidate {
                tier: ZOrderTier::TopLayer,
                rect: full,
                payload: "top",
            },
            ZOrderCandidate {
                tier: ZOrderTier::OverlayLayer,
                rect: full,
                payload: "overlay",
            },
        ];

        assert_eq!(
            resolve_z_order_hit(false, pt, &all_layers),
            Some((ZOrderTier::OverlayLayer, "overlay"))
        );
        assert_eq!(
            resolve_z_order_hit(false, pt, &all_layers[..5]),
            Some((ZOrderTier::TopLayer, "top"))
        );
        assert_eq!(
            resolve_z_order_hit(false, pt, &all_layers[..4]),
            Some((ZOrderTier::FloatingOrSpecialWindow, "floating"))
        );
        assert_eq!(
            resolve_z_order_hit(false, pt, &all_layers[..3]),
            Some((ZOrderTier::TiledWindow, "tiled"))
        );
        assert_eq!(
            resolve_z_order_hit(false, pt, &all_layers[..2]),
            Some((ZOrderTier::BottomLayer, "bottom"))
        );
        assert_eq!(
            resolve_z_order_hit(false, pt, &all_layers[..1]),
            Some((ZOrderTier::BackgroundLayer, "bg"))
        );

        // When Locked/Abandoned without a LockSurface, ALL normal layers/windows are unreachable
        assert_eq!(resolve_z_order_hit(true, pt, &all_layers), None);

        let mut with_lock = all_layers;
        with_lock.push(ZOrderCandidate {
            tier: ZOrderTier::LockSurface,
            rect: full,
            payload: "lock",
        });
        assert_eq!(
            resolve_z_order_hit(true, pt, &with_lock),
            Some((ZOrderTier::LockSurface, "lock"))
        );
        assert_eq!(
            resolve_z_order_hit(false, pt, &with_lock),
            Some((ZOrderTier::OverlayLayer, "overlay"))
        );
    }

    #[test]
    fn keyboard_interactivity_ranks_exclusive_ondemand_none_and_lock() {
        assert_eq!(
            resolve_keyboard_focus_source(false, false, true, true),
            KeyboardFocusSource::ExclusiveLayer
        );
        assert_eq!(
            resolve_keyboard_focus_source(false, false, false, true),
            KeyboardFocusSource::OnDemandLayer
        );
        assert_eq!(
            resolve_keyboard_focus_source(false, false, false, false),
            KeyboardFocusSource::ActiveWindow
        );

        // Lock screen ALWAYS preempts Exclusive & OnDemand layer surfaces
        assert_eq!(
            resolve_keyboard_focus_source(true, true, true, true),
            KeyboardFocusSource::LockSurface
        );
        // Abandoned or unmapped LockSurface steals focus away from desktop into None
        assert_eq!(
            resolve_keyboard_focus_source(true, false, true, true),
            KeyboardFocusSource::LockBlackoutNone
        );
    }

    pub(crate) fn make_headless_test_state() -> (
        calloop::EventLoop<'static, KomaState>,
        smithay::reexports::wayland_server::Display<KomaState>,
        KomaState,
    ) {
        let event_loop: calloop::EventLoop<'static, KomaState> =
            calloop::EventLoop::try_new().expect("calloop EventLoop");
        let display: smithay::reexports::wayland_server::Display<KomaState> =
            smithay::reexports::wayland_server::Display::new().expect("wayland Display");
        let state = KomaState::new(
            display.handle(),
            event_loop.get_signal(),
            event_loop.handle(),
            OsString::from("wayland-koma-unit-test"),
            "seat0",
            LogicalViewport::new(1920, 1080, 1.0),
        );
        (event_loop, display, state)
    }

    fn insert_test_window(state: &mut KomaState, ws_id: &WorkspaceId, win_id: WindowId) {
        let now_ms = state.elapsed_ms();
        state
            .workspaces
            .get_mut(ws_id)
            .expect("workspace exists")
            .insert_window(win_id, false, None, None, None, None, false, true, now_ms);
    }

    fn arm_test_repeat(state: &mut KomaState) {
        let now_ms = state.elapsed_ms();
        assert!(state.keybinds.repeat.arm(
            crate::keybind_repeat::RepeatRequest {
                bind_id: koma_lua::BindHandleId(13),
                keycode: 42,
                mods: 1,
                action: koma_lua::ActionDescriptor::FocusDirection("left".to_owned()),
                allow_when_locked: false,
            },
            now_ms,
        ));
        assert!(state.keybinds.repeat.is_active());
    }

    #[test]
    fn pending_keybind_repeat_is_cancelled_by_lock_workspace_change_and_pointer_grab() {
        let (_el, _disp, mut state) = make_headless_test_state();

        // Session lock: `clear_inputs_for_lock` runs from the lock handler.
        arm_test_repeat(&mut state);
        assert!(state.session_lock.try_lock(None));
        state.clear_inputs_for_lock();
        assert!(
            !state.keybinds.repeat.is_active(),
            "locking the session must cancel a pending repeat"
        );
        assert!(state.session_lock.unlock());

        // External workspace switch cancels.
        arm_test_repeat(&mut state);
        let dst = state.ensure_workspace_by_name_or_id("2");
        assert!(state.focus_workspace(&dst));
        assert!(
            !state.keybinds.repeat.is_active(),
            "switching workspace must cancel a pending repeat"
        );

        // A pointer grab attempt cancels before the gesture owns the pointer.
        arm_test_repeat(&mut state);
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let _ = crate::pointer_grabs::start_compositor_move_grab(
            &mut state,
            crate::pointer_grabs::BTN_LEFT,
            serial,
        );
        assert!(
            !state.keybinds.repeat.is_active(),
            "starting a pointer grab must cancel a pending repeat"
        );

        // A repeat that switches workspace itself keeps running.
        arm_test_repeat(&mut state);
        state.keybinds.repeat.begin_dispatch();
        let dst = state.ensure_workspace_by_name_or_id("3");
        assert!(state.focus_workspace(&dst));
        state.keybinds.repeat.end_dispatch();
        assert!(
            state.keybinds.repeat.is_active(),
            "a workspace cycling bind must keep repeating after its own switch"
        );
    }

    #[test]
    fn move_to_workspace_follows_window_and_focuses_destination() {
        let (_el, _disp, mut state) = make_headless_test_state();
        let src = state.active_ws_id.clone();
        let dst = state.ensure_workspace_by_name_or_id("2");
        let win = WindowId::new(1, 1);
        insert_test_window(&mut state, &src, win);
        assert_eq!(state.workspaces[&src].focused_window, Some(win));

        assert!(state.move_active_window_between_workspaces_with_follow(&src, &dst, true));

        assert_eq!(state.active_ws_id, dst);
        assert_eq!(state.effective_interactive_ws_id(), dst);
        assert_eq!(state.workspaces[&dst].focused_window, Some(win));
        assert_eq!(state.workspaces[&dst].active_window(), Some(win));
        assert_ne!(state.workspaces[&src].active_window(), Some(win));
        assert!(
            state.workspaces[&src].active_window().is_none(),
            "source workspace must not keep a dangling active window"
        );
    }

    #[test]
    fn silent_move_to_workspace_keeps_source_focused_and_window_unfocused() {
        let (_el, _disp, mut state) = make_headless_test_state();
        let src = state.active_ws_id.clone();
        let dst = state.ensure_workspace_by_name_or_id("2");
        let stay = WindowId::new(1, 1);
        let moved = WindowId::new(2, 1);
        insert_test_window(&mut state, &src, stay);
        insert_test_window(&mut state, &src, moved);
        assert_eq!(state.workspaces[&src].focused_window, Some(moved));

        assert!(state.move_active_window_between_workspaces_with_follow(&src, &dst, false));

        assert_eq!(state.active_ws_id, src);
        assert_eq!(state.effective_interactive_ws_id(), src);
        assert_eq!(state.workspaces[&src].focused_window, Some(stay));
        assert_eq!(state.workspaces[&dst].active_window(), Some(moved));
        assert_ne!(
            state.workspaces[&state.effective_interactive_ws_id()].active_window(),
            Some(moved),
            "silent move must not leave the moved window keyboard-focused"
        );
    }

    #[test]
    fn moving_last_window_away_leaves_no_dangling_focus() {
        let (_el, _disp, mut state) = make_headless_test_state();
        let src = state.active_ws_id.clone();
        let dst = state.ensure_workspace_by_name_or_id("2");
        let moved = WindowId::new(1, 1);
        insert_test_window(&mut state, &src, moved);

        assert!(state.move_active_window_between_workspaces_with_follow(&src, &dst, false));

        assert_eq!(state.effective_interactive_ws_id(), src);
        assert_eq!(state.workspaces[&src].focused_window, None);
        assert_eq!(state.workspaces[&src].active_window(), None);
        assert!(
            state.active_deck_wl_surface().is_none(),
            "an emptied source workspace must not resolve a keyboard focus surface"
        );
    }

    #[test]
    fn closing_window_frame_lingers_until_the_close_spring_settles() {
        use smithay::{
            backend::allocator::Fourcc, backend::renderer::element::memory::MemoryRenderBuffer,
            utils::Transform,
        };

        let (_el, _disp, mut state) = make_headless_test_state();
        let ws_id = state.active_ws_id.clone();
        let win_id = WindowId::new(1, 0);
        let rect = LogicalRect::new(120, 90, 640, 480);
        let viewport = state.workspaces.get(&ws_id).expect("workspace").viewport;
        let now_ms = state.elapsed_ms();
        let cfg = koma_model::AnimationConfig::default();
        assert!(
            state
                .window_animations
                .borrow_mut()
                .start_close(win_id, rect, viewport, now_ms, &cfg, None,),
            "closing a mapped window must report an in-flight close spring"
        );
        state.closing_windows.push(ClosingWindow {
            win_id,
            ws_id: ws_id.clone(),
            snapshot: MemoryRenderBuffer::from_slice(
                &[0u8; 64 * 64 * 4],
                Fourcc::Abgr8888,
                (64, 64),
                1,
                Transform::Normal,
                None,
            ),
            snapshot_size: (64, 64),
        });

        let items = state.closing_window_scene_items(&ws_id);
        assert_eq!(items.len(), 1, "the captured frame must keep compositing");
        assert!(state.window_animations.borrow().is_closing(win_id));
        assert!(
            state.has_active_animations(),
            "an in-flight close spring must keep the frame loop repainting"
        );

        // In flight: the compositor must not drop the frame yet.
        state.reap_settled_closing_windows();
        assert_eq!(state.closing_windows.len(), 1);

        // Advance the compositor clock past the spring: only then is the frame released.
        state.start_time -= Duration::from_millis(5_000);
        state.reap_settled_closing_windows();
        assert!(
            state.closing_windows.is_empty(),
            "a settled close spring must release the captured frame"
        );
        assert!(!state.window_animations.borrow().is_closing(win_id));
        assert!(state.closing_window_scene_items(&ws_id).is_empty());
        assert!(!state.has_active_animations());
    }

    #[test]
    fn ime_protocols_and_touch_capability_registered_on_headless_state() {
        use smithay::wayland::input_method::InputMethodSeat as _;

        let (_el, _disp, state) = make_headless_test_state();
        assert!(
            state.seat.get_touch().is_some(),
            "seat must advertise wl_touch capability at startup"
        );
        assert!(state.seat.get_keyboard().is_some());
        assert!(state.seat.get_pointer().is_some());
        assert!(state.ime_popups.is_empty());
        assert!(!state.seat.input_method().has_instance());
        assert!(!state.seat.input_method().keyboard_grabbed());
        assert!(state.seat.text_input().focus().is_none());
    }

    #[test]
    fn runtime_input_config_updates_xkb_options_and_repeat_info() {
        let (_el, _disp, mut state) = make_headless_test_state();

        let mut registry = KomaConfigRegistry::default();
        registry.input.keyboard = KeyboardInputSpec {
            rules: None,
            model: None,
            layout: Some("us".to_owned()),
            variant: Some("intl".to_owned()),
            options: Some("altwin:swap_alt_win,caps:escape".to_owned()),
            repeat_delay: Some(180),
            repeat_rate: Some(50),
            numlock: Some(true),
        };
        state.apply_runtime_input_config(&registry);

        assert_eq!(
            state.applied_keyboard_xkb,
            Some(KeyboardXkbSignature {
                rules: String::new(),
                model: String::new(),
                layout: "us".to_owned(),
                variant: "intl".to_owned(),
                options: Some("altwin:swap_alt_win,caps:escape".to_owned()),
            })
        );
        assert_eq!(state.applied_keyboard_repeat, (180, 50));
        assert!(
            state
                .seat
                .get_keyboard()
                .expect("keyboard handle")
                .modifier_state()
                .num_lock
        );

        // Live reload removing options and changing repeat delay/rate
        registry.input.keyboard.options = None;
        registry.input.keyboard.repeat_delay = Some(300);
        registry.input.keyboard.repeat_rate = Some(25);
        state.apply_runtime_input_config(&registry);

        assert_eq!(
            state
                .applied_keyboard_xkb
                .as_ref()
                .and_then(|s| s.options.as_ref()),
            None
        );
        assert_eq!(state.applied_keyboard_repeat, (300, 25));

        state.virtual_keyboard_keymap_active = true;
        state.force_reapply_configured_xkb_config();
        assert!(!state.virtual_keyboard_keymap_active);
    }

    #[test]
    fn locked_session_suppresses_ime_popups_and_touch_surfaces() {
        use smithay::{
            backend::input::{InputTime, KeyState},
            input::{SeatHandler, keyboard::Keycode},
            output::{Output, PhysicalProperties, Subpixel},
            wayland::text_input::TextInputActivation,
        };

        let (_el, _disp, mut state) = make_headless_test_state();
        let test_output = Output::new(
            "test-out".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Koma".into(),
                model: "Test".into(),
                serial_number: "0".into(),
            },
        );

        state.session_lock.phase = SessionLockPhase::Abandoned;
        assert!(state.is_locked_or_abandoned());

        let seat = state.seat.clone();
        <KomaState as TextInputActivation>::activated(&mut state, None);
        <KomaState as SeatHandler>::focus_changed(&mut state, &seat, None);
        state.clear_inputs_for_lock();

        assert!(state.seat.text_input().focus().is_none());
        assert!(state.ime_popups.is_empty());
        assert!(state.ime_popup_render_targets(&test_output).is_empty());
        assert!(state.surface_under(Point::from((100.0, 100.0))).is_none());

        // Exercise locked keyboard routing (must cleanly traverse grab-bypassing path without panicking)
        crate::keybind_dispatcher::process_keyboard_key_event(
            &mut state,
            Keycode::from(38_u32),
            KeyState::Pressed,
            InputTime::from_millis(1),
            false,
        );
        crate::keybind_dispatcher::process_keyboard_key_event(
            &mut state,
            Keycode::from(38_u32),
            KeyState::Released,
            InputTime::from_millis(2),
            false,
        );
    }

    #[test]
    fn notch_safe_area_and_per_output_workspaces_update_cleanly() {
        use smithay::output::{Mode, PhysicalProperties, Subpixel};

        let (_el, _disp, mut state) = make_headless_test_state();
        let retina = Output::new(
            "eDP-1".to_owned(),
            PhysicalProperties {
                size: (302, 196).into(),
                subpixel: Subpixel::HorizontalRgb,
                make: "APP".into(),
                model: "Color LCD".into(),
                serial_number: "RETINA1".into(),
            },
        );
        retina.change_current_state(
            Some(Mode {
                size: (3024, 1964).into(),
                refresh: 120_000,
            }),
            None,
            Some(Scale::Fractional(2.0)),
            Some((0, 0).into()),
        );
        state.test_outputs.push(retina.clone());

        let notch = SafeAreaInsets {
            top: 38,
            bottom: 0,
            left: 0,
            right: 0,
        };
        state.register_or_update_output(&retina, Some((0, 0)), notch, true);

        let ws_id = state.active_workspace_for_output("eDP-1");
        let ws = state.workspaces.get(&ws_id).expect("eDP-1 workspace");
        assert_eq!(ws.viewport.width, 1512);
        assert_eq!(ws.viewport.height, 982);
        assert_eq!(
            ws.viewport.work_area,
            Some(LogicalRect::new(0, 38, 1512, 944))
        );

        // Second output receives its own independent workspace & 2D placement.
        let external = test_output("HDMI-A-1");
        state.test_outputs.push(external.clone());
        state.register_or_update_output(&external, None, SafeAreaInsets::default(), true);
        let ws_ext = state.active_workspace_for_output("HDMI-A-1");
        assert_ne!(ws_id, ws_ext);
        assert_eq!(external.current_location(), Point::from((1512, 0)));
    }

    #[test]
    fn multi_monitor_2d_pointer_crossing_focus_and_hotplug_persistence() {
        use koma_lua::ActionDescriptor;
        use smithay::backend::input::InputTime;

        let (_el, _disp, mut state) = make_headless_test_state();
        let left = test_output("eDP-1");
        let right = test_output("HDMI-A-1");
        state.test_outputs = vec![left.clone(), right.clone()];

        state.register_or_update_output(&left, Some((0, 0)), SafeAreaInsets::default(), true);
        state.register_or_update_output(
            &right,
            Some((2000, -120)),
            SafeAreaInsets::default(),
            true,
        );

        // Pointer crossing an 80px gap from left output into right output switches focus to HDMI-A-1.
        state.pointer_location = Point::from((1918.0, 200.0));
        state.handle_pointer_relative_motion(
            (6.0, 0.0).into(),
            (6.0, 0.0).into(),
            InputTime::from_millis(10),
        );
        assert_eq!(state.focused_output_name().as_deref(), Some("HDMI-A-1"));
        assert!(state.pointer_location.x >= 2000.0);

        // Directional & named monitor focus actions update focused output & active_ws_id.
        crate::keybind_dispatcher::execute_action_descriptor(
            &mut state,
            ActionDescriptor::MonitorFocusDirection("left".to_owned()),
            false,
        );
        assert_eq!(state.focused_output_name().as_deref(), Some("eDP-1"));

        crate::keybind_dispatcher::execute_action_descriptor(
            &mut state,
            ActionDescriptor::MonitorFocus("HDMI-A-1".to_owned()),
            false,
        );
        assert_eq!(state.focused_output_name().as_deref(), Some("HDMI-A-1"));

        // Assign workspace 4 + scale 1.5 + Transform::_90 to HDMI-A-1 before unplugging.
        crate::keybind_dispatcher::execute_action_descriptor(
            &mut state,
            ActionDescriptor::WorkspaceFocus("4".to_owned()),
            false,
        );
        let ws4 = state.active_workspace_for_output("HDMI-A-1");
        right.change_current_state(
            None,
            Some(Transform::_90),
            Some(Scale::Fractional(1.5)),
            None,
        );
        state.register_or_update_output(
            &right,
            Some((2000, -120)),
            SafeAreaInsets::default(),
            true,
        );
        assert_eq!(state.active_workspace_for_output("HDMI-A-1"), ws4);

        // Hot-unplug HDMI-A-1: focus returns cleanly to eDP-1.
        state.test_outputs.retain(|o| o.name() != "HDMI-A-1");
        state.unregister_output(&right);
        assert_eq!(state.focused_output_name().as_deref(), Some("eDP-1"));

        // Reconnect a brand-new Output instance for HDMI-A-1 (default scale 1.0, Transform::Normal):
        // hotplug persistence restores workspace 4, explicit pos (2000, -120), scale 1.5, and Transform::_90.
        let right_replugged = test_output("HDMI-A-1");
        state.test_outputs.push(right_replugged.clone());
        state.register_or_update_output(&right_replugged, None, SafeAreaInsets::default(), true);

        assert_eq!(state.active_workspace_for_output("HDMI-A-1"), ws4);
        assert_eq!(
            right_replugged.current_location(),
            Point::from((2000, -120))
        );
        assert!((right_replugged.current_scale().fractional_scale() - 1.5).abs() < 1e-4);
        assert_eq!(right_replugged.current_transform(), Transform::_90);
    }

    #[test]
    fn workspace_activation_event_fires_for_physical_special_and_output_switches() {
        use koma_lua::ActionDescriptor;
        use smithay::output::{Mode, PhysicalProperties, Subpixel};

        let (_el, _disp, mut state) = make_headless_test_state();
        state
            .lua_sandbox
            .eval_init_lua(
                r##"
                koma.on("workspace.focused", function(ctx, ev)
                    ctx:dispatch(koma.act.workspace.set_layout("deck", ev.name))
                end)
                "##,
            )
            .expect("Lua event hooks must evaluate");

        let output = |name: &str| {
            let output = Output::new(
                name.to_owned(),
                PhysicalProperties {
                    size: (800, 340).into(),
                    subpixel: Subpixel::Unknown,
                    make: "Test".to_owned(),
                    model: "Panel".to_owned(),
                    serial_number: name.to_owned(),
                },
            );
            output.change_current_state(
                Some(Mode {
                    size: (1920, 1080).into(),
                    refresh: 60_000,
                }),
                None,
                Some(Scale::Integer(1)),
                None,
            );
            output
        };
        let left = output("eDP-1");
        let right = output("HDMI-A-1");
        state.test_outputs = vec![left.clone(), right.clone()];
        state.register_or_update_output(&left, Some((0, 0)), SafeAreaInsets::default(), true);
        state.register_or_update_output(&right, Some((1920, 0)), SafeAreaInsets::default(), true);

        // 1. Physical workspace switch (keybind / `ws.focus` / followed move path).
        let ws2 = state.ensure_workspace_by_name_or_id("2");
        assert!(state.focus_workspace(&ws2));
        assert_eq!(
            state.workspaces[&ws2].layout_mode,
            LayoutMode::Deck,
            "a physical switch must emit `workspace.focused` with the new name"
        );

        // 2. Special workspace overlay switch.
        let special = state.ensure_special_workspace(Some("magic"));
        assert!(state.focus_workspace(&special));
        assert_eq!(
            state.workspaces[&special].layout_mode,
            LayoutMode::Deck,
            "opening a special workspace must emit `workspace.focused` too"
        );

        // 3. Output switch.
        assert!(state.focus_output("HDMI-A-1"));
        let ws_right = state.active_workspace_for_output("HDMI-A-1");
        assert_eq!(
            state.workspaces[&ws_right].layout_mode,
            LayoutMode::Deck,
            "focusing another output must emit `workspace.focused` for its workspace"
        );

        // 4. Re-focusing the already focused workspace is not a transition: the hook must not run
        //    again, so the sentinel layout restored below survives the call.
        state.workspaces.get_mut(&ws_right).expect("ws").layout_mode = LayoutMode::Dwindle;
        assert!(state.focus_workspace(&ws_right));
        assert_eq!(
            state.workspaces[&ws_right].layout_mode,
            LayoutMode::Dwindle,
            "a no-op re-focus must not emit a second `workspace.focused`"
        );

        // 5. A followed window move lands on the destination exactly like an explicit focus and
        //    emits the activation event for that destination.
        let ws_move = state.ensure_workspace_by_name_or_id("7");
        let src = state.active_ws_id.clone();
        insert_test_window(&mut state, &src, WindowId::new(7, 1));
        crate::keybind_dispatcher::execute_action_descriptor(
            &mut state,
            ActionDescriptor::WindowMoveToWorkspace {
                workspace: "7".to_owned(),
                follow: true,
            },
            false,
        );
        assert_eq!(state.active_ws_id, ws_move);
        assert_eq!(
            state.workspaces[&ws_move].layout_mode,
            LayoutMode::Deck,
            "a followed window move must emit `workspace.focused` for the destination"
        );
    }

    #[test]
    fn special_overlay_switches_emit_exactly_once_per_real_transition() {
        use smithay::output::{Mode, PhysicalProperties, Subpixel};

        let (_el, _disp, mut state) = make_headless_test_state();
        state
            .lua_sandbox
            .eval_init_lua(
                r##"
                koma.on("workspace.focused", function(ctx, ev)
                    ctx:dispatch(koma.act.workspace.set_layout("deck", ev.name))
                end)
                "##,
            )
            .expect("Lua event hooks must evaluate");

        let output = Output::new(
            "eDP-1".to_owned(),
            PhysicalProperties {
                size: (800, 340).into(),
                subpixel: Subpixel::Unknown,
                make: "Test".to_owned(),
                model: "Panel".to_owned(),
                serial_number: "edp-1".to_owned(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: (1920, 1080).into(),
                refresh: 60_000,
            }),
            None,
            Some(Scale::Integer(1)),
            None,
        );
        state.test_outputs = vec![output.clone()];
        state.register_or_update_output(&output, Some((0, 0)), SafeAreaInsets::default(), true);

        let underlying = state.active_workspace_for_output("eDP-1");
        let special = state.ensure_special_workspace(Some("magic"));
        assert!(state.focus_workspace(&special));
        assert_eq!(state.workspaces[&special].layout_mode, LayoutMode::Deck);

        // Re-opening the already focused special workspace is not a transition: the hook must not
        // run again, so the sentinel layout survives.
        state
            .workspaces
            .get_mut(&special)
            .expect("special workspace")
            .layout_mode = LayoutMode::Dwindle;
        assert!(state.focus_workspace(&special));
        assert_eq!(
            state.workspaces[&special].layout_mode,
            LayoutMode::Dwindle,
            "re-focusing the open special workspace must not emit `workspace.focused` again"
        );

        // Dismissing the overlay with an output focus lands on the underlying workspace, which is
        // a real transition even though `active_ws_id` never changed.
        state
            .workspaces
            .get_mut(&underlying)
            .expect("underlying workspace")
            .layout_mode = LayoutMode::Dwindle;
        assert!(state.focus_output("eDP-1"));
        assert_eq!(state.active_ws_id, underlying);
        assert_eq!(
            state.workspaces[&underlying].layout_mode,
            LayoutMode::Deck,
            "dismissing a special overlay must emit the underlying workspace activation"
        );
    }

    #[test]
    fn output_changed_hook_and_workspace_transition_dispatch_end_to_end() {
        use koma_lua::ActionDescriptor;
        use smithay::output::{Mode, PhysicalProperties, Subpixel};

        let (_el, _disp, mut state) = make_headless_test_state();
        state
            .lua_sandbox
            .eval_init_lua(
                r##"
                koma.on("output.changed", function(ctx, ev)
                    if ev.width and ev.width >= 3440 then
                        ctx:dispatch(koma.act.workspace.set_layout("scrolling"))
                    end
                end)
                koma.on("workspace.focused", function(ctx, ev)
                    if ev.name == "2" then
                        ctx:dispatch(koma.act.workspace.set_layout("deck", "2"))
                    end
                end)
                "##,
            )
            .expect("Lua event hooks must evaluate");

        let ultrawide = Output::new(
            "DP-1".to_owned(),
            PhysicalProperties {
                size: (800, 340).into(),
                subpixel: Subpixel::Unknown,
                make: "Test".to_owned(),
                model: "Ultrawide".to_owned(),
                serial_number: "uw-1".to_owned(),
            },
        );
        ultrawide.change_current_state(
            Some(Mode {
                size: (3440, 1440).into(),
                refresh: 165_000,
            }),
            None,
            Some(Scale::Integer(1)),
            None,
        );
        state.test_outputs = vec![ultrawide.clone()];
        state.register_or_update_output(&ultrawide, Some((0, 0)), SafeAreaInsets::default(), true);

        let ws1 = state.active_workspace_for_output("DP-1");
        assert_eq!(
            state.workspaces.get(&ws1).unwrap().layout_mode,
            LayoutMode::Scrolling,
            "output.changed Lua hook should auto-switch ultrawide workspace to scrolling"
        );

        crate::keybind_dispatcher::execute_action_descriptor(
            &mut state,
            ActionDescriptor::WorkspaceFocus("2".to_owned()),
            false,
        );
        let ws2 = state.active_workspace_for_output("DP-1");
        assert_eq!(
            state.workspaces.get(&ws2).unwrap().layout_mode,
            LayoutMode::Deck,
            "workspace.focused Lua hook should switch workspace 2 to deck"
        );
        assert!(
            state.has_active_animations(),
            "switching workspace should start a WorkspaceTransitionTable slide animation"
        );
    }

    mod close_snapshot_tests {
        use super::*;
        use std::collections::BTreeMap;
        use std::fs::{self, OpenOptions};
        use std::os::fd::{AsFd as _, OwnedFd};
        use std::os::unix::net::UnixStream;
        use std::sync::atomic::{AtomicU64, Ordering};

        use calloop::EventLoop;
        use koma_model::{AnimationChannelConfig, AnimationConfig};
        use smithay::reexports::wayland_protocols::xdg::shell::client::{
            xdg_surface, xdg_toplevel, xdg_wm_base,
        };
        use smithay::reexports::wayland_server::Display;
        use wayland_client::protocol::{
            wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool,
            wl_subcompositor, wl_subsurface, wl_surface,
        };
        use wayland_client::{Connection, Dispatch, EventQueue, delegate_noop};

        static SHM_SEQ: AtomicU64 = AtomicU64::new(0);

        fn create_shm_fd(len: usize) -> OwnedFd {
            let seq = SHM_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "koma-close-snapshot-test-{}-{seq}.bin",
                std::process::id()
            ));
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .expect("create shm backing file");
            let _ = fs::remove_file(&path);
            file.set_len(len as u64).expect("set shm file length");
            OwnedFd::from(file)
        }

        #[derive(Default)]
        struct TestClient {
            sync_done: bool,
            globals: BTreeMap<String, (u32, u32)>,
            compositor: Option<wl_compositor::WlCompositor>,
            subcompositor: Option<wl_subcompositor::WlSubcompositor>,
            shm: Option<wl_shm::WlShm>,
            wm_base: Option<xdg_wm_base::XdgWmBase>,
        }

        impl TestClient {
            fn bind<I: wayland_client::Proxy + 'static>(
                &self,
                registry: &wl_registry::WlRegistry,
                qh: &wayland_client::QueueHandle<Self>,
                interface: &str,
            ) -> I
            where
                Self: Dispatch<I, ()>,
            {
                let (name, version) = self
                    .globals
                    .get(interface)
                    .copied()
                    .unwrap_or_else(|| panic!("missing global {interface}"));
                registry.bind::<I, (), Self>(name, version, qh, ())
            }
        }

        impl Dispatch<wl_registry::WlRegistry, ()> for TestClient {
            fn event(
                state: &mut Self,
                _: &wl_registry::WlRegistry,
                event: wl_registry::Event,
                _: &(),
                _: &Connection,
                _: &wayland_client::QueueHandle<Self>,
            ) {
                if let wl_registry::Event::Global {
                    name,
                    interface,
                    version,
                } = event
                {
                    state.globals.insert(interface, (name, version));
                }
            }
        }

        impl Dispatch<wl_callback::WlCallback, ()> for TestClient {
            fn event(
                state: &mut Self,
                _: &wl_callback::WlCallback,
                event: wl_callback::Event,
                _: &(),
                _: &Connection,
                _: &wayland_client::QueueHandle<Self>,
            ) {
                if matches!(event, wl_callback::Event::Done { .. }) {
                    state.sync_done = true;
                }
            }
        }

        impl Dispatch<xdg_wm_base::XdgWmBase, ()> for TestClient {
            fn event(
                _: &mut Self,
                wm_base: &xdg_wm_base::XdgWmBase,
                event: xdg_wm_base::Event,
                _: &(),
                _: &Connection,
                _: &wayland_client::QueueHandle<Self>,
            ) {
                if let xdg_wm_base::Event::Ping { serial } = event {
                    wm_base.pong(serial);
                }
            }
        }

        impl Dispatch<xdg_surface::XdgSurface, ()> for TestClient {
            fn event(
                _: &mut Self,
                xdg_surface: &xdg_surface::XdgSurface,
                event: xdg_surface::Event,
                _: &(),
                _: &Connection,
                _: &wayland_client::QueueHandle<Self>,
            ) {
                if let xdg_surface::Event::Configure { serial } = event {
                    xdg_surface.ack_configure(serial);
                }
            }
        }

        delegate_noop!(TestClient: ignore wl_compositor::WlCompositor);
        delegate_noop!(TestClient: ignore wl_subcompositor::WlSubcompositor);
        delegate_noop!(TestClient: ignore wl_subsurface::WlSubsurface);
        delegate_noop!(TestClient: ignore wl_surface::WlSurface);
        delegate_noop!(TestClient: ignore wl_shm::WlShm);
        delegate_noop!(TestClient: ignore wl_shm_pool::WlShmPool);
        delegate_noop!(TestClient: ignore wl_buffer::WlBuffer);
        delegate_noop!(TestClient: ignore xdg_toplevel::XdgToplevel);

        struct CloseTestEnv {
            event_loop: EventLoop<'static, KomaState>,
            display: Display<KomaState>,
            state: KomaState,
            _output: Output,
            _output_global: smithay::reexports::wayland_server::backend::GlobalId,
            conn: Connection,
            queue: EventQueue<TestClient>,
            client: TestClient,
            registry: wl_registry::WlRegistry,
        }

        impl CloseTestEnv {
            fn new() -> Self {
                let (event_loop, display, mut state) = make_headless_test_state();
                let output = test_output("DP-1");
                let output_global = output.create_global::<KomaState>(&state.display_handle);
                state.test_outputs.push(output.clone());

                let (server_sock, client_sock) = UnixStream::pair().expect("UnixStream::pair");
                display
                    .handle()
                    .insert_client(server_sock, Arc::new(ClientState::default()))
                    .expect("insert client");
                let conn = Connection::from_socket(client_sock).expect("Connection::from_socket");
                let queue = conn.new_event_queue::<TestClient>();
                let qh = queue.handle();
                let registry = conn.display().get_registry(&qh, ());

                let mut env = Self {
                    event_loop,
                    display,
                    state,
                    _output: output,
                    _output_global: output_global,
                    conn,
                    queue,
                    client: TestClient::default(),
                    registry,
                };
                env.pump();
                env
            }

            fn pump(&mut self) {
                let qh = self.queue.handle();
                self.client.sync_done = false;
                let _cb = self.conn.display().sync(&qh, ());
                self.conn.flush().expect("flush client");
                for _ in 0..16 {
                    self.display
                        .dispatch_clients(&mut self.state)
                        .expect("dispatch server clients");
                    self.event_loop
                        .dispatch(Some(Duration::ZERO), &mut self.state)
                        .expect("dispatch calloop");
                    self.display.flush_clients().expect("flush server clients");
                    if let Some(guard) = self.queue.prepare_read() {
                        let _ = guard.read();
                    }
                    self.queue
                        .dispatch_pending(&mut self.client)
                        .expect("dispatch client pending");
                    self.conn.flush().expect("flush client post-dispatch");
                    if self.client.sync_done {
                        break;
                    }
                }
                assert!(self.client.sync_done, "client-server sync failed");
            }

            fn ensure_globals(
                &mut self,
            ) -> (
                wl_compositor::WlCompositor,
                wl_subcompositor::WlSubcompositor,
                wl_shm::WlShm,
                xdg_wm_base::XdgWmBase,
            ) {
                let qh = self.queue.handle();
                let compositor = self.client.compositor.clone().unwrap_or_else(|| {
                    self.client.bind::<wl_compositor::WlCompositor>(
                        &self.registry,
                        &qh,
                        "wl_compositor",
                    )
                });
                self.client.compositor = Some(compositor.clone());
                let subcompositor = self.client.subcompositor.clone().unwrap_or_else(|| {
                    self.client.bind::<wl_subcompositor::WlSubcompositor>(
                        &self.registry,
                        &qh,
                        "wl_subcompositor",
                    )
                });
                self.client.subcompositor = Some(subcompositor.clone());
                let shm = self.client.shm.clone().unwrap_or_else(|| {
                    self.client
                        .bind::<wl_shm::WlShm>(&self.registry, &qh, "wl_shm")
                });
                self.client.shm = Some(shm.clone());
                let wm_base = self.client.wm_base.clone().unwrap_or_else(|| {
                    self.client
                        .bind::<xdg_wm_base::XdgWmBase>(&self.registry, &qh, "xdg_wm_base")
                });
                self.client.wm_base = Some(wm_base.clone());
                (compositor, subcompositor, shm, wm_base)
            }

            fn create_buffer(
                &self,
                shm: &wl_shm::WlShm,
            ) -> (wl_shm_pool::WlShmPool, wl_buffer::WlBuffer) {
                let qh = self.queue.handle();
                let fd = create_shm_fd(64 * 64 * 4);
                let pool = shm.create_pool(fd.as_fd(), 64 * 64 * 4, &qh, ());
                let buf = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &qh, ());
                (pool, buf)
            }

            fn spawn_window(
                &mut self,
                title: &str,
            ) -> (
                wl_surface::WlSurface,
                xdg_surface::XdgSurface,
                xdg_toplevel::XdgToplevel,
            ) {
                let qh = self.queue.handle();
                let (compositor, _, shm, wm_base) = self.ensure_globals();
                let surface = compositor.create_surface(&qh, ());
                let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
                let toplevel = xdg_surface.get_toplevel(&qh, ());
                toplevel.set_title(title.to_owned());
                surface.commit();
                self.pump();

                let (pool, buf) = self.create_buffer(&shm);
                surface.attach(Some(&buf), 0, 0);
                surface.commit();
                self.pump();

                buf.destroy();
                pool.destroy();
                (surface, xdg_surface, toplevel)
            }
        }

        #[test]
        fn close_animation_enabled_respects_global_channel_instant_and_none_style() {
            let mut cfg = AnimationConfig::default();
            assert!(KomaState::close_animation_enabled(&cfg));

            cfg.enabled = false;
            assert!(!KomaState::close_animation_enabled(&cfg));

            cfg.enabled = true;
            cfg.windows_close = AnimationChannelConfig {
                enabled: Some(false),
                ..Default::default()
            };
            assert!(!KomaState::close_animation_enabled(&cfg));

            cfg.windows_close = AnimationChannelConfig {
                curve: Some("instant".to_owned()),
                ..Default::default()
            };
            assert!(!KomaState::close_animation_enabled(&cfg));

            cfg.windows_close = AnimationChannelConfig {
                style: Some("none".to_owned()),
                ..Default::default()
            };
            assert!(!KomaState::close_animation_enabled(&cfg));

            cfg.windows_close = AnimationChannelConfig {
                style: Some("popin 85%".to_owned()),
                ..Default::default()
            };
            assert!(KomaState::close_animation_enabled(&cfg));
        }

        #[test]
        fn normal_commits_and_initial_map_never_capture_fbo_and_evict_stale_cache() {
            let mut env = CloseTestEnv::new();
            let (surface, xdg_surface, toplevel) = env.spawn_window("VideoPlayer");
            assert_eq!(env.state.mapped_windows.len(), 1);
            let win_id = env.state.mapped_windows[0].win_id;

            assert_eq!(env.state.toplevel_pre_commit_hooks.len(), 1);
            assert_eq!(
                env.state.unmap_snapshot_attempts, 0,
                "initial map promotion must not trigger offscreen FBO rendering"
            );
            assert!(env.state.window_frames.is_empty());

            // Simulate repeated active frame commits (new buffer + damage-only commit).
            let (_, _, shm, _) = env.ensure_globals();
            for _ in 0..5 {
                let (pool, buf) = env.create_buffer(&shm);
                surface.attach(Some(&buf), 0, 0);
                surface.damage_buffer(0, 0, 64, 64);
                surface.commit();
                env.pump();
                buf.destroy();
                pool.destroy();
            }
            surface.damage_buffer(0, 0, 16, 16);
            surface.commit();
            env.pump();

            assert_eq!(
                env.state.unmap_snapshot_attempts, 0,
                "normal surface commits must do zero offscreen FBO rendering"
            );
            assert!(env.state.window_frames.is_empty());

            // A stale entry (e.g. from an earlier non-fatal subsurface teardown) is evicted on
            // both new-buffer and bufferless commits without re-rendering.
            env.state
                .window_frames
                .insert(win_id, WindowFrameCache::default());
            let (pool, buf) = env.create_buffer(&shm);
            surface.attach(Some(&buf), 0, 0);
            surface.commit();
            env.pump();
            buf.destroy();
            pool.destroy();
            assert!(env.state.window_frames.is_empty());
            assert_eq!(env.state.unmap_snapshot_attempts, 0);

            env.state
                .window_frames
                .insert(win_id, WindowFrameCache::default());
            surface.damage_buffer(0, 0, 8, 8);
            surface.commit();
            env.pump();
            assert!(env.state.window_frames.is_empty());
            assert_eq!(env.state.unmap_snapshot_attempts, 0);

            toplevel.destroy();
            xdg_surface.destroy();
            surface.destroy();
            env.pump();
            assert!(env.state.toplevel_pre_commit_hooks.is_empty());
            assert!(env.state.window_frames.is_empty());
        }

        #[test]
        fn explicit_unmap_commit_captures_snapshot_only_when_windows_close_enabled() {
            let mut env = CloseTestEnv::new();
            let (surface, xdg_surface, toplevel) = env.spawn_window("UnmapTest");
            assert_eq!(env.state.mapped_windows.len(), 1);
            let win_id = env.state.mapped_windows[0].win_id;

            // Explicit unmap commit with windows_close enabled: pre-commit hook runs while the
            // surface still holds its buffer, then commit transitions the window back to unmapped.
            surface.attach(None, 0, 0);
            surface.commit();
            env.pump();

            assert_eq!(
                env.state.unmap_snapshot_attempts, 1,
                "attach(null) + commit must trigger pre-commit snapshot capture once"
            );
            assert!(
                env.state.last_snapshot_had_live_buffer,
                "pre-commit hook must capture before on_commit_buffer_handler clears the buffer"
            );
            assert!(env.state.mapped_windows.is_empty());
            assert_eq!(env.state.unmapped_windows.len(), 1);
            assert!(env.state.window_frames.is_empty());
            assert_eq!(env.state.toplevel_pre_commit_hooks.len(), 1);

            // Re-map the same toplevel afresh (initial commit -> configure/ack -> buffer commit).
            surface.commit();
            env.pump();
            let (_, _, shm, _) = env.ensure_globals();
            let (pool, buf) = env.create_buffer(&shm);
            surface.attach(Some(&buf), 0, 0);
            surface.commit();
            env.pump();
            buf.destroy();
            pool.destroy();
            assert_eq!(env.state.mapped_windows.len(), 1);
            assert_eq!(env.state.unmap_snapshot_attempts, 1);

            // Disable windows_close and unmap again: zero snapshot capture.
            env.state
                .lua_sandbox
                .eval_init_lua(
                    "koma.appearance({ animations = { windows_close = { enabled = false } } })",
                )
                .expect("disable windows_close");
            surface.attach(None, 0, 0);
            surface.commit();
            env.pump();

            assert_eq!(
                env.state.unmap_snapshot_attempts, 1,
                "unmapping with windows_close disabled must skip snapshot capture"
            );
            assert!(env.state.window_frames.is_empty());
            assert!(env.state.closing_windows.is_empty());
            assert!(!env.state.window_animations.borrow().is_closing(win_id));

            // Re-map and disable global animations, then unmap: also skips capture.
            surface.commit();
            env.pump();
            let (pool, buf) = env.create_buffer(&shm);
            surface.attach(Some(&buf), 0, 0);
            surface.commit();
            env.pump();
            buf.destroy();
            pool.destroy();
            env.state
                .lua_sandbox
                .eval_init_lua("koma.appearance({ animations = { enabled = false } })")
                .expect("disable all animations");
            surface.attach(None, 0, 0);
            surface.commit();
            env.pump();
            assert_eq!(env.state.unmap_snapshot_attempts, 1);

            toplevel.destroy();
            xdg_surface.destroy();
            surface.destroy();
            env.pump();
            assert!(env.state.toplevel_pre_commit_hooks.is_empty());
            assert!(env.state.unmapped_windows.is_empty());
            assert!(env.state.window_frames.is_empty());
        }

        #[test]
        fn subsurface_destruction_and_both_destroy_orders_cleanup_hooks_without_leaks() {
            let mut env = CloseTestEnv::new();
            let (surface, xdg_surface, toplevel) = env.spawn_window("SubsurfaceOwner");
            let win_id = env.state.mapped_windows[0].win_id;
            let qh = env.queue.handle();
            let (compositor, subcompositor, shm, _) = env.ensure_globals();

            let sub_surface = compositor.create_surface(&qh, ());
            let subsurface = subcompositor.get_subsurface(&sub_surface, &surface, &qh, ());
            subsurface.set_desync();
            let (pool, buf) = env.create_buffer(&shm);
            sub_surface.attach(Some(&buf), 0, 0);
            sub_surface.commit();
            surface.commit();
            env.pump();
            buf.destroy();
            pool.destroy();

            assert_eq!(env.state.unmap_snapshot_attempts, 0);
            assert!(env.state.window_frames.is_empty());

            // Async subsurface commit evicts any stale cached frame without re-rendering.
            env.state
                .window_frames
                .insert(win_id, WindowFrameCache::default());
            let (pool2, buf2) = env.create_buffer(&shm);
            sub_surface.attach(Some(&buf2), 0, 0);
            sub_surface.commit();
            env.pump();
            buf2.destroy();
            pool2.destroy();
            assert!(env.state.window_frames.is_empty());
            assert_eq!(env.state.unmap_snapshot_attempts, 0);

            // Destroying the subsurface while windows_close is enabled triggers snapshot capture
            // before Smithay detaches the subsurface.
            subsurface.destroy();
            sub_surface.destroy();
            env.pump();
            assert_eq!(env.state.unmap_snapshot_attempts, 1);
            assert!(env.state.last_snapshot_had_live_buffer);

            // A second subsurface destroyed while windows_close is disabled does not capture.
            env.state
                .lua_sandbox
                .eval_init_lua(
                    "koma.appearance({ animations = { windows_close = { enabled = false } } })",
                )
                .expect("disable windows_close");
            let sub_surface2 = compositor.create_surface(&qh, ());
            let subsurface2 = subcompositor.get_subsurface(&sub_surface2, &surface, &qh, ());
            subsurface2.set_desync();
            sub_surface2.commit();
            env.pump();
            subsurface2.destroy();
            sub_surface2.destroy();
            env.pump();
            assert_eq!(env.state.unmap_snapshot_attempts, 1);

            // Standard destroy order (`toplevel` before `wl_surface`) frees everything cleanly.
            toplevel.destroy();
            xdg_surface.destroy();
            surface.destroy();
            env.pump();
            assert!(env.state.mapped_windows.is_empty());
            assert!(env.state.unmapped_windows.is_empty());
            assert!(env.state.window_frames.is_empty());
            assert!(env.state.toplevel_pre_commit_hooks.is_empty());
            assert!(env.state.subsurface_roots.is_empty());

            // Reverse destroy order (`wl_surface` destroyed before `xdg_toplevel`, as can happen
            // on unexpected client disconnect) still captures once before cleanup and leaks zero hooks.
            env.state
                .lua_sandbox
                .eval_init_lua(
                    "koma.appearance({ animations = { windows_close = { enabled = true } } })",
                )
                .expect("re-enable windows_close");
            let (surface2, xdg_surface2, toplevel2) = env.spawn_window("ReverseDestroy");
            assert_eq!(env.state.toplevel_pre_commit_hooks.len(), 1);
            surface2.destroy();
            env.pump();
            assert_eq!(env.state.unmap_snapshot_attempts, 2);
            assert!(env.state.last_snapshot_had_live_buffer);
            assert!(
                env.state.toplevel_pre_commit_hooks.is_empty(),
                "WlSurface::destroyed must drop the pre-commit hook even before toplevel_destroyed"
            );

            toplevel2.destroy();
            xdg_surface2.destroy();
            env.pump();
            assert!(env.state.mapped_windows.is_empty());
            assert!(env.state.unmapped_windows.is_empty());
            assert!(env.state.window_frames.is_empty());
            assert!(env.state.toplevel_pre_commit_hooks.is_empty());
            assert!(env.state.subsurface_roots.is_empty());

            // Never-mapped toplevel destroyed cleanly removes its hook with zero snapshot attempts.
            let wm_base = env.client.wm_base.clone().unwrap();
            let unmapped_surface = compositor.create_surface(&qh, ());
            let unmapped_xdg = wm_base.get_xdg_surface(&unmapped_surface, &qh, ());
            let unmapped_tl = unmapped_xdg.get_toplevel(&qh, ());
            unmapped_surface.commit();
            env.pump();
            assert_eq!(env.state.toplevel_pre_commit_hooks.len(), 1);
            assert_eq!(env.state.unmap_snapshot_attempts, 2);
            unmapped_tl.destroy();
            unmapped_xdg.destroy();
            unmapped_surface.destroy();
            env.pump();
            assert!(env.state.toplevel_pre_commit_hooks.is_empty());
            assert!(env.state.unmapped_windows.is_empty());
            assert_eq!(env.state.unmap_snapshot_attempts, 2);
        }
    }
}
