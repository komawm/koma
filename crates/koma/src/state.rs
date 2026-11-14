//! Compositor runtime state (`KomaState`), unified `ActiveBackend` (`Winit` | `Tty`),
//! Hyprland `CSpace` dual-layer (`Dwindle` / `Scrolling` / `Deck` + `FloatingStack`) &
//! `special:<name>` Scratchpad orchestration, `security-context-v1` isolation, and
//! two-phase `Unmapped -> Mapped` window lifecycle.

use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsString,
    sync::Arc,
    time::{Duration, Instant},
};

use calloop::{LoopHandle, LoopSignal, RegistrationToken};
use koma_lua::{ActionDescriptor, TrampolineScheduler, phodopus_bridge::PhodopusSandbox};
use koma_model::{
    LayoutMode, LogicalPoint, LogicalRect, LogicalViewport, OpenMode, PresentationTarget,
    WindowAnimationTable, WindowMatchContext, WindowRuleEngine, WorkspaceLeaseState,
    WorkspaceState,
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
        renderer::{ImportDma as _, gles::GlesRenderer},
    },
    desktop::{PopupManager, Window, WindowSurfaceType},
    input::{Seat, SeatState, keyboard::XkbConfig, pointer::CursorImageStatus},
    output::Output,
    reexports::wayland_server::{
        DisplayHandle,
        backend::{ClientData, ClientId, DisconnectReason},
        protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Size},
    wayland::{
        compositor::{CompositorClientState, CompositorState, with_states},
        dmabuf::DmabufState,
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        pointer_gestures::PointerGesturesState,
        security_context::{SecurityContext, SecurityContextState},
        selection::{
            data_device::DataDeviceState,
            ext_data_control::DataControlState as ExtDataControlState,
            primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState as WlrDataControlState,
        },
        session_lock::SessionLockManagerState,
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{SurfaceCachedState, ToplevelSurface, XdgShellState, XdgToplevelSurfaceData},
        },
        shm::ShmState,
        viewporter::ViewporterState,
        virtual_keyboard::VirtualKeyboardManagerState,
    },
};
use tracing::{debug, info, warn};

#[cfg(feature = "smithay-tty")]
use crate::backend_tty::TtyBackendState;
use crate::{
    backend_winit::WinitBackendState,
    config_watcher::{ConfigWatcherHandle, KomaConfigPath},
    gesture_dispatcher::GestureTable,
    ipc_server::IpcClientConn,
    keybind_dispatcher::KeybindTable,
    macro_runtime::{ActivePointerAnimation, WorkspaceDamageTracker},
    security_context::{
        JailedWaylandSocket, NewClient, PeerSocketCredentials, client_can_view_primary_selection,
        client_is_unrestricted, is_surface_from_restricted_client, query_peer_credentials,
        resolve_surface_jailed_workspace,
    },
    xwayland_satellite::SatelliteSlot,
};

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
    pub is_sandboxed: bool,
    pub preferred_size: (u32, u32),
    pub rect: LogicalRect,
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
    pub is_session_locked: bool,
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
    pub dmabuf_state: DmabufState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,
    pub popups: PopupManager,

    // Physical/nested host seat (`seat0` / `libseat`)
    pub seat: Seat<Self>,
    pub pointer_location: Point<f64, Logical>,
    pub cursor_status: CursorImageStatus,

    // Koma pure domain model (`koma-model` + `koma-lua` + `koma-seat`)
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
    pub redraw_queued: bool,
}

impl KomaState {
    pub fn new(
        display_handle: DisplayHandle,
        loop_signal: LoopSignal,
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

        let pointer_gestures_state = PointerGesturesState::new::<Self>(&display_handle);
        let dmabuf_state = DmabufState::new();
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(&display_handle);

        let mut seat = seat_state.new_wl_seat(&display_handle, seat_name);
        seat.add_keyboard(XkbConfig::default(), 220, 35)
            .expect("failed to initialize XKB keyboard on host seat");
        seat.add_pointer();

        // Human `Headed` workspaces default to Hyprland's `Dwindle` binary tree (`LayoutMode::Dwindle`)!
        let active_ws_id = WorkspaceId::from("ws_01");
        let vseat_id = SeatId::from("vseat_01");
        let ws = WorkspaceState::new(
            active_ws_id.clone(),
            "1",
            vseat_id.clone(),
            initial_viewport,
            PresentationTarget::Physical(OutputId::from("primary")),
            LayoutMode::Dwindle,
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
            loop_handle: None,
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
            is_session_locked: false,
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
            dmabuf_state,
            viewporter_state,
            fractional_scale_state,
            popups: PopupManager::default(),
            seat,
            pointer_location: Point::from((
                f64::from(initial_viewport.width) * 0.5,
                f64::from(initial_viewport.height) * 0.5,
            )),
            cursor_status: CursorImageStatus::default_named(),
            active_ws_id,
            active_special_ws_id: None,
            workspaces,
            vseats,
            rules: WindowRuleEngine::new(),
            keybinds: KeybindTable::default(),
            gestures: GestureTable::default(),
            pending_passthrough_actions: Vec::new(),
            lua_scheduler: TrampolineScheduler::default(),
            lua_sandbox: PhodopusSandbox::default(),
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

        let ws = WorkspaceState::new(
            ws_id.clone(),
            canonical_name,
            vseat_id.clone(),
            viewport,
            PresentationTarget::Physical(OutputId::from("primary")),
            LayoutMode::Dwindle,
            now_ms,
        );
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
        self.backend
            .as_ref()
            .and_then(ActiveBackend::primary_output)
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
            let inner_w = viewport.width.saturating_sub(24) as i32;
            let inner_h = viewport.height.saturating_sub(24) as i32;
            Some(Size::from((inner_w.max(64), inner_h.max(64))))
        };

        toplevel.with_pending_state(|state| {
            state.size = initial_size;
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
        let anim_cfg = self.lua_sandbox.config_snapshot().appearance.animations;

        for (win_id, target_rect, is_float) in visible {
            if let Some(mapped) = self.mapped_windows.iter_mut().find(|m| m.win_id == win_id) {
                let rect_changed = mapped.rect != target_rect;
                let size_changed = mapped.rect.w != target_rect.w || mapped.rect.h != target_rect.h;
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
                if size_changed {
                    let inner_w = target_rect.w.saturating_sub(4).max(48) as i32;
                    let inner_h = target_rect.h.saturating_sub(4).max(48) as i32;
                    if let Some(toplevel) = mapped.window.toplevel() {
                        toplevel.with_pending_state(|st| {
                            st.size = Some(Size::from((inner_w, inner_h)));
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
        let preferred_size = if geom_bbox.size.w > 0 && geom_bbox.size.h > 0 {
            (geom_bbox.size.w as u32, geom_bbox.size.h as u32)
        } else if max_sz.0 > 0 && max_sz.1 > 0 {
            max_sz
        } else {
            (
                (viewport.width * 3 / 5).max(360),
                (viewport.height * 3 / 5).max(260),
            )
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
                    .resolve_in(viewport.full_rect())
                    .ok()?;
                    Some(LogicalRect::new(
                        pt.x.round() as i32,
                        pt.y.round() as i32,
                        preferred_size.0,
                        preferred_size.1,
                    ))
                });

        let opt_max_size = (max_sz.0 > 0 || max_sz.1 > 0).then_some(max_sz);
        let cursor_logical = LogicalPoint {
            x: self.pointer_location.x,
            y: self.pointer_location.y,
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
            is_sandboxed,
            preferred_size,
            rect: viewport.full_rect(),
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

        if ws_id == self.active_ws_id || Some(&ws_id) == self.active_special_ws_id.as_ref() {
            self.focus_active_deck_window();
            self.queue_redraw();
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
        let viewport = LogicalViewport {
            width: logical_w.max(64),
            height: logical_h.max(64),
            scale,
        };

        let mut affected_ws: Vec<WorkspaceId> = Vec::new();
        for (ws_id, ws) in &mut self.workspaces {
            if matches!(ws.presentation, PresentationTarget::Physical(_)) {
                ws.viewport = viewport;
                ws.presentation = PresentationTarget::Physical(OutputId::new(output.name()));
                ws.recalculate(self.start_time.elapsed().as_millis() as u64);
                affected_ws.push(ws_id.clone());
            }
        }

        for ws_id in affected_ws {
            self.sync_workspace_geometries(&ws_id);
        }
        self.queue_redraw();
    }

    /// Focuses the active window of `effective_interactive_ws_id` (`special:<name>` when open, else `active_ws_id`).
    pub fn focus_active_deck_window(&mut self) {
        let target_ws_id = self.effective_interactive_ws_id();
        let active_id = self
            .workspaces
            .get(&target_ws_id)
            .and_then(WorkspaceState::active_window);
        let target_surface = active_id.and_then(|id| {
            self.mapped_windows
                .iter()
                .find(|m| m.win_id == id)
                .and_then(|m| m.window.toplevel().map(|t| t.wl_surface().clone()))
        });
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, target_surface, serial);
        }
    }

    /// Hit-tests `global_pos` across any open `special:<name>` Scratchpad first, then `active_ws_id`,
    /// strictly in **front-to-back** Z-order (`FloatingStack` topmost -> bottom -> Tiled panes).
    #[must_use]
    pub fn hit_test_visible_surface(
        &self,
        global_pos: Point<f64, Logical>,
    ) -> Option<(WindowId, WorkspaceId, WlSurface, Point<f64, Logical>)> {
        let candidate_workspaces = self
            .active_special_ws_id
            .iter()
            .chain(std::iter::once(&self.active_ws_id));

        for ws_id in candidate_workspaces {
            let Some(ws) = self.workspaces.get(ws_id) else {
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
                let origin = Point::from((f64::from(rect.x) + 2.0, f64::from(rect.y) + 2.0));
                let local_pos = global_pos - origin;
                if let Some((surf, surf_offset)) = mapped
                    .window
                    .surface_under(local_pos, WindowSurfaceType::ALL)
                {
                    return Some((win_id, ws_id.clone(), surf, surf_offset.to_f64() + origin));
                }
                let in_frame = global_pos.x >= f64::from(rect.x)
                    && global_pos.x <= f64::from(rect.x + rect.w as i32)
                    && global_pos.y >= f64::from(rect.y)
                    && global_pos.y <= f64::from(rect.y + rect.h as i32);
                if in_frame && let Some(toplevel) = mapped.window.toplevel() {
                    return Some((win_id, ws_id.clone(), toplevel.wl_surface().clone(), origin));
                }
            }
        }
        None
    }

    /// Activates & raises the window clicked at `global_pos` (and dismisses an open Scratchpad
    /// if the user clicks outside Scratchpad windows onto a background window!).
    pub fn focus_window_at_point(&mut self, global_pos: Point<f64, Logical>) {
        // First check if the user clicked a Scrolling `tab-indicator` pill segment
        let target_ws_id = self.effective_interactive_ws_id();
        let now_ms = self.elapsed_ms();
        let tab_hit = self.workspaces.get(&target_ws_id).and_then(|ws| {
            if ws.layout_mode == LayoutMode::Scrolling && ws.fullscreen_window.is_none() {
                ws.scrolling.hit_test_tab_indicator(
                    LogicalPoint {
                        x: global_pos.x,
                        y: global_pos.y,
                    },
                    ws.viewport,
                    now_ms,
                )
            } else {
                None
            }
        });
        if let Some(tab_win_id) = tab_hit {
            if let Some(ws) = self.workspaces.get_mut(&target_ws_id) {
                ws.activate_window(tab_win_id, now_ms);
            }
            self.sync_workspace_geometries(&target_ws_id);
            self.focus_active_deck_window();
            self.queue_redraw();
            return;
        }

        if let Some((win_id, hit_ws_id, _, _)) = self.hit_test_visible_surface(global_pos) {
            if self.active_special_ws_id.is_some()
                && self.active_special_ws_id.as_ref() != Some(&hit_ws_id)
            {
                self.active_special_ws_id = None;
            }
            let now_ms = self.elapsed_ms();
            if let Some(ws) = self.workspaces.get_mut(&hit_ws_id) {
                ws.activate_window(win_id, now_ms);
            }
            self.sync_workspace_geometries(&hit_ws_id);
            self.focus_active_deck_window();
            self.queue_redraw();
        } else {
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

        let mut items = Vec::with_capacity(candidates_bottom_to_top.len());
        for (win_id, target_rect, is_floating) in candidates_bottom_to_top.into_iter().rev() {
            if let Some(mapped) = self.mapped_windows.iter().find(|m| m.win_id == win_id) {
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
                });
            }
        }
        items
    }

    /// Collects **front-to-back** scene lists for physical output rendering:
    /// `(base_ws_items, Option<(special_lease_state, special_ws_items)>)`.
    #[must_use]
    pub fn collect_physical_scene_items(
        &self,
    ) -> (
        Vec<WindowSceneItem<'_>>,
        Option<(WorkspaceLeaseState, Vec<WindowSceneItem<'_>>)>,
    ) {
        let base_items = self.collect_workspace_scene_items(&self.active_ws_id, false);
        let special_overlay = self.active_special_ws_id.as_ref().and_then(|sp_id| {
            let sp_ws = self.workspaces.get(sp_id)?;
            let sp_items = self.collect_workspace_scene_items(sp_id, false);
            Some((sp_ws.lease.state().clone(), sp_items))
        });
        (base_items, special_overlay)
    }

    /// Returns all `WlSurface` roots currently visible on the physical output so `wl_surface.frame`
    /// callbacks advance smoothly across every tiled/floating/Scratchpad window!
    #[must_use]
    pub fn visible_physical_toplevel_surfaces(&self) -> Vec<WlSurface> {
        let mut surfaces = Vec::new();
        for ws_id in self
            .active_special_ws_id
            .iter()
            .chain(std::iter::once(&self.active_ws_id))
        {
            if let Some(ws) = self.workspaces.get(ws_id) {
                for (win_id, _, _) in ws.compute_visible_geometries() {
                    if let Some(m) = self.mapped_windows.iter().find(|m| m.win_id == win_id)
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
        if let Some(jail) = target_ws.and_then(|ws_id| self.jailed_wayland_sockets.get(ws_id)) {
            cmd.env("WAYLAND_DISPLAY", &jail.socket_path);
        } else {
            cmd.env("WAYLAND_DISPLAY", &self.socket_name);
        }

        if let Some(sig) = &self.instance_signature {
            cmd.env("KOMA_INSTANCE_SIGNATURE", sig);
        }

        let x11_display = target_ws
            .and_then(|ws_id| self.workspace_satellites.get(ws_id))
            .or(self.primary_satellite.as_ref())
            .map(|s| s.display_name());

        if let Some(disp) = x11_display {
            cmd.env("DISPLAY", disp);
        } else {
            cmd.env_remove("DISPLAY");
        }

        for (k, v) in &self.lua_sandbox.config_snapshot().env_vars {
            cmd.env(k, v);
        }
    }

    pub fn queue_redraw(&mut self) {
        self.redraw_queued = true;
        if let Some(backend) = &mut self.backend {
            backend.request_redraw();
        }
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
