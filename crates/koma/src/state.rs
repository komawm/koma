//! Compositor runtime state (`KomaState`), unified `ActiveBackend` (`Winit` | `Tty`),
//! Hyprland `CSpace` dual-layer (`Dwindle` / `Scrolling` / `Deck` + `FloatingStack`) &
//! `special:<name>` Scratchpad orchestration, `security-context-v1` isolation, and
//! two-phase `Unmapped -> Mapped` window lifecycle.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::OsString,
    sync::Arc,
    time::{Duration, Instant},
};

use calloop::{LoopHandle, LoopSignal, RegistrationToken};
use koma_lua::{ActionDescriptor, TrampolineScheduler, phodopus_bridge::PhodopusSandbox};
use koma_model::{
    LayoutMode, LogicalPoint, LogicalRect, LogicalViewport, OpenMode, OutputLayout,
    OutputTransform, PresentationTarget, SafeAreaInsets, SavedOutputConfig, WindowAnimationTable,
    WindowMatchContext, WindowRuleEngine, WorkspaceLeaseState, WorkspaceState,
};
use koma_proto::{
    CapabilityToken, Epoch, EventTopic, OutputId, SeatId, TokenClaims, WindowId, WorkspaceEvent,
    WorkspaceId,
};
use koma_render::{
    LazyFboPool, blur_pipeline::CachedBackdropBlur, frame_composer::WindowSceneItem,
    offscreen_fbo::LazyOffscreenSurface,
};
use koma_seat::VirtualSeatLedger;
use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        renderer::{ImportDma as _, gles::GlesRenderer, utils::with_renderer_surface_state},
    },
    desktop::{
        LayerMap, PopupManager, Window, WindowSurfaceType, layer_map_for_output,
        utils::under_from_surface_tree,
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
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{IsAlive as _, Logical, Point, SERIAL_COUNTER, Size, Transform},
    wayland::{
        compositor::{
            CompositorClientState, CompositorState, TraversalAction, get_parent, with_states,
            with_surface_tree_downward,
        },
        cursor_shape::CursorShapeManagerState,
        dmabuf::DmabufState,
        foreign_toplevel_list::ForeignToplevelListState,
        fractional_scale::{FractionalScaleManagerState, with_fractional_scale},
        idle_inhibit::IdleInhibitManagerState,
        idle_notify::IdleNotifierState,
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        pointer_gestures::PointerGesturesState,
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
                SurfaceCachedState, ToplevelSurface, XdgShellState, XdgToplevelSurfaceData,
                decoration::XdgDecorationState,
            },
        },
        shm::ShmState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
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
    backend_winit::WinitBackendState,
    config_watcher::{ConfigWatcherHandle, KomaConfigPath},
    cursor::CursorManager,
    desktop_protocols::{ActivationTokenPolicy, DeferredXdgActivation, StoredToplevelIcon},
    foreign_toplevel::WlrForeignToplevelState,
    gamma_control::{GammaControlState, GammaLut},
    gesture_dispatcher::GestureTable,
    ipc_server::IpcClientConn,
    keybind_dispatcher::KeybindTable,
    macro_runtime::{ActivePointerAnimation, WorkspaceDamageTracker},
    output_management::WlrOutputManagerState,
    output_power::{OutputPowerMode, OutputPowerState},
    screencopy::ScreencopyState,
    security_context::{
        JailedWaylandSocket, NewClient, PeerSocketCredentials, client_can_view_primary_selection,
        client_is_unrestricted, is_surface_from_restricted_client, query_peer_credentials,
        resolve_surface_jailed_workspace,
    },
    xwayland_satellite::SatelliteSlot,
};
use koma_lua::schema::{
    DeviceOverrideSpec, InputConfigSpec, KeyboardInputSpec, KomaConfigRegistry,
};
use smithay::{
    backend::input::{Event as _, InputBackend, TouchEvent as _},
    input::touch::{
        DownEvent as TouchDownPayload, MotionEvent as TouchMotionPayload, UpEvent as TouchUpPayload,
    },
    utils::Rectangle,
    wayland::{
        input_method::{InputMethodManagerState, PopupSurface as ImePopupSurface},
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
    },
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
    pub is_maximized: bool,
    pub is_minimized: bool,
    pub is_sandboxed: bool,
    pub preferred_size: (u32, u32),
    pub rect: LogicalRect,
    pub decoration_mode: DecorationMode,
    pub icon: Option<StoredToplevelIcon>,
}

pub struct KomaState {
    pub start_time: Instant,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub loop_handle: Option<LoopHandle<'static, Self>>,
    pub trampoline_timer_token: Option<RegistrationToken>,
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
    pub idle_notifier_state: IdleNotifierState<Self>,
    #[allow(dead_code)]
    pub idle_inhibit_state: IdleInhibitManagerState,
    pub idle_inhibitors: HashSet<WlSurface>,
    #[allow(dead_code)]
    pub pointer_constraints_state: PointerConstraintsState,
    #[allow(dead_code)]
    pub relative_pointer_manager_state: RelativePointerManagerState,
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

    // Physical/nested host seat (`seat0` / `libseat`)
    pub seat: Seat<Self>,
    pub pointer_location: Point<f64, Logical>,
    pub cursor_manager: CursorManager,
    pub cursor_anim_timer_token: Option<RegistrationToken>,

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
    pub window_animations: RefCell<WindowAnimationTable>,
    /// Live 4-quadrant `smart_split` / `Scrolling` column drop zone `(ws_id, preview_rect)` during drag.
    pub active_drop_preview: Option<(WorkspaceId, LogicalRect)>,
    /// Active `MoveWindowGrab` cursor state `(ws_id, cursor_x)` driving continuous VBlank edge auto-scroll.
    pub active_drag_edge_scroll: Option<(WorkspaceId, f64)>,
    pub edge_scroll_keep_alive: bool,
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

        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
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
        let idle_notifier_state = IdleNotifierState::new(&display_handle, loop_handle.clone());
        let idle_inhibit_state = IdleInhibitManagerState::new::<Self>(&display_handle);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);
        let relative_pointer_manager_state =
            RelativePointerManagerState::new::<Self>(&display_handle);
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
            display_handle,
            loop_signal,
            loop_handle: Some(loop_handle),
            trampoline_timer_token: None,
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
            idle_notifier_state,
            idle_inhibit_state,
            idle_inhibitors: HashSet::new(),
            pointer_constraints_state,
            relative_pointer_manager_state,
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
            seat,
            pointer_location: Point::from((
                f64::from(initial_viewport.width) * 0.5,
                f64::from(initial_viewport.height) * 0.5,
            )),
            cursor_manager: CursorManager::new(),
            cursor_anim_timer_token: None,
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
            window_animations: RefCell::new(WindowAnimationTable::default()),
            active_drop_preview: None,
            active_drag_edge_scroll: None,
            edge_scroll_keep_alive: false,
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

        self.vseats
            .insert(vseat_id.clone(), VirtualSeatLedger::new(vseat_id));
        self.workspaces.insert(ws_id.clone(), ws);
        ws_id
    }

    /// Resolves an existing workspace by `WorkspaceId` or `name`, or lazily creates a new workspace
    /// applying `registry.default_layout` and any matching `WorkspaceSpec` (`layout`, `gaps`).
    pub fn ensure_workspace_by_name_or_id(&mut self, query: &str) -> WorkspaceId {
        if query.starts_with(koma_model::SPECIAL_WORKSPACE_PREFIX) {
            return self.ensure_special_workspace(Some(query));
        }
        if let Some((existing_id, _)) = self
            .workspaces
            .iter()
            .find(|(id, ws)| id.as_str() == query || ws.name == query)
        {
            return existing_id.clone();
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

        self.vseats
            .insert(vseat_id.clone(), VirtualSeatLedger::new(vseat_id));
        self.workspaces.insert(ws_id.clone(), ws);
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
            if let Some(pointer) = self.seat.get_pointer()
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
        true
    }

    pub fn move_active_window_between_workspaces(
        &mut self,
        src_ws_id: &WorkspaceId,
        dst_ws_id: &WorkspaceId,
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
        self.focus_active_deck_window();
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
        self.gestures.reset_active_sessions();

        let now_input = smithay::backend::input::InputTime::now();
        self.break_active_pointer_constraints();
        let _ = crate::pointer_grabs::cancel_active_pointer_grab(
            self,
            SERIAL_COUNTER.next_serial(),
            now_input,
        );
        if let Some(pointer) = self.seat.get_pointer() {
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
        if let Some(touch) = self.seat.get_touch() {
            touch.cancel(self);
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
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
        let (_, max_sz) = read_toplevel_min_max_size(toplevel.wl_surface());
        let has_parent = toplevel.parent().is_some();
        let initial_size = if has_parent || (max_sz.0 > 0 && max_sz.1 > 0) {
            None
        } else {
            let wa = viewport.work_area_rect();
            let inner_w = wa.w.saturating_sub(24) as i32;
            let inner_h = wa.h.saturating_sub(24) as i32;
            Some(Size::from((inner_w.max(64), inner_h.max(64))))
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
        let fullscreen_win = ws.fullscreen_window;
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
                        anim_cfg,
                        bypass,
                    );
                }
                let is_fs = fullscreen_win == Some(win_id);
                let is_max = mapped.is_maximized;
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
            is_maximized: false,
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
        self.mapped_windows.push(mapped);
        self.sync_workspace_geometries(&ws_id);
        self.sync_window_output_membership(&ws_id);
        if let Some(final_rect) = self
            .mapped_windows
            .iter()
            .find(|m| m.win_id == win_id_created)
            .map(|m| m.rect)
        {
            let anim_cfg = self.lua_sandbox.config_snapshot().appearance.animations;
            self.window_animations.borrow_mut().on_window_opened(
                win_id_created,
                final_rect,
                now_ms,
                anim_cfg,
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

    #[must_use]
    fn active_deck_wl_surface(&self) -> Option<WlSurface> {
        let target_ws_id = self.effective_interactive_ws_id();
        let active_id = self
            .workspaces
            .get(&target_ws_id)
            .and_then(WorkspaceState::active_window)?;
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

        if let Some(keyboard) = self.seat.get_keyboard()
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
        let active_id = self
            .workspaces
            .get(&target_ws_id)
            .and_then(WorkspaceState::active_window)
            .filter(|id| {
                self.mapped_windows
                    .iter()
                    .any(|m| m.win_id == *id && !m.is_minimized)
            });
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

        if let Some((_, _, surface, origin)) = self.hit_test_visible_surface(global_pos) {
            return Some(PointerHitTarget::Window { surface, origin });
        }

        self.layer_surface_under_tiers(global_pos, [Layer::Bottom, Layer::Background])
    }

    /// Unified surface hit-test returning the focused Wayland surface and logical origin.
    #[must_use]
    pub fn surface_under(
        &self,
        global_pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.hit_test_target(global_pos).map(|target| match target {
            PointerHitTarget::LockSurface { surface, origin }
            | PointerHitTarget::Layer {
                surface, origin, ..
            }
            | PointerHitTarget::Window {
                surface, origin, ..
            } => (surface, origin),
        })
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
            let Some(ws) = self.workspaces.get(&ws_id) else {
                continue;
            };
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
                let local_origin =
                    Point::from((f64::from(rect.x) + b_off, f64::from(rect.y) + b_off));
                let local_pos = output_local - local_origin;
                let global_origin = local_origin + out_origin;
                if let Some((surf, surf_offset)) = mapped
                    .window
                    .surface_under(local_pos, WindowSurfaceType::ALL)
                {
                    return Some((win_id, ws_id, surf, surf_offset.to_f64() + global_origin));
                }
                let in_frame = output_local.x >= f64::from(rect.x)
                    && output_local.x <= f64::from(rect.x + rect.w as i32)
                    && output_local.y >= f64::from(rect.y)
                    && output_local.y <= f64::from(rect.y + rect.h as i32);
                if in_frame && let Some(toplevel) = mapped.window.toplevel() {
                    return Some((win_id, ws_id, toplevel.wl_surface().clone(), global_origin));
                }
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
            if ws.layout_mode == LayoutMode::Scrolling && ws.fullscreen_window.is_none() {
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
            self.queue_redraw();
            return;
        }

        if let Some((win_id, hit_ws_id, _, _)) = self.hit_test_visible_surface(global_pos) {
            self.focused_ondemand_layer = None;
            if self.active_special_ws_id.is_some()
                && self.active_special_ws_id.as_ref() != Some(&hit_ws_id)
            {
                self.active_special_ws_id = None;
            }
            if let Some(hit_out) = self.output_at_point_strict(global_pos) {
                let hit_out_name = hit_out.name();
                self.focused_output_name = Some(hit_out_name.clone());
                if !self
                    .workspaces
                    .get(&hit_ws_id)
                    .is_some_and(|w| w.is_special)
                {
                    self.output_workspaces
                        .insert(hit_out_name, hit_ws_id.clone());
                    self.active_ws_id = hit_ws_id.clone();
                }
            }
            let now_ms = self.elapsed_ms();
            if let Some(ws) = self.workspaces.get_mut(&hit_ws_id) {
                ws.activate_window(win_id, now_ms);
            }
            self.sync_workspace_geometries(&hit_ws_id);
            self.focus_active_deck_window();
            self.queue_redraw();
        } else {
            self.focused_ondemand_layer = None;
            if let Some(hit_out) = self.output_at_point_strict(global_pos) {
                let hit_out_name = hit_out.name();
                self.focused_output_name = Some(hit_out_name.clone());
                self.active_ws_id = self.active_workspace_for_output(&hit_out_name);
            }
            self.focus_active_deck_window();
        }
    }

    /// Collects **front-to-back** `WindowSceneItem`s for a single workspace (`ws_id`).
    #[must_use]
    pub fn drop_preview_for_workspace(&self, ws_id: &WorkspaceId) -> Option<LogicalRect> {
        self.active_drop_preview
            .as_ref()
            .and_then(|(id, rect)| (id == ws_id).then_some(*rect))
    }

    /// Advances per-frame `ScrollingStrip::camera_spring` settlement and interactive drag edge
    /// auto-scroll (`30 px` trigger zone, `100 ms` debounce, `1500 px/s` ramp) before rendering!
    pub fn tick_frame_animations_and_edge_scroll(&mut self) {
        let now_ms = self.elapsed_ms();
        for ws in self.workspaces.values_mut() {
            let _ = ws.scrolling.sample_camera_x(now_ms);
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
        self.edge_scroll_keep_alive
            || self
                .window_animations
                .borrow_mut()
                .has_active_animations(now_ms)
            || self
                .workspaces
                .values()
                .any(|ws| ws.scrolling.is_camera_animating(now_ms))
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
        let fullscreen_id = ws.fullscreen_window;
        let is_agent_ws = matches!(ws.presentation, PresentationTarget::Offscreen { .. });
        let now_ms = self.elapsed_ms();
        let is_scrolling_strip = fullscreen_id.is_none() && ws.layout_mode == LayoutMode::Scrolling;
        let camera_shift_px = if is_scrolling_strip {
            if is_screencast_or_agent_capture || is_agent_ws {
                ws.scrolling.camera_x.round() as i32
            } else {
                ws.scrolling.peek_camera_x(now_ms).round() as i32
            }
        } else {
            0
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

                let base_opacity = dyn_rules.opacity.unwrap_or(1.0);
                let is_fs = fullscreen_id == Some(win_id);
                let corner_radius = if is_fs {
                    0.0
                } else {
                    f32::from(dyn_rules.corner_radius.unwrap_or(default_radius))
                };

                let (mut render_rect, eff_opacity) =
                    if is_screencast_or_agent_capture || is_agent_ws {
                        (target_rect, base_opacity)
                    } else {
                        let sampled = self.window_animations.borrow_mut().sample_visual(
                            win_id,
                            target_rect,
                            now_ms,
                        );
                        (
                            sampled.rect,
                            (base_opacity * sampled.opacity_multiplier).clamp(0.0, 1.0),
                        )
                    };

                if is_scrolling_strip && !is_floating {
                    render_rect.x -= camera_shift_px;
                    if render_rect.x + render_rect.w as i32 <= 0
                        || render_rect.x >= ws.viewport.width as i32
                    {
                        continue;
                    }
                }

                items.push(WindowSceneItem {
                    window: &mapped.window,
                    rect: render_rect,
                    is_floating,
                    is_focused,
                    is_protected_surface: is_protected,
                    opacity: eff_opacity,
                    corner_radius,
                    wants_blur: blur_enabled && !is_fs,
                    border: border_cfg.clone(),
                    focus_ring: focus_ring_cfg.clone(),
                    shadow: shadow_cfg.clone(),
                    dim: dim_cfg,
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
}

impl KomaState {
    /// Fail-closed IME barrier enforced whenever `SessionLockPhase != Unlocked`:
    /// drops active `zwp_text_input_v3` focus/enter and discards all IME popup surfaces.
    pub fn clear_ime_state_for_lock(&mut self) {
        let text_input = self.seat.text_input();
        if text_input.focus().is_some() {
            text_input.leave();
            text_input.set_focus(None);
        }
        self.ime_popups.clear();
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
            let win_geo_loc = m.window.geometry().loc;
            let ws_origin = self.workspace_output_origin(&m.ws_id).to_i32_round::<i32>();
            return Rectangle::new(
                Point::from((
                    ws_origin.x + m.rect.x - win_geo_loc.x,
                    ws_origin.y + m.rect.y - win_geo_loc.y,
                )),
                Size::from((m.rect.w.max(1) as i32, m.rect.h.max(1) as i32)),
            );
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
        let Some(keyboard) = self.seat.get_keyboard() else {
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
        let Some(touch) = self.seat.get_touch() else {
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
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let under = self.surface_under(pos);
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
        let Some(touch) = self.seat.get_touch() else {
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
        if let Some(touch) = self.seat.get_touch() {
            touch.frame(self);
        }
    }

    pub fn handle_touch_cancel(&mut self) {
        if let Some(touch) = self.seat.get_touch() {
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
        self.break_active_pointer_constraints();
        self.sync_workspace_geometries(&ws_id);
        self.focus_active_deck_window();
        self.reconcile_idle_inhibitors();
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

    pub fn set_window_maximized(&mut self, win_id: WindowId, maximized: bool) {
        let Some(mapped) = self.mapped_windows.iter_mut().find(|m| m.win_id == win_id) else {
            return;
        };
        if mapped.is_maximized == maximized {
            if let Some(toplevel) = mapped.window.toplevel() {
                toplevel.send_pending_configure();
            }
            return;
        }
        mapped.is_maximized = maximized;
        let ws_id = mapped.ws_id.clone();
        self.sync_workspace_geometries(&ws_id);
        self.sync_foreign_toplevels();
        self.queue_redraw();
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
        } else {
            let now_ms = self.elapsed_ms();
            if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                ws.activate_window(win_id, now_ms);
            }
        }
        self.focus_active_deck_window();
        self.reconcile_idle_inhibitors();
        self.sync_foreign_toplevels();
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
            let is_fs = ws.fullscreen_window == Some(win_id);
            if is_fs != fullscreen {
                ws.toggle_fullscreen(win_id);
            }
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
}
