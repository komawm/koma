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
    ffi::CString,
    fs::{self, File},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    os::{
        fd::AsFd as _,
        unix::{fs::PermissionsExt as _, net::UnixListener},
    },
    time::{Duration, Instant},
};

use anyhow::Context as _;
use calloop::{
    EventLoop, Interest, Mode, PostAction,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use koma_model::LogicalRect;
use koma_proto::{WorkspaceId, rpc::WorkspaceModeSpec};
use rustix::fs::{MemfdFlags, memfd_create};
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
        ARGB_BACKDROP_DIM, CardVisualMeta, RawRgbaFrame, clear_argb8888_canvas,
        paint_workspace_card,
    },
    grid_layout::OverviewGridState,
    hover_fsm::{
        HoverDwellMachine, HoverEffect, POPUP_CARD_HEIGHT, POPUP_CARD_WIDTH, POPUP_HEADER_HEIGHT,
        POPUP_PADDING_PX, PREVIEW_REFRESH_MS, THUMBNAIL_HEIGHT, THUMBNAIL_WIDTH,
    },
    ipc_bridge::{AdminIpcClient, InventoryWorkspaceItem, control_socket_path},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerRoleTag {
    HoverPopup,
    FullscreenGrid,
}

struct ActiveLayerWindow {
    wl_surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    configured_size: Option<(u32, u32)>,
}

impl ActiveLayerWindow {
    fn destroy(self) {
        self.layer_surface.destroy();
        self.wl_surface.destroy();
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

        let meta = self.build_card_meta(ws_id, true);
        let mut canvas = vec![0_u8; (POPUP_CARD_WIDTH * POPUP_CARD_HEIGHT * 4) as usize];
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
        self.grid
            .recompute_layout(self.output_size.0, self.output_size.1, &ws_ids);

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
        });
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
        let Some(wl_surface) = self.grid_win.as_ref().map(|g| g.wl_surface.clone()) else {
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

        let mut canvas = vec![0_u8; (w * h * 4) as usize];
        clear_argb8888_canvas(&mut canvas, ARGB_BACKDROP_DIM);
        for slot in &slots_snapshot {
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

        self.attach_and_commit_shm(&wl_surface, w, h, &canvas);
    }

    fn build_card_meta(&self, ws_id: &WorkspaceId, is_selected: bool) -> CardVisualMeta {
        if let Some(item) = self.inventory_cache.iter().find(|i| &i.ws_id == ws_id) {
            let is_agent = item.frame_tier.contains("headless") || item.name.starts_with("agent");
            CardVisualMeta {
                title_label: format!("{} | E:{} | {}", item.name, item.epoch, item.frame_tier),
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
        let cname = CString::new("koma_overview_shm").unwrap();
        let Ok(fd) = memfd_create(&cname, MemfdFlags::CLOEXEC) else {
            return;
        };
        let mut file = File::from(fd);
        if file.write_all(pixels).is_err() || file.seek(SeekFrom::Start(0)).is_err() {
            return;
        }

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

pub fn run_overview_daemon() -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()
        .context("failed connecting to host Wayland display (WAYLAND_DISPLAY)")?;
    let mut event_queue = conn.new_event_queue::<OverviewDaemon>();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut daemon = OverviewDaemon {
        start_time: Instant::now(),
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
        hover_fsm: HoverDwellMachine::default(),
        grid: OverviewGridState::default(),
        ipc: None,
        inventory_cache: Vec::new(),
        frame_cache: HashMap::new(),
    };

    event_queue
        .roundtrip(&mut daemon)
        .context("initial Wayland registry roundtrip failed")?;

    if daemon.layer_shell.is_none() {
        anyhow::bail!("host Wayland compositor does not advertise zwlr_layer_shell_v1");
    }

    let mut event_loop: EventLoop<'static, OverviewDaemon> = EventLoop::try_new()?;

    // 1. Multiplex Wayland client queue onto `calloop`
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("failed inserting WaylandSource into calloop: {e}"))?;

    // 2. Bind local control socket (`koma-overview.ctl.sock`) for Waybar / Ironbar / keybind triggers
    if let Ok(ctl_path) = control_socket_path() {
        if ctl_path.exists() {
            let _ = fs::remove_file(&ctl_path);
        }
        if let Some(parent) = ctl_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let listener = UnixListener::bind(&ctl_path)
            .with_context(|| format!("failed binding {}", ctl_path.display()))?;
        fs::set_permissions(&ctl_path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        info!(ctl_socket = %ctl_path.display(), "koma-overview control socket listening");

        event_loop
            .handle()
            .insert_source(
                Generic::new(listener, Interest::READ, Mode::Level),
                |_, listener, state| {
                    while let Ok((mut stream, _)) = listener.accept() {
                        let mut cmd = String::new();
                        let _ = stream.read_to_string(&mut cmd);
                        for line in cmd.lines() {
                            state.handle_control_command(line.trim());
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| anyhow::anyhow!("failed inserting control socket source: {e}"))?;
    }

    // 3. Periodic timer driving 150ms hover dwell checks & 8Hz live preview refreshes only when active
    event_loop
        .handle()
        .insert_source(
            Timer::from_duration(Duration::from_millis(25)),
            |_, _, state| {
                let now = state.elapsed_ms();
                if let Some(fx) = state.hover_fsm.poll_tick(now) {
                    state.apply_hover_effect(fx);
                }
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
        )
        .map_err(|e| anyhow::anyhow!("failed inserting overview timer: {e}"))?;

    while !daemon.should_exit {
        event_loop.dispatch(Some(Duration::from_millis(50)), &mut daemon)?;
    }

    Ok(())
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
                    if let Some(fx) = state.hover_fsm.on_hover_leave() {
                        state.apply_hover_effect(fx);
                    }
                }
                LayerRoleTag::FullscreenGrid => {
                    state.close_fullscreen_grid();
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
                    state.capture_and_draw_grid();
                }
                38 | 106 => {
                    // L or Right
                    state.grid.move_selection(1, 0);
                    state.capture_and_draw_grid();
                }
                37 | 103 => {
                    // K or Up
                    state.grid.move_selection(0, -1);
                    state.capture_and_draw_grid();
                }
                36 | 108 => {
                    // J or Down
                    state.grid.move_selection(0, 1);
                    state.capture_and_draw_grid();
                }
                28 => {
                    // Enter -> Peek Read-Only Shield mode
                    if let Some(ws_id) = state.grid.selected_workspace().cloned() {
                        if let Some(ipc) = state.ensure_ipc() {
                            let _ = ipc.set_workspace_mode(&ws_id, WorkspaceModeSpec::Peek);
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
                        state.capture_and_draw_grid();
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
                    if let Some(ipc) = state.ensure_ipc() {
                        let _ = ipc.set_workspace_mode(&ws_id, WorkspaceModeSpec::Peek);
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
