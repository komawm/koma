//! `wlr-layer-shell` (`zwlr_layer_shell_v1`) Wayland client daemon multiplexed on `calloop`.
//!
//! Manages two independent layer surfaces:
//! 1. **Hover Thumbnail Popup** (`koma-overview-popup`): `400x252` `Overlay` surface anchored
//!    `Top | Left` with `KeyboardInteractivity::None` and `150 ms` dwell gate.
//! 2. **Full-Screen Live Overview Grid** (`koma-overview`): `Overlay` surface anchored to all 4
//!    screen edges with `KeyboardInteractivity::Exclusive`, `H/J/K/L` + click selection, `Return`
//!    to `Peek` (`Read-Only Shield`), and `T` to `Takeover`!

use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read as _,
    os::{
        fd::AsFd as _,
        unix::{fs::PermissionsExt as _, net::UnixListener},
    },
    panic::AssertUnwindSafe,
    path::PathBuf,
    time::{Duration, Instant},
};

use calloop::{
    EventLoop, Interest, Mode, PostAction,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use koma_model::LogicalRect;
use koma_proto::{
    WorkspaceId,
    overview::{OverviewProbe, probe_overview_daemon, remove_owned_file, write_pid_file},
    rpc::WorkspaceModeSpec,
    scm_unix::create_writable_shm_fd,
};
use tracing::{debug, info, warn};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::{self, WlShmPool},
        wl_surface::{self, WlSurface},
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::{
    compositor_card::{
        ARGB_BACKDROP_DIM, ARGB_TEXT_PRIMARY, CardVisualMeta, RawRgbaFrame, clear_argb8888_canvas,
        draw_label_5x7, paint_workspace_card,
    },
    grid_layout::OverviewGridState,
    hover_fsm::{
        HoverDwellMachine, HoverEffect, POPUP_CARD_HEIGHT, POPUP_CARD_WIDTH, POPUP_HEADER_HEIGHT,
        POPUP_PADDING_PX, PREVIEW_REFRESH_MS, THUMBNAIL_HEIGHT, THUMBNAIL_WIDTH,
    },
    ipc_bridge::{AdminIpcClient, InventoryWorkspaceItem},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerRoleTag {
    HoverPopup,
    FullscreenGrid,
}

/// How long a freshly created layer surface may stay unconfigured before it is recreated.
pub const CONFIGURE_TIMEOUT: Duration = Duration::from_millis(750);

/// How many times one role's layer surface is recreated before the daemon gives up on it.
pub const MAX_SURFACE_RECREATES: u8 = 3;

/// Largest canvas the daemon will allocate for one frame (`256 MiB`), so a bogus configure size
/// cannot abort the process through an allocation failure.
pub const MAX_CANVAS_BYTES: u64 = 256 * 1024 * 1024;

/// Whether a layer surface created at `created_at` is still unconfigured after
/// [`CONFIGURE_TIMEOUT`].
///
/// Split out of [`ActiveLayerWindow`] so the watchdog decision is testable without a live Wayland
/// object: a compositor that never sends `configure` is the case that used to leave the daemon
/// waiting silently.
#[must_use]
fn configure_is_overdue(configured: bool, created_at: Instant, now: Instant) -> bool {
    !configured && now.saturating_duration_since(created_at) >= CONFIGURE_TIMEOUT
}

/// Why [`run_overview_daemon`] returned.
///
/// The daemon used to return a bare `anyhow::Result`, so a normal `quit` was indistinguishable from
/// a fatal error and a panic left nothing behind. Every arm now carries a human-readable reason that
/// `main` logs with `info!`/`error!` and mirrors to stderr, which is journald's stream for this
/// process (komawm/koma#75).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonExit {
    /// A `quit` line arrived on the control socket (only `komawm/koma` tests and operators do this).
    ControlQuit {
        /// Control socket the daemon owned.
        socket: PathBuf,
        /// How long the daemon served before the command.
        uptime_ms: u64,
    },
    /// The host Wayland connection broke: the compositor exited, or it dropped this client.
    WaylandDisconnected {
        /// The IO error that ended the loop (`Broken pipe`, `Connection reset`, ...).
        detail: String,
        /// How long the daemon served before the connection broke.
        uptime_ms: u64,
    },
    /// The `calloop` event loop failed for a reason other than a lost Wayland connection.
    EventLoopError {
        /// Full error chain reported by `calloop`.
        detail: String,
        /// How long the daemon served before the loop failed.
        uptime_ms: u64,
    },
    /// A callback panicked; the loop was stopped instead of letting the process abort silently.
    Panicked {
        /// Panic message.
        detail: String,
        /// How long the daemon served before the panic.
        uptime_ms: u64,
    },
    /// The daemon never reached its event loop (display, protocol, socket or pid file failure).
    Startup {
        /// What failed.
        detail: String,
    },
}

impl DaemonExit {
    /// Builds a startup failure.
    #[must_use]
    pub fn startup(detail: impl Into<String>) -> Self {
        Self::Startup {
            detail: detail.into(),
        }
    }

    /// Human-readable reason, used verbatim in the journal line.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::ControlQuit { socket, uptime_ms } => {
                format!(
                    "`quit` requested on {} after {uptime_ms} ms",
                    socket.display()
                )
            }
            Self::WaylandDisconnected { detail, uptime_ms } => {
                format!("Wayland connection lost after {uptime_ms} ms: {detail}")
            }
            Self::EventLoopError { detail, uptime_ms } => {
                format!("event loop failed after {uptime_ms} ms: {detail}")
            }
            Self::Panicked { detail, uptime_ms } => {
                format!("panicked after {uptime_ms} ms: {detail}")
            }
            Self::Startup { detail } => format!("startup failed: {detail}"),
        }
    }

    /// Process exit code: `0` only for a requested `quit`.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::ControlQuit { .. } => 0,
            _ => 1,
        }
    }
}

impl std::fmt::Display for DaemonExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Classifies a `calloop` dispatch failure.
///
/// `calloop-wayland-source` surfaces a lost display as an IO error inside a
/// [`calloop::Error::OtherError`] chain (`other error during loop operation: underlying IO error:
/// Broken pipe`), which is the exact line the daemon logged before it exited during compositor
/// shutdown. Those are reported as [`DaemonExit::WaylandDisconnected`] so the reason reads
/// `Wayland connection lost` instead of a generic loop error.
#[must_use]
pub fn classify_loop_error(err: &calloop::Error, uptime_ms: u64) -> DaemonExit {
    let io = find_io_error(err);
    match io.map(std::io::Error::kind) {
        Some(
            std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof,
        ) => DaemonExit::WaylandDisconnected {
            detail: io.map_or_else(|| err.to_string(), std::string::ToString::to_string),
            uptime_ms,
        },
        _ => DaemonExit::EventLoopError {
            detail: err.to_string(),
            uptime_ms,
        },
    }
}

/// Walks an error's `source()` chain looking for an [`std::io::Error`].
fn find_io_error<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a std::io::Error> {
    let mut current = Some(error);
    while let Some(link) = current {
        if let Some(io) = link.downcast_ref::<std::io::Error>() {
            return Some(io);
        }
        current = link.source();
    }
    None
}

struct ActiveLayerWindow {
    wl_surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    configured_size: Option<(u32, u32)>,
    /// When this surface was created, used to notice a missing `configure`.
    created_at: Instant,
    /// How many times this role's surface was already recreated for lack of a configure.
    recreates: u8,
    /// Anchor margins of the popup surface, preserved across recreations (`(left, top)`).
    popup_margins: (i32, i32),
}

impl ActiveLayerWindow {
    fn destroy(self) {
        self.layer_surface.destroy();
        self.wl_surface.destroy();
    }

    /// `true` once the surface waited longer than [`CONFIGURE_TIMEOUT`] without a `configure`.
    fn configure_is_overdue(&self, now: Instant) -> bool {
        configure_is_overdue(self.configured_size.is_some(), self.created_at, now)
    }
}

pub struct OverviewDaemon {
    start_time: Instant,
    should_exit: bool,
    qh: QueueHandle<Self>,

    // Bound Wayland globals
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    seat: Option<WlSeat>,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    output_size: (u32, u32),
    pointer_xy: (f64, f64),

    // Active layer surfaces
    popup_win: Option<ActiveLayerWindow>,
    grid_win: Option<ActiveLayerWindow>,
    /// Set once a role exhausted [`MAX_SURFACE_RECREATES`], so the give-up warning is emitted once.
    grid_recreate_suppressed: bool,
    popup_recreate_suppressed: bool,

    // State machines & Koma IPC cache
    hover_fsm: HoverDwellMachine,
    grid: OverviewGridState,
    ipc: Option<AdminIpcClient>,
    inventory_cache: Vec<InventoryWorkspaceItem>,
    frame_cache: HashMap<WorkspaceId, RawRgbaFrame>,
}

impl OverviewDaemon {
    fn elapsed_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    fn ensure_ipc(&mut self) -> Option<&mut AdminIpcClient> {
        if self.ipc.is_none() {
            match AdminIpcClient::connect() {
                Ok(client) => self.ipc = Some(client),
                Err(err) => warn!("unable to connect to koma-admin.sock: {err}"),
            }
        }
        self.ipc.as_mut()
    }

    fn refresh_inventory(&mut self) {
        if let Some(ipc) = self.ensure_ipc()
            && let Ok(items) = ipc.fetch_inventory()
        {
            self.inventory_cache = items;
        }
    }

    fn handle_control_command(&mut self, line: &str) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let now_ms = self.elapsed_ms();
        match parts.first().copied() {
            Some("hover") => {
                let ws_str = parts.get(1).copied().unwrap_or("ws_01");
                let anchor_x: i32 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(16);
                let bar_h: i32 = parts.get(3).and_then(|s| s.parse().ok()).unwrap_or(32);
                if let Some(fx) =
                    self.hover_fsm
                        .on_hover_enter(WorkspaceId::new(ws_str), anchor_x, bar_h, now_ms)
                {
                    self.apply_hover_effect(fx);
                }
            }
            Some("leave") => {
                if let Some(fx) = self.hover_fsm.on_hover_leave() {
                    self.apply_hover_effect(fx);
                }
            }
            Some("toggle") => {
                self.toggle_fullscreen_grid();
            }
            Some("quit") => {
                self.should_exit = true;
            }
            _ => {}
        }
    }

    fn apply_hover_effect(&mut self, effect: HoverEffect) {
        match effect {
            HoverEffect::ArmDwellTimer { .. } => {
                // Handled by the daemon's periodic tick timer when active/dwelling.
            }
            HoverEffect::ShowOrMovePopup {
                unsubscribe_prev,
                subscribe_new,
                margin_left,
                margin_top,
            } => {
                if let Some(prev) = unsubscribe_prev
                    && let Some(ipc) = self.ensure_ipc()
                {
                    let _ = ipc.set_preview_subscribe(&prev, false);
                }
                if let Some(ipc) = self.ensure_ipc() {
                    let _ = ipc.set_preview_subscribe(&subscribe_new, true);
                }
                self.ensure_popup_surface_mapped(margin_left, margin_top);
                self.capture_and_draw_popup(&subscribe_new);
            }
            HoverEffect::RefreshPopupFrame { ws_id } => {
                self.capture_and_draw_popup(&ws_id);
            }
            HoverEffect::HidePopup { unsubscribe_ws } => {
                if let Some(win) = self.popup_win.take() {
                    win.destroy();
                }
                if let Some(ipc) = self.ensure_ipc() {
                    let _ = ipc.set_preview_subscribe(&unsubscribe_ws, false);
                }
            }
        }
    }

    fn ensure_popup_surface_mapped(&mut self, margin_left: i32, margin_top: i32) {
        if let Some(existing) = &self.popup_win {
            existing
                .layer_surface
                .set_margin(margin_top, 0, 0, margin_left);
            existing.wl_surface.commit();
            return;
        }
        self.create_popup_surface(margin_left, margin_top, 0);
    }

    fn create_popup_surface(&mut self, margin_left: i32, margin_top: i32, recreates: u8) {
        let (Some(comp), Some(ls)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        let wl_surface = comp.create_surface(&self.qh, ());
        let layer_surface = ls.get_layer_surface(
            &wl_surface,
            None,
            Layer::Overlay,
            "koma-overview-popup".to_owned(),
            &self.qh,
            LayerRoleTag::HoverPopup,
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Left);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_size(POPUP_CARD_WIDTH, POPUP_CARD_HEIGHT);
        layer_surface.set_margin(margin_top, 0, 0, margin_left);
        wl_surface.commit();

        self.popup_win = Some(ActiveLayerWindow {
            wl_surface,
            layer_surface,
            configured_size: None,
            created_at: Instant::now(),
            recreates,
            popup_margins: (margin_left, margin_top),
        });
    }

    fn capture_and_draw_popup(&mut self, ws_id: &WorkspaceId) {
        self.refresh_inventory();
        if let Some(ipc) = self.ensure_ipc()
            && let Ok(frame) = ipc.capture_zero_copy_frame(ws_id, THUMBNAIL_WIDTH)
        {
            self.frame_cache.insert(ws_id.clone(), frame);
        }
        let Some(popup) = &self.popup_win else {
            return;
        };
        if popup.configured_size.is_none() {
            return;
        }
        let wl_surface = popup.wl_surface.clone();

        let Some(canvas_len) = canvas_byte_len(POPUP_CARD_WIDTH, POPUP_CARD_HEIGHT) else {
            warn!(
                width = POPUP_CARD_WIDTH,
                height = POPUP_CARD_HEIGHT,
                "refusing to allocate an oversized hover-popup frame"
            );
            return;
        };
        let meta = self.build_card_meta(ws_id, true);
        let mut canvas = vec![0_u8; canvas_len];
        let card_rect = LogicalRect::new(0, 0, POPUP_CARD_WIDTH, POPUP_CARD_HEIGHT);
        let header_rect = LogicalRect::new(
            POPUP_PADDING_PX as i32,
            4,
            THUMBNAIL_WIDTH,
            POPUP_HEADER_HEIGHT,
        );
        let thumb_rect = LogicalRect::new(
            POPUP_PADDING_PX as i32,
            (POPUP_HEADER_HEIGHT + 4) as i32,
            THUMBNAIL_WIDTH,
            THUMBNAIL_HEIGHT,
        );

        paint_workspace_card(
            &mut canvas,
            POPUP_CARD_WIDTH,
            POPUP_CARD_HEIGHT,
            card_rect,
            header_rect,
            thumb_rect,
            &meta,
            self.frame_cache.get(ws_id),
        );
        self.attach_and_commit_shm(&wl_surface, POPUP_CARD_WIDTH, POPUP_CARD_HEIGHT, &canvas);
    }

    fn toggle_fullscreen_grid(&mut self) {
        if self.grid.is_open {
            self.close_fullscreen_grid();
        } else {
            self.open_fullscreen_grid();
        }
    }

    fn open_fullscreen_grid(&mut self) {
        if let Some(fx) = self.hover_fsm.on_hover_leave() {
            self.apply_hover_effect(fx);
        }
        self.refresh_inventory();
        let ws_ids: Vec<WorkspaceId> = self
            .inventory_cache
            .iter()
            .map(|i| i.ws_id.clone())
            .collect();
        for ws_id in &ws_ids {
            if let Some(ipc) = self.ensure_ipc() {
                let _ = ipc.set_preview_subscribe(ws_id, true);
            }
        }

        self.grid.is_open = true;
        self.grid.selected_idx = self
            .inventory_cache
            .iter()
            .position(|i| i.focused)
            .unwrap_or(0);
        self.grid
            .recompute_layout(self.output_size.0, self.output_size.1, &ws_ids);
        self.create_grid_surface(0);
    }

    /// Creates (or recreates) the full-screen grid surface.
    ///
    /// `recreates` counts how many times this surface was already recreated because the compositor
    /// closed it or never configured it; a transient configure failure must never end the daemon.
    fn create_grid_surface(&mut self, recreates: u8) {
        let (Some(comp), Some(ls)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        let wl_surface = comp.create_surface(&self.qh, ());
        let layer_surface = ls.get_layer_surface(
            &wl_surface,
            None,
            Layer::Overlay,
            "koma-overview".to_owned(),
            &self.qh,
            LayerRoleTag::FullscreenGrid,
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_size(0, 0);
        wl_surface.commit();

        self.grid_win = Some(ActiveLayerWindow {
            wl_surface,
            layer_surface,
            configured_size: None,
            created_at: Instant::now(),
            recreates,
            popup_margins: (0, 0),
        });
    }

    /// Recreates the grid surface after a compositor `closed` event or a missing `configure`.
    ///
    /// Bounded by [`MAX_SURFACE_RECREATES`]: after that the daemon keeps running and reports once
    /// that the compositor is refusing the surface, instead of exiting.
    fn recreate_grid_surface(&mut self, reason: &str) {
        let recreates = self.grid_win.as_ref().map_or(0, |win| win.recreates);
        if recreates >= MAX_SURFACE_RECREATES {
            if !self.grid_recreate_suppressed {
                self.grid_recreate_suppressed = true;
                warn!(
                    namespace = "koma-overview",
                    reason,
                    recreates,
                    "compositor keeps refusing the overview layer surface; keeping the daemon alive \
                     without the grid"
                );
            }
            if let Some(win) = self.grid_win.take() {
                win.destroy();
            }
            self.grid.is_open = false;
            return;
        }
        warn!(
            namespace = "koma-overview",
            reason,
            attempt = recreates + 1,
            "recreating the overview layer surface"
        );
        if let Some(win) = self.grid_win.take() {
            win.destroy();
        }
        self.create_grid_surface(recreates + 1);
    }

    /// Recreates the hover popup after a compositor `closed` event, bounded like the grid.
    fn recreate_popup_surface(&mut self, reason: &str) {
        let Some(win) = self.popup_win.take() else {
            return;
        };
        let recreates = win.recreates;
        let (margin_left, margin_top) = win.popup_margins;
        win.destroy();
        if recreates >= MAX_SURFACE_RECREATES {
            if !self.popup_recreate_suppressed {
                self.popup_recreate_suppressed = true;
                warn!(
                    namespace = "koma-overview-popup",
                    reason,
                    recreates,
                    "compositor keeps refusing the hover popup layer surface; hover preview disabled"
                );
            }
            return;
        }
        self.create_popup_surface(margin_left, margin_top, recreates + 1);
    }

    /// `calloop` timer hook: notices a layer surface that never received a `configure`.
    ///
    /// A compositor that refuses the surface (`zwlr_layer_surface_v1.closed`, or a configure it
    /// drops) used to leave the daemon waiting forever with no diagnostic; here the surface is
    /// recreated a bounded number of times and the daemon keeps serving the control socket.
    fn poll_surface_configure(&mut self) {
        let now = Instant::now();
        if self
            .grid_win
            .as_ref()
            .is_some_and(|win| win.configure_is_overdue(now))
        {
            self.recreate_grid_surface("configure timeout");
            return;
        }
        if self
            .popup_win
            .as_ref()
            .is_some_and(|win| win.configure_is_overdue(now))
        {
            self.recreate_popup_surface("configure timeout");
        }
    }

    fn close_fullscreen_grid(&mut self) {
        self.grid.is_open = false;
        if let Some(win) = self.grid_win.take() {
            win.destroy();
        }
        let ws_ids: Vec<WorkspaceId> = self
            .inventory_cache
            .iter()
            .map(|i| i.ws_id.clone())
            .collect();
        for ws_id in &ws_ids {
            if let Some(ipc) = self.ensure_ipc() {
                let _ = ipc.set_preview_subscribe(ws_id, false);
            }
        }
    }

    fn capture_and_draw_grid(&mut self) {
        if !self.grid.is_open {
            return;
        }
        let Some((w, h)) = self.grid_win.as_ref().and_then(|g| g.configured_size) else {
            return;
        };

        self.refresh_inventory();
        let ws_ids: Vec<WorkspaceId> = self
            .inventory_cache
            .iter()
            .map(|i| i.ws_id.clone())
            .collect();
        self.grid.recompute_layout(w, h, &ws_ids);

        let slots_snapshot = self.grid.slots.clone();
        for slot in &slots_snapshot {
            if let Some(ipc) = self.ensure_ipc()
                && let Ok(frame) = ipc.capture_zero_copy_frame(&slot.ws_id, slot.thumb_rect.w)
            {
                self.frame_cache.insert(slot.ws_id.clone(), frame);
            }
        }
        self.draw_grid_from_cache();
    }

    /// Redraws the overview grid from `frame_cache` and `inventory_cache` without re-capturing
    /// workspaces over IPC on every hover or keyboard selection step.
    fn draw_grid_from_cache(&mut self) {
        if !self.grid.is_open {
            return;
        }
        let Some((w, h)) = self.grid_win.as_ref().and_then(|g| g.configured_size) else {
            return;
        };
        let Some(wl_surface) = self.grid_win.as_ref().map(|g| g.wl_surface.clone()) else {
            return;
        };

        let Some(canvas_len) = canvas_byte_len(w, h) else {
            warn!(
                width = w,
                height = h,
                "refusing to allocate an oversized overview grid frame"
            );
            return;
        };
        let mut canvas = vec![0_u8; canvas_len];
        clear_argb8888_canvas(&mut canvas, ARGB_BACKDROP_DIM);
        for slot in &self.grid.slots {
            let meta = self.build_card_meta(&slot.ws_id, slot.is_selected);
            paint_workspace_card(
                &mut canvas,
                w,
                h,
                slot.card_rect,
                slot.header_rect,
                slot.thumb_rect,
                &meta,
                self.frame_cache.get(&slot.ws_id),
            );
        }

        let hint = "CLICK/ENTER: SWITCH  |  ARROWS/HJKL: SELECT  |  ESC: CLOSE";
        let hint_w = (hint.chars().count() as i32) * 12;
        let hint_x = ((w as i32 - hint_w) / 2).max(16);
        let hint_y = (h as i32 - 28).max(8);
        draw_label_5x7(
            &mut canvas,
            w,
            h,
            hint_x,
            hint_y,
            (w as i32 - 16).max(hint_x),
            hint,
            ARGB_TEXT_PRIMARY,
        );

        self.attach_and_commit_shm(&wl_surface, w, h, &canvas);
    }

    fn build_card_meta(&self, ws_id: &WorkspaceId, is_selected: bool) -> CardVisualMeta {
        if let Some(item) = self.inventory_cache.iter().find(|i| &i.ws_id == ws_id) {
            let is_agent = item.frame_tier.contains("headless") || item.name.starts_with("agent");
            CardVisualMeta {
                title_label: format_card_title(
                    &item.name,
                    item.focused,
                    is_agent,
                    item.is_human_preempted,
                ),
                is_selected,
                is_agent_leased: is_agent,
                is_human_preempted: item.is_human_preempted,
            }
        } else {
            CardVisualMeta {
                title_label: ws_id.as_str().to_owned(),
                is_selected,
                is_agent_leased: false,
                is_human_preempted: false,
            }
        }
    }

    fn attach_and_commit_shm(&self, wl_surface: &WlSurface, w: u32, h: u32, pixels: &[u8]) {
        let Some(shm) = &self.shm else {
            return;
        };
        let Ok(fd) = create_writable_shm_fd("koma_overview_shm", pixels) else {
            return;
        };
        let file = File::from(fd);

        let stride = (w * 4) as i32;
        let pool = shm.create_pool(file.as_fd(), pixels.len() as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            stride,
            wl_shm::Format::Argb8888,
            &self.qh,
            (),
        );
        pool.destroy(); // Buffer retains kernel reference until compositor sends WlBuffer::Release!

        wl_surface.attach(Some(&buffer), 0, 0);
        wl_surface.damage_buffer(0, 0, w as i32, h as i32);
        wl_surface.commit();
    }
}

/// Runs the persistent daemon until something ends it, and reports why.
///
/// Every exit path is a [`DaemonExit`]; nothing returns silently. A layer-surface configure failure
/// is *not* an exit path any more: the surface is recreated a bounded number of times while the
/// control socket keeps answering (`recreate_grid_surface` / `poll_surface_configure`).
pub fn run_overview_daemon() -> DaemonExit {
    let start = Instant::now();
    let paths = match crate::ipc_bridge::runtime_lifecycle_paths() {
        Ok(paths) => paths,
        Err(err) => {
            return DaemonExit::startup(format!(
                "instance runtime directory is unavailable: {err}"
            ));
        }
    };

    // Refuse to steal a live daemon's socket: the old code unlinked and rebound, which left the
    // first daemon running with no reachable socket (`komawm/koma#75`).
    if let OverviewProbe::Live { pid } = probe_overview_daemon(&paths) {
        return DaemonExit::startup(format!(
            "another koma-overview daemon {} already listens on {}",
            pid.map_or_else(|| "of unknown pid".to_owned(), |pid| format!("(pid {pid})")),
            paths.socket.display()
        ));
    }

    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(err) => {
            return DaemonExit::startup(format!(
                "failed connecting to the host Wayland display: {err:#}"
            ));
        }
    };
    let mut event_queue = conn.new_event_queue::<OverviewDaemon>();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut daemon = OverviewDaemon {
        start_time: start,
        should_exit: false,
        qh,
        compositor: None,
        shm: None,
        layer_shell: None,
        seat: None,
        keyboard: None,
        pointer: None,
        output_size: (1920, 1080),
        pointer_xy: (0.0, 0.0),
        popup_win: None,
        grid_win: None,
        grid_recreate_suppressed: false,
        popup_recreate_suppressed: false,
        hover_fsm: HoverDwellMachine::default(),
        grid: OverviewGridState::default(),
        ipc: None,
        inventory_cache: Vec::new(),
        frame_cache: HashMap::new(),
    };

    if let Err(err) = event_queue.roundtrip(&mut daemon) {
        return DaemonExit::startup(format!("initial Wayland registry roundtrip failed: {err}"));
    }
    if daemon.layer_shell.is_none() {
        return DaemonExit::startup(
            "the host compositor does not advertise zwlr_layer_shell_v1 (not a wlroots-compatible \
             layer-shell host?)",
        );
    }

    let mut event_loop: EventLoop<'static, OverviewDaemon> = match EventLoop::try_new() {
        Ok(event_loop) => event_loop,
        Err(err) => return DaemonExit::startup(format!("failed creating the calloop loop: {err}")),
    };

    // 1. Multiplex the Wayland client queue onto `calloop`.
    if let Err(err) = WaylandSource::new(conn, event_queue).insert(event_loop.handle()) {
        return DaemonExit::startup(format!(
            "failed inserting the Wayland source into calloop: {err}"
        ));
    }

    // 2. Bind the control socket for Waybar / Ironbar / keybind triggers.
    if let Some(parent) = paths.socket.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        return DaemonExit::startup(format!("failed creating {}: {err}", parent.display()));
    }
    // Only reached when no listener answered, so the file is a stale inode from a dead daemon.
    if paths.socket.exists() {
        if let Err(err) = fs::remove_file(&paths.socket) {
            return DaemonExit::startup(format!(
                "failed reclaiming stale control socket {}: {err}",
                paths.socket.display()
            ));
        }
        debug!(
            socket = %paths.socket.display(),
            "reclaimed a stale koma-overview control socket"
        );
    }
    let listener = match UnixListener::bind(&paths.socket) {
        Ok(listener) => listener,
        Err(err) => {
            return DaemonExit::startup(format!(
                "failed binding control socket {}: {err}",
                paths.socket.display()
            ));
        }
    };
    if let Err(err) = fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600)) {
        return DaemonExit::startup(format!(
            "failed setting control socket permissions on {}: {err}",
            paths.socket.display()
        ));
    }
    if let Err(err) = listener.set_nonblocking(true) {
        return DaemonExit::startup(format!(
            "failed making the control socket non-blocking: {err}"
        ));
    }
    if let Err(err) = write_pid_file(&paths.pid_file) {
        // Not fatal: the pid file only sharpens diagnostics, the socket is what callers use.
        warn!(
            pid_file = %paths.pid_file.display(),
            %err,
            "failed writing the koma-overview pid file"
        );
    }
    info!(
        ctl_socket = %paths.socket.display(),
        pid = std::process::id(),
        "koma-overview control socket listening"
    );

    if let Err(err) = event_loop.handle().insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        |_, listener, state| {
            while let Ok((mut stream, _)) = listener.accept() {
                // A client that connects and never writes must not freeze the daemon: the
                // accepted socket is blocking, so bound the read instead of waiting for EOF.
                let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                let mut cmd = String::new();
                let _ = stream.read_to_string(&mut cmd);
                for line in cmd.lines() {
                    state.handle_control_command(line.trim());
                }
            }
            Ok(PostAction::Continue)
        },
    ) {
        let _ = fs::remove_file(&paths.socket);
        remove_owned_file(&paths.pid_file, std::process::id());
        return DaemonExit::startup(format!("failed inserting the control socket source: {err}"));
    }

    // 3. Periodic timer: 150ms hover dwell checks, 8Hz preview refresh while active, and a
    //    configure watchdog for both layer surfaces.
    if let Err(err) = event_loop.handle().insert_source(
        Timer::from_duration(Duration::from_millis(25)),
        |_, _, state| {
            let now = state.elapsed_ms();
            if let Some(fx) = state.hover_fsm.poll_tick(now) {
                state.apply_hover_effect(fx);
            }
            state.poll_surface_configure();
            if state.grid.is_open {
                state.capture_and_draw_grid();
            }
            let next_ms = if state.grid.is_open {
                PREVIEW_REFRESH_MS
            } else {
                25
            };
            TimeoutAction::ToDuration(Duration::from_millis(next_ms))
        },
    ) {
        let _ = fs::remove_file(&paths.socket);
        remove_owned_file(&paths.pid_file, std::process::id());
        return DaemonExit::startup(format!("failed inserting the overview timer: {err}"));
    }

    let exit = loop {
        if daemon.should_exit {
            break DaemonExit::ControlQuit {
                socket: paths.socket.clone(),
                uptime_ms: uptime_ms(start),
            };
        }
        // A panic in a rendering callback used to end the process with only a stderr line; the
        // guard turns it into a described exit that `main` logs, and the panic hook adds the
        // backtrace-friendly line.
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            event_loop.dispatch(Some(Duration::from_millis(50)), &mut daemon)
        })) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => break classify_loop_error(&err, uptime_ms(start)),
            Err(payload) => {
                break DaemonExit::Panicked {
                    detail: panic_payload_message(payload.as_ref()),
                    uptime_ms: uptime_ms(start),
                };
            }
        }
    };

    // Closing the loop first releases the `UnixListener`, so removing the path below cannot take
    // away a socket that another daemon rebound in the meantime.
    drop(event_loop);
    // Remove this process' own socket and pid file so the next daemon binds cleanly and
    // `komactl doctor` does not report a phantom socket.
    let _ = fs::remove_file(&paths.socket);
    remove_owned_file(&paths.pid_file, std::process::id());
    debug!(
        socket = %paths.socket.display(),
        "removed the koma-overview control socket"
    );
    exit
}

/// Milliseconds since the daemon start, saturating for very long sessions.
fn uptime_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Renders a panic payload without assuming it is a `String`.
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Largest canvas for `width x height` ARGB buffers, or `None` when the configure size is absurd.
///
/// A bogus size used to reach `vec![0; w * h * 4]`, whose multiplication could overflow or abort
/// the whole daemon; refusing the frame keeps the socket alive.
fn canvas_byte_len(width: u32, height: u32) -> Option<usize> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(4)?;
    if bytes == 0 || bytes > MAX_CANVAS_BYTES {
        return None;
    }
    usize::try_from(bytes).ok()
}

// ==============================================================================
// Wayland Client Protocol `Dispatch` Implementations (`wayland-client 0.31.15`)
// ==============================================================================

impl Dispatch<WlRegistry, ()> for OverviewDaemon {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor =
                        Some(registry.bind::<WlCompositor, _, _>(name, version.min(6), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind::<WlShm, _, _>(name, version.min(1), qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell =
                        Some(registry.bind::<ZwlrLayerShellV1, _, _>(name, version.min(4), qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind::<WlSeat, _, _>(name, version.min(7), qh, ()));
                }
                "wl_output" => {
                    let _ = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, LayerRoleTag> for OverviewDaemon {
    fn event(
        state: &mut Self,
        surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        role: &LayerRoleTag,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                surface.ack_configure(serial);
                match role {
                    LayerRoleTag::HoverPopup => {
                        if let Some(pop) = &mut state.popup_win {
                            let w = if width == 0 { POPUP_CARD_WIDTH } else { width };
                            let h = if height == 0 {
                                POPUP_CARD_HEIGHT
                            } else {
                                height
                            };
                            pop.configured_size = Some((w, h));
                        }
                        if let Some(ws_id) = state.hover_fsm.active_workspace().cloned() {
                            state.capture_and_draw_popup(&ws_id);
                        }
                    }
                    LayerRoleTag::FullscreenGrid => {
                        let w = if width == 0 {
                            state.output_size.0
                        } else {
                            width
                        };
                        let h = if height == 0 {
                            state.output_size.1
                        } else {
                            height
                        };
                        state.output_size = (w, h);
                        if let Some(grid_w) = &mut state.grid_win {
                            grid_w.configured_size = Some((w, h));
                        }
                        state.capture_and_draw_grid();
                    }
                }
            }
            zwlr_layer_surface_v1::Event::Closed => match role {
                LayerRoleTag::HoverPopup => {
                    // Drop the closed proxy before the hover FSM tears the popup down, so a later
                    // hover cannot send requests to a destroyed object.
                    if let Some(win) = state.popup_win.take() {
                        win.destroy();
                    }
                    if let Some(fx) = state.hover_fsm.on_hover_leave() {
                        state.apply_hover_effect(fx);
                    }
                    debug!(
                        namespace = "koma-overview-popup",
                        "compositor closed the hover popup layer surface"
                    );
                }
                LayerRoleTag::FullscreenGrid => {
                    // The compositor closed the surface (no output, refused namespace, reload):
                    // recreate it a bounded number of times instead of ending the daemon.
                    state.recreate_grid_surface("compositor closed the layer surface");
                }
            },
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for OverviewDaemon {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for OverviewDaemon {
    fn event(
        state: &mut Self,
        _kb: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if !state.grid.is_open {
            return;
        }
        if let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        {
            // Linux evdev keycodes (raw evdev without +8 XKB offset in wl_keyboard::Event::Key!)
            match key {
                1 => state.close_fullscreen_grid(), // KEY_ESC
                35 | 105 => {
                    // H or Left
                    state.grid.move_selection(-1, 0);
                    state.draw_grid_from_cache();
                }
                38 | 106 => {
                    // L or Right
                    state.grid.move_selection(1, 0);
                    state.draw_grid_from_cache();
                }
                37 | 103 => {
                    // K or Up
                    state.grid.move_selection(0, -1);
                    state.draw_grid_from_cache();
                }
                36 | 108 => {
                    // J or Down
                    state.grid.move_selection(0, 1);
                    state.draw_grid_from_cache();
                }
                28 => {
                    // Enter -> switch to normal workspace or Peek agent workspace
                    if let Some(ws_id) = state.grid.selected_workspace().cloned() {
                        let is_agent = state.inventory_cache.iter().any(|i| {
                            i.ws_id == ws_id
                                && (i.frame_tier.contains("headless")
                                    || i.name.starts_with("agent"))
                        });
                        if let Some(ipc) = state.ensure_ipc() {
                            if is_agent {
                                let _ = ipc.set_workspace_mode(&ws_id, WorkspaceModeSpec::Peek);
                            } else {
                                let _ = ipc.focus_workspace(&ws_id);
                            }
                        }
                        state.close_fullscreen_grid();
                    }
                }
                20 | 34 => {
                    // T or G -> Human Takeover mode
                    if let Some(ws_id) = state.grid.selected_workspace().cloned() {
                        if let Some(ipc) = state.ensure_ipc() {
                            let _ = ipc.set_workspace_mode(&ws_id, WorkspaceModeSpec::Takeover);
                        }
                        state.close_fullscreen_grid();
                    }
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for OverviewDaemon {
    fn event(
        state: &mut Self,
        _ptr: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_xy = (surface_x, surface_y);
                if state.grid.is_open
                    && let Some(slot) = state.grid.hit_test(surface_x, surface_y)
                {
                    let idx = slot.index;
                    if idx != state.grid.selected_idx {
                        state.grid.set_selected(idx);
                        state.draw_grid_from_cache();
                    }
                }
            }
            wl_pointer::Event::Button {
                button: 0x110, // BTN_LEFT
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } if state.grid.is_open => {
                let (px, py) = state.pointer_xy;
                if let Some(ws_id) = state.grid.hit_test(px, py).map(|s| s.ws_id.clone()) {
                    let is_agent = state.inventory_cache.iter().any(|i| {
                        i.ws_id == ws_id
                            && (i.frame_tier.contains("headless") || i.name.starts_with("agent"))
                    });
                    if let Some(ipc) = state.ensure_ipc() {
                        if is_agent {
                            let _ = ipc.set_workspace_mode(&ws_id, WorkspaceModeSpec::Peek);
                        } else {
                            let _ = ipc.focus_workspace(&ws_id);
                        }
                    }
                    state.close_fullscreen_grid();
                } else {
                    state.close_fullscreen_grid();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for OverviewDaemon {
    fn event(
        state: &mut Self,
        _output: &WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { width, height, .. } = event
            && width > 0
            && height > 0
        {
            state.output_size = (width as u32, height as u32);
            debug!(
                width,
                height, "updated host output resolution for koma-overview"
            );
        }
    }
}

impl Dispatch<WlBuffer, ()> for OverviewDaemon {
    fn event(
        _state: &mut Self,
        buffer: &WlBuffer,
        event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            buffer.destroy();
        }
    }
}

impl Dispatch<WlCompositor, ()> for OverviewDaemon {
    fn event(
        _s: &mut Self,
        _p: &WlCompositor,
        _e: <WlCompositor as wayland_client::Proxy>::Event,
        _d: &(),
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for OverviewDaemon {
    fn event(
        _s: &mut Self,
        _p: &WlSurface,
        _e: wl_surface::Event,
        _d: &(),
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShm, ()> for OverviewDaemon {
    fn event(
        _s: &mut Self,
        _p: &WlShm,
        _e: wl_shm::Event,
        _d: &(),
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for OverviewDaemon {
    fn event(
        _s: &mut Self,
        _p: &WlShmPool,
        _e: wl_shm_pool::Event,
        _d: &(),
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for OverviewDaemon {
    fn event(
        _s: &mut Self,
        _p: &ZwlrLayerShellV1,
        _e: zwlr_layer_shell_v1::Event,
        _d: &(),
        _c: &Connection,
        _q: &QueueHandle<Self>,
    ) {
    }
}

fn format_card_title(
    name: &str,
    focused: bool,
    is_agent: bool,
    is_human_preempted: bool,
) -> String {
    let tag = if is_human_preempted {
        Some("PREEMPT")
    } else if focused {
        Some("ACTIVE")
    } else if is_agent {
        Some("AGENT")
    } else {
        None
    };
    match tag {
        Some(t) => format!("{name} | {t}"),
        None => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn a_broken_wayland_pipe_is_reported_as_a_lost_display() {
        // The exact shape `calloop-wayland-source` produced in the live session:
        // `other error during loop operation: underlying IO error: Broken pipe (os error 32)`.
        let observed =
            calloop::Error::OtherError(Box::new(io::Error::from(io::ErrorKind::BrokenPipe)));
        let exit = classify_loop_error(&observed, 1_234);
        match &exit {
            DaemonExit::WaylandDisconnected { detail, uptime_ms } => {
                assert!(*uptime_ms == 1_234);
                assert!(
                    detail.to_lowercase().contains("broken pipe"),
                    "the io detail must be preserved: {detail}"
                );
            }
            other => panic!("expected a lost display, got {other:?}"),
        }
        assert_eq!(exit.exit_code(), 1);
        let described = exit.describe();
        assert!(described.contains("Wayland connection lost"), "{described}");
        assert!(described.contains("1234 ms"), "{described}");

        let direct = classify_loop_error(
            &calloop::Error::IoError(io::Error::from(io::ErrorKind::ConnectionReset)),
            5,
        );
        assert!(matches!(direct, DaemonExit::WaylandDisconnected { .. }));
    }

    #[test]
    fn other_loop_failures_keep_their_full_error_chain() {
        let exit = classify_loop_error(&calloop::Error::InvalidToken, 10);
        match &exit {
            DaemonExit::EventLoopError { detail, uptime_ms } => {
                assert_eq!(*uptime_ms, 10);
                assert!(detail.contains("invalid token"), "{detail}");
            }
            other => panic!("expected an event-loop error, got {other:?}"),
        }
        assert!(exit.describe().contains("event loop failed"));
        assert_eq!(exit.exit_code(), 1);
    }

    #[test]
    fn a_requested_quit_is_a_clean_exit_with_the_socket_path() {
        let exit = DaemonExit::ControlQuit {
            socket: PathBuf::from("/run/user/1000/koma/1_wayland-1/koma-overview.ctl.sock"),
            uptime_ms: 42,
        };
        assert_eq!(exit.exit_code(), 0);
        let described = exit.describe();
        assert!(described.contains("quit"), "{described}");
        assert!(described.contains("koma-overview.ctl.sock"), "{described}");
        assert!(described.contains("42 ms"), "{described}");
    }

    #[test]
    fn startup_failures_and_panics_are_never_clean_exits() {
        assert_eq!(DaemonExit::startup("no display").exit_code(), 1);
        assert!(
            DaemonExit::startup("no display")
                .describe()
                .contains("startup failed: no display")
        );
        let panicked = DaemonExit::Panicked {
            detail: "index out of bounds".to_owned(),
            uptime_ms: 7,
        };
        assert_eq!(panicked.exit_code(), 1);
        assert!(panicked.describe().contains("panicked after 7 ms"));
    }

    #[test]
    fn a_surface_without_a_configure_is_recreated_after_the_timeout() {
        let created = Instant::now();
        assert!(!configure_is_overdue(false, created, created));
        assert!(!configure_is_overdue(
            false,
            created,
            created + CONFIGURE_TIMEOUT / 2
        ));
        assert!(configure_is_overdue(
            false,
            created,
            created + CONFIGURE_TIMEOUT
        ));
        // A configured surface is never "overdue", however long it has been on screen.
        assert!(!configure_is_overdue(
            true,
            created,
            created + CONFIGURE_TIMEOUT * 100
        ));
        // At least one recreation must be allowed, otherwise the watchdog would only give up.
        const { assert!(MAX_SURFACE_RECREATES >= 1) };
    }

    #[test]
    fn oversized_configure_sizes_are_refused_instead_of_allocating() {
        assert_eq!(canvas_byte_len(2560, 1600), Some(2560 * 1600 * 4));
        assert_eq!(canvas_byte_len(0, 10), None);
        assert_eq!(canvas_byte_len(u32::MAX, u32::MAX), None);
        assert_eq!(canvas_byte_len(65_536, 65_536), None);
    }

    #[test]
    fn issue_120_card_titles_omit_internal_epoch_and_tier_noise() {
        assert_eq!(format_card_title("1", false, false, false), "1");
        assert_eq!(format_card_title("3", true, false, false), "3 | ACTIVE");
        assert_eq!(
            format_card_title("agent:work", false, true, false),
            "agent:work | AGENT"
        );
        assert_eq!(
            format_card_title("agent:work", false, true, true),
            "agent:work | PREEMPT"
        );
        assert_eq!(
            format_card_title("special:magic", true, false, false),
            "special:magic | ACTIVE"
        );
    }
}
