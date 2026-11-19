//! Unit and Wayland client-server integration tests for `KOM-0018` protocols.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Seek as _, SeekFrom, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use calloop::EventLoop;
use koma_lua::ActionDescriptor;
use smithay::backend::input::InputTime;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1,
};
use smithay::reexports::wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1, ext_idle_notifier_v1,
};
use smithay::reexports::wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1,
};
use smithay::reexports::wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1, ext_workspace_handle_v1, ext_workspace_manager_v1,
};
use smithay::reexports::wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use smithay::reexports::wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1,
};
use smithay::reexports::wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1, zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use smithay::reexports::wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
};
use smithay::reexports::wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1, xdg_activation_v1,
};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use smithay::reexports::wayland_protocols::xdg::toplevel_icon::v1::client::{
    xdg_toplevel_icon_manager_v1, xdg_toplevel_icon_v1,
};
use smithay::reexports::wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_v1::Mode as ServerOutputPowerMode;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Point, Rectangle, SERIAL_COUNTER, Size};
use smithay::wayland::compositor::{RectangleKind, RegionAttributes};
use smithay::wayland::seat::WaylandFocus as _;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager,
    wl_data_offer, wl_data_source, wl_keyboard, wl_output, wl_pointer, wl_region, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy as _, WEnum, delegate_noop, event_created_child,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};
use wayland_protocols_wlr::gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1, zwlr_gamma_control_v1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1, zwlr_output_configuration_v1, zwlr_output_head_v1,
    zwlr_output_manager_v1, zwlr_output_mode_v1,
};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1, zwlr_output_power_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::desktop_protocols::{
    DeferredXdgActivation, XDG_ACTIVATION_TOKEN_TTL, clamp_local_point_to_constraint,
    extract_validated_toplevel_icon, point_inside_surface_constraint,
};
use crate::foreign_toplevel::encode_wlr_toplevel_states;
use crate::gamma_control::{GammaValidationError, read_and_validate_gamma_lut};
use crate::keybind_dispatcher::execute_action_descriptor;
use crate::output_power::OutputPowerMode;
use crate::screencopy::{compute_clamped_physical_region, timestamp_parts};
use crate::state::tests::{make_headless_test_state, test_output};
use crate::state::{ClientState, KomaState};

static SHM_TEST_SEQ: AtomicU64 = AtomicU64::new(0);

/// `ext_workspace_handle_v1.state`
const EXT_WORKSPACE_STATE_ACTIVE: u32 = 1;
/// `ext_workspace_handle_v1.state`
const EXT_WORKSPACE_STATE_HIDDEN: u32 = 4;
/// `ext_workspace_handle_v1.workspace_capabilities`
const EXT_WORKSPACE_CAPABILITY_ACTIVATE: u32 = 1;

fn create_portable_shm_fd(len: usize, contents: Option<&[u8]>) -> OwnedFd {
    let seq = SHM_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "koma-protocol-test-{}-{seq}.bin",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("create portable shm backing file");
    let _ = fs::remove_file(&path);
    if let Some(bytes) = contents {
        file.write_all(bytes).expect("write shm payload");
        file.flush().expect("flush shm payload");
        file.seek(SeekFrom::Start(0)).expect("rewind shm fd");
    } else {
        file.set_len(len as u64).expect("set shm file length");
    }
    OwnedFd::from(file)
}

#[derive(Debug, Clone, Default)]
struct CapturedWlrToplevel {
    title: String,
    app_id: String,
    states: Vec<u32>,
    done_count: usize,
    closed: bool,
}

#[derive(Debug, Clone, Default)]
struct CapturedExtToplevel {
    title: String,
    app_id: String,
    identifier: String,
    done_count: usize,
    closed: bool,
}

#[derive(Debug, Clone)]
struct ScreencopyBufferOffer {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

#[derive(Debug, Clone, Default)]
struct CapturedOutputMode {
    proxy: Option<zwlr_output_mode_v1::ZwlrOutputModeV1>,
    width: i32,
    height: i32,
    refresh: i32,
    preferred: bool,
    finished: bool,
}

#[derive(Debug, Clone, Default)]
struct CapturedOutputHead {
    proxy: Option<zwlr_output_head_v1::ZwlrOutputHeadV1>,
    name: String,
    description: String,
    make: String,
    model: String,
    serial_number: String,
    phys_mm: (i32, i32),
    enabled: Option<bool>,
    current_mode_id: Option<u32>,
    position: Option<(i32, i32)>,
    transform: Option<WEnum<wl_output::Transform>>,
    scale: Option<f64>,
    mode_ids: Vec<u32>,
    finished: bool,
}

#[derive(Debug, Clone, Default)]
struct CapturedExtWorkspace {
    id: Option<String>,
    name: String,
    coordinates: Vec<u32>,
    state: u32,
    capabilities: u32,
    removed: bool,
}

#[derive(Debug, Clone, Default)]
struct CapturedExtWorkspaceManager {
    groups: Vec<u32>,
    workspaces: Vec<u32>,
    /// Number of `ext_workspace_manager_v1.workspace` events seen, including re-created handles.
    workspaces_created: usize,
    done: usize,
}

#[derive(Debug, Clone, Default)]
struct CapturedExtWorkspaceGroup {
    capabilities: Option<u32>,
    outputs: Vec<u32>,
    workspaces: Vec<u32>,
    removed: bool,
}

#[derive(Debug, Clone, Default)]
struct CapturedDataSource {
    cancelled: usize,
    drop_performed: usize,
    finished: usize,
    targets: Vec<Option<String>>,
}

#[derive(Default)]
struct ClientHarness {
    sync_done: bool,
    globals: BTreeMap<String, (u32, u32)>,
    outputs: Vec<wl_output::WlOutput>,
    seats: Vec<wl_seat::WlSeat>,
    pointers: Vec<wl_pointer::WlPointer>,
    /// Surface-local `wl_pointer.motion` coordinates recorded per delivered motion event.
    pointer_motion_coords: Vec<(f64, f64)>,

    /// `wl_pointer.button` events with the server serial that clients use for `start_drag`.
    pointer_buttons: Vec<(u32, wl_pointer::ButtonState, u32)>,
    data_device: Option<wl_data_device::WlDataDevice>,
    data_source_events: BTreeMap<u32, CapturedDataSource>,
    data_offer_mimes: BTreeMap<u32, Vec<String>>,
    data_offer_proxies: BTreeMap<u32, wl_data_offer::WlDataOffer>,
    data_device_enter_serial: Option<u32>,
    data_device_enters: usize,
    data_device_motions: usize,
    data_device_leaves: usize,
    data_device_drops: usize,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    xdg_wm_base: Option<xdg_wm_base::XdgWmBase>,
    xdg_surface_configures: usize,
    /// `xdg_toplevel.configure(w, h)` sizes recorded per toplevel protocol id.
    toplevel_configures: BTreeMap<u32, Vec<(i32, i32)>>,
    /// `xdg_toplevel.configure` state arrays (decoded `u32` enum values) per toplevel protocol id.
    toplevel_states: BTreeMap<u32, Vec<Vec<u32>>>,
    toplevel_close_count: usize,
    decoration_modes: Vec<WEnum<zxdg_toplevel_decoration_v1::Mode>>,
    activation_tokens: Vec<String>,
    screencopy_shm_offers: Vec<ScreencopyBufferOffer>,
    screencopy_dmabuf_offers: usize,
    screencopy_buffer_done: usize,
    screencopy_ready: usize,
    screencopy_failed: usize,
    wlr_toplevels: BTreeMap<u32, CapturedWlrToplevel>,
    wlr_toplevel_proxies: Vec<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1>,
    ext_toplevels: BTreeMap<u32, CapturedExtToplevel>,
    idle_events: Vec<&'static str>,
    output_power_modes: Vec<WEnum<zwlr_output_power_v1::Mode>>,
    output_power_failed: usize,
    gamma_sizes: Vec<u32>,
    gamma_failed: usize,
    locked_events: Vec<&'static str>,
    session_lock_locked: usize,
    session_lock_finished: usize,
    session_lock_configures: Vec<(u32, u32, u32)>,
    confined_events: Vec<&'static str>,
    relative_motions: Vec<(f64, f64, f64, f64)>,
    icon_sizes: Vec<i32>,
    icon_done_count: usize,
    output_heads: BTreeMap<u32, CapturedOutputHead>,
    output_modes: BTreeMap<u32, CapturedOutputMode>,
    output_manager_serials: Vec<u32>,
    output_config_results: BTreeMap<u32, &'static str>,
    fractional_scales: Vec<u32>,
    layer_configures: usize,
    ext_workspace_manager: Option<ext_workspace_manager_v1::ExtWorkspaceManagerV1>,
    ext_workspace_manager_proxies: BTreeMap<u32, ext_workspace_manager_v1::ExtWorkspaceManagerV1>,
    ext_workspace_managers: BTreeMap<u32, CapturedExtWorkspaceManager>,
    ext_workspace_done: usize,
    ext_workspace_finished: usize,
    ext_workspace_groups: BTreeMap<u32, CapturedExtWorkspaceGroup>,
    ext_workspace_group_proxies:
        BTreeMap<u32, ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1>,
    ext_workspaces: BTreeMap<u32, CapturedExtWorkspace>,
    ext_workspace_proxies: BTreeMap<u32, ext_workspace_handle_v1::ExtWorkspaceHandleV1>,
}

impl ClientHarness {
    fn connect(
        display: &Display<KomaState>,
        restricted: bool,
    ) -> (Connection, EventQueue<Self>, Self, wl_registry::WlRegistry) {
        Self::connect_with(
            display,
            ClientState {
                restricted,
                ..Default::default()
            },
        )
    }

    fn connect_with(
        display: &Display<KomaState>,
        client_state: ClientState,
    ) -> (Connection, EventQueue<Self>, Self, wl_registry::WlRegistry) {
        let (server_sock, client_sock) = UnixStream::pair().expect("UnixStream::pair");
        display
            .handle()
            .insert_client(server_sock, Arc::new(client_state))
            .expect("insert wayland test client");
        let conn = Connection::from_socket(client_sock).expect("Connection::from_socket");
        let queue = conn.new_event_queue::<Self>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        (conn, queue, Self::default(), registry)
    }

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

    fn ext_workspace_by_name(
        &self,
        name: &str,
    ) -> Option<&ext_workspace_handle_v1::ExtWorkspaceHandleV1> {
        self.ext_workspace_proxies
            .iter()
            .find_map(|(id, proxy)| (self.ext_workspaces.get(id)?.name == name).then_some(proxy))
    }

    fn ext_workspace_state(&self, name: &str) -> u32 {
        self.ext_workspace_proxies
            .iter()
            .find_map(|(id, _)| {
                let ws = self.ext_workspaces.get(id)?;
                (ws.name == name).then_some(ws.state)
            })
            .unwrap_or_default()
    }

    fn ext_workspace_handles_of_manager(&self, manager_id: u32) -> Vec<u32> {
        self.ext_workspace_managers
            .get(&manager_id)
            .map(|captured| captured.workspaces.clone())
            .unwrap_or_default()
    }

    fn ext_workspace_handle_for_manager(
        &self,
        manager_id: u32,
        name: &str,
    ) -> Option<&ext_workspace_handle_v1::ExtWorkspaceHandleV1> {
        self.ext_workspace_managers
            .get(&manager_id)?
            .workspaces
            .iter()
            .find_map(|id| {
                let ws = self.ext_workspaces.get(id)?;
                (ws.name == name).then(|| self.ext_workspace_proxies.get(id))?
            })
    }

    fn ext_workspace_id(&self, name: &str) -> Option<String> {
        self.ext_workspace_proxies.iter().find_map(|(id, _)| {
            let ws = self.ext_workspaces.get(id)?;
            if ws.name == name { ws.id.clone() } else { None }
        })
    }
}

fn pump(
    conn: &Connection,
    queue: &mut EventQueue<ClientHarness>,
    client: &mut ClientHarness,
    display: &mut Display<KomaState>,
    event_loop: &mut EventLoop<'static, KomaState>,
    state: &mut KomaState,
) {
    let qh = queue.handle();
    client.sync_done = false;
    let _cb = conn.display().sync(&qh, ());
    conn.flush().expect("flush client");
    for _ in 0..16 {
        display
            .dispatch_clients(state)
            .expect("dispatch server clients");
        event_loop
            .dispatch(Some(Duration::ZERO), state)
            .expect("dispatch calloop");
        display.flush_clients().expect("flush server clients");
        if let Some(guard) = queue.prepare_read() {
            let _ = guard.read();
        }
        queue
            .dispatch_pending(client)
            .expect("dispatch client pending");
        conn.flush().expect("flush client post-dispatch");
        if client.sync_done {
            break;
        }
    }
    assert!(
        client.sync_done,
        "Wayland client-server pump failed to observe wl_callback.done"
    );
}

struct ClientEnv {
    event_loop: EventLoop<'static, KomaState>,
    display: Display<KomaState>,
    state: KomaState,
    _output: Output,
    _output_global: smithay::reexports::wayland_server::backend::GlobalId,
    conn: Connection,
    queue: EventQueue<ClientHarness>,
    client: ClientHarness,
    registry: wl_registry::WlRegistry,
}

impl ClientEnv {
    fn new() -> Self {
        Self::with_client_state(|_| ClientState::default())
    }

    /// Builds the harness with a client connection that carries `client_state`
    /// (used to model jailed sandbox clients).
    fn with_client_state(make_client_state: impl FnOnce(&KomaState) -> ClientState) -> Self {
        let (mut event_loop, mut display, mut state) = make_headless_test_state();
        let output = test_output("DP-1");
        let output_global = output.create_global::<KomaState>(&state.display_handle);
        state.test_outputs.push(output.clone());
        let client_state = make_client_state(&state);
        let (conn, mut queue, mut client, registry) =
            ClientHarness::connect_with(&display, client_state);
        pump(
            &conn,
            &mut queue,
            &mut client,
            &mut display,
            &mut event_loop,
            &mut state,
        );
        Self {
            event_loop,
            display,
            state,
            _output: output,
            _output_global: output_global,
            conn,
            queue,
            client,
            registry,
        }
    }

    fn pump(&mut self) {
        pump(
            &self.conn,
            &mut self.queue,
            &mut self.client,
            &mut self.display,
            &mut self.event_loop,
            &mut self.state,
        );
    }

    fn bind_ext_workspace_manager(&mut self) -> ext_workspace_manager_v1::ExtWorkspaceManagerV1 {
        let qh = self.queue.handle();
        let manager = self
            .client
            .bind::<ext_workspace_manager_v1::ExtWorkspaceManagerV1>(
                &self.registry,
                &qh,
                "ext_workspace_manager_v1",
            );
        self.client.ext_workspace_manager = Some(manager.clone());
        self.client
            .ext_workspace_manager_proxies
            .insert(manager.id().protocol_id(), manager.clone());
        self.pump();
        manager
    }

    fn spawn_window(
        &mut self,
        title: &str,
        app_id: &str,
    ) -> (
        wl_surface::WlSurface,
        xdg_surface::XdgSurface,
        xdg_toplevel::XdgToplevel,
    ) {
        let qh = self.queue.handle();
        let compositor = self.client.compositor.clone().unwrap_or_else(|| {
            self.client
                .bind::<wl_compositor::WlCompositor>(&self.registry, &qh, "wl_compositor")
        });
        self.client.compositor = Some(compositor.clone());
        let wm_base = self.client.xdg_wm_base.clone().unwrap_or_else(|| {
            self.client
                .bind::<xdg_wm_base::XdgWmBase>(&self.registry, &qh, "xdg_wm_base")
        });
        self.client.xdg_wm_base = Some(wm_base.clone());
        let shm = self.client.shm.clone().unwrap_or_else(|| {
            self.client
                .bind::<wl_shm::WlShm>(&self.registry, &qh, "wl_shm")
        });
        self.client.shm = Some(shm.clone());

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        toplevel.set_title(title.to_owned());
        toplevel.set_app_id(app_id.to_owned());
        surface.commit();
        self.pump();

        let shm_fd = create_portable_shm_fd(64 * 64 * 4, None);
        let pool = shm.create_pool(shm_fd.as_fd(), 64 * 64 * 4, &qh, ());
        let buffer = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        self.pump();

        buffer.destroy();
        pool.destroy();
        (surface, xdg_surface, toplevel)
    }

    /// Spawns a client-side-decorated toplevel.
    ///
    /// The `64x64` harness buffer carries a decoration margin around the visible window
    /// geometry: `xdg_surface.set_window_geometry` reports the inner rect in surface-local
    /// coordinates and the input region covers exactly that rect, so the decoration band is not
    /// clickable (matching how CSD clients avoid shadow clicks).
    fn spawn_csd_window(
        &mut self,
        title: &str,
        app_id: &str,
        geometry: koma_model::LogicalRect,
    ) -> (
        wl_surface::WlSurface,
        xdg_surface::XdgSurface,
        xdg_toplevel::XdgToplevel,
    ) {
        let (surface, xdg_surface, toplevel) = self.spawn_window(title, app_id);
        let qh = self.queue.handle();
        let compositor = self
            .client
            .compositor
            .clone()
            .expect("compositor bound by spawn_window");

        xdg_surface.set_window_geometry(
            geometry.x,
            geometry.y,
            geometry.w as i32,
            geometry.h as i32,
        );
        let input_region = compositor.create_region(&qh, ());
        input_region.add(geometry.x, geometry.y, geometry.w as i32, geometry.h as i32);
        surface.set_input_region(Some(&input_region));
        input_region.destroy();
        surface.commit();
        self.pump();
        (surface, xdg_surface, toplevel)
    }

    /// Moves the window under test to a deterministic floating rect.
    fn float_window_at(
        &mut self,
        win_id: koma_proto::WindowId,
        rect: koma_model::LogicalRect,
    ) -> koma_model::LogicalRect {
        let ws_id = self.state.active_ws_id.clone();
        let now_ms = self.state.elapsed_ms();
        if let Some(ws) = self.state.workspaces.get_mut(&ws_id) {
            ws.toggle_floating(win_id, None, None, now_ms);
            ws.floating.set_rect(win_id, rect);
        }
        self.state.sync_workspace_geometries(&ws_id);
        self.state
            .workspaces
            .get(&ws_id)
            .expect("workspace")
            .compute_visible_geometries_at(self.state.elapsed_ms())
            .into_iter()
            .find(|(id, _, _)| *id == win_id)
            .map(|(_, rect, _)| rect)
            .expect("visible geometry")
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &wayland_client::QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_output" => {
                    let out = registry.bind::<wl_output::WlOutput, (), Self>(
                        name,
                        version.min(4),
                        qh,
                        (),
                    );
                    state.outputs.push(out);
                }
                "wl_seat" => {
                    let seat =
                        registry.bind::<wl_seat::WlSeat, (), Self>(name, version.min(9), qh, ());
                    let pointer = seat.get_pointer(qh, ());
                    state.pointers.push(pointer);
                    state.seats.push(seat);
                }
                _ => {}
            }
            state.globals.insert(interface, (name, version));
        }
    }
}

impl Dispatch<ext_session_lock_manager_v1::ExtSessionLockManagerV1, ()> for ClientHarness {
    fn event(
        _: &mut Self,
        _: &ext_session_lock_manager_v1::ExtSessionLockManagerV1,
        _: ext_session_lock_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ext_session_lock_v1::ExtSessionLockV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &ext_session_lock_v1::ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => state.session_lock_locked += 1,
            ext_session_lock_v1::Event::Finished => state.session_lock_finished += 1,
            _ => {}
        }
    }
}

impl Dispatch<ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            state.session_lock_configures.push((serial, width, height));
        }
    }
}

impl Dispatch<ext_workspace_manager_v1::ExtWorkspaceManagerV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        manager: &ext_workspace_manager_v1::ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        let manager_id = manager.id().protocol_id();
        state
            .ext_workspace_manager_proxies
            .insert(manager_id, manager.clone());
        let captured = state.ext_workspace_managers.entry(manager_id).or_default();
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                let id = workspace_group.id().protocol_id();
                captured.groups.push(id);
                state
                    .ext_workspace_groups
                    .insert(id, CapturedExtWorkspaceGroup::default());
                state
                    .ext_workspace_group_proxies
                    .insert(id, workspace_group);
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                let id = workspace.id().protocol_id();
                captured.workspaces.push(id);
                captured.workspaces_created += 1;
                state
                    .ext_workspaces
                    .insert(id, CapturedExtWorkspace::default());
                state.ext_workspace_proxies.insert(id, workspace);
            }
            ext_workspace_manager_v1::Event::Done => {
                captured.done += 1;
                state.ext_workspace_done += 1;
            }
            ext_workspace_manager_v1::Event::Finished => state.ext_workspace_finished += 1,
            _ => {}
        }
    }

    event_created_child!(
        ClientHarness,
        ext_workspace_manager_v1::ExtWorkspaceManagerV1,
        [
            ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (
                ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
                ()
            ),
            ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (
                ext_workspace_handle_v1::ExtWorkspaceHandleV1,
                ()
            ),
        ]
    );
}

impl Dispatch<ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        group: &ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        let id = group.id().protocol_id();
        let Some(captured) = state.ext_workspace_groups.get_mut(&id) else {
            return;
        };
        match event {
            ext_workspace_group_handle_v1::Event::Capabilities { capabilities } => {
                captured.capabilities = Some(match capabilities {
                    WEnum::Value(value) => value.bits(),
                    WEnum::Unknown(raw) => raw,
                });
            }
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                captured.outputs.push(output.id().protocol_id());
            }
            ext_workspace_group_handle_v1::Event::OutputLeave { output } => {
                captured
                    .outputs
                    .retain(|id| *id != output.id().protocol_id());
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                captured.workspaces.push(workspace.id().protocol_id());
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                captured
                    .workspaces
                    .retain(|id| *id != workspace.id().protocol_id());
            }
            ext_workspace_group_handle_v1::Event::Removed => captured.removed = true,
            _ => {}
        }
    }
}

impl Dispatch<ext_workspace_handle_v1::ExtWorkspaceHandleV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        workspace: &ext_workspace_handle_v1::ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        let id = workspace.id().protocol_id();
        let Some(captured) = state.ext_workspaces.get_mut(&id) else {
            return;
        };
        match event {
            ext_workspace_handle_v1::Event::Id { id } => captured.id = Some(id),
            ext_workspace_handle_v1::Event::Name { name } => captured.name = name,
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                captured.coordinates = coordinates
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| u32::from_ne_bytes(*chunk))
                    .collect();
            }
            ext_workspace_handle_v1::Event::State { state } => {
                captured.state = match state {
                    WEnum::Value(value) => value.bits(),
                    WEnum::Unknown(raw) => raw,
                };
            }
            ext_workspace_handle_v1::Event::Capabilities { capabilities } => {
                captured.capabilities = match capabilities {
                    WEnum::Value(value) => value.bits(),
                    WEnum::Unknown(raw) => raw,
                };
            }
            ext_workspace_handle_v1::Event::Removed => captured.removed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for ClientHarness {
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

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for ClientHarness {
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

impl Dispatch<xdg_surface::XdgSurface, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.xdg_surface_configures += 1;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        toplevel: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if matches!(event, xdg_toplevel::Event::Close) {
            state.toplevel_close_count += 1;
        }
        if let xdg_toplevel::Event::Configure {
            width,
            height,
            states,
        } = event
        {
            let proto_id = toplevel.id().protocol_id();
            state
                .toplevel_configures
                .entry(proto_id)
                .or_default()
                .push((width, height));
            let decoded: Vec<u32> = states
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&c| u32::from_le_bytes(c))
                .collect();
            state
                .toplevel_states
                .entry(proto_id)
                .or_default()
                .push(decoded);
        }
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event {
            state.decoration_modes.push(mode);
        }
    }
}

impl Dispatch<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1,
        event: xdg_toplevel_icon_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel_icon_manager_v1::Event::IconSize { size } => state.icon_sizes.push(size),
            xdg_toplevel_icon_manager_v1::Event::Done => state.icon_done_count += 1,
            _ => {}
        }
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.activation_tokens.push(token);
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => state.screencopy_shm_offers.push(ScreencopyBufferOffer {
                format,
                width,
                height,
                stride,
            }),
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf { .. } => {
                state.screencopy_dmabuf_offers += 1;
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => state.screencopy_buffer_done += 1,
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.screencopy_ready += 1,
            zwlr_screencopy_frame_v1::Event::Failed => state.screencopy_failed += 1,
            _ => {}
        }
    }
}

impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, ()>
    for ClientHarness
{
    fn event(
        state: &mut Self,
        _: &zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state
                .wlr_toplevels
                .entry(toplevel.id().protocol_id())
                .or_default();
            state.wlr_toplevel_proxies.push(toplevel);
        }
    }

    event_created_child!(ClientHarness, zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ())
    ]);
}

impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        handle: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        let entry = state
            .wlr_toplevels
            .entry(handle.id().protocol_id())
            .or_default();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => entry.title = title,
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => entry.app_id = app_id,
            zwlr_foreign_toplevel_handle_v1::Event::State { state: bytes } => {
                entry.states = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_ne_bytes(*c))
                    .collect();
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => entry.done_count += 1,
            zwlr_foreign_toplevel_handle_v1::Event::Closed => entry.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state
                .ext_toplevels
                .entry(toplevel.id().protocol_id())
                .or_default();
        }
    }

    event_created_child!(ClientHarness, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ())
    ]);
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        handle: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        let entry = state
            .ext_toplevels
            .entry(handle.id().protocol_id())
            .or_default();
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => entry.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => entry.app_id = app_id,
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                entry.identifier = identifier;
            }
            ext_foreign_toplevel_handle_v1::Event::Done => entry.done_count += 1,
            ext_foreign_toplevel_handle_v1::Event::Closed => entry.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &ext_idle_notification_v1::ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => state.idle_events.push("idled"),
            ext_idle_notification_v1::Event::Resumed => state.idle_events.push("resumed"),
            _ => {}
        }
    }
}

impl Dispatch<zwlr_output_power_v1::ZwlrOutputPowerV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &zwlr_output_power_v1::ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_power_v1::Event::Mode { mode } => state.output_power_modes.push(mode),
            zwlr_output_power_v1::Event::Failed => state.output_power_failed += 1,
            _ => {}
        }
    }
}

impl Dispatch<zwlr_gamma_control_v1::ZwlrGammaControlV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &zwlr_gamma_control_v1::ZwlrGammaControlV1,
        event: zwlr_gamma_control_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => state.gamma_sizes.push(size),
            zwlr_gamma_control_v1::Event::Failed => state.gamma_failed += 1,
            _ => {}
        }
    }
}

impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => state.locked_events.push("locked"),
            zwp_locked_pointer_v1::Event::Unlocked => state.locked_events.push("unlocked"),
            _ => {}
        }
    }
}

impl Dispatch<zwp_confined_pointer_v1::ZwpConfinedPointerV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        event: zwp_confined_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zwp_confined_pointer_v1::Event::Confined => state.confined_events.push("confined"),
            zwp_confined_pointer_v1::Event::Unconfined => state.confined_events.push("unconfined"),
            _ => {}
        }
    }
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
        {
            state
                .relative_motions
                .push((dx, dy, dx_unaccel, dy_unaccel));
        }
    }
}

delegate_noop!(ClientHarness: ignore wl_compositor::WlCompositor);
delegate_noop!(ClientHarness: ignore wl_surface::WlSurface);
delegate_noop!(ClientHarness: ignore wl_region::WlRegion);
delegate_noop!(ClientHarness: ignore wl_shm::WlShm);
delegate_noop!(ClientHarness: ignore wl_shm_pool::WlShmPool);
delegate_noop!(ClientHarness: ignore wl_buffer::WlBuffer);
delegate_noop!(ClientHarness: ignore wl_output::WlOutput);
delegate_noop!(ClientHarness: ignore wl_seat::WlSeat);
delegate_noop!(ClientHarness: ignore xdg_popup::XdgPopup);
delegate_noop!(ClientHarness: ignore xdg_positioner::XdgPositioner);

delegate_noop!(ClientHarness: ignore wl_data_device_manager::WlDataDeviceManager);
delegate_noop!(ClientHarness: ignore wl_keyboard::WlKeyboard);
delegate_noop!(ClientHarness: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(ClientHarness: ignore xdg_toplevel_icon_v1::XdgToplevelIconV1);
delegate_noop!(ClientHarness: ignore xdg_activation_v1::XdgActivationV1);
delegate_noop!(ClientHarness: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(ClientHarness: ignore ext_idle_notifier_v1::ExtIdleNotifierV1);
delegate_noop!(ClientHarness: ignore zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1);
delegate_noop!(ClientHarness: ignore zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1);
delegate_noop!(ClientHarness: ignore zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1);
delegate_noop!(ClientHarness: ignore zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1);
delegate_noop!(ClientHarness: ignore zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);
delegate_noop!(ClientHarness: ignore zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1);
delegate_noop!(ClientHarness: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(ClientHarness: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(ClientHarness: ignore zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1);

impl Dispatch<wl_pointer::WlPointer, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            // Surface-local coordinates as the client sees them: the CSD pointer-offset
            // regression asserts these equal `global - (inner_rect.loc - geometry.loc)`.
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.pointer_motion_coords.push((surface_x, surface_y)),
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(btn_state),
                serial,
                ..
            } => state.pointer_buttons.push((button, btn_state, serial)),
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        let captured = state
            .data_source_events
            .entry(source.id().protocol_id())
            .or_default();
        match event {
            wl_data_source::Event::Cancelled => captured.cancelled += 1,
            wl_data_source::Event::DndDropPerformed => captured.drop_performed += 1,
            wl_data_source::Event::DndFinished => captured.finished += 1,
            wl_data_source::Event::Target { mime_type } => captured.targets.push(mime_type),
            _ => {}
        }
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state
                .data_offer_mimes
                .entry(offer.id().protocol_id())
                .or_default()
                .push(mime_type);
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for ClientHarness {
    event_created_child!(ClientHarness, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ())
    ]);

    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                let id_key = id.id().protocol_id();
                state.data_offer_mimes.entry(id_key).or_default();
                state.data_offer_proxies.insert(id_key, id);
            }
            wl_data_device::Event::Enter { serial, .. } => {
                state.data_device_enters += 1;
                state.data_device_enter_serial = Some(serial);
            }
            wl_data_device::Event::Motion { .. } => state.data_device_motions += 1,
            wl_data_device::Event::Leave => state.data_device_leaves += 1,
            wl_data_device::Event::Drop => state.data_device_drops += 1,
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.fractional_scales.push(scale);
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            state.layer_configures += 1;
            surface.ack_configure(serial);
        }
    }
}

impl Dispatch<zwlr_output_manager_v1::ZwlrOutputManagerV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        _: &zwlr_output_manager_v1::ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => {
                let id = head.id().protocol_id();
                state.output_heads.entry(id).or_default().proxy = Some(head);
            }
            zwlr_output_manager_v1::Event::Done { serial } => {
                state.output_manager_serials.push(serial);
            }
            _ => {}
        }
    }

    event_created_child!(ClientHarness, zwlr_output_manager_v1::ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (zwlr_output_head_v1::ZwlrOutputHeadV1, ())
    ]);
}

impl Dispatch<zwlr_output_head_v1::ZwlrOutputHeadV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        head: &zwlr_output_head_v1::ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        let head_id = head.id().protocol_id();
        if let zwlr_output_head_v1::Event::Mode { mode } = &event {
            let mode_id = mode.id().protocol_id();
            state.output_modes.entry(mode_id).or_default().proxy = Some(mode.clone());
            state
                .output_heads
                .entry(head_id)
                .or_default()
                .mode_ids
                .push(mode_id);
            return;
        }
        let entry = state.output_heads.entry(head_id).or_default();
        match event {
            zwlr_output_head_v1::Event::Name { name } => entry.name = name,
            zwlr_output_head_v1::Event::Description { description } => {
                entry.description = description;
            }
            zwlr_output_head_v1::Event::PhysicalSize { width, height } => {
                entry.phys_mm = (width, height);
            }
            zwlr_output_head_v1::Event::Enabled { enabled } => entry.enabled = Some(enabled != 0),
            zwlr_output_head_v1::Event::CurrentMode { mode } => {
                entry.current_mode_id = Some(mode.id().protocol_id());
            }
            zwlr_output_head_v1::Event::Position { x, y } => entry.position = Some((x, y)),
            zwlr_output_head_v1::Event::Transform { transform } => {
                entry.transform = Some(transform);
            }
            zwlr_output_head_v1::Event::Scale { scale } => entry.scale = Some(scale),
            zwlr_output_head_v1::Event::Make { make } => entry.make = make,
            zwlr_output_head_v1::Event::Model { model } => entry.model = model,
            zwlr_output_head_v1::Event::SerialNumber { serial_number } => {
                entry.serial_number = serial_number;
            }
            zwlr_output_head_v1::Event::Finished => entry.finished = true,
            _ => {}
        }
    }

    event_created_child!(ClientHarness, zwlr_output_head_v1::ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (zwlr_output_mode_v1::ZwlrOutputModeV1, ())
    ]);
}

impl Dispatch<zwlr_output_mode_v1::ZwlrOutputModeV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        mode: &zwlr_output_mode_v1::ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        let entry = state
            .output_modes
            .entry(mode.id().protocol_id())
            .or_default();
        match event {
            zwlr_output_mode_v1::Event::Size { width, height } => {
                entry.width = width;
                entry.height = height;
            }
            zwlr_output_mode_v1::Event::Refresh { refresh } => entry.refresh = refresh,
            zwlr_output_mode_v1::Event::Preferred => entry.preferred = true,
            zwlr_output_mode_v1::Event::Finished => entry.finished = true,
            _ => {}
        }
    }
}

impl Dispatch<zwlr_output_configuration_v1::ZwlrOutputConfigurationV1, ()> for ClientHarness {
    fn event(
        state: &mut Self,
        cfg: &zwlr_output_configuration_v1::ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        let result = match event {
            zwlr_output_configuration_v1::Event::Succeeded => "succeeded",
            zwlr_output_configuration_v1::Event::Failed => "failed",
            zwlr_output_configuration_v1::Event::Cancelled => "cancelled",
            _ => return,
        };
        state
            .output_config_results
            .insert(cfg.id().protocol_id(), result);
    }
}

#[test]
fn all_kom_0018_protocols_registered_and_sandbox_filtered() {
    let (mut event_loop, mut display, mut state) = make_headless_test_state();
    let (conn_unrestricted, mut q_unrestricted, mut c_unrestricted, _) =
        ClientHarness::connect(&display, false);
    let (conn_restricted, mut q_restricted, mut c_restricted, _) =
        ClientHarness::connect(&display, true);

    pump(
        &conn_unrestricted,
        &mut q_unrestricted,
        &mut c_unrestricted,
        &mut display,
        &mut event_loop,
        &mut state,
    );
    pump(
        &conn_restricted,
        &mut q_restricted,
        &mut c_restricted,
        &mut display,
        &mut event_loop,
        &mut state,
    );

    let expected_unrestricted = [
        "zwlr_screencopy_manager_v1",
        "zxdg_decoration_manager_v1",
        "xdg_toplevel_icon_manager_v1",
        "xdg_activation_v1",
        "zwlr_foreign_toplevel_manager_v1",
        "ext_foreign_toplevel_list_v1",
        "ext_idle_notifier_v1",
        "zwp_idle_inhibit_manager_v1",
        "zwlr_output_manager_v1",
        "zwlr_output_power_manager_v1",
        "zwlr_gamma_control_manager_v1",
        "wp_fractional_scale_manager_v1",
        "wp_viewporter",
        "zwp_pointer_constraints_v1",
        "zwp_relative_pointer_manager_v1",
    ];
    for iface in expected_unrestricted {
        assert!(
            c_unrestricted.globals.contains_key(iface),
            "unrestricted client missing global {iface}: {:?}",
            c_unrestricted.globals.keys().collect::<Vec<_>>()
        );
    }

    for privileged in [
        "zwlr_screencopy_manager_v1",
        "zwlr_foreign_toplevel_manager_v1",
        "ext_foreign_toplevel_list_v1",
        "zwlr_output_manager_v1",
        "zwlr_output_power_manager_v1",
        "zwlr_gamma_control_manager_v1",
    ] {
        assert!(
            !c_restricted.globals.contains_key(privileged),
            "restricted client should not see privileged global {privileged}"
        );
    }
    for ordinary in [
        "zxdg_decoration_manager_v1",
        "xdg_toplevel_icon_manager_v1",
        "xdg_activation_v1",
        "ext_idle_notifier_v1",
        "zwp_idle_inhibit_manager_v1",
        "wp_fractional_scale_manager_v1",
        "wp_viewporter",
        "zwp_pointer_constraints_v1",
        "zwp_relative_pointer_manager_v1",
    ] {
        assert!(
            c_restricted.globals.contains_key(ordinary),
            "restricted client should still see ordinary global {ordinary}"
        );
    }
}

#[test]
fn portals_conf_and_screencopy_lock_fail_closed_behavior() {
    let portals = include_str!("../../../resources/koma-portals.conf");
    assert!(portals.contains("default=gnome;gtk;"));
    assert!(portals.contains("org.freedesktop.impl.portal.ScreenCast=wlr;"));
    assert!(portals.contains("org.freedesktop.impl.portal.Screenshot=wlr;"));

    assert_eq!(timestamp_parts(Duration::new(10, 5)), (0, 10, 5));
    let clamped = compute_clamped_physical_region(
        Size::from((1920, 1080)),
        Size::from((3840, 2160)),
        2.0,
        Some(Rectangle::new((100, 100).into(), (2000, 2000).into())),
    )
    .expect("valid clamped subregion");
    assert_eq!(
        clamped.0,
        Rectangle::new((100, 100).into(), (1820, 980).into())
    );
    assert_eq!(clamped.1, Point::from((200, 200)));
    assert_eq!(clamped.2, Size::from((3640, 1960)));
    assert!(
        compute_clamped_physical_region(
            Size::from((1920, 1080)),
            Size::from((1920, 1080)),
            1.0,
            Some(Rectangle::new((2000, 0).into(), (100, 100).into())),
        )
        .is_none()
    );

    let mut env = ClientEnv::new();
    let qh = env.queue.handle();
    let manager = env
        .client
        .bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>(
            &env.registry,
            &qh,
            "zwlr_screencopy_manager_v1",
        );
    let wl_out = env.client.outputs[0].clone();

    let frame_unlocked = manager.capture_output(0, &wl_out, &qh, ());
    env.pump();
    assert_eq!(env.client.screencopy_buffer_done, 1);
    assert_eq!(
        env.client.screencopy_shm_offers.len(),
        1,
        "zwlr_screencopy_frame_v1.buffer must be sent once per frame"
    );
    assert_eq!(env.client.screencopy_failed, 0);

    let offer = env.client.screencopy_shm_offers[0].clone();
    let shm = env
        .client
        .bind::<wl_shm::WlShm>(&env.registry, &qh, "wl_shm");
    let shm_fd = create_portable_shm_fd((offer.stride * offer.height) as usize, None);
    let pool = shm.create_pool(
        shm_fd.as_fd(),
        (offer.stride * offer.height) as i32,
        &qh,
        (),
    );
    let buf = pool.create_buffer(
        0,
        offer.width as i32,
        offer.height as i32,
        offer.stride as i32,
        offer.format,
        &qh,
        (),
    );

    assert!(env.state.session_lock.try_lock(None));
    frame_unlocked.copy(&buf);
    env.pump();
    assert_eq!(
        env.client.screencopy_failed, 1,
        "locked session must emit screencopy failed instead of copying"
    );

    let before_offers = env.client.screencopy_shm_offers.len();
    let _locked_full = manager.capture_output(1, &wl_out, &qh, ());
    let _locked_region = manager.capture_output_region(1, &wl_out, 0, 0, 128, 128, &qh, ());
    env.pump();
    assert_eq!(env.client.screencopy_shm_offers.len(), before_offers);
    assert_eq!(env.client.screencopy_failed, 3);

    assert!(env.state.session_lock.abandon_active_lock());
    let _abandoned = manager.capture_output(0, &wl_out, &qh, ());
    env.pump();
    assert_eq!(env.client.screencopy_failed, 4);

    buf.destroy();
    pool.destroy();
}

#[test]
fn idle_notify_and_idle_inhibit_honor_activity_and_mapping_visibility() {
    let mut env = ClientEnv::new();
    let qh = env.queue.handle();
    let notifier = env.client.bind::<ext_idle_notifier_v1::ExtIdleNotifierV1>(
        &env.registry,
        &qh,
        "ext_idle_notifier_v1",
    );
    let inhibit_mgr = env
        .client
        .bind::<zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1>(
            &env.registry,
            &qh,
            "zwp_idle_inhibit_manager_v1",
        );
    let seat = env.client.seats[0].clone();

    let notif = notifier.get_idle_notification(15, &seat, &qh, ());
    env.pump();
    std::thread::sleep(Duration::from_millis(35));
    env.pump();
    assert!(
        env.client.idle_events.contains(&"idled"),
        "expected idled event after timeout: {:?}",
        env.client.idle_events
    );

    env.state.notify_user_activity();
    env.pump();
    assert_eq!(env.client.idle_events.last().copied(), Some("resumed"));

    let (surface, _xs, _tl) = env.spawn_window("Player", "org.koma.player");
    let win_id = env.state.mapped_windows[0].win_id;

    let inhibitor = inhibit_mgr.create_inhibitor(&surface, &qh, ());
    env.pump();
    assert!(env.state.idle_notifier_state.is_inhibited());

    env.client.idle_events.clear();
    let notif_inhibited = notifier.get_idle_notification(10, &seat, &qh, ());
    std::thread::sleep(Duration::from_millis(30));
    env.pump();
    assert!(
        !env.client.idle_events.contains(&"idled"),
        "idle notification fired while inhibited: {:?}",
        env.client.idle_events
    );

    env.state.set_window_minimized(win_id, true);
    assert!(!env.state.idle_notifier_state.is_inhibited());

    env.state.set_window_minimized(win_id, false);
    assert!(env.state.idle_notifier_state.is_inhibited());

    assert!(env.state.session_lock.try_lock(None));
    env.state.reconcile_idle_inhibitors();
    assert!(!env.state.idle_notifier_state.is_inhibited());

    assert!(env.state.session_lock.unlock());
    env.state.reconcile_idle_inhibitors();
    assert!(env.state.idle_notifier_state.is_inhibited());

    inhibitor.destroy();
    env.pump();
    assert!(!env.state.idle_notifier_state.is_inhibited());

    notif.destroy();
    notif_inhibited.destroy();
}

#[test]
fn xdg_decoration_and_toplevel_icon_negotiation_and_validation() {
    let mut env = ClientEnv::new();
    let qh = env.queue.handle();
    let deco_mgr = env
        .client
        .bind::<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>(
            &env.registry,
            &qh,
            "zxdg_decoration_manager_v1",
        );
    let icon_mgr = env
        .client
        .bind::<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1>(
            &env.registry,
            &qh,
            "xdg_toplevel_icon_manager_v1",
        );

    let (surface, _xs, toplevel) = env.spawn_window("Decorated", "org.koma.deco");
    let deco = deco_mgr.get_toplevel_decoration(&toplevel, &qh, ());
    env.pump();
    assert_eq!(
        env.client.decoration_modes.last().copied(),
        Some(WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide))
    );

    deco.set_mode(zxdg_toplevel_decoration_v1::Mode::ClientSide);
    surface.commit();
    env.pump();
    assert_eq!(
        env.state.mapped_windows[0].decoration_mode,
        DecorationMode::ClientSide
    );
    assert_eq!(
        env.client.decoration_modes.last().copied(),
        Some(WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ClientSide))
    );

    deco.unset_mode();
    env.pump();
    assert_eq!(
        env.state.mapped_windows[0].decoration_mode,
        DecorationMode::ServerSide
    );
    assert_eq!(
        env.client.decoration_modes.last().copied(),
        Some(WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide))
    );

    assert!(!env.client.icon_sizes.is_empty());
    assert!(env.client.icon_done_count >= 1);

    let empty_icon = icon_mgr.create_icon(&qh, ());
    icon_mgr.set_icon(&toplevel, Some(&empty_icon));
    surface.commit();
    env.pump();
    assert!(env.state.mapped_windows[0].icon.is_none());

    let blank_name_icon = icon_mgr.create_icon(&qh, ());
    blank_name_icon.set_name("   ".to_owned());
    icon_mgr.set_icon(&toplevel, Some(&blank_name_icon));
    surface.commit();
    env.pump();
    assert!(env.state.mapped_windows[0].icon.is_none());

    let shm = env.client.shm.clone().expect("wl_shm bound");
    let good_fd = create_portable_shm_fd(16 * 16 * 4, None);
    let good_pool = shm.create_pool(good_fd.as_fd(), 16 * 16 * 4, &qh, ());
    let good_buf = good_pool.create_buffer(0, 16, 16, 16 * 4, wl_shm::Format::Argb8888, &qh, ());
    let valid_icon = icon_mgr.create_icon(&qh, ());
    valid_icon.set_name("koma-test".to_owned());
    valid_icon.add_buffer(&good_buf, 1);
    icon_mgr.set_icon(&toplevel, Some(&valid_icon));
    surface.commit();
    env.pump();

    let stored = env.state.mapped_windows[0]
        .icon
        .as_ref()
        .expect("valid icon stored on MappedWindow");
    assert_eq!(stored.name.as_deref(), Some("koma-test"));
    assert_eq!(stored.buffers.len(), 1);
    assert_eq!(stored.buffers[0].width, 16);
    assert_eq!(stored.buffers[0].height, 16);
    assert_eq!(stored.buffers[0].scale, 1);
    let wl_surf = env.state.mapped_windows[0]
        .window
        .wl_surface()
        .expect("wl_surface")
        .into_owned();
    assert!(extract_validated_toplevel_icon(&wl_surf).is_some());

    icon_mgr.set_icon(&toplevel, None);
    surface.commit();
    env.pump();
    assert!(env.state.mapped_windows[0].icon.is_none());

    deco.destroy();
    empty_icon.destroy();
    blank_name_icon.destroy();
    valid_icon.destroy();
    good_buf.destroy();
    good_pool.destroy();

    // Pre-map xdg_toplevel_icon_v1 attached before the first buffer commit must survive
    // unmapped -> mapped promotion.
    let compositor = env.client.compositor.clone().expect("compositor");
    let wm_base = env.client.xdg_wm_base.clone().expect("wm_base");
    let pre_surf = compositor.create_surface(&qh, ());
    let pre_xs = wm_base.get_xdg_surface(&pre_surf, &qh, ());
    let pre_tl = pre_xs.get_toplevel(&qh, ());
    let pre_icon = icon_mgr.create_icon(&qh, ());
    pre_icon.set_name("premap-icon".to_owned());
    let pre_icon_fd = create_portable_shm_fd(16 * 16 * 4, None);
    let pre_icon_pool = shm.create_pool(pre_icon_fd.as_fd(), 16 * 16 * 4, &qh, ());
    let pre_icon_buf =
        pre_icon_pool.create_buffer(0, 16, 16, 16 * 4, wl_shm::Format::Argb8888, &qh, ());
    pre_icon.add_buffer(&pre_icon_buf, 1);
    icon_mgr.set_icon(&pre_tl, Some(&pre_icon));
    pre_surf.commit();
    env.pump();
    assert_eq!(env.state.unmapped_windows.len(), 1);

    let win_fd = create_portable_shm_fd(64 * 64 * 4, None);
    let win_pool = shm.create_pool(win_fd.as_fd(), 64 * 64 * 4, &qh, ());
    let win_buf = win_pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &qh, ());
    pre_surf.attach(Some(&win_buf), 0, 0);
    pre_surf.commit();
    env.pump();

    assert_eq!(env.state.mapped_windows.len(), 2);
    assert_eq!(
        env.state.mapped_windows[1]
            .icon
            .as_ref()
            .and_then(|i| i.name.as_deref()),
        Some("premap-icon")
    );
    pre_icon.destroy();
    pre_icon_buf.destroy();
    pre_icon_pool.destroy();
    pre_tl.destroy();
    pre_xs.destroy();
    pre_surf.destroy();
    win_buf.destroy();
    win_pool.destroy();
}

#[test]
fn xdg_activation_token_validation_and_session_lock_deferral() {
    let mut env = ClientEnv::new();
    let (conn_sb, mut q_sb, mut c_sb, reg_sb) = ClientHarness::connect(&env.display, true);
    pump(
        &conn_sb,
        &mut q_sb,
        &mut c_sb,
        &mut env.display,
        &mut env.event_loop,
        &mut env.state,
    );

    let qh = env.queue.handle();
    let act_mgr = env.client.bind::<xdg_activation_v1::XdgActivationV1>(
        &env.registry,
        &qh,
        "xdg_activation_v1",
    );
    let qh_sb = q_sb.handle();
    let sb_act =
        c_sb.bind::<xdg_activation_v1::XdgActivationV1>(&reg_sb, &qh_sb, "xdg_activation_v1");

    let (surf_a, _xs_a, _tl_a) = env.spawn_window("Window A", "org.koma.a");
    let win_a = env.state.mapped_windows[0].win_id;
    let ws1 = env.state.active_ws_id.clone();

    execute_action_descriptor(
        &mut env.state,
        ActionDescriptor::WorkspaceFocus("2".to_owned()),
        false,
    );
    let ws2 = env.state.active_ws_id.clone();
    assert_ne!(ws1, ws2);

    let (surf_b, _xs_b, _tl_b) = env.spawn_window("Window B", "org.koma.b");
    let win_b = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.win_id != win_a)
        .expect("win_b")
        .win_id;

    let qh_sb = q_sb.handle();
    let sb_comp = c_sb.bind::<wl_compositor::WlCompositor>(&reg_sb, &qh_sb, "wl_compositor");
    let sb_surf = sb_comp.create_surface(&qh_sb, ());
    let sb_wm = c_sb.bind::<xdg_wm_base::XdgWmBase>(&reg_sb, &qh_sb, "xdg_wm_base");
    let sb_xs = sb_wm.get_xdg_surface(&sb_surf, &qh_sb, ());
    let _sb_tl = sb_xs.get_toplevel(&qh_sb, ());
    let sb_seat = c_sb.seats[0].clone();
    sb_surf.commit();
    pump(
        &conn_sb,
        &mut q_sb,
        &mut c_sb,
        &mut env.display,
        &mut env.event_loop,
        &mut env.state,
    );

    let seat = env.client.seats[0].clone();
    let req_sb = sb_act.get_activation_token(&qh_sb, ());
    req_sb.set_serial(u32::from(SERIAL_COUNTER.next_serial()), &sb_seat);
    req_sb.set_surface(&sb_surf);
    req_sb.commit();
    pump(
        &conn_sb,
        &mut q_sb,
        &mut c_sb,
        &mut env.display,
        &mut env.event_loop,
        &mut env.state,
    );
    let sb_token = c_sb.activation_tokens.pop().expect("sandbox token");

    let req1 = act_mgr.get_activation_token(&qh, ());
    req1.set_serial(u32::from(SERIAL_COUNTER.next_serial()), &seat);
    req1.set_surface(&surf_b);
    req1.set_app_id("org.koma.a".to_owned());
    req1.commit();
    env.pump();
    let token1 = env.client.activation_tokens.pop().expect("token1");

    // Cross-jail activation (sandboxed token attempting to activate an unrestricted surface)
    // must be blocked.
    act_mgr.activate(sb_token, &surf_a);
    env.pump();
    assert_eq!(env.state.active_ws_id, ws2);

    // Even if the sandboxed creator disconnects before `activate`, its snapshotted policy must
    // still block cross-sandbox activation.
    let req_sb_disc = sb_act.get_activation_token(&qh_sb, ());
    req_sb_disc.set_serial(u32::from(SERIAL_COUNTER.next_serial()), &sb_seat);
    req_sb_disc.set_surface(&sb_surf);
    req_sb_disc.commit();
    pump(
        &conn_sb,
        &mut q_sb,
        &mut c_sb,
        &mut env.display,
        &mut env.event_loop,
        &mut env.state,
    );
    let sb_token_disc = c_sb.activation_tokens.pop().expect("sandbox token 2");
    drop(sb_xs);
    drop(sb_surf);
    drop(sb_seat);
    drop(sb_act);
    drop(q_sb);
    drop(conn_sb);
    env.pump();

    act_mgr.activate(sb_token_disc, &surf_a);
    env.pump();
    assert_eq!(env.state.active_ws_id, ws2);

    // Unknown token ignored cleanly.
    act_mgr.activate("unknown-token".to_owned(), &surf_a);
    env.pump();
    assert_eq!(env.state.active_ws_id, ws2);

    // Valid token switches workspace to ws1 and activates win_a.
    act_mgr.activate(token1, &surf_a);
    env.pump();
    assert_eq!(env.state.active_ws_id, ws1);
    assert_eq!(env.state.workspaces[&ws1].active_window(), Some(win_a));

    // Request token while unlocked, then lock session before activating win_b.
    let req2 = act_mgr.get_activation_token(&qh, ());
    req2.set_serial(u32::from(SERIAL_COUNTER.next_serial()), &seat);
    req2.set_surface(&surf_a);
    req2.commit();
    env.pump();
    let token2 = env.client.activation_tokens.pop().expect("token2");

    assert!(env.state.session_lock.try_lock(None));
    act_mgr.activate(token2, &surf_b);
    env.pump();
    assert_eq!(env.state.active_ws_id, ws1);
    assert_eq!(env.state.deferred_xdg_activations.len(), 1);

    // Push a stale expired activation that must be pruned on unlock.
    let stale_surf = env.state.mapped_windows[0]
        .window
        .wl_surface()
        .expect("win_a surface")
        .into_owned();
    env.state
        .deferred_xdg_activations
        .push(DeferredXdgActivation {
            surface: stale_surf,
            created_at: Instant::now() - XDG_ACTIVATION_TOKEN_TTL - Duration::from_secs(1),
        });

    assert!(env.state.session_lock.unlock());
    env.state.flush_deferred_xdg_activations();
    assert_eq!(env.state.active_ws_id, ws2);
    assert_eq!(env.state.workspaces[&ws2].active_window(), Some(win_b));
}

#[test]
fn foreign_toplevel_wlr_and_ext_list_lifecycle_and_lock_guard() {
    assert_eq!(
        encode_wlr_toplevel_states(true, false, true, false).len(),
        8
    );

    let mut env = ClientEnv::new();
    let qh = env.queue.handle();
    let _wlr_mgr = env
        .client
        .bind::<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1>(
            &env.registry,
            &qh,
            "zwlr_foreign_toplevel_manager_v1",
        );
    let _ext_mgr = env
        .client
        .bind::<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1>(
            &env.registry,
            &qh,
            "ext_foreign_toplevel_list_v1",
        );

    let (surf1, _xs1, _tl1) = env.spawn_window("Editor", "org.koma.editor");
    let win1 = env.state.mapped_windows[0].win_id;
    let ws1 = env.state.active_ws_id.clone();

    execute_action_descriptor(
        &mut env.state,
        ActionDescriptor::WorkspaceFocus("2".to_owned()),
        false,
    );
    let ws2 = env.state.active_ws_id.clone();
    let (_surf2, _xs2, tl2) = env.spawn_window("Browser", "org.koma.browser");

    assert_eq!(env.client.wlr_toplevels.len(), 2);
    assert_eq!(env.client.ext_toplevels.len(), 2);
    assert!(
        env.client
            .wlr_toplevels
            .values()
            .any(|t| t.title == "Editor" && t.app_id == "org.koma.editor" && t.done_count >= 1)
    );
    assert!(
        env.client
            .ext_toplevels
            .values()
            .any(|t| t.title == "Editor"
                && t.app_id == "org.koma.editor"
                && !t.identifier.is_empty())
    );

    let editor_handle = env
        .client
        .wlr_toplevel_proxies
        .iter()
        .find(|p| {
            env.client
                .wlr_toplevels
                .get(&p.id().protocol_id())
                .is_some_and(|t| t.title == "Editor")
        })
        .cloned()
        .expect("Editor wlr handle");
    let seat = env.client.seats[0].clone();

    editor_handle.set_maximized();
    editor_handle.set_minimized();
    env.pump();
    let m1 = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win1)
        .expect("win1");
    assert!(env.state.is_window_maximized(win1));
    assert!(m1.is_minimized);
    assert_eq!(env.state.workspaces[&ws1].fullscreen_window(), None);
    assert_eq!(
        m1.rect,
        env.state.workspaces[&ws1].maximized_rect(),
        "maximize must apply real work-area geometry, not the dwindle tile"
    );

    editor_handle.set_fullscreen(None);
    editor_handle.set_rectangle(&surf1, 10, 20, 300, 200);
    env.pump();
    assert_eq!(env.state.workspaces[&ws1].fullscreen_window(), Some(win1));
    assert!(
        !env.state.is_window_maximized(win1),
        "fullscreen and maximize are mutually exclusive"
    );

    editor_handle.unset_maximized();
    editor_handle.unset_minimized();
    editor_handle.unset_fullscreen();
    env.pump();
    let m1 = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win1)
        .expect("win1");
    assert!(!env.state.is_window_maximized(win1));
    assert!(!m1.is_minimized);
    assert_eq!(env.state.workspaces[&ws1].fullscreen_window(), None);

    // Locked session blocks foreign activation.
    assert!(env.state.session_lock.try_lock(None));
    editor_handle.activate(&seat);
    env.pump();
    assert_eq!(env.state.active_ws_id, ws2);

    // Unlock allows foreign activation to switch workspaces and focus Editor.
    assert!(env.state.session_lock.unlock());
    editor_handle.activate(&seat);
    env.pump();
    assert_eq!(env.state.active_ws_id, ws1);
    assert_eq!(env.state.workspaces[&ws1].active_window(), Some(win1));

    // Close on Browser sends xdg_toplevel.close cleanly.
    let browser_handle = env
        .client
        .wlr_toplevel_proxies
        .iter()
        .find(|p| {
            env.client
                .wlr_toplevels
                .get(&p.id().protocol_id())
                .is_some_and(|t| t.title == "Browser")
        })
        .cloned()
        .expect("Browser wlr handle");
    let close_before = env.client.toplevel_close_count;
    browser_handle.close();
    env.pump();
    assert!(env.client.toplevel_close_count > close_before);

    // Destroying the toplevel emits Closed on both wlr and ext handles.
    tl2.destroy();
    env.pump();
    assert!(env.client.wlr_toplevels.values().any(|t| t.closed));
    assert!(env.client.ext_toplevels.values().any(|t| t.closed));
}

#[test]
fn pointer_constraints_clamping_relative_motion_and_lock_break() {
    let attrs = RegionAttributes {
        rects: vec![
            (
                RectangleKind::Add,
                Rectangle::new((10, 10).into(), (100, 100).into()),
            ),
            (
                RectangleKind::Subtract,
                Rectangle::new((40, 40).into(), (20, 20).into()),
            ),
        ],
    };
    let bounds = Rectangle::new((0, 0).into(), (200, 200).into());
    assert_eq!(
        clamp_local_point_to_constraint(bounds, Point::from((500.0, 500.0)), Some(&attrs)),
        Point::from((109.0, 109.0))
    );
    let inside_hole =
        clamp_local_point_to_constraint(bounds, Point::from((50.0, 50.0)), Some(&attrs));
    assert!(!(40.0..60.0).contains(&inside_hole.x) || !(40.0..60.0).contains(&inside_hole.y));

    let mut env = ClientEnv::new();
    let qh = env.queue.handle();
    let constraints = env
        .client
        .bind::<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>(
            &env.registry,
            &qh,
            "zwp_pointer_constraints_v1",
        );
    let rel_mgr = env
        .client
        .bind::<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>(
            &env.registry,
            &qh,
            "zwp_relative_pointer_manager_v1",
        );

    let (surface, _xs, _tl) = env.spawn_window("Game", "org.koma.game");
    let srv_surf = env.state.mapped_windows[0]
        .window
        .wl_surface()
        .expect("wl_surface")
        .into_owned();
    let pointer = env.client.pointers[0].clone();
    let rel_ptr = rel_mgr.get_relative_pointer(&pointer, &qh, ());
    env.pump();

    assert!(point_inside_surface_constraint(
        &srv_surf,
        Point::from((25.0, 25.0)),
        Some(&attrs),
    ));

    // Move pointer into focused surface center before creating lock constraint.
    let origin = env
        .state
        .surface_global_origin(&srv_surf)
        .unwrap_or_else(|| Point::from((0.0, 0.0)));
    env.state.handle_pointer_absolute_motion(
        origin + Point::from((30.0, 30.0)),
        InputTime::from_millis(1),
    );
    env.pump();
    assert!(!env.client.relative_motions.is_empty());

    let compositor = env.client.compositor.clone().expect("wl_compositor");
    let region = compositor.create_region(&qh, ());
    region.add(10, 10, 80, 80);
    let confined = constraints.confine_pointer(
        &surface,
        &pointer,
        Some(&region),
        zwp_pointer_constraints_v1::Lifetime::Persistent,
        &qh,
        (),
    );
    env.state.maybe_activate_pointer_constraint();
    env.pump();
    assert!(env.client.confined_events.contains(&"confined"));

    // Motion past right bound clamps inside confined region.
    env.state.handle_pointer_absolute_motion(
        origin + Point::from((500.0, 500.0)),
        InputTime::from_millis(2),
    );
    assert!(env.state.pointer_location.x <= origin.x + 90.5);
    confined.destroy();
    env.pump();

    // Locked pointer prevents location escape and breaks cleanly on session lock.
    let locked = constraints.lock_pointer(
        &surface,
        &pointer,
        None,
        zwp_pointer_constraints_v1::Lifetime::Persistent,
        &qh,
        (),
    );
    env.state.maybe_activate_pointer_constraint();
    env.pump();
    assert!(env.client.locked_events.contains(&"locked"));
    assert!(env.state.has_active_locked_pointer_constraint());

    let locked_pos = env.state.pointer_location;
    env.state.handle_pointer_relative_motion(
        (12.5, -4.0).into(),
        (14.0, -5.0).into(),
        InputTime::from_millis(3),
    );
    assert_eq!(env.state.pointer_location, locked_pos);
    env.pump();
    assert!(
        env.client
            .relative_motions
            .iter()
            .any(|&(dx, dy, ux, uy)| (dx - 12.5).abs() < 1e-6
                && (ux - 14.0).abs() < 1e-6
                && dy < 0.0
                && uy < 0.0)
    );

    env.state.break_active_pointer_constraints();
    env.pump();
    assert!(!env.state.has_active_locked_pointer_constraint());
    assert_eq!(env.client.locked_events.last().copied(), Some("unlocked"));

    locked.destroy();
    region.destroy();
    rel_ptr.destroy();
}

/// Asserts that pointer focus, `wl_pointer.motion` surface-local coordinates and
/// `surface_global_origin` all agree with the rendering origin `inner_rect.loc - geometry.loc`.
///
/// `csd == true` selects a `set_window_geometry(12, 12, 40, 40)` client-side-decorated toplevel;
/// `csd == false` selects a server-side-decorated toplevel whose geometry origin is `(0, 0)`.
fn assert_toplevel_surface_origin(scale: f64, csd: bool) {
    use smithay::output::Scale;

    let mut env = ClientEnv::new();
    if (scale - 1.0).abs() > f64::EPSILON {
        let out = env._output.clone();
        let (logical_w, logical_h) = (
            (1920.0 / scale).round() as u32,
            (1080.0 / scale).round() as u32,
        );
        out.change_current_state(None, None, Some(Scale::Fractional(scale)), None);
        env.state
            .handle_output_resized(&out, logical_w, logical_h, scale);
        env.pump();
    }

    let geometry = koma_model::LogicalRect::new(12, 12, 40, 40);
    let window_xdg_surface = if csd {
        let (_, xdg_surface, _) = env.spawn_csd_window("CSD", "org.koma.csd", geometry);
        Some(xdg_surface)
    } else {
        let _ = env.spawn_window("SSD", "org.koma.ssd");
        None
    };
    let win_id = env
        .state
        .mapped_windows
        .last()
        .expect("mapped window")
        .win_id;
    let ws_id = env.state.active_ws_id.clone();
    let rect = env.float_window_at(win_id, koma_model::LogicalRect::new(100, 100, 320, 240));
    let out_origin = env.state.workspace_output_origin(&ws_id);

    let border = env.state.lua_sandbox.config_snapshot().appearance.border;
    assert!(border.enabled, "test expects the default window border");
    let b_off = f64::from(border.width.max(0.0));

    // Frame/border space: the inner rect is the visible window geometry.
    let inner = Point::from((f64::from(rect.x) + b_off, f64::from(rect.y) + b_off));
    // Surface space: CSD surfaces extend above/left of the geometry rect by its inset.
    let geometry_loc = if csd {
        Point::from((geometry.x, geometry.y))
    } else {
        Point::from((0, 0))
    };
    let surface_origin = inner - geometry_loc.to_f64();
    let global_origin = out_origin + surface_origin;

    let server_surface = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win_id)
        .and_then(|m| m.window.toplevel())
        .expect("toplevel")
        .wl_surface()
        .clone();

    // A point just inside the visible geometry rect must reach the client with surface-local
    // coordinates derived from the same origin rendering used.
    let inside = inner + Point::from((5.0, 5.0));

    // Deliver pointer motion. The first motion establishes `wl_pointer.enter`; the second one
    // emits `wl_pointer.motion`.
    env.state.handle_pointer_absolute_motion(
        inside - Point::from((1.0, 1.0)),
        InputTime::from_millis(1),
    );
    env.state
        .handle_pointer_absolute_motion(inside, InputTime::from_millis(2));
    env.pump();
    let (surface_x, surface_y) = *env
        .client
        .pointer_motion_coords
        .last()
        .expect("client must receive wl_pointer.motion over the window geometry");
    let expected = inside - global_origin;
    assert!(
        (surface_x - expected.x).abs() < 1e-3 && (surface_y - expected.y).abs() < 1e-3,
        "wl_pointer.motion must be surface-local (scale {scale}, csd {csd}): \
         expected ({}, {}), got ({surface_x}, {surface_y})",
        expected.x,
        expected.y
    );

    // The hit test selects the toplevel and reports the surface origin that rendering used.
    let hit = env
        .state
        .hit_test_target(inside)
        .expect("point inside the window geometry must hit the toplevel");
    match hit {
        crate::state::PointerHitTarget::Window {
            win_id: hit_win,
            origin,
            ..
        } => {
            assert_eq!(hit_win, win_id);
            assert_eq!(
                origin, global_origin,
                "hit-test origin must be the surface origin (scale {scale})"
            );
        }
        other => panic!("expected a window hit, got {other:?}"),
    }

    assert_eq!(
        env.state.surface_global_origin(&server_surface),
        Some(global_origin),
        "surface_global_origin must use inner_rect.loc - geometry.loc (scale {scale})"
    );

    // The decoration band lies outside the window geometry (and outside the frame). With an
    // input region covering only the geometry rect it must not be a hit target.
    let margin = inner - Point::from((b_off + 2.0, b_off + 2.0));
    assert!(
        env.state.surface_under(margin).is_none(),
        "CSD decoration band outside the window geometry must not be a pointer target (scale {scale})"
    );

    // An xdg_popup anchored at the parent's window-geometry origin sits at `inner` and must be
    // hit-tested through the same surface origin (Smithay's POPUP branch already adds the root
    // geometry inset, so it needs a surface-space point).
    if let Some(parent_xdg_surface) = window_xdg_surface {
        let qh = env.queue.handle();
        let wm = env.client.xdg_wm_base.clone().expect("xdg_wm_base");
        let compositor = env.client.compositor.clone().expect("wl_compositor");
        let shm = env.client.shm.clone().expect("wl_shm");

        let positioner = wm.create_positioner(&qh, ());
        positioner.set_size(32, 32);
        positioner.set_anchor_rect(0, 0, geometry.w as i32, geometry.h as i32);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        let popup_surface = compositor.create_surface(&qh, ());
        let popup_xdg_surface = wm.get_xdg_surface(&popup_surface, &qh, ());
        let popup = popup_xdg_surface.get_popup(Some(&parent_xdg_surface), &positioner, &qh, ());
        popup_surface.commit();
        env.pump();

        let popup_fd = create_portable_shm_fd(32 * 32 * 4, None);
        let popup_pool = shm.create_pool(popup_fd.as_fd(), 32 * 32 * 4, &qh, ());
        let popup_buffer =
            popup_pool.create_buffer(0, 32, 32, 32 * 4, wl_shm::Format::Argb8888, &qh, ());
        popup_surface.attach(Some(&popup_buffer), 0, 0);
        popup_surface.commit();
        env.pump();

        let popup_origin = inner;
        let popup_probe = popup_origin + Point::from((5.0, 5.0));
        let (_, hit_origin) = env
            .state
            .surface_under(popup_probe)
            .expect("csd popup must be hit-testable at the parent window-geometry origin");
        assert_eq!(
            hit_origin, popup_origin,
            "popup origin must be inner_rect.loc - root geometry.loc + popup window-geometry
             position (scale {scale})"
        );

        popup.destroy();
        popup_xdg_surface.destroy();
        popup_surface.destroy();
        positioner.destroy();
        popup_buffer.destroy();
        popup_pool.destroy();
    }
}

#[test]
fn csd_window_geometry_defines_surface_origin_for_pointer_motion() {
    assert_toplevel_surface_origin(1.0, true);
    // SSD/XWayland (`geometry.loc == (0, 0)`) keeps its previous origin and coordinates.
    assert_toplevel_surface_origin(1.0, false);
}

#[test]
fn csd_window_geometry_defines_surface_origin_at_fractional_scale() {
    assert_toplevel_surface_origin(1.6, true);
    assert_toplevel_surface_origin(1.6, false);
}

#[test]
fn output_power_and_gamma_control_validation_and_transitions() {
    assert_eq!(OutputPowerMode::On.to_protocol(), ServerOutputPowerMode::On);
    assert_eq!(
        OutputPowerMode::Off.to_protocol(),
        ServerOutputPowerMode::Off
    );

    // Gamma LUT validation unit cases.
    let mut payload = Vec::with_capacity(256 * 3 * 2);
    for i in 0..(256 * 3) {
        payload.extend_from_slice(&((i as u16).wrapping_mul(100)).to_ne_bytes());
    }
    let valid_fd = create_portable_shm_fd(payload.len(), Some(&payload));
    let lut = read_and_validate_gamma_lut(valid_fd, 256).expect("valid 256 LUT");
    assert_eq!(lut.red.len(), 256);
    assert_eq!(lut.green.len(), 256);
    assert_eq!(lut.blue.len(), 256);

    let zero_fd = create_portable_shm_fd(8, None);
    assert!(matches!(
        read_and_validate_gamma_lut(zero_fd, 0),
        Err(GammaValidationError::UnsupportedGammaSize)
    ));

    let short_fd = create_portable_shm_fd(256 * 3 * 2 - 2, None);
    assert!(matches!(
        read_and_validate_gamma_lut(short_fd, 256),
        Err(GammaValidationError::InvalidFileSize { .. })
    ));

    let long_fd = create_portable_shm_fd(256 * 3 * 2 + 16, None);
    assert!(matches!(
        read_and_validate_gamma_lut(long_fd, 256),
        Err(GammaValidationError::InvalidFileSize { .. })
    ));

    // Output power protocol + headless GammaControl fail-closed test.
    let mut env = ClientEnv::new();
    let qh = env.queue.handle();
    let power_mgr = env
        .client
        .bind::<zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1>(
            &env.registry,
            &qh,
            "zwlr_output_power_manager_v1",
        );
    let gamma_mgr = env
        .client
        .bind::<zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1>(
            &env.registry,
            &qh,
            "zwlr_gamma_control_manager_v1",
        );
    let wl_out = env.client.outputs[0].clone();

    let power = power_mgr.get_output_power(&wl_out, &qh, ());
    env.pump();
    assert_eq!(
        env.client.output_power_modes.last().copied(),
        Some(WEnum::Value(zwlr_output_power_v1::Mode::On))
    );

    power.set_mode(zwlr_output_power_v1::Mode::Off);
    env.pump();
    assert!(env.state.output_power_state.is_output_off("DP-1"));
    assert_eq!(
        env.client.output_power_modes.last().copied(),
        Some(WEnum::Value(zwlr_output_power_v1::Mode::Off))
    );

    // Concurrent second controller for the same output must fail immediately.
    let dup_power = power_mgr.get_output_power(&wl_out, &qh, ());
    env.pump();
    assert_eq!(env.client.output_power_failed, 1);

    power.set_mode(zwlr_output_power_v1::Mode::On);
    env.pump();
    assert!(!env.state.output_power_state.is_output_off("DP-1"));

    env.state.output_power_state.notify_output_removed("DP-1");
    env.pump();
    assert_eq!(env.client.output_power_failed, 2);

    // Headless state has no DRM CRTC LUT, so get_gamma_control emits Failed cleanly.
    let gamma = gamma_mgr.get_gamma_control(&wl_out, &qh, ());
    env.pump();
    assert_eq!(env.client.gamma_failed, 1);

    dup_power.destroy();
    power.destroy();
    gamma.destroy();
}

#[test]
fn toplevel_destroy_without_a_renderer_frees_window_state_synchronously() {
    let mut env = ClientEnv::new();
    let (surface, xdg_surface, toplevel) = env.spawn_window("Closing", "org.koma.closing");
    assert_eq!(env.state.mapped_windows.len(), 1);
    let win_id = env.state.mapped_windows[0].win_id;

    toplevel.destroy();
    xdg_surface.destroy();
    surface.destroy();
    env.pump();

    assert!(
        env.state.mapped_windows.is_empty(),
        "a destroyed toplevel must leave layout, hit-testing and IPC immediately"
    );
    assert!(env.state.unmapped_windows.is_empty());
    assert!(
        !env.state.window_animations.borrow().is_closing(win_id),
        "without a renderer no frame can be captured, so the close spring must not linger"
    );
    assert!(
        env.state.closing_windows.is_empty(),
        "no captured frame may outlive the toplevel when no snapshot was possible"
    );
}

#[test]
fn screencopy_copy_with_damage_waits_for_damage_and_minimizing_clears_focus() {
    let mut env = ClientEnv::new();
    let qh = env.queue.handle();

    // Verify minimizing the active window excludes it from keyboard focus selection.
    let (_s, _xs, _tl) = env.spawn_window("MinFocus", "org.koma.minfocus");
    let win_id = env.state.mapped_windows[0].win_id;
    env.state.reconcile_keyboard_focus();
    assert!(
        env.state
            .seat
            .get_keyboard()
            .unwrap()
            .current_focus()
            .is_some()
    );
    env.state.set_window_minimized(win_id, true);
    env.state.reconcile_keyboard_focus();
    assert!(
        env.state
            .seat
            .get_keyboard()
            .unwrap()
            .current_focus()
            .is_none(),
        "minimized active window must not retain keyboard focus"
    );

    // Verify copy_with_damage stays pending while there is no unseen damage.
    let manager = env
        .client
        .bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>(
            &env.registry,
            &qh,
            "zwlr_screencopy_manager_v1",
        );
    let shm = env.client.shm.clone().expect("wl_shm bound");
    let wl_out = env.client.outputs[0].clone();

    let initial_frame = manager.capture_output(0, &wl_out, &qh, ());
    env.pump();
    let offer = env
        .client
        .screencopy_shm_offers
        .last()
        .expect("initial screencopy offer")
        .clone();
    let fd = create_portable_shm_fd((offer.stride * offer.height) as usize, None);
    let pool = shm.create_pool(fd.as_fd(), (offer.stride * offer.height) as i32, &qh, ());
    let buf = pool.create_buffer(
        0,
        offer.width as i32,
        offer.height as i32,
        offer.stride as i32,
        offer.format,
        &qh,
        (),
    );
    initial_frame.copy(&buf);
    env.pump();

    // Next copy_with_damage happens when manager has already copied current damage epoch.
    let dmg_frame = manager.capture_output(0, &wl_out, &qh, ());
    env.pump();
    dmg_frame.copy_with_damage(&buf);
    env.pump();

    crate::screencopy::flush_pending_screencopy_frames(&mut env.state);
    crate::screencopy::flush_pending_screencopy_frames(&mut env.state);
    assert_eq!(
        env.state.screencopy_state.pending_damage_copies.len(),
        1,
        "copy_with_damage must stay pending without unseen damage"
    );

    // Once new damage occurs, locking or flushing processes the pending frame.
    env.state.screencopy_state.mark_damage();
    assert!(env.state.session_lock.try_lock(None));
    crate::screencopy::flush_pending_screencopy_frames(&mut env.state);
    assert!(env.state.screencopy_state.pending_damage_copies.is_empty());

    buf.destroy();
    pool.destroy();
}

#[test]
fn ext_workspace_global_publishes_every_workspace_and_activate_switches_compositor() {
    let mut env = ClientEnv::new();
    let ws1 = env.state.active_ws_id.clone();
    let ws2 = env.state.ensure_workspace_by_name_or_id("2");
    // A headless/agent workspace has no on-screen representation and must only be `hidden`.
    let headless = env.state.ensure_workspace_by_name_or_id("agent");
    env.state
        .workspaces
        .get_mut(&headless)
        .expect("headless workspace")
        .presentation = koma_model::PresentationTarget::Offscreen { lazy_fbo_slot: 0 };
    let manager = env.bind_ext_workspace_manager();
    env.pump();

    assert_eq!(
        env.client.ext_workspace_groups.len(),
        1,
        "one group must hold the Koma workspace model"
    );
    let group = env
        .client
        .ext_workspace_groups
        .values()
        .next()
        .unwrap()
        .clone();
    assert!(
        group.capabilities.is_some(),
        "the group must advertise its capabilities once on creation"
    );
    assert_eq!(
        group.outputs.len(),
        1,
        "the bar's bound wl_output must be reported through output_enter"
    );
    assert_eq!(group.workspaces.len(), 3, "every workspace joins the group");
    assert!(
        env.client.ext_workspace_done >= 1,
        "sync must end with done"
    );

    assert_eq!(env.client.ext_workspaces.len(), 3);
    assert_eq!(
        env.client.ext_workspace_id("1"),
        Some(ws1.as_str().to_owned())
    );
    assert_eq!(
        env.client.ext_workspace_id("2"),
        Some(ws2.as_str().to_owned())
    );
    assert_eq!(
        env.client.ext_workspace_state("1") & EXT_WORKSPACE_STATE_ACTIVE,
        EXT_WORKSPACE_STATE_ACTIVE,
        "the focused workspace must be advertised as active"
    );
    assert_eq!(
        env.client.ext_workspace_state("2") & EXT_WORKSPACE_STATE_HIDDEN,
        EXT_WORKSPACE_STATE_HIDDEN,
        "an empty workspace is hidden, so a bar with `ignore-hidden` lists only workspaces in use"
    );
    assert_eq!(
        env.client.ext_workspace_state("agent") & EXT_WORKSPACE_STATE_HIDDEN,
        EXT_WORKSPACE_STATE_HIDDEN,
        "a headless/agent workspace must be advertised as hidden"
    );
    let coordinates: Vec<u32> = env
        .client
        .ext_workspaces
        .values()
        .flat_map(|ws| ws.coordinates.clone())
        .collect();
    assert_eq!(
        coordinates,
        vec![0, 1, 2],
        "coordinates must be unique and 1D"
    );
    assert!(
        env.client
            .ext_workspaces
            .values()
            .all(|ws| ws.capabilities & EXT_WORKSPACE_CAPABILITY_ACTIVATE != 0),
        "activate must be advertised on every workspace"
    );

    // A status-bar click: `workspace.activate` followed by `commit`.
    env.client
        .ext_workspace_by_name("2")
        .expect("workspace 2 handle")
        .activate();
    manager.commit();
    env.pump();

    assert_eq!(
        env.state.active_ws_id, ws2,
        "ext-workspace activate must switch the compositor"
    );
    assert_eq!(
        env.client.ext_workspace_state("2") & EXT_WORKSPACE_STATE_ACTIVE,
        EXT_WORKSPACE_STATE_ACTIVE
    );
    assert_eq!(
        env.client.ext_workspace_state("1") & EXT_WORKSPACE_STATE_HIDDEN,
        EXT_WORKSPACE_STATE_HIDDEN,
        "the switched-away workspace is empty, so it becomes hidden instead of staying listed"
    );
}

#[test]
fn ext_workspace_states_track_compositor_initiated_workspace_switches() {
    let mut env = ClientEnv::new();
    let ws1 = env.state.active_ws_id.clone();
    let ws2 = env.state.ensure_workspace_by_name_or_id("2");
    let manager = env.bind_ext_workspace_manager();
    env.pump();

    assert_eq!(
        env.client.ext_workspace_state("1") & EXT_WORKSPACE_STATE_ACTIVE,
        EXT_WORKSPACE_STATE_ACTIVE
    );
    assert_eq!(
        env.client.ext_workspace_state("2") & EXT_WORKSPACE_STATE_HIDDEN,
        EXT_WORKSPACE_STATE_HIDDEN,
        "an empty inactive workspace is hidden"
    );

    // Compositor-side switch (`ws.focus`/keybind path), not a bar request.
    assert!(env.state.focus_workspace(&ws2));
    env.pump();
    assert_eq!(
        env.client.ext_workspace_state("2") & EXT_WORKSPACE_STATE_ACTIVE,
        EXT_WORKSPACE_STATE_ACTIVE,
        "the compositor-side switch must publish active"
    );
    assert_eq!(
        env.client.ext_workspace_state("1") & EXT_WORKSPACE_STATE_HIDDEN,
        EXT_WORKSPACE_STATE_HIDDEN,
        "the switched-away workspace must drop its active bit and be hidden while empty"
    );

    assert!(env.state.focus_workspace(&ws1));
    env.pump();
    assert_eq!(
        env.client.ext_workspace_state("1") & EXT_WORKSPACE_STATE_ACTIVE,
        EXT_WORKSPACE_STATE_ACTIVE
    );
    assert_eq!(
        env.client.ext_workspace_state("2") & EXT_WORKSPACE_STATE_HIDDEN,
        EXT_WORKSPACE_STATE_HIDDEN
    );

    // Fail closed: a locked session must ignore a committed `activate`.
    assert!(env.state.session_lock.try_lock(None));
    env.client
        .ext_workspace_by_name("2")
        .expect("workspace 2 handle")
        .activate();
    manager.commit();
    env.pump();
    assert_eq!(
        env.state.active_ws_id, ws1,
        "activate must be ignored while the session is locked"
    );
}

#[test]
fn ext_workspace_visibility_follows_workspace_occupancy() {
    let mut env = ClientEnv::new();
    let ws1 = env.state.active_ws_id.clone();
    let ws2 = env.state.ensure_workspace_by_name_or_id("2");
    let _manager = env.bind_ext_workspace_manager();
    env.pump();

    assert_eq!(
        env.client.ext_workspace_state("2") & EXT_WORKSPACE_STATE_HIDDEN,
        EXT_WORKSPACE_STATE_HIDDEN,
        "a workspace that never held a window is hidden, so the bar does not list it"
    );

    // Opening the first window on a workspace must publish it as visible before the next frame.
    let (surface, xdg_surface, toplevel) = env.spawn_window("Occupant", "org.koma.occupant");
    env.pump();
    assert_eq!(
        env.client.ext_workspace_state("1") & EXT_WORKSPACE_STATE_ACTIVE,
        EXT_WORKSPACE_STATE_ACTIVE
    );
    assert_eq!(
        env.client.ext_workspace_state("2") & EXT_WORKSPACE_STATE_HIDDEN,
        EXT_WORKSPACE_STATE_HIDDEN,
        "an unrelated empty workspace stays hidden while another one is used"
    );
    assert_eq!(env.state.mapped_windows.len(), 1);

    // A silent move (`follow = false`) never reaches `focus_workspace`, so the occupancy re-sync
    // inside the move path is what republishes both workspaces.
    assert!(
        env.state
            .move_active_window_between_workspaces_with_follow(&ws1, &ws2, false)
    );
    env.pump();
    assert_eq!(
        env.client.ext_workspace_state("2"),
        0,
        "the destination now holds a window: visible with no state bits so its click target \
         survives `ignore-hidden: true`"
    );
    assert_eq!(
        env.client.ext_workspace_state("1") & EXT_WORKSPACE_STATE_ACTIVE,
        EXT_WORKSPACE_STATE_ACTIVE,
        "a silent move keeps the source workspace displayed"
    );

    // Closing the last window of a workspace hides it again in the same dispatch as the unmap.
    toplevel.destroy();
    xdg_surface.destroy();
    surface.destroy();
    env.pump();
    assert_eq!(
        env.client.ext_workspace_state("2") & EXT_WORKSPACE_STATE_HIDDEN,
        EXT_WORKSPACE_STATE_HIDDEN,
        "closing the last window must re-hide the workspace instead of leaving a stale bar entry"
    );

    // `persistent = true` is the per-workspace opt-out from hiding empty workspaces.
    env.state
        .lua_sandbox
        .eval_init_lua(r#"koma.workspace({ name = "3", persistent = true })"#)
        .expect("persistent workspace spec must evaluate");
    let ws3 = env.state.ensure_workspace_by_name_or_id("3");
    env.pump();
    assert!(env.state.workspaces[&ws3].persistent);
    assert_eq!(
        env.client.ext_workspace_state("3"),
        0,
        "a persistent empty workspace stays in the bar with no state bits"
    );
}

#[test]
fn ext_workspace_activate_waits_for_commit_and_keeps_only_the_last_request() {
    let mut env = ClientEnv::new();
    env.state
        .lua_sandbox
        .eval_init_lua(
            r##"
            koma.on("workspace.focused", function(ctx, ev)
                ctx:dispatch(koma.act.workspace.set_layout("deck", ev.name))
            end)
            "##,
        )
        .expect("workspace.focused hook must evaluate");
    let ws1 = env.state.active_ws_id.clone();
    let ws2 = env.state.ensure_workspace_by_name_or_id("2");
    let ws3 = env.state.ensure_workspace_by_name_or_id("3");
    let manager = env.bind_ext_workspace_manager();
    env.pump();

    // An activation that is never committed must not switch.
    env.client
        .ext_workspace_by_name("2")
        .expect("workspace 2 handle")
        .activate();
    env.pump();
    assert_eq!(
        env.state.active_ws_id, ws1,
        "activate must not take effect before commit"
    );

    // Two activations in one batch: only the last one applies, and no intermediate workspace
    // reaches `workspace.focused` (the hook would have set its layout to `Deck`).
    env.client
        .ext_workspace_by_name("2")
        .expect("workspace 2 handle")
        .activate();
    env.client
        .ext_workspace_by_name("3")
        .expect("workspace 3 handle")
        .activate();
    manager.commit();
    env.pump();
    assert_eq!(env.state.active_ws_id, ws3);
    assert_eq!(
        env.state.workspaces[&ws3].layout_mode,
        koma_model::LayoutMode::Deck,
        "the committed workspace must emit `workspace.focused`"
    );
    assert_ne!(
        env.state.workspaces[&ws2].layout_mode,
        koma_model::LayoutMode::Deck,
        "an intermediate activation in the same batch must never run"
    );

    // A pending request is dropped when the manager stops instead of committing.
    env.client
        .ext_workspace_by_name("2")
        .expect("workspace 2 handle")
        .activate();
    manager.stop();
    env.pump();
    assert_eq!(
        env.state.active_ws_id, ws3,
        "an uncommitted activation must be dropped on stop"
    );
    assert_ne!(
        env.state.workspaces[&ws2].layout_mode,
        koma_model::LayoutMode::Deck
    );
}

#[test]
fn ext_workspace_handle_destroy_is_scoped_to_one_manager() {
    let mut env = ClientEnv::new();
    let _ = env.state.ensure_workspace_by_name_or_id("2");
    let manager_a = env.bind_ext_workspace_manager();
    let manager_b = env.bind_ext_workspace_manager();
    env.pump();

    let manager_a_id = manager_a.id().protocol_id();
    let manager_b_id = manager_b.id().protocol_id();
    let handles_named = |harness: &ClientHarness, manager_id: u32, name: &str| -> Vec<u32> {
        harness
            .ext_workspace_handles_of_manager(manager_id)
            .into_iter()
            .filter(|id| harness.ext_workspaces.get(id).map(|ws| ws.name.as_str()) == Some(name))
            .collect()
    };
    assert_eq!(
        env.client
            .ext_workspace_handles_of_manager(manager_a_id)
            .len(),
        2,
        "each manager instance gets its own handles"
    );
    let manager_b_handles = env.client.ext_workspace_handles_of_manager(manager_b_id);
    assert_eq!(manager_b_handles.len(), 2);
    let created_a = env.client.ext_workspace_managers[&manager_a_id].workspaces_created;
    let created_b = env.client.ext_workspace_managers[&manager_b_id].workspaces_created;
    assert_eq!((created_a, created_b), (2, 2));

    let destroyed = env
        .client
        .ext_workspace_handle_for_manager(manager_a_id, "2")
        .expect("manager A workspace 2 handle")
        .clone();
    destroyed.destroy();
    env.pump();

    // A later sync recreates only the destroyed manager's handle; the other manager keeps its
    // original object instead of receiving a duplicate for the same workspace.
    crate::ext_workspace::sync_ext_workspaces(&mut env.state);
    env.pump();

    assert_eq!(
        env.client.ext_workspace_handles_of_manager(manager_b_id),
        manager_b_handles,
        "destroying one manager's handle must not touch another manager's objects"
    );
    assert_eq!(
        handles_named(&env.client, manager_b_id, "2").len(),
        1,
        "the surviving manager must keep exactly one handle per workspace"
    );
    assert_eq!(
        env.client.ext_workspace_managers[&manager_a_id].workspaces_created,
        created_a + 1,
        "the owning manager must get one fresh resource for the destroyed handle"
    );
    assert_eq!(
        env.client.ext_workspace_managers[&manager_b_id].workspaces_created, created_b,
        "no handle may be re-created for the untouched manager"
    );
}

#[test]
fn ext_workspace_cycle_focus_keeps_bar_state_in_sync() {
    let mut env = ClientEnv::new();
    env.state
        .lua_sandbox
        .eval_init_lua(
            r##"
            koma.on("workspace.focused", function(ctx, ev)
                ctx:dispatch(koma.act.workspace.set_layout("deck", ev.name))
            end)
            "##,
        )
        .expect("workspace.focused hook must evaluate");
    let ws2 = env.state.ensure_workspace_by_name_or_id("2");
    let _manager = env.bind_ext_workspace_manager();
    env.pump();
    assert_eq!(
        env.client.ext_workspace_state("1") & EXT_WORKSPACE_STATE_ACTIVE,
        EXT_WORKSPACE_STATE_ACTIVE
    );

    execute_action_descriptor(&mut env.state, ActionDescriptor::WorkspaceFocusNext, false);
    env.pump();

    assert_eq!(env.state.active_ws_id, ws2);
    assert_eq!(
        env.client.ext_workspace_state("2") & EXT_WORKSPACE_STATE_ACTIVE,
        EXT_WORKSPACE_STATE_ACTIVE,
        "`WorkspaceFocusNext` must publish the new active workspace"
    );
    assert_eq!(
        env.client.ext_workspace_state("1") & EXT_WORKSPACE_STATE_ACTIVE,
        0,
        "the bar must stop reporting the previous workspace as active"
    );
    assert_eq!(
        env.client.ext_workspace_state("1") & EXT_WORKSPACE_STATE_HIDDEN,
        EXT_WORKSPACE_STATE_HIDDEN,
        "the switched-away workspace holds no window in this test, so it is hidden"
    );
    assert_eq!(
        env.state.workspaces[&ws2].layout_mode,
        koma_model::LayoutMode::Deck,
        "`WorkspaceFocusNext` must emit `workspace.focused` like every other switch"
    );
}

#[test]
fn ext_workspace_group_delivers_output_leave_after_output_removal() {
    let mut env = ClientEnv::new();
    let _manager = env.bind_ext_workspace_manager();
    env.pump();
    let group_outputs = |env: &ClientEnv| -> Vec<u32> {
        env.client
            .ext_workspace_groups
            .values()
            .next()
            .map(|group| group.outputs.clone())
            .unwrap_or_default()
    };
    assert_eq!(group_outputs(&env).len(), 1);

    // A second binding of the same physical output must be tracked separately.
    let (global_name, version) = env.client.globals["wl_output"];
    let qh = env.queue.handle();
    let _second_binding =
        env.registry
            .bind::<wl_output::WlOutput, (), ClientHarness>(global_name, version, &qh, ());
    env.pump();
    assert_eq!(
        group_outputs(&env).len(),
        2,
        "two bindings of one physical output are two group memberships"
    );

    // Hot-unplug: the retained `wl_output` resources still receive `output_leave`, even though
    // `all_outputs()` no longer resolves the output.
    env.state.test_outputs.clear();
    crate::ext_workspace::sync_ext_workspaces(&mut env.state);
    env.pump();
    assert!(
        group_outputs(&env).is_empty(),
        "output_leave must reach the client after the output is gone"
    );
}

#[test]
fn move_to_workspace_follow_and_silent_variants_move_keyboard_focus() {
    let mut env = ClientEnv::new();
    let ws1 = env.state.active_ws_id.clone();
    let _surf_a = env.spawn_window("Stay", "org.koma.stay").0;
    let (_surf_b, _xs_b, _tl_b) = env.spawn_window("Moved", "org.koma.moved");
    let moved_win = env.state.mapped_windows[1].win_id;
    let moved_server_surface = mapped_surface(&env.state, moved_win);
    env.state.focus_active_deck_window();
    assert_eq!(
        env.state.seat.get_keyboard().unwrap().current_focus(),
        Some(moved_server_surface.clone()),
        "the newest window must start keyed"
    );

    execute_action_descriptor(
        &mut env.state,
        ActionDescriptor::WindowMoveToWorkspace {
            workspace: "2".to_owned(),
            follow: true,
        },
        false,
    );
    let ws2 = env.state.active_ws_id.clone();
    assert_ne!(ws1, ws2, "follow must switch the active workspace");
    assert_eq!(env.state.workspaces[&ws2].active_window(), Some(moved_win));
    assert_eq!(
        env.state.seat.get_keyboard().unwrap().current_focus(),
        Some(moved_server_surface),
        "the moved window must hold keyboard focus after a followed move"
    );

    // A new window on the destination becomes active; the silent move of that window
    // must keep the destination workspace and the previous window focused.
    let (_surf_c, _xs_c, _tl_c) = env.spawn_window("Silent", "org.koma.silent");
    let silent_win = env.state.mapped_windows[2].win_id;
    let silent_server_surface = mapped_surface(&env.state, silent_win);
    env.state.focus_active_deck_window();
    assert_eq!(
        env.state.seat.get_keyboard().unwrap().current_focus(),
        Some(silent_server_surface.clone())
    );

    execute_action_descriptor(
        &mut env.state,
        ActionDescriptor::WindowMoveToWorkspace {
            workspace: "3".to_owned(),
            follow: false,
        },
        false,
    );
    assert_eq!(
        env.state.active_ws_id, ws2,
        "silent move must keep the source workspace focused"
    );
    assert_eq!(env.state.workspaces[&ws2].active_window(), Some(moved_win));
    assert_eq!(
        env.state.seat.get_keyboard().unwrap().current_focus(),
        Some(mapped_surface(&env.state, moved_win))
    );
    assert_ne!(
        env.state.seat.get_keyboard().unwrap().current_focus(),
        Some(silent_server_surface),
        "the silently moved window must not keep keyboard focus"
    );
}

fn mapped_surface(
    state: &KomaState,
    win_id: koma_proto::WindowId,
) -> smithay::reexports::wayland_server::protocol::wl_surface::WlSurface {
    state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win_id)
        .and_then(|m| m.window.toplevel())
        .map(|t| t.wl_surface().clone())
        .expect("mapped toplevel")
}

fn keyboard_focus(
    state: &KomaState,
) -> Option<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface> {
    state.seat.get_keyboard().and_then(|k| k.current_focus())
}

/// Reads the pending `xdg_toplevel` `Activated` flag, which `focus_active_deck_window` keeps in
/// sync with the keyboard-focused tiling/floating window.
fn toplevel_activated(state: &KomaState, win_id: koma_proto::WindowId) -> bool {
    state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win_id)
        .and_then(|m| m.window.toplevel())
        .is_some_and(|t| {
            t.with_pending_state(|s| {
                s.states
                    .contains(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated)
            })
        })
}

/// Applies `follow_mouse` through the real config path (`koma.input.pointer.follow_mouse`).
fn apply_follow_mouse(env: &mut ClientEnv, value: &str) {
    let mut registry = koma_lua::KomaConfigRegistry::default();
    registry.input.pointer.follow_mouse = Some(value.to_owned());
    env.state.apply_runtime_input_config(&registry);
}

/// Spawns two side-by-side floating windows (`left` below `right`) and returns both ids.
///
/// Floating rects keep the pointer probes deterministic: `left` covers `(0,0)-(800,600)` and
/// `right` covers `(1000,0)-(1800,600)`, leaving a `(800,0)-(1000,600)` gap without any window.
fn spawn_follow_mouse_windows(env: &mut ClientEnv) -> (koma_proto::WindowId, koma_proto::WindowId) {
    let _left = env.spawn_window("FollowLeft", "org.koma.followleft").0;
    let _right = env.spawn_window("FollowRight", "org.koma.followright").0;
    let ws_id = env.state.active_ws_id.clone();
    let now_ms = env.state.elapsed_ms();
    let left = env.state.mapped_windows[0].win_id;
    let right = env.state.mapped_windows[1].win_id;
    if let Some(ws) = env.state.workspaces.get_mut(&ws_id) {
        for (win_id, rect) in [
            (left, koma_model::LogicalRect::new(0, 0, 800, 600)),
            (right, koma_model::LogicalRect::new(1000, 0, 800, 600)),
        ] {
            ws.toggle_floating(win_id, None, None, now_ms);
            ws.floating.set_rect(win_id, rect);
        }
    }
    env.state.sync_workspace_geometries(&ws_id);
    env.state.focus_active_deck_window();
    assert_eq!(env.state.workspaces[&ws_id].active_window(), Some(right));
    (left, right)
}

/// Pointer probe inside `win_id`'s current visible geometry (the harness buffer is `64x64`).
fn probe_inside(
    env: &ClientEnv,
    ws_id: &koma_proto::WorkspaceId,
    win_id: koma_proto::WindowId,
) -> Point<f64, smithay::utils::Logical> {
    let rect = env
        .state
        .workspaces
        .get(ws_id)
        .expect("workspace")
        .compute_visible_geometries_at(env.state.elapsed_ms())
        .into_iter()
        .find(|(id, _, _)| *id == win_id)
        .map(|(_, rect, _)| rect)
        .expect("visible geometry");
    Point::from((f64::from(rect.x) + 24.0, f64::from(rect.y) + 24.0))
}

fn move_pointer(env: &mut ClientEnv, pos: Point<f64, smithay::utils::Logical>) {
    env.state
        .handle_pointer_absolute_motion(pos, InputTime::from_millis(1));
}

fn move_pointer_onto(
    env: &mut ClientEnv,
    ws_id: &koma_proto::WorkspaceId,
    win_id: koma_proto::WindowId,
) {
    let pos = probe_inside(env, ws_id, win_id);
    move_pointer(env, pos);
}

#[test]
fn follow_mouse_config_strings_map_to_modes() {
    let mut env = ClientEnv::new();
    use koma_lua::FollowMouseMode;
    assert_eq!(env.state.follow_mouse_mode, FollowMouseMode::Normal);

    apply_follow_mouse(&mut env, "off");
    assert_eq!(env.state.follow_mouse_mode, FollowMouseMode::Off);
    apply_follow_mouse(&mut env, "focus_only");
    assert_eq!(env.state.follow_mouse_mode, FollowMouseMode::FocusOnly);
    apply_follow_mouse(&mut env, "normal");
    assert_eq!(env.state.follow_mouse_mode, FollowMouseMode::Normal);
    // Unknown values fall back to `normal` (`Hyprland` `follow_mouse = 1`).
    apply_follow_mouse(&mut env, "bogus");
    assert_eq!(env.state.follow_mouse_mode, FollowMouseMode::Normal);
}

#[test]
fn follow_mouse_off_keeps_click_to_focus() {
    let mut env = ClientEnv::new();
    let ws_id = env.state.active_ws_id.clone();
    apply_follow_mouse(&mut env, "off");
    let (left, right) = spawn_follow_mouse_windows(&mut env);
    let right_surface = mapped_surface(&env.state, right);

    move_pointer_onto(&mut env, &ws_id, left);

    assert_eq!(
        keyboard_focus(&env.state),
        Some(right_surface),
        "off must not transfer keyboard focus on hover"
    );
    assert_eq!(env.state.workspaces[&ws_id].active_window(), Some(right));
    assert!(!toplevel_activated(&env.state, left));
    assert!(toplevel_activated(&env.state, right));
    assert_eq!(env.state.pointer_focus_window, None);

    // `off` is click-to-focus, so an explicit click on the other window still focuses it.
    let left_probe = probe_inside(&env, &ws_id, left);
    env.state.focus_window_at_point(left_probe);
    assert_eq!(env.state.workspaces[&ws_id].active_window(), Some(left));
    assert_eq!(
        keyboard_focus(&env.state),
        Some(mapped_surface(&env.state, left))
    );
    assert!(toplevel_activated(&env.state, left));
}

#[test]
fn follow_mouse_normal_focuses_and_raises_window_under_cursor() {
    let mut env = ClientEnv::new();
    let ws_id = env.state.active_ws_id.clone();
    apply_follow_mouse(&mut env, "normal");
    let (left, right) = spawn_follow_mouse_windows(&mut env);
    assert_eq!(env.state.workspaces[&ws_id].floating.topmost(), Some(right));

    move_pointer_onto(&mut env, &ws_id, left);

    assert_eq!(
        keyboard_focus(&env.state),
        Some(mapped_surface(&env.state, left)),
        "normal (`Hyprland` 1) must key the window under the cursor"
    );
    assert_eq!(
        env.state.workspaces[&ws_id].active_window(),
        Some(left),
        "normal (`Hyprland` 1) must also raise/activate it in the workspace stack"
    );
    assert_eq!(
        env.state.workspaces[&ws_id].floating.topmost(),
        Some(left),
        "raising must reorder the floating stack"
    );
    assert!(toplevel_activated(&env.state, left));
    assert!(!toplevel_activated(&env.state, right));
    assert_eq!(env.state.last_notified_focus_win_id, Some(left));
    assert_eq!(env.state.focused_window_id(), Some(left));
}

#[test]
fn follow_mouse_focus_only_transfers_keyboard_without_stack_change() {
    let mut env = ClientEnv::new();
    let ws_id = env.state.active_ws_id.clone();
    apply_follow_mouse(&mut env, "focus_only");
    let (left, right) = spawn_follow_mouse_windows(&mut env);

    move_pointer_onto(&mut env, &ws_id, left);

    assert_eq!(
        keyboard_focus(&env.state),
        Some(mapped_surface(&env.state, left)),
        "focus_only must transfer keyboard focus to the window under the cursor"
    );
    assert_eq!(
        env.state.workspaces[&ws_id].active_window(),
        Some(right),
        "focus_only must not change the workspace's active-window stacking"
    );
    assert_eq!(
        env.state.workspaces[&ws_id].floating.topmost(),
        Some(right),
        "focus_only must not raise the hovered window"
    );
    assert_eq!(
        env.state.focused_window_id(),
        Some(left),
        "the keyboard-focus source must follow the pointer in focus_only"
    );
    // `xdg_toplevel` `Activated` follows the transferred focus even though the stack is unchanged.
    assert!(toplevel_activated(&env.state, left));
    assert!(!toplevel_activated(&env.state, right));

    // A click restores ordinary click-to-focus semantics and clears the pointer override.
    let left_probe = probe_inside(&env, &ws_id, left);
    env.state.focus_window_at_point(left_probe);
    assert_eq!(env.state.pointer_focus_window, None);
    assert_eq!(env.state.workspaces[&ws_id].active_window(), Some(left));
}

#[test]
fn follow_mouse_same_window_gap_and_lock_do_not_transfer_focus() {
    let mut env = ClientEnv::new();
    let ws_id = env.state.active_ws_id.clone();
    apply_follow_mouse(&mut env, "normal");
    let (left, right) = spawn_follow_mouse_windows(&mut env);
    let right_surface = mapped_surface(&env.state, right);

    // Motion inside the already-focused window is a no-op.
    move_pointer_onto(&mut env, &ws_id, right);
    let right_probe = probe_inside(&env, &ws_id, right);
    move_pointer(&mut env, right_probe + Point::from((16.0, 16.0)));
    assert_eq!(keyboard_focus(&env.state), Some(right_surface.clone()));
    assert_eq!(env.state.workspaces[&ws_id].active_window(), Some(right));

    // Motion over the gap between both windows resolves no window and keeps focus.
    let gap = Point::from((900.0, 300.0));
    assert!(
        env.state.hit_test_visible_surface(gap).is_none(),
        "the probe must land on a real gap between the two windows"
    );
    move_pointer(&mut env, gap);
    assert_eq!(keyboard_focus(&env.state), Some(right_surface.clone()));
    assert_eq!(env.state.workspaces[&ws_id].active_window(), Some(right));

    // Motion while the session is locked must never transfer focus to a window.
    assert!(env.state.session_lock.try_lock(None));
    move_pointer_onto(&mut env, &ws_id, left);
    assert_eq!(env.state.workspaces[&ws_id].active_window(), Some(right));
    assert_ne!(
        keyboard_focus(&env.state),
        Some(mapped_surface(&env.state, left)),
        "locked sessions must not hand keyboard focus to a window under the pointer"
    );
    assert_eq!(env.state.pointer_focus_window, None);
}

#[test]
fn wlr_output_manager_head_advertisement_test_apply_cancel_and_sandbox() {
    use koma_model::SafeAreaInsets;
    use smithay::{
        output::{Mode, PhysicalProperties, Scale, Subpixel},
        utils::Transform,
    };

    let (mut event_loop, mut display, mut state) = make_headless_test_state();

    let edp = Output::new(
        "eDP-1".to_owned(),
        PhysicalProperties {
            size: (302, 196).into(),
            subpixel: Subpixel::HorizontalRgb,
            make: "APP".to_owned(),
            model: "Color LCD".to_owned(),
            serial_number: "RETINA123".to_owned(),
        },
    );
    let mode_native = Mode {
        size: (3024, 1964).into(),
        refresh: 120_000,
    };
    let mode_scaled = Mode {
        size: (1920, 1200).into(),
        refresh: 60_000,
    };
    edp.change_current_state(
        Some(mode_native),
        Some(Transform::Normal),
        Some(Scale::Fractional(2.0)),
        Some((0, 0).into()),
    );
    edp.set_preferred(mode_native);
    edp.add_mode(mode_scaled);
    let _edp_global = edp.create_global::<KomaState>(&state.display_handle);

    let hdmi = test_output("HDMI-A-1");
    let _hdmi_global = hdmi.create_global::<KomaState>(&state.display_handle);

    state.test_outputs = vec![edp.clone(), hdmi.clone()];
    state.register_or_update_output(&edp, Some((0, 0)), SafeAreaInsets::default(), true);
    state.register_or_update_output(&hdmi, None, SafeAreaInsets::default(), true);

    let (conn_r, mut q_r, mut c_r, _) = ClientHarness::connect(&display, true);
    pump(
        &conn_r,
        &mut q_r,
        &mut c_r,
        &mut display,
        &mut event_loop,
        &mut state,
    );
    assert!(
        !c_r.globals.contains_key("zwlr_output_manager_v1"),
        "restricted sandbox client must not see zwlr_output_manager_v1"
    );

    let (conn, mut queue, mut client, registry) = ClientHarness::connect(&display, false);
    pump(
        &conn,
        &mut queue,
        &mut client,
        &mut display,
        &mut event_loop,
        &mut state,
    );

    let qh = queue.handle();
    let mgr = client.bind::<zwlr_output_manager_v1::ZwlrOutputManagerV1>(
        &registry,
        &qh,
        "zwlr_output_manager_v1",
    );
    pump(
        &conn,
        &mut queue,
        &mut client,
        &mut display,
        &mut event_loop,
        &mut state,
    );

    assert_eq!(client.output_heads.len(), 2);
    let initial_serial = *client
        .output_manager_serials
        .last()
        .expect("done serial emitted on bind");

    let edp_head = client
        .output_heads
        .values()
        .find(|h| h.name == "eDP-1")
        .expect("eDP-1 head advertised")
        .clone();
    assert_eq!(edp_head.make, "APP");
    assert_eq!(edp_head.model, "Color LCD");
    assert_eq!(edp_head.serial_number, "RETINA123");
    assert_eq!(edp_head.phys_mm, (302, 196));
    assert_eq!(edp_head.enabled, Some(true));
    assert_eq!(edp_head.position, Some((0, 0)));
    assert_eq!(edp_head.scale, Some(2.0));
    assert_eq!(edp_head.mode_ids.len(), 2);

    let alt_mode_id = *edp_head
        .mode_ids
        .iter()
        .find(|mid| {
            client
                .output_modes
                .get(mid)
                .is_some_and(|m| m.width == 1920 && m.height == 1200)
        })
        .expect("secondary eDP-1 mode advertised");
    let alt_mode_proxy = client.output_modes[&alt_mode_id]
        .proxy
        .clone()
        .expect("mode proxy");

    let hdmi_head = client
        .output_heads
        .values()
        .find(|h| h.name == "HDMI-A-1")
        .expect("HDMI-A-1 head advertised")
        .clone();
    assert_eq!(hdmi_head.position, Some((1512, 0)));

    // 1. Test configuration validates without mutating state.
    let edp_proxy = edp_head.proxy.clone().unwrap();
    let hdmi_proxy = hdmi_head.proxy.clone().unwrap();
    let cfg_test = mgr.create_configuration(initial_serial, &qh, ());
    let cfg_test_edp = cfg_test.enable_head(&edp_proxy, &qh, ());
    cfg_test_edp.set_mode(&alt_mode_proxy);
    cfg_test_edp.set_transform(wl_output::Transform::_90);
    cfg_test_edp.set_scale(1.5);
    cfg_test.disable_head(&hdmi_proxy);
    cfg_test.test();
    pump(
        &conn,
        &mut queue,
        &mut client,
        &mut display,
        &mut event_loop,
        &mut state,
    );
    assert_eq!(
        client
            .output_config_results
            .get(&cfg_test.id().protocol_id())
            .copied(),
        Some("succeeded")
    );
    assert_eq!(edp.current_mode(), Some(mode_native));
    assert!(state.output_layout.entry("HDMI-A-1").unwrap().enabled);

    // 2. Apply configuration mutates mode/transform/scale/position, disables HDMI-A-1, and bumps serial.
    let cfg_apply = mgr.create_configuration(initial_serial, &qh, ());
    let cfg_apply_edp = cfg_apply.enable_head(&edp_proxy, &qh, ());
    cfg_apply_edp.set_mode(&alt_mode_proxy);
    cfg_apply_edp.set_position(100, 50);
    cfg_apply_edp.set_transform(wl_output::Transform::_90);
    cfg_apply_edp.set_scale(1.5);
    cfg_apply.disable_head(&hdmi_proxy);
    cfg_apply.apply();
    pump(
        &conn,
        &mut queue,
        &mut client,
        &mut display,
        &mut event_loop,
        &mut state,
    );

    assert_eq!(
        client
            .output_config_results
            .get(&cfg_apply.id().protocol_id())
            .copied(),
        Some("succeeded")
    );
    assert_eq!(edp.current_mode(), Some(mode_scaled));
    assert_eq!(edp.current_transform(), Transform::_90);
    assert!((edp.current_scale().fractional_scale() - 1.5).abs() < 1e-4);
    assert_eq!(edp.current_location(), Point::from((100, 50)));
    assert!(!state.output_layout.entry("HDMI-A-1").unwrap().enabled);

    let new_serial = *client.output_manager_serials.last().unwrap();
    assert_ne!(new_serial, initial_serial);
    let updated_hdmi = client
        .output_heads
        .values()
        .find(|h| h.name == "HDMI-A-1")
        .unwrap();
    assert_eq!(updated_hdmi.enabled, Some(false));

    // 3. Stale serial is rejected with `cancelled`.
    let cfg_stale = mgr.create_configuration(initial_serial, &qh, ());
    let _stale_edp = cfg_stale.enable_head(&edp_proxy, &qh, ());
    let _stale_hdmi = cfg_stale.enable_head(&hdmi_proxy, &qh, ());
    cfg_stale.apply();
    pump(
        &conn,
        &mut queue,
        &mut client,
        &mut display,
        &mut event_loop,
        &mut state,
    );
    assert_eq!(
        client
            .output_config_results
            .get(&cfg_stale.id().protocol_id())
            .copied(),
        Some("cancelled")
    );

    cfg_stale.destroy();
    cfg_apply.destroy();
    cfg_test.destroy();
}

#[test]
fn multi_monitor_window_move_fractional_scale_and_notch_layer_shell_composition() {
    use koma_model::SafeAreaInsets;
    use smithay::output::Scale;

    let mut env = ClientEnv::new();
    let edp = env._output.clone();
    let hdmi = test_output("HDMI-A-1");
    let _hdmi_global = hdmi.create_global::<KomaState>(&env.state.display_handle);
    env.state.test_outputs.push(hdmi.clone());

    let notch = SafeAreaInsets {
        top: 38,
        bottom: 0,
        left: 0,
        right: 0,
    };
    env.state
        .register_or_update_output(&edp, Some((0, 0)), notch, true);
    env.state
        .register_or_update_output(&hdmi, None, SafeAreaInsets::default(), true);
    env.pump();

    // Verify notch safe_area composes with wlr-layer-shell top bar exclusive_zone.
    let qh = env.queue.handle();
    let compositor =
        env.client
            .bind::<wl_compositor::WlCompositor>(&env.registry, &qh, "wl_compositor");
    let layer_shell = env.client.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1>(
        &env.registry,
        &qh,
        "zwlr_layer_shell_v1",
    );
    let shm = env
        .client
        .bind::<wl_shm::WlShm>(&env.registry, &qh, "wl_shm");
    let frac_mgr = env
        .client
        .bind::<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>(
            &env.registry,
            &qh,
            "wp_fractional_scale_manager_v1",
        );
    env.client.compositor = Some(compositor.clone());
    env.client.shm = Some(shm.clone());

    let bar_surf = compositor.create_surface(&qh, ());
    let wl_out_edp = env.client.outputs[0].clone();
    let layer_surf = layer_shell.get_layer_surface(
        &bar_surf,
        Some(&wl_out_edp),
        zwlr_layer_shell_v1::Layer::Top,
        "koma-bar".to_owned(),
        &qh,
        (),
    );
    layer_surf.set_size(1920, 24);
    layer_surf.set_anchor(zwlr_layer_surface_v1::Anchor::Top);
    layer_surf.set_exclusive_zone(24);
    bar_surf.commit();
    env.pump();

    let bar_fd = create_portable_shm_fd(1920 * 24 * 4, None);
    let bar_pool = shm.create_pool(bar_fd.as_fd(), 1920 * 24 * 4, &qh, ());
    let bar_buf = bar_pool.create_buffer(0, 1920, 24, 1920 * 4, wl_shm::Format::Argb8888, &qh, ());
    bar_surf.attach(Some(&bar_buf), 0, 0);
    bar_surf.commit();
    env.pump();

    let edp_ws_id = env.state.active_workspace_for_output("DP-1");
    let edp_ws = env.state.workspaces.get(&edp_ws_id).unwrap();
    // 24px bar intersects cleanly with 38px camera notch (protected y=38).
    assert_eq!(edp_ws.viewport.work_area.expect("work_area").y, 38);

    // Increasing top exclusive_zone to 52px extends work_area below both notch and bar.
    layer_surf.set_size(1920, 52);
    layer_surf.set_exclusive_zone(52);
    bar_surf.commit();
    env.pump();
    let bar2_fd = create_portable_shm_fd(1920 * 52 * 4, None);
    let bar2_pool = shm.create_pool(bar2_fd.as_fd(), 1920 * 52 * 4, &qh, ());
    let bar2_buf =
        bar2_pool.create_buffer(0, 1920, 52, 1920 * 4, wl_shm::Format::Argb8888, &qh, ());
    bar_surf.attach(Some(&bar2_buf), 0, 0);
    bar_surf.commit();
    env.pump();

    let edp_ws = env.state.workspaces.get(&edp_ws_id).unwrap();
    assert_eq!(edp_ws.viewport.work_area.expect("work_area").y, 52);

    // Verify wp_fractional_scale_v1 receives initial scale (120) and updates when DP-1 scale changes to 1.5 (180).
    let (win_surf, _xs, _tl) = env.spawn_window("CrossMon", "org.koma.cross");
    let frac = frac_mgr.get_fractional_scale(&win_surf, &qh, ());
    env.pump();
    assert!(env.client.fractional_scales.contains(&120));

    edp.change_current_state(None, None, Some(Scale::Fractional(1.5)), None);
    env.state.handle_output_resized(&edp, 1280, 720, 1.5);
    env.pump();
    assert_eq!(env.client.fractional_scales.last().copied(), Some(180));

    // Exercise WindowMoveToMonitorDirection("right") moving focused window from DP-1 to HDMI-A-1.
    let hdmi_ws_id = env.state.active_workspace_for_output("HDMI-A-1");
    assert_ne!(edp_ws_id, hdmi_ws_id);
    assert_eq!(env.state.mapped_windows[0].ws_id, edp_ws_id);

    execute_action_descriptor(
        &mut env.state,
        ActionDescriptor::WindowMoveToMonitorDirection("right".to_owned()),
        false,
    );
    assert_eq!(env.state.mapped_windows[0].ws_id, hdmi_ws_id);
    assert_eq!(env.state.focused_output_name().as_deref(), Some("HDMI-A-1"));

    frac.destroy();
    layer_surf.destroy();
    bar2_buf.destroy();
    bar2_pool.destroy();
    bar_buf.destroy();
    bar_pool.destroy();
}

#[test]
fn fullscreen_window_suppresses_top_and_overlay_layer_tiers_but_keeps_background() {
    use smithay::desktop::layer_map_for_output;
    use smithay::wayland::shell::wlr_layer::Layer;

    let mut env = ClientEnv::new();
    let qh = env.queue.handle();
    let compositor =
        env.client
            .bind::<wl_compositor::WlCompositor>(&env.registry, &qh, "wl_compositor");
    let layer_shell = env.client.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1>(
        &env.registry,
        &qh,
        "zwlr_layer_shell_v1",
    );
    let shm = env
        .client
        .bind::<wl_shm::WlShm>(&env.registry, &qh, "wl_shm");
    env.client.compositor = Some(compositor.clone());
    env.client.shm = Some(shm.clone());

    // Real Waybar-shaped client: `Top` tier, anchored top edge with a 24 px exclusive zone.
    let bar_surf = compositor.create_surface(&qh, ());
    let wl_out = env.client.outputs[0].clone();
    let bar_layer = layer_shell.get_layer_surface(
        &bar_surf,
        Some(&wl_out),
        zwlr_layer_shell_v1::Layer::Top,
        "koma-bar".to_owned(),
        &qh,
        (),
    );
    bar_layer.set_size(1920, 24);
    bar_layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top);
    bar_layer.set_exclusive_zone(24);
    bar_surf.commit();
    env.pump();
    let bar_fd = create_portable_shm_fd(1920 * 24 * 4, None);
    let bar_pool = shm.create_pool(bar_fd.as_fd(), 1920 * 24 * 4, &qh, ());
    let bar_buf = bar_pool.create_buffer(0, 1920, 24, 1920 * 4, wl_shm::Format::Argb8888, &qh, ());
    bar_surf.attach(Some(&bar_buf), 0, 0);
    bar_surf.commit();
    env.pump();

    let (_surf1, _xs1, _tl1) = env.spawn_window("Editor", "org.koma.editor");
    let (_surf2, _xs2, _tl2) = env.spawn_window("Browser", "org.koma.browser");

    let ws_id = env.state.active_workspace_for_output("DP-1");
    let win1 = env.state.mapped_windows[0].win_id;
    assert_eq!(env.state.mapped_windows.len(), 2);

    // Panels are composited above toplevels and the desktop background stays behind them.
    let tiers = env.state.output_layer_tiers(&ws_id);
    assert_eq!(
        tiers.above_toplevels,
        [Layer::Overlay, Layer::Top].as_slice(),
        "Top/Overlay tiers composite above tiled windows while nothing is fullscreen"
    );
    assert_eq!(
        tiers.below_toplevels,
        [Layer::Bottom, Layer::Background].as_slice()
    );

    let ws = env
        .state
        .workspaces
        .get_mut(&ws_id)
        .expect("DP-1 workspace");
    let work_area = ws.viewport.work_area_rect();
    assert_eq!(
        work_area.y, 24,
        "the 24 px bar must reserve work area before fullscreen"
    );
    ws.set_fullscreen(win1, true);
    env.state.sync_workspace_geometries(&ws_id);

    // Fullscreen: the bar tier is skipped for this output, background tier survives.
    let tiers = env.state.output_layer_tiers(&ws_id);
    assert!(
        tiers.above_toplevels.is_empty(),
        "Top/Overlay tiers must not composite over a fullscreen window"
    );
    assert_eq!(
        tiers.below_toplevels,
        [Layer::Bottom, Layer::Background].as_slice(),
        "background layers must stay composited behind a fullscreen window"
    );

    // The policy is render-time only: the bar stays mapped and reclaims the output afterwards.
    let map = layer_map_for_output(&env._output);
    assert_eq!(
        map.layers_on(Layer::Top).count(),
        1,
        "the layer surface must remain mapped, only its tier is skipped"
    );
    drop(map);

    // The fullscreen toplevel is the only window element, hence front-most, and covers the
    // physical output rect including the reserved bar strip.
    let ws = env.state.workspaces.get(&ws_id).expect("DP-1 workspace");
    assert_eq!(
        ws.compute_visible_geometries(),
        vec![(win1, koma_model::LogicalRect::new(0, 0, 1920, 1080), false)]
    );
    let items = env.state.collect_workspace_scene_items(&ws_id, false);
    assert_eq!(items.len(), 1, "sibling windows are not composited");
    let expected_root = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win1)
        .and_then(|m| m.window.toplevel().map(|t| t.wl_surface().clone()))
        .expect("fullscreen toplevel surface");

    // A fullscreen window is the screen: no border ring, no focus ring, no shadow. The owner
    // reported the border outline surviving on `Mod + Shift + F` (komawm/koma#30 follow-up).
    let item = &items[0];
    assert!(
        !item.border.enabled,
        "a fullscreen window must not paint its border ring"
    );
    assert!(
        !item.focus_ring.enabled,
        "a fullscreen window must not paint its focus ring"
    );
    assert!(
        !item.shadow.enabled && item.no_shadow,
        "a fullscreen window must not paint a drop shadow"
    );
    assert_eq!(
        item.corner_radius, 0.0,
        "a fullscreen window must not be clipped to rounded corners"
    );

    // Leaving fullscreen restores the decorated presentation.
    env.state
        .workspaces
        .get_mut(&ws_id)
        .expect("DP-1 workspace")
        .set_fullscreen(win1, false);
    env.state.sync_workspace_geometries(&ws_id);
    let restored = env.state.collect_workspace_scene_items(&ws_id, false);
    let restored_item = restored
        .iter()
        .find(|i| {
            i.window
                .toplevel()
                .is_some_and(|t| *t.wl_surface() == expected_root)
        })
        .expect("tiled window is composited again");
    assert!(
        restored_item.border.enabled,
        "the border ring comes back once the window is tiled again"
    );

    // Leaving fullscreen restores the bar tier.
    env.state
        .workspaces
        .get_mut(&ws_id)
        .expect("DP-1 workspace")
        .set_fullscreen(win1, false);
    let tiers = env.state.output_layer_tiers(&ws_id);
    assert_eq!(
        tiers.above_toplevels,
        [Layer::Overlay, Layer::Top].as_slice()
    );

    // Maximize is distinct from fullscreen: the bar stays composited and the window fills only
    // the gapped work area.
    let maximized = {
        let ws = env
            .state
            .workspaces
            .get_mut(&ws_id)
            .expect("DP-1 workspace");
        ws.set_maximized(win1, true);
        let maximized = ws.maximized_rect();
        assert_eq!(maximized.y, work_area.y + ws.outer_gap() as i32);
        assert_ne!(maximized, ws.viewport.full_rect());
        maximized
    };
    env.state.sync_workspace_geometries(&ws_id);
    let tiers = env.state.output_layer_tiers(&ws_id);
    assert_eq!(
        tiers.above_toplevels,
        [Layer::Overlay, Layer::Top].as_slice(),
        "maximize must not hide the bar"
    );
    let ws = env.state.workspaces.get(&ws_id).expect("DP-1 workspace");
    assert_eq!(
        ws.compute_visible_geometries(),
        vec![(win1, maximized, false)]
    );

    bar_layer.destroy();
    bar_buf.destroy();
    bar_pool.destroy();
}

#[test]
fn screencopy_v1_emits_single_buffer_event_and_ready() {
    let mut env = ClientEnv::new();
    let qh = env.queue.handle();
    let (mgr_name, advertised_ver) = env
        .client
        .globals
        .get("zwlr_screencopy_manager_v1")
        .copied()
        .expect("missing zwlr_screencopy_manager_v1");
    assert_eq!(advertised_ver, 3);
    // Clients such as grim can bind v1 where `buffer_done` does not exist and each
    // `buffer` event implies format advertisement is complete.
    let manager_v1 = env
        .registry
        .bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, (), ClientHarness>(
            mgr_name,
            1,
            &qh,
            (),
        );
    let wl_out = env.client.outputs[0].clone();
    let frame = manager_v1.capture_output(0, &wl_out, &qh, ());
    env.pump();

    assert_eq!(env.client.screencopy_shm_offers.len(), 1);
    assert_eq!(env.client.screencopy_dmabuf_offers, 0);
    assert_eq!(env.client.screencopy_buffer_done, 0);
    assert_eq!(env.client.screencopy_failed, 0);

    let offer = env.client.screencopy_shm_offers[0].clone();
    let shm = env
        .client
        .bind::<wl_shm::WlShm>(&env.registry, &qh, "wl_shm");
    let shm_fd = create_portable_shm_fd((offer.stride * offer.height) as usize, None);
    let pool = shm.create_pool(
        shm_fd.as_fd(),
        (offer.stride * offer.height) as i32,
        &qh,
        (),
    );
    let buf = pool.create_buffer(
        0,
        offer.width as i32,
        offer.height as i32,
        offer.stride as i32,
        offer.format,
        &qh,
        (),
    );
    frame.copy(&buf);
    env.pump();

    assert_eq!(env.client.screencopy_ready, 1);
    assert_eq!(env.client.screencopy_failed, 0);

    buf.destroy();
    pool.destroy();
}

#[test]
fn background_layer_surface_enters_frame_list_below_toplevels() {
    use crate::backend_winit::{
        BACKGROUND_LAYER_TIERS, FOREGROUND_LAYER_TIERS, layer_entry_location, layer_tier_entries,
    };
    use smithay::{desktop::layer_map_for_output, wayland::shell::wlr_layer::Layer};

    let mut env = ClientEnv::new();
    let qh = env.queue.handle();
    let compositor =
        env.client
            .bind::<wl_compositor::WlCompositor>(&env.registry, &qh, "wl_compositor");
    let layer_shell = env.client.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1>(
        &env.registry,
        &qh,
        "zwlr_layer_shell_v1",
    );
    let shm = env
        .client
        .bind::<wl_shm::WlShm>(&env.registry, &qh, "wl_shm");
    env.client.compositor = Some(compositor.clone());
    env.client.shm = Some(shm.clone());

    // The workspace toplevel the wallpaper must be composited below.
    let (_win_surface, _xdg_surface, _toplevel) = env.spawn_window("Wall", "org.koma.wall");
    assert_eq!(
        env.state.mapped_windows.len(),
        1,
        "toplevel must be mapped for the ordering fixture"
    );

    // Reproduce the awww/swww wallpaper shape: `zwlr_layer_shell_v1` Background tier, anchored to
    // all four edges, `exclusive_zone == -1`, size `0x0` (the compositor picks the output size).
    let wl_out = env.client.outputs[0].clone();
    let wallpaper = compositor.create_surface(&qh, ());
    let wallpaper_layer = layer_shell.get_layer_surface(
        &wallpaper,
        Some(&wl_out),
        zwlr_layer_shell_v1::Layer::Background,
        "awww-daemon".to_owned(),
        &qh,
        (),
    );
    wallpaper_layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    wallpaper_layer.set_exclusive_zone(-1);
    wallpaper_layer.set_size(0, 0);
    wallpaper.commit();
    env.pump();
    assert!(
        env.client.layer_configures >= 1,
        "compositor must configure the Background surface"
    );

    // First commit is buffer-less by protocol: the layer is mapped and arranged, but contributes no
    // pixels yet, so it must not emit a stale element.
    let map = layer_map_for_output(&env._output);
    assert_eq!(map.layers_on(Layer::Background).count(), 1);
    assert!(
        layer_tier_entries(&map, &BACKGROUND_LAYER_TIERS).is_empty(),
        "a buffer-less Background surface must be skipped instead of emitting a 0x0 element"
    );
    drop(map);

    // Attach the wallpaper buffer: from here the Background element must be part of every frame.
    let wallpaper_fd = create_portable_shm_fd(1920 * 1080 * 4, None);
    let wallpaper_pool = shm.create_pool(wallpaper_fd.as_fd(), 1920 * 1080 * 4, &qh, ());
    let wallpaper_buf =
        wallpaper_pool.create_buffer(0, 1920, 1080, 1920 * 4, wl_shm::Format::Argb8888, &qh, ());
    wallpaper.attach(Some(&wallpaper_buf), 0, 0);
    wallpaper.damage_buffer(0, 0, 1920, 1080);
    wallpaper.commit();
    env.pump();

    let map = layer_map_for_output(&env._output);
    let background = layer_tier_entries(&map, &BACKGROUND_LAYER_TIERS);
    assert_eq!(
        background.len(),
        1,
        "the Background wallpaper must enter the frame element list (komawm/koma#31)"
    );
    let (bg_layer, bg_location) = &background[0];
    assert_eq!(bg_layer.namespace(), "awww-daemon");
    assert_eq!(bg_layer.layer(), Layer::Background);
    assert_eq!(*bg_location, Point::from((0, 0)));
    let bg_geometry = map
        .layer_geometry(bg_layer)
        .expect("buffer-backed layer must have a usable geometry");
    assert_eq!(bg_geometry.size, Size::from((1920, 1080)));
    // A Background surface must never be collected as a foreground layer (no double draw).
    assert!(
        layer_tier_entries(&map, &FOREGROUND_LAYER_TIERS).is_empty(),
        "Background tier must not leak into the Overlay/Top list"
    );
    drop(map);

    // The Top-tier bar (e.g. waybar) is collected by the other tier list, so the two tiers stay
    // disjoint and the wallpaper cannot displace it.
    let bar_surface = compositor.create_surface(&qh, ());
    let bar_layer = layer_shell.get_layer_surface(
        &bar_surface,
        Some(&wl_out),
        zwlr_layer_shell_v1::Layer::Top,
        "koma-bar".to_owned(),
        &qh,
        (),
    );
    bar_layer.set_size(1920, 24);
    bar_layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top);
    bar_layer.set_exclusive_zone(24);
    bar_surface.commit();
    env.pump();
    let bar_fd = create_portable_shm_fd(1920 * 24 * 4, None);
    let bar_pool = shm.create_pool(bar_fd.as_fd(), 1920 * 24 * 4, &qh, ());
    let bar_buf = bar_pool.create_buffer(0, 1920, 24, 1920 * 4, wl_shm::Format::Argb8888, &qh, ());
    bar_surface.attach(Some(&bar_buf), 0, 0);
    bar_surface.damage_buffer(0, 0, 1920, 24);
    bar_surface.commit();
    env.pump();

    let map = layer_map_for_output(&env._output);
    let foreground = layer_tier_entries(&map, &FOREGROUND_LAYER_TIERS);
    assert_eq!(
        foreground.len(),
        1,
        "the Top bar must stay in the foreground list"
    );
    assert_eq!(
        layer_tier_entries(&map, &BACKGROUND_LAYER_TIERS).len(),
        1,
        "the wallpaper must survive a Top-tier commit"
    );
    drop(map);

    // Shared front-to-back tier order: `Bottom` is emitted before `Background`, so it paints above
    // the wallpaper; both are appended after the workspace toplevels by every backend.
    assert_eq!(BACKGROUND_LAYER_TIERS, [Layer::Bottom, Layer::Background]);
    assert_eq!(FOREGROUND_LAYER_TIERS, [Layer::Overlay, Layer::Top]);
    // A missing cached geometry must not drop a buffer-backed surface: it renders at the origin.
    assert_eq!(layer_entry_location(None), Point::from((0, 0)));
    assert_eq!(
        layer_entry_location(Some(Rectangle::new(
            Point::from((8, 16)),
            Size::from((64, 32))
        ))),
        Point::from((8, 16))
    );

    wallpaper_buf.destroy();
    wallpaper_pool.destroy();
    bar_buf.destroy();
    bar_pool.destroy();
    wallpaper_layer.destroy();
    bar_layer.destroy();
}

mod kom_0024_compat_tests {
    use super::*;
    use koma_proto::WindowId;
    use smithay::{
        backend::input::{InputTime, KeyState},
        desktop::{PopupManager, layer_map_for_output},
        input::keyboard::Keycode,
        reexports::wayland_protocols::wp::{
            keyboard_shortcuts_inhibit::zv1::client::{
                zwp_keyboard_shortcuts_inhibit_manager_v1, zwp_keyboard_shortcuts_inhibitor_v1,
            },
            presentation_time::client::{wp_presentation, wp_presentation_feedback},
            single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1,
        },
        reexports::wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner},
    };
    use std::collections::BTreeMap;
    use wayland_client::{Connection, Dispatch, EventQueue, WEnum, delegate_noop};
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
    };

    #[derive(Default)]
    struct CompatHarness {
        sync_done: bool,
        globals: BTreeMap<String, (u32, u32)>,
        shm_formats: Vec<wl_shm::Format>,
        compositor: Option<wl_compositor::WlCompositor>,
        shm: Option<wl_shm::WlShm>,
        wm_base: Option<xdg_wm_base::XdgWmBase>,
        seats: Vec<wl_seat::WlSeat>,
        outputs: Vec<wl_output::WlOutput>,
        xdg_surface_configures: usize,
        popup_configures: Vec<(i32, i32, i32, i32)>,
        layer_configures: usize,
        presentation_presented: usize,
        presentation_discarded: usize,
        presentation_sync_outputs: usize,
        inhibitor_active: usize,
        inhibitor_inactive: usize,
        keyboard_keys: Vec<u32>,
        keyboard_key_events: Vec<(u32, wl_keyboard::KeyState)>,
        pointer_motions: usize,
        pointer_buttons: Vec<(u32, wl_pointer::ButtonState)>,
        last_button_serial: Option<u32>,
        pointer_axes: Vec<(wl_pointer::Axis, f64)>,
        pointer_axis_v120: Vec<(wl_pointer::Axis, i32)>,
    }

    impl CompatHarness {
        fn connect(
            display: &Display<KomaState>,
            restricted: bool,
        ) -> (Connection, EventQueue<Self>, Self, wl_registry::WlRegistry) {
            let (server_sock, client_sock) = UnixStream::pair().expect("UnixStream::pair");
            display
                .handle()
                .insert_client(
                    server_sock,
                    Arc::new(ClientState {
                        restricted,
                        ..Default::default()
                    }),
                )
                .expect("insert client");
            let conn = Connection::from_socket(client_sock).expect("Connection::from_socket");
            let queue = conn.new_event_queue::<Self>();
            let qh = queue.handle();
            let registry = conn.display().get_registry(&qh, ());
            (conn, queue, Self::default(), registry)
        }

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

    struct CompatEnv {
        event_loop: EventLoop<'static, KomaState>,
        display: Display<KomaState>,
        state: KomaState,
        output: Output,
        _output_global: smithay::reexports::wayland_server::backend::GlobalId,
        conn: Connection,
        queue: EventQueue<CompatHarness>,
        client: CompatHarness,
        registry: wl_registry::WlRegistry,
    }

    impl CompatEnv {
        fn new(restricted: bool) -> Self {
            let (mut event_loop, mut display, mut state) = make_headless_test_state();
            let output = test_output("DP-1");
            let output_global = output.create_global::<KomaState>(&state.display_handle);
            state.test_outputs.push(output.clone());
            let (conn, mut queue, mut client, registry) =
                CompatHarness::connect(&display, restricted);
            Self::pump_with(
                &conn,
                &mut queue,
                &mut client,
                &mut display,
                &mut event_loop,
                &mut state,
            );
            // Second pump receives initial events (e.g. `wl_shm.format`, `wl_seat.capabilities`)
            // emitted by globals bound during the initial registry enumeration above.
            Self::pump_with(
                &conn,
                &mut queue,
                &mut client,
                &mut display,
                &mut event_loop,
                &mut state,
            );
            Self {
                event_loop,
                display,
                state,
                output,
                _output_global: output_global,
                conn,
                queue,
                client,
                registry,
            }
        }

        fn pump_with(
            conn: &Connection,
            queue: &mut EventQueue<CompatHarness>,
            client: &mut CompatHarness,
            display: &mut Display<KomaState>,
            event_loop: &mut EventLoop<'static, KomaState>,
            state: &mut KomaState,
        ) {
            let qh = queue.handle();
            client.sync_done = false;
            let _cb = conn.display().sync(&qh, ());
            conn.flush().expect("flush client");
            for _ in 0..16 {
                display
                    .dispatch_clients(state)
                    .expect("dispatch server clients");
                event_loop
                    .dispatch(Some(Duration::ZERO), state)
                    .expect("dispatch calloop");
                display.flush_clients().expect("flush server clients");
                if let Some(guard) = queue.prepare_read() {
                    let _ = guard.read();
                }
                queue
                    .dispatch_pending(client)
                    .expect("dispatch client pending");
                conn.flush().expect("flush client post-dispatch");
                if client.sync_done {
                    break;
                }
            }
            assert!(client.sync_done, "CompatEnv pump did not complete sync");
        }

        fn pump(&mut self) {
            Self::pump_with(
                &self.conn,
                &mut self.queue,
                &mut self.client,
                &mut self.display,
                &mut self.event_loop,
                &mut self.state,
            );
        }

        fn ensure_globals(
            &mut self,
        ) -> (
            wl_compositor::WlCompositor,
            xdg_wm_base::XdgWmBase,
            wl_shm::WlShm,
        ) {
            let qh = self.queue.handle();
            let comp = self.client.compositor.clone().unwrap_or_else(|| {
                self.client.bind::<wl_compositor::WlCompositor>(
                    &self.registry,
                    &qh,
                    "wl_compositor",
                )
            });
            self.client.compositor = Some(comp.clone());
            let wm = self.client.wm_base.clone().unwrap_or_else(|| {
                self.client
                    .bind::<xdg_wm_base::XdgWmBase>(&self.registry, &qh, "xdg_wm_base")
            });
            self.client.wm_base = Some(wm.clone());
            let shm = self.client.shm.clone().unwrap_or_else(|| {
                self.client
                    .bind::<wl_shm::WlShm>(&self.registry, &qh, "wl_shm")
            });
            self.client.shm = Some(shm.clone());
            (comp, wm, shm)
        }

        fn spawn_window(
            &mut self,
            title: &str,
            app_id: &str,
            empty_input_region: bool,
        ) -> (
            wl_surface::WlSurface,
            xdg_surface::XdgSurface,
            xdg_toplevel::XdgToplevel,
            WindowId,
        ) {
            let qh = self.queue.handle();
            let (comp, wm, shm) = self.ensure_globals();
            let surface = comp.create_surface(&qh, ());
            let xdg_surface = wm.get_xdg_surface(&surface, &qh, ());
            let toplevel = xdg_surface.get_toplevel(&qh, ());
            toplevel.set_title(title.to_owned());
            toplevel.set_app_id(app_id.to_owned());
            surface.commit();
            self.pump();

            if empty_input_region {
                let empty_region = comp.create_region(&qh, ());
                surface.set_input_region(Some(&empty_region));
                empty_region.destroy();
            }

            let shm_fd = create_portable_shm_fd(64 * 64 * 4, None);
            let pool = shm.create_pool(shm_fd.as_fd(), 64 * 64 * 4, &qh, ());
            let buf = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &qh, ());
            surface.attach(Some(&buf), 0, 0);
            surface.commit();
            self.pump();

            buf.destroy();
            pool.destroy();
            let win_id = self
                .state
                .mapped_windows
                .last()
                .expect("mapped window")
                .win_id;
            (surface, xdg_surface, toplevel, win_id)
        }
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for CompatHarness {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &wayland_client::QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                match interface.as_str() {
                    "wl_output" => {
                        let out = registry.bind::<wl_output::WlOutput, (), Self>(
                            name,
                            version.min(4),
                            qh,
                            (),
                        );
                        state.outputs.push(out);
                    }
                    "wl_seat" => {
                        let seat = registry.bind::<wl_seat::WlSeat, (), Self>(
                            name,
                            version.min(9),
                            qh,
                            (),
                        );
                        let _ = seat.get_pointer(qh, ());
                        let _ = seat.get_keyboard(qh, ());
                        state.seats.push(seat);
                    }
                    "wl_shm" => {
                        let shm =
                            registry.bind::<wl_shm::WlShm, (), Self>(name, version.min(2), qh, ());
                        state.shm = Some(shm);
                    }
                    _ => {}
                }
                state.globals.insert(interface, (name, version));
            }
        }
    }

    impl Dispatch<wl_shm::WlShm, ()> for CompatHarness {
        fn event(
            state: &mut Self,
            _: &wl_shm::WlShm,
            event: wl_shm::Event,
            _: &(),
            _: &Connection,
            _: &wayland_client::QueueHandle<Self>,
        ) {
            if let wl_shm::Event::Format {
                format: WEnum::Value(fmt),
            } = event
            {
                state.shm_formats.push(fmt);
            }
        }
    }

    impl Dispatch<wl_callback::WlCallback, ()> for CompatHarness {
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

    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for CompatHarness {
        fn event(
            _: &mut Self,
            wm: &xdg_wm_base::XdgWmBase,
            event: xdg_wm_base::Event,
            _: &(),
            _: &Connection,
            _: &wayland_client::QueueHandle<Self>,
        ) {
            if let xdg_wm_base::Event::Ping { serial } = event {
                wm.pong(serial);
            }
        }
    }

    impl Dispatch<xdg_surface::XdgSurface, ()> for CompatHarness {
        fn event(
            state: &mut Self,
            xdg_surface: &xdg_surface::XdgSurface,
            event: xdg_surface::Event,
            _: &(),
            _: &Connection,
            _: &wayland_client::QueueHandle<Self>,
        ) {
            if let xdg_surface::Event::Configure { serial } = event {
                xdg_surface.ack_configure(serial);
                state.xdg_surface_configures += 1;
            }
        }
    }

    impl Dispatch<xdg_popup::XdgPopup, ()> for CompatHarness {
        fn event(
            state: &mut Self,
            _: &xdg_popup::XdgPopup,
            event: xdg_popup::Event,
            _: &(),
            _: &Connection,
            _: &wayland_client::QueueHandle<Self>,
        ) {
            if let xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } = event
            {
                state.popup_configures.push((x, y, width, height));
            }
        }
    }

    impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for CompatHarness {
        fn event(
            state: &mut Self,
            surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
            event: zwlr_layer_surface_v1::Event,
            _: &(),
            _: &Connection,
            _: &wayland_client::QueueHandle<Self>,
        ) {
            if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
                state.layer_configures += 1;
                surface.ack_configure(serial);
            }
        }
    }

    impl Dispatch<wp_presentation::WpPresentation, ()> for CompatHarness {
        fn event(
            _: &mut Self,
            _: &wp_presentation::WpPresentation,
            _: wp_presentation::Event,
            _: &(),
            _: &Connection,
            _: &wayland_client::QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for CompatHarness {
        fn event(
            state: &mut Self,
            _: &wp_presentation_feedback::WpPresentationFeedback,
            event: wp_presentation_feedback::Event,
            _: &(),
            _: &Connection,
            _: &wayland_client::QueueHandle<Self>,
        ) {
            match event {
                wp_presentation_feedback::Event::SyncOutput { .. } => {
                    state.presentation_sync_outputs += 1;
                }
                wp_presentation_feedback::Event::Presented { .. } => {
                    state.presentation_presented += 1;
                }
                wp_presentation_feedback::Event::Discarded => {
                    state.presentation_discarded += 1;
                }
                _ => {}
            }
        }
    }

    impl Dispatch<zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1, ()>
        for CompatHarness
    {
        fn event(
            state: &mut Self,
            _: &zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
            event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
            _: &(),
            _: &Connection,
            _: &wayland_client::QueueHandle<Self>,
        ) {
            match event {
                zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => {
                    state.inhibitor_active += 1;
                }
                zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                    state.inhibitor_inactive += 1;
                }
                _ => {}
            }
        }
    }

    impl Dispatch<wl_keyboard::WlKeyboard, ()> for CompatHarness {
        fn event(
            state: &mut Self,
            _: &wl_keyboard::WlKeyboard,
            event: wl_keyboard::Event,
            _: &(),
            _: &Connection,
            _: &wayland_client::QueueHandle<Self>,
        ) {
            if let wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } = event
            {
                state.keyboard_keys.push(key);
                if let WEnum::Value(ks) = key_state {
                    state.keyboard_key_events.push((key, ks));
                }
            }
        }
    }

    impl Dispatch<wl_pointer::WlPointer, ()> for CompatHarness {
        fn event(
            state: &mut Self,
            _: &wl_pointer::WlPointer,
            event: wl_pointer::Event,
            _: &(),
            _: &Connection,
            _: &wayland_client::QueueHandle<Self>,
        ) {
            match event {
                wl_pointer::Event::Motion { .. } => state.pointer_motions += 1,
                wl_pointer::Event::Button {
                    button,
                    state: WEnum::Value(btn_state),
                    serial,
                    ..
                } => {
                    state.last_button_serial = Some(serial);
                    state.pointer_buttons.push((button, btn_state));
                }
                wl_pointer::Event::Axis {
                    axis: WEnum::Value(axis),
                    value,
                    ..
                } => state.pointer_axes.push((axis, value)),
                wl_pointer::Event::AxisValue120 {
                    axis: WEnum::Value(axis),
                    value120,
                } => state.pointer_axis_v120.push((axis, value120)),
                _ => {}
            }
        }
    }

    delegate_noop!(CompatHarness: ignore wl_compositor::WlCompositor);
    delegate_noop!(CompatHarness: ignore wl_surface::WlSurface);
    delegate_noop!(CompatHarness: ignore wl_region::WlRegion);
    delegate_noop!(CompatHarness: ignore wl_shm_pool::WlShmPool);
    delegate_noop!(CompatHarness: ignore wl_buffer::WlBuffer);
    delegate_noop!(CompatHarness: ignore wl_output::WlOutput);
    delegate_noop!(CompatHarness: ignore wl_seat::WlSeat);
    delegate_noop!(CompatHarness: ignore xdg_toplevel::XdgToplevel);
    delegate_noop!(CompatHarness: ignore xdg_positioner::XdgPositioner);
    delegate_noop!(CompatHarness: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
    delegate_noop!(CompatHarness: ignore wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1);
    delegate_noop!(CompatHarness: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
    delegate_noop!(CompatHarness: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
    delegate_noop!(CompatHarness: ignore zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1);

    #[test]
    fn shm_abgr_formats_single_pixel_buffer_and_presentation_feedback() {
        let mut env = CompatEnv::new(false);
        assert!(env.client.shm_formats.contains(&wl_shm::Format::Argb8888));
        assert!(env.client.shm_formats.contains(&wl_shm::Format::Xrgb8888));
        assert!(env.client.shm_formats.contains(&wl_shm::Format::Abgr8888));
        assert!(env.client.shm_formats.contains(&wl_shm::Format::Xbgr8888));

        let qh = env.queue.handle();
        let (comp, wm, _shm) = env.ensure_globals();
        let spb = env
            .client
            .bind::<wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1>(
                &env.registry,
                &qh,
                "wp_single_pixel_buffer_manager_v1",
            );
        let pres = env.client.bind::<wp_presentation::WpPresentation>(
            &env.registry,
            &qh,
            "wp_presentation",
        );

        let surf = comp.create_surface(&qh, ());
        let xdg_surf = wm.get_xdg_surface(&surf, &qh, ());
        let toplevel = xdg_surf.get_toplevel(&qh, ());
        toplevel.set_app_id("org.quickshell.test".to_owned());
        surf.commit();
        env.pump();

        let px_buf = spb.create_u32_rgba_buffer(0xffff_ffff, 0, 0, 0xffff_ffff, &qh, ());
        let _fb = pres.feedback(&surf, &qh, ());
        surf.attach(Some(&px_buf), 0, 0);
        surf.commit();
        env.pump();

        let out = env.output.clone();
        crate::backend_winit::dispatch_output_frame_callbacks_filtered(&env.state, &out, None);
        env.pump();

        assert!(env.client.presentation_presented >= 1);
        assert!(env.client.presentation_sync_outputs >= 1);
        assert_eq!(env.client.presentation_discarded, 0);

        px_buf.destroy();
    }

    #[test]
    fn layer_shell_get_popup_and_xdg_popup_unconstrain() {
        let mut env = CompatEnv::new(false);
        let qh = env.queue.handle();
        let (comp, wm, shm) = env.ensure_globals();
        let layer_shell = env.client.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1>(
            &env.registry,
            &qh,
            "zwlr_layer_shell_v1",
        );

        let bar_surf = comp.create_surface(&qh, ());
        let layer_surf = layer_shell.get_layer_surface(
            &bar_surf,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "waybar".to_owned(),
            &qh,
            (),
        );
        layer_surf.set_size(1920, 32);
        layer_surf.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer_surf.set_exclusive_zone(32);
        bar_surf.commit();
        env.pump();
        assert!(env.client.layer_configures >= 1);

        let bar_fd = create_portable_shm_fd(1920 * 32 * 4, None);
        let bar_pool = shm.create_pool(bar_fd.as_fd(), 1920 * 32 * 4, &qh, ());
        let bar_buf =
            bar_pool.create_buffer(0, 1920, 32, 1920 * 4, wl_shm::Format::Argb8888, &qh, ());
        bar_surf.attach(Some(&bar_buf), 0, 0);
        bar_surf.commit();
        env.pump();

        // Layer-shell surfaces keep their own output-local origin: no window-geometry inset.
        let (bar_hit_surface, bar_origin) = env
            .state
            .surface_under(Point::from((100.0, 10.0)))
            .expect("top bar must be hit-testable");
        let mapped_bar = {
            let map = layer_map_for_output(&env.output);
            map.layers()
                .next()
                .expect("mapped layer surface")
                .wl_surface()
                .clone()
        };
        assert_eq!(bar_hit_surface, mapped_bar);
        assert_eq!(
            bar_origin,
            Point::from((0.0, 0.0)),
            "layer-shell origin must stay output-local"
        );

        // Popup anchored near the far right edge so unconstrained positioner flips/slides back on-screen.
        let pos = wm.create_positioner(&qh, ());
        pos.set_size(200, 120);
        pos.set_anchor_rect(1850, 0, 40, 32);
        pos.set_anchor(xdg_positioner::Anchor::BottomRight);
        pos.set_gravity(xdg_positioner::Gravity::BottomRight);
        pos.set_constraint_adjustment(
            xdg_positioner::ConstraintAdjustment::FlipX
                | xdg_positioner::ConstraintAdjustment::SlideX
                | xdg_positioner::ConstraintAdjustment::FlipY
                | xdg_positioner::ConstraintAdjustment::SlideY,
        );

        let popup_surf = comp.create_surface(&qh, ());
        let popup_xdg = wm.get_xdg_surface(&popup_surf, &qh, ());
        let popup = popup_xdg.get_popup(None, &pos, &qh, ());
        layer_surf.get_popup(&popup);
        popup_surf.commit();
        env.pump();

        assert_eq!(env.client.popup_configures.len(), 1);
        let (px, py, pw, ph) = env.client.popup_configures[0];
        assert!(
            px + pw <= 1920,
            "popup should be unconstrained inside output width: x={px}, w={pw}"
        );
        assert!(
            py + ph <= 1080,
            "popup should stay within output height: y={py}, h={ph}"
        );

        let pop_fd = create_portable_shm_fd(200 * 120 * 4, None);
        let pop_pool = shm.create_pool(pop_fd.as_fd(), 200 * 120 * 4, &qh, ());
        let pop_buf =
            pop_pool.create_buffer(0, 200, 120, 200 * 4, wl_shm::Format::Argb8888, &qh, ());
        popup_surf.attach(Some(&pop_buf), 0, 0);
        popup_surf.commit();
        env.pump();

        let map = layer_map_for_output(&env.output);
        let srv_layer = map
            .layers()
            .next()
            .expect("mapped layer surface")
            .wl_surface()
            .clone();
        drop(map);
        assert!(
            PopupManager::popups_for_surface(&srv_layer)
                .next()
                .is_some(),
            "layer surface must track child xdg_popup"
        );
        let hit = env
            .state
            .surface_under(Point::from((f64::from(px + 20), f64::from(py + 20))));
        assert!(
            hit.is_some(),
            "hit-test must hit mapped layer popup surface"
        );

        pop_buf.destroy();
        pop_pool.destroy();
        bar_buf.destroy();
        bar_pool.destroy();
    }

    #[test]
    fn issue_109_popup_grab_teardown_and_pointer_motion_never_deadlocks_and_layer_none_skips_keyboard_grab()
     {
        use crate::pointer_grabs::BTN_RIGHT;
        use smithay::{backend::input::ButtonState, desktop::PopupPointerGrab};

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let mut env = CompatEnv::new(false);
            let qh = env.queue.handle();
            let (comp, wm, shm) = env.ensure_globals();
            let layer_shell = env.client.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1>(
                &env.registry,
                &qh,
                "zwlr_layer_shell_v1",
            );
            let seat = env.client.seats.first().expect("wl_seat").clone();

            // 1. Layer-shell surface with KeyboardInteractivity::None (e.g. Waybar)
            let bar_surf = comp.create_surface(&qh, ());
            let layer_surf = layer_shell.get_layer_surface(
                &bar_surf,
                None,
                zwlr_layer_shell_v1::Layer::Top,
                "waybar".to_owned(),
                &qh,
                (),
            );
            layer_surf.set_size(1920, 32);
            layer_surf.set_anchor(
                zwlr_layer_surface_v1::Anchor::Top
                    | zwlr_layer_surface_v1::Anchor::Left
                    | zwlr_layer_surface_v1::Anchor::Right,
            );
            layer_surf
                .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
            bar_surf.commit();
            env.pump();

            let bar_fd = create_portable_shm_fd(1920 * 32 * 4, None);
            let bar_pool = shm.create_pool(bar_fd.as_fd(), 1920 * 32 * 4, &qh, ());
            let bar_buf =
                bar_pool.create_buffer(0, 1920, 32, 1920 * 4, wl_shm::Format::Argb8888, &qh, ());
            bar_surf.attach(Some(&bar_buf), 0, 0);
            bar_surf.commit();
            env.pump();

            env.client.last_button_serial = None;
            env.state.handle_pointer_absolute_motion(
                Point::from((1350.0, 15.0)),
                InputTime::from_millis(10),
            );
            crate::keybind_dispatcher::process_pointer_button_event(
                &mut env.state,
                BTN_RIGHT,
                ButtonState::Pressed,
                InputTime::from_millis(20),
                false,
            );
            env.pump();
            let bar_serial = env.client.last_button_serial.expect("bar click serial");

            let pos = wm.create_positioner(&qh, ());
            pos.set_size(200, 120);
            pos.set_anchor_rect(1300, 0, 40, 32);
            let layer_pop_surf = comp.create_surface(&qh, ());
            let layer_pop_xdg = wm.get_xdg_surface(&layer_pop_surf, &qh, ());
            let layer_pop = layer_pop_xdg.get_popup(None, &pos, &qh, ());
            layer_surf.get_popup(&layer_pop);
            layer_pop.grab(&seat, bar_serial);
            layer_pop_surf.commit();
            env.pump();

            assert_eq!(
                env.state
                    .seat
                    .get_pointer()
                    .unwrap()
                    .with_grab(|_, grab| grab.is::<PopupPointerGrab<KomaState>>()),
                Some(true),
                "PopupPointerGrab must be active on layer surface popup"
            );
            assert!(
                !env.state.seat.get_keyboard().unwrap().is_grabbed(),
                "KeyboardInteractivity::None layer surface popup must not steal keyboard grab"
            );

            layer_pop.destroy();
            layer_pop_xdg.destroy();
            layer_pop_surf.destroy();
            env.pump();
            assert_eq!(
                env.state
                    .seat
                    .get_pointer()
                    .unwrap()
                    .with_grab(|_, grab| grab.is::<PopupPointerGrab<KomaState>>()),
                Some(true),
                "PopupPointerGrab remains installed until pointer motion observes ended popup"
            );
            env.state.handle_pointer_absolute_motion(
                Point::from((1325.0, 15.0)),
                InputTime::from_millis(30),
            );
            assert!(
                !env.state.seat.get_pointer().unwrap().is_grabbed(),
                "pointer motion after layer popup teardown must clear PopupPointerGrab before button release"
            );
            crate::keybind_dispatcher::process_pointer_button_event(
                &mut env.state,
                BTN_RIGHT,
                ButtonState::Released,
                InputTime::from_millis(40),
                false,
            );
            env.pump();
            assert!(!env.state.seat.get_pointer().unwrap().is_grabbed());

            // 2. Toplevel xdg_popup WITH keyboard+pointer grab: destroying popup then moving
            //    pointer triggers `PopupPointerGrab::motion` -> `unset_keyboard_grab` -> `focus_changed`.
            let (_win_surf, win_xdg, _win_tl, win_id) =
                env.spawn_window("popup-parent", "popup.parent", false);
            let parent_srv_surf = mapped_surface(&env.state, win_id);
            let win_rect = env
                .state
                .mapped_windows
                .iter()
                .find(|m| m.win_id == win_id)
                .expect("mapped win")
                .rect;
            env.client.last_button_serial = None;
            env.state.handle_pointer_absolute_motion(
                Point::from((f64::from(win_rect.x) + 20.0, f64::from(win_rect.y) + 20.0)),
                InputTime::from_millis(50),
            );
            crate::keybind_dispatcher::process_pointer_button_event(
                &mut env.state,
                BTN_RIGHT,
                ButtonState::Pressed,
                InputTime::from_millis(60),
                false,
            );
            env.pump();
            let win_serial = env
                .client
                .last_button_serial
                .expect("toplevel click serial");

            let win_pop_pos = wm.create_positioner(&qh, ());
            win_pop_pos.set_size(120, 80);
            win_pop_pos.set_anchor_rect(10, 10, 20, 20);
            let win_pop_surf = comp.create_surface(&qh, ());
            let win_pop_xdg = wm.get_xdg_surface(&win_pop_surf, &qh, ());
            let win_pop = win_pop_xdg.get_popup(Some(&win_xdg), &win_pop_pos, &qh, ());
            win_pop.grab(&seat, win_serial);
            win_pop_surf.commit();
            env.pump();
            assert_eq!(
                env.state
                    .seat
                    .get_pointer()
                    .unwrap()
                    .with_grab(|_, grab| grab.is::<PopupPointerGrab<KomaState>>()),
                Some(true),
                "PopupPointerGrab must be active on toplevel popup"
            );
            assert!(env.state.seat.get_keyboard().unwrap().is_grabbed());
            let popup_kb_focus = env.state.seat.get_keyboard().unwrap().current_focus();
            assert!(
                popup_kb_focus.is_some() && popup_kb_focus.as_ref() != Some(&parent_srv_surf),
                "keyboard focus must move onto the popup while grabbed (got {popup_kb_focus:?})"
            );

            win_pop.destroy();
            win_pop_xdg.destroy();
            win_pop_surf.destroy();
            env.pump();
            assert_eq!(
                env.state
                    .seat
                    .get_pointer()
                    .unwrap()
                    .with_grab(|_, grab| grab.is::<PopupPointerGrab<KomaState>>()),
                Some(true),
                "PopupPointerGrab remains active until motion handles ended popup"
            );
            assert!(env.state.seat.get_keyboard().unwrap().is_grabbed());

            env.state.handle_pointer_absolute_motion(
                Point::from((f64::from(win_rect.x) + 25.0, f64::from(win_rect.y) + 25.0)),
                InputTime::from_millis(70),
            );
            assert!(!env.state.seat.get_pointer().unwrap().is_grabbed());
            assert!(!env.state.seat.get_keyboard().unwrap().is_grabbed());
            assert!(!env.state.pending_pointer_constraint_break);
            assert_eq!(
                env.state.seat.get_keyboard().unwrap().current_focus(),
                Some(parent_srv_surf),
                "ungrabbing popup keyboard grab via focus_changed must restore focus to parent window"
            );

            bar_buf.destroy();
            bar_pool.destroy();
            let _ = tx.send(());
        });

        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(()) => handle.join().expect("test thread panicked"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                handle.join().expect("test thread panicked");
                panic!("test thread disconnected without signaling completion");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("popup grab teardown and pointer motion must not deadlock the event loop");
            }
        }
    }

    #[test]
    fn virtual_pointer_motion_click_and_security_gates() {
        // Restricted client must not see `zwlr_virtual_pointer_manager_v1`.
        let restricted_env = CompatEnv::new(true);
        assert!(
            !restricted_env
                .client
                .globals
                .contains_key("zwlr_virtual_pointer_manager_v1")
        );

        let mut env = CompatEnv::new(false);
        let (_surf, _xs, _tl, _) = env.spawn_window("RustDesk", "com.rustdesk.RustDesk", false);
        let qh = env.queue.handle();
        let vptr_mgr = env
            .client
            .bind::<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>(
                &env.registry,
                &qh,
                "zwlr_virtual_pointer_manager_v1",
            );
        let vptr = vptr_mgr.create_virtual_pointer(None, &qh, ());
        vptr.motion_absolute(100, 500, 500, 1000, 1000);
        vptr.frame();
        env.pump();
        assert!((env.state.pointer_location.x - 960.0).abs() <= 1.0);
        assert!((env.state.pointer_location.y - 540.0).abs() <= 1.0);

        // Move inside the 64x64 committed RustDesk surface so `wl_pointer` enters and forwards motion + click.
        vptr.motion_absolute(101, 20, 20, 1000, 1000);
        vptr.frame();
        vptr.motion_absolute(102, 25, 25, 1000, 1000);
        vptr.frame();
        vptr.button(103, 0x110, wl_pointer::ButtonState::Pressed);
        vptr.frame();
        vptr.button(104, 0x110, wl_pointer::ButtonState::Released);
        vptr.frame();
        env.pump();

        assert!(env.client.pointer_motions >= 1);
        assert!(
            env.client
                .pointer_buttons
                .contains(&(0x110, wl_pointer::ButtonState::Pressed))
        );

        // Fail-closed during locked/abandoned session state.
        env.state.session_lock.phase = crate::state::SessionLockPhase::Abandoned;
        let locked_pos = env.state.pointer_location;
        vptr.motion(200, 35.0, 25.0);
        vptr.frame();
        env.pump();
        assert_eq!(env.state.pointer_location, locked_pos);
    }

    #[test]
    fn vseat_click_and_drag_path_deliver_press_motion_and_release_through_smithay_click_grab() {
        let mut env = CompatEnv::new(false);
        let (_surf, _xs, _tl, _win_id) = env.spawn_window("paint", "paint-test", false);
        let ws_id = env.state.active_ws_id.clone();
        let expected_epoch = env.state.workspaces[&ws_id].lease.epoch();
        let human_pos = env.state.pointer_location;

        crate::macro_runtime::dispatch_single_host_action(
            &mut env.state,
            koma_lua::phodopus_bridge::QueuedHostAction::ClickPointer {
                ws_id: ws_id.clone(),
                expected_epoch,
                target: koma_model::AnchorPoint::AnchorOffset {
                    anchor: koma_model::Anchor9::TopLeft,
                    dx: 12.0,
                    dy: 12.0,
                },
                opts: koma_lua::schema::LuaClickOptsInput::default(),
            },
        );
        env.pump();

        assert_eq!(
            env.client.pointer_buttons,
            vec![
                (272, wl_pointer::ButtonState::Pressed),
                (272, wl_pointer::ButtonState::Released),
            ],
            "vseat click must deliver both press and release through Smithay ClickGrab"
        );
        assert!(
            !env.state.seat.get_pointer().expect("pointer").is_grabbed(),
            "ClickGrab must clear once click release is delivered"
        );
        assert_eq!(env.state.pointer_location, human_pos);

        env.client.pointer_buttons.clear();
        env.client.pointer_motions = 0;

        let waypoints = vec![
            koma_model::AnchorPoint::AnchorOffset {
                anchor: koma_model::Anchor9::TopLeft,
                dx: 16.0,
                dy: 16.0,
            },
            koma_model::AnchorPoint::AnchorOffset {
                anchor: koma_model::Anchor9::TopLeft,
                dx: 32.0,
                dy: 28.0,
            },
            koma_model::AnchorPoint::AnchorOffset {
                anchor: koma_model::Anchor9::TopLeft,
                dx: 48.0,
                dy: 44.0,
            },
        ];
        crate::macro_runtime::queue_pointer_path_trajectory(
            &mut env.state,
            ws_id,
            expected_epoch,
            waypoints,
            crate::macro_runtime::PointerPathLaunchOpts {
                spline: koma_model::PathSplineKind::CatmullRom,
                kinematics: koma_lua::schema::LuaKinematicsInput {
                    duration_ms: Some(20),
                    ..Default::default()
                },
                hold_start_ms: 0,
                hold_end_ms: 0,
                press_button_at_start: Some("left".to_owned()),
                release_button_on_finish: Some("left".to_owned()),
                ..Default::default()
            },
        )
        .expect("queue drag path");
        env.pump();

        assert_eq!(
            env.client.pointer_buttons,
            vec![(272, wl_pointer::ButtonState::Pressed)]
        );
        assert!(
            env.state.seat.get_pointer().expect("pointer").is_grabbed(),
            "Smithay ClickGrab should be active while left drag button is held"
        );

        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(5));
            crate::macro_runtime::drive_macro_tick(&mut env.state, &[]);
            env.pump();
            if env.state.pointer_animations.is_empty() {
                break;
            }
        }

        assert!(
            env.state.pointer_animations.is_empty(),
            "drag path trajectory should finish within test budget"
        );
        assert!(
            env.client.pointer_motions >= 2,
            "drag path must deliver motion events while left button grab is active; got {}",
            env.client.pointer_motions
        );
        assert_eq!(
            env.client.pointer_buttons,
            vec![
                (272, wl_pointer::ButtonState::Pressed),
                (272, wl_pointer::ButtonState::Released),
            ],
            "drag path finish must deliver release button through Smithay ClickGrab"
        );
        assert!(
            !env.state.seat.get_pointer().expect("pointer").is_grabbed(),
            "ClickGrab must clear once drag path release finishes"
        );
        assert_eq!(
            env.state.pointer_location, human_pos,
            "virtual cursor drag must keep hardware cursor parked"
        );
    }

    #[test]
    fn keyboard_shortcuts_inhibit_forwards_wm_binds_but_preserves_mod_g_preempt() {
        let mut env = CompatEnv::new(false);
        let (surf, _xs, _tl, win_id) = env.spawn_window("Remote", "com.rustdesk.RustDesk", false);
        env.state.reconcile_keyboard_focus();
        let qh = env.queue.handle();
        let seat = env.client.seats[0].clone();

        // Helper to press Mod(Alt)+key on nested/non-TTY test seat.
        let send_mod_chord = |state: &mut KomaState, evdev_key: u32, t: u32| {
            let mod_code = Keycode::from((56 + 8) as u32);
            let key_code = Keycode::from(evdev_key + 8);
            crate::keybind_dispatcher::process_keyboard_key_event(
                state,
                mod_code,
                KeyState::Pressed,
                InputTime::from_millis(t),
                false,
            );
            crate::keybind_dispatcher::process_keyboard_key_event(
                state,
                key_code,
                KeyState::Pressed,
                InputTime::from_millis(t + 1),
                false,
            );
            crate::keybind_dispatcher::process_keyboard_key_event(
                state,
                key_code,
                KeyState::Released,
                InputTime::from_millis(t + 2),
                false,
            );
            crate::keybind_dispatcher::process_keyboard_key_event(
                state,
                mod_code,
                KeyState::Released,
                InputTime::from_millis(t + 3),
                false,
            );
        };

        // Sanity check: before inhibition, Mod+V toggles float on focused window.
        send_mod_chord(&mut env.state, 47, 10);
        assert!(
            env.state
                .mapped_windows
                .iter()
                .find(|m| m.win_id == win_id)
                .unwrap()
                .is_floating
        );
        send_mod_chord(&mut env.state, 47, 20);
        assert!(
            !env.state
                .mapped_windows
                .iter()
                .find(|m| m.win_id == win_id)
                .unwrap()
                .is_floating
        );

        // Activate `zwp_keyboard_shortcuts_inhibitor_v1`.
        let inh_mgr = env
            .client
            .bind::<zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1>(
                &env.registry,
                &qh,
                "zwp_keyboard_shortcuts_inhibit_manager_v1",
            );
        let _inh = inh_mgr.inhibit_shortcuts(&surf, &seat, &qh, ());
        env.pump();
        assert_eq!(env.client.inhibitor_active, 1);
        assert!(env.state.are_shortcuts_inhibited_for_focused_surface());

        // Normal WM chord Mod+V (47) is forwarded to the client rather than toggling float.
        let before_keys = env.client.keyboard_keys.len();
        send_mod_chord(&mut env.state, 47, 30);
        env.pump();
        assert!(
            !env.state
                .mapped_windows
                .iter()
                .find(|m| m.win_id == win_id)
                .unwrap()
                .is_floating
        );
        assert!(env.client.keyboard_keys.len() > before_keys);

        // Ring-0 `Mod+G` (34) remains un-inhibitable and cancels inhibitor via preempt takeover.
        send_mod_chord(&mut env.state, 34, 40);
        env.pump();
        let active_ws = &env.state.workspaces[&env.state.active_ws_id];
        assert!(active_ws.lease.is_human_preempted());
        assert!(!env.state.are_shortcuts_inhibited_for_focused_surface());
        assert!(env.client.inhibitor_inactive >= 1);
    }

    #[test]
    fn waydroid_defaults_blur_clip_and_empty_input_region_click_through() {
        let mut env = CompatEnv::new(false);
        let (_s1, _xs1, _tl1, win_base) =
            env.spawn_window("Terminal", "org.example.Terminal", false);
        let (_s2, _xs2, _tl2, win_container) = env.spawn_window("Waydroid", "Waydroid", true);
        let (_s3, _xs3, _tl3, win_android_app) =
            env.spawn_window("Settings", "waydroid.com.android.settings", false);

        // Make click-through Waydroid container float over the base window so hit-testing would
        // normally encounter it first unless empty `input_region` click-through passes through.
        let ws_id = env.state.active_ws_id.clone();
        let now_ms = env.state.elapsed_ms();
        if let Some(ws) = env.state.workspaces.get_mut(&ws_id) {
            ws.toggle_floating(win_container, None, None, now_ms);
            ws.floating.set_rect(
                win_container,
                koma_model::LogicalRect::new(0, 0, 1920, 1080),
            );
        }
        env.state.sync_workspace_geometries(&ws_id);
        let base_rect = env
            .state
            .workspaces
            .get(&ws_id)
            .expect("ws")
            .compute_visible_geometries_at(env.state.elapsed_ms())
            .into_iter()
            .find(|(id, _, _)| *id == win_base)
            .map(|(_, rect, _)| rect)
            .expect("base rect");
        let probe = Point::from((f64::from(base_rect.x + 24), f64::from(base_rect.y + 24)));
        let hit = env
            .state
            .hit_test_visible_surface(probe)
            .expect("click-through should hit base window");
        assert_eq!(hit.0, win_base);

        let items = env.state.collect_workspace_scene_items(&ws_id, false);
        let container_item = items
            .iter()
            .find(|it| {
                it.window
                    .toplevel()
                    .and_then(|t| {
                        env.state.mapped_windows.iter().find(|m| {
                            m.win_id == win_container
                                && m.window
                                    .toplevel()
                                    .is_some_and(|mt| mt.wl_surface() == t.wl_surface())
                        })
                    })
                    .is_some()
            })
            .expect("container scene item");
        assert!(!container_item.wants_blur);
        assert!(!container_item.clip_to_geometry);
        assert!(!container_item.border.enabled);
        assert!(container_item.no_shadow);

        let android_item = items
            .iter()
            .find(|it| {
                it.window
                    .toplevel()
                    .and_then(|t| {
                        env.state.mapped_windows.iter().find(|m| {
                            m.win_id == win_android_app
                                && m.window
                                    .toplevel()
                                    .is_some_and(|mt| mt.wl_surface() == t.wl_surface())
                        })
                    })
                    .is_some()
            })
            .expect("android scene item");
        assert!(
            !android_item.wants_blur,
            "Waydroid defaults to no_blur = true"
        );
        assert!(android_item.clip_to_geometry);
        assert!(android_item.border.enabled);
    }

    fn insert_test_admin_ipc_client(state: &mut KomaState, conn_id: u64) -> koma_proto::SessionId {
        use std::collections::{BTreeSet, HashSet};
        let (stream, _peer) = UnixStream::pair().expect("unix stream pair");
        let session_id = koma_proto::SessionId::new(format!("test-admin-{conn_id}"));
        state.ipc_clients.insert(
            conn_id,
            crate::ipc_server::IpcClientConn {
                conn_id,
                stream,
                channel: crate::ipc_server::SocketChannel::Admin,
                auth: crate::ipc_server::PeerAuthState::AdminVerified {
                    uid: 1000,
                    gid: 1000,
                    session_id: session_id.clone(),
                },
                reg_token: None,
                inbox: koma_proto::scm_unix::FrameInbox::default(),
                outbox: koma_proto::scm_unix::FrameOutbox::default(),
                event_ring: koma_proto::EventRingBuffer::default(),
                subscribed_workspaces: BTreeSet::new(),
                subscribed_topics: HashSet::new(),
                armed_writable: false,
            },
        );
        session_id
    }

    #[test]
    fn vseat_relative_move_honors_locked_pointer_constraint_and_delivers_unaccel_deltas() {
        let mut env = ClientEnv::new();
        let qh = env.queue.handle();
        let constraints = env
            .client
            .bind::<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>(
                &env.registry,
                &qh,
                "zwp_pointer_constraints_v1",
            );
        let rel_mgr = env
            .client
            .bind::<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>(
                &env.registry,
                &qh,
                "zwp_relative_pointer_manager_v1",
            );
        let (surface, _xs, _tl) = env.spawn_window("FPSGame", "org.koma.fps");
        let pointer = env.client.pointers[0].clone();
        let _rel_ptr = rel_mgr.get_relative_pointer(&pointer, &qh, ());
        env.pump();

        let ws_id = env.state.active_ws_id.clone();
        let conn_id = 701_u64;
        let _sess = insert_test_admin_ipc_client(&mut env.state, conn_id);
        let mut epoch = env.state.workspaces[&ws_id].lease.epoch().get();
        let human_pos = Point::from((812.0, 415.0));
        env.state.pointer_location = human_pos;

        // Place virtual cursor inside the 64x64 surface and focus it.
        let (mv_res, _) = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 1,
                method: "vseat.pointer_move".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "expected_epoch": epoch,
                    "target": [24.0, 24.0],
                    "kinematics": { "profile": "instant" },
                }),
            },
            Vec::new(),
        )
        .expect("vseat.pointer_move succeeds");
        epoch = mv_res["epoch"].as_u64().expect("epoch");
        env.pump();

        let locked = constraints.lock_pointer(
            &surface,
            &pointer,
            None,
            zwp_pointer_constraints_v1::Lifetime::Persistent,
            &qh,
            (),
        );
        env.pump();

        // Trigger constraint activation at virtual cursor location via a tiny relative move.
        let _ = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 2,
                method: "vseat.relative_move".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "expected_epoch": epoch,
                    "dx": 2.0,
                    "dy": 2.0,
                }),
            },
            Vec::new(),
        )
        .expect("relative_move activates lock");
        env.pump();
        assert!(
            env.client.locked_events.contains(&"locked"),
            "pointer lock must activate under vseat cursor"
        );

        let locked_ws_pos = env.state.agent_cursors[&ws_id].ws_local_pos;
        env.client.relative_motions.clear();

        let (rel_res, _) = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 3,
                method: "vseat.relative_move".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "expected_epoch": epoch,
                    "dx": 24.5,
                    "dy": -13.25,
                    "dx_unaccel": 20.0,
                    "dy_unaccel": -10.0,
                }),
            },
            Vec::new(),
        )
        .expect("locked relative_move succeeds");
        env.pump();

        assert_eq!(
            env.state.agent_cursors[&ws_id].ws_local_pos, locked_ws_pos,
            "Locked pointer constraint must pin virtual cursor position"
        );
        assert_eq!(
            rel_res["disposition"],
            serde_json::to_value(koma_proto::DeliverySemantics::delivered_verified()).unwrap()
        );
        assert!(
            env.client.relative_motions.iter().any(|&(dx, dy, ux, uy)| {
                (dx - 24.5).abs() < 1e-3
                    && (dy + 13.25).abs() < 1e-3
                    && (ux - 20.0).abs() < 1e-3
                    && (uy + 10.0).abs() < 1e-3
            }),
            "relative_motions must record raw and unaccelerated deltas, got {:?}",
            env.client.relative_motions
        );
        assert_eq!(env.state.pointer_location, human_pos);

        // Unlock and verify subsequent relative_move advances cursor freely.
        locked.destroy();
        env.pump();
        let _ = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 4,
                method: "vseat.relative_move".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "expected_epoch": epoch,
                    "dx": 10.0,
                    "dy": 5.0,
                }),
            },
            Vec::new(),
        )
        .expect("unlocked relative_move succeeds");
        let unlocked_ws_pos = env.state.agent_cursors[&ws_id].ws_local_pos;
        assert!((unlocked_ws_pos.x - (locked_ws_pos.x + 10.0)).abs() < 1e-3);
        assert!((unlocked_ws_pos.y - (locked_ws_pos.y + 5.0)).abs() < 1e-3);
    }

    #[test]
    fn vseat_wheel_chord_scoping_and_disconnect_cleanup_release_keys_and_buttons() {
        let mut env = CompatEnv::new(false);
        let (_surf, _xs, _tl, _win_id) = env.spawn_window("Canvas", "org.koma.canvas", false);
        let ws_id = env.state.active_ws_id.clone();
        let conn_id = 702_u64;
        let _sess = insert_test_admin_ipc_client(&mut env.state, conn_id);
        let mut epoch = env.state.workspaces[&ws_id].lease.epoch().get();

        // 1. Wheel scroll with temporary modifier chord ["Ctrl", "Shift"]
        let (wheel_res, _) = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 1,
                method: "vseat.wheel".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "expected_epoch": epoch,
                    "target": [24.0, 24.0],
                    "dy": 30.0,
                    "discrete_y": 2,
                    "modifiers": ["Ctrl", "Shift"],
                }),
            },
            Vec::new(),
        )
        .expect("vseat.wheel succeeds");
        epoch = wheel_res["epoch"].as_u64().expect("epoch");
        assert_eq!(
            wheel_res["disposition"],
            serde_json::to_value(koma_proto::DeliverySemantics::delivered_verified()).unwrap()
        );
        env.pump();

        assert!(
            env.client
                .pointer_axes
                .iter()
                .any(|&(axis, val)| axis == wl_pointer::Axis::VerticalScroll
                    && (val - 30.0).abs() < 1e-3),
            "expected vertical wheel axis event, got {:?}",
            env.client.pointer_axes
        );
        assert!(
            env.client
                .pointer_axis_v120
                .iter()
                .any(|&(axis, v)| axis == wl_pointer::Axis::VerticalScroll && v == 240),
            "expected vertical v120=240 wheel event, got {:?}",
            env.client.pointer_axis_v120
        );
        // Ctrl (29) + Shift (42) must both press and release cleanly.
        assert_eq!(
            env.client.keyboard_key_events,
            vec![
                (29, wl_keyboard::KeyState::Pressed),
                (42, wl_keyboard::KeyState::Pressed),
                (42, wl_keyboard::KeyState::Released),
                (29, wl_keyboard::KeyState::Released),
            ]
        );
        {
            let vseat_id = &env.state.workspaces[&ws_id].vseat_id;
            let ledger = &env.state.vseats[vseat_id];
            assert!(!ledger.is_key_held("ctrl"));
            assert!(!ledger.is_key_held("shift"));
        }

        // 2. Click with modifier ["Alt"] + label, verify LabelPill + AgentCapture isolation.
        env.client.keyboard_key_events.clear();
        env.client.pointer_buttons.clear();
        let (click_res, _) = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 2,
                method: "vseat.click".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "expected_epoch": epoch,
                    "x": 20.0,
                    "y": 20.0,
                    "modifiers": ["Alt"],
                    "label": "CLICK LAYER",
                }),
            },
            Vec::new(),
        )
        .expect("vseat.click with modifiers succeeds");
        assert_eq!(
            click_res["disposition"],
            serde_json::to_value(koma_proto::DeliverySemantics::delivered_verified()).unwrap()
        );
        env.pump();
        assert_eq!(
            env.client.keyboard_key_events,
            vec![
                (56, wl_keyboard::KeyState::Pressed),
                (56, wl_keyboard::KeyState::Released),
            ]
        );
        assert_eq!(
            env.client.pointer_buttons,
            vec![
                (0x110, wl_pointer::ButtonState::Pressed),
                (0x110, wl_pointer::ButtonState::Released),
            ]
        );
        let snap = env
            .state
            .step_agent_cursor_for_workspace(&ws_id)
            .expect("agent cursor visible with label");
        assert_eq!(
            snap.label_pill.as_ref().map(|p| p.text.as_str()),
            Some("CLICK LAYER")
        );
        assert!(!koma_render::frame_composer::should_render_agent_cursor(
            koma_render::RenderTargetKind::AgentCapture
        ));

        // 3. Disconnect mid-drag with held Ctrl + left button must release both and release lease.
        env.client.keyboard_key_events.clear();
        env.client.pointer_buttons.clear();
        let (drag_res, _) = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 3,
                method: "vseat.drag".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "expected_epoch": epoch,
                    "from": [12.0, 12.0],
                    "to": [48.0, 48.0],
                    "duration_ms": 200,
                    "modifiers": ["Ctrl"],
                }),
            },
            Vec::new(),
        )
        .expect("in-flight drag succeeds");
        assert_eq!(
            drag_res["disposition"],
            serde_json::to_value(koma_proto::DeliverySemantics::delivered_unverified_unsafe())
                .unwrap()
        );
        env.pump();
        assert_eq!(
            env.client.pointer_buttons,
            vec![(0x110, wl_pointer::ButtonState::Pressed)]
        );
        assert_eq!(
            env.client.keyboard_key_events,
            vec![(29, wl_keyboard::KeyState::Pressed)]
        );

        crate::ipc_server::cleanup_disconnected_ipc_peer(&mut env.state, conn_id);
        env.pump();

        assert_eq!(
            env.client.pointer_buttons,
            vec![
                (0x110, wl_pointer::ButtonState::Pressed),
                (0x110, wl_pointer::ButtonState::Released),
            ],
            "disconnect cleanup must release held mouse button"
        );
        assert_eq!(
            env.client.keyboard_key_events,
            vec![
                (29, wl_keyboard::KeyState::Pressed),
                (29, wl_keyboard::KeyState::Released),
            ],
            "disconnect cleanup must release held modifier key"
        );
        assert!(env.state.pointer_animations.is_empty());
        assert!(matches!(
            env.state.workspaces[&ws_id].lease.state(),
            koma_model::WorkspaceLeaseState::Unheld
        ));
    }

    #[test]
    fn vseat_hit_test_occlusion_stability_cursor_label_and_takeover_semantics() {
        let mut env = CompatEnv::new(false);
        let (_s1, _xs1, _tl1, win_base) = env.spawn_window("Editor", "org.koma.editor", false);
        let ws_id = env.state.active_ws_id.clone();
        let conn_id = 703_u64;
        let _sess = insert_test_admin_ipc_client(&mut env.state, conn_id);
        let epoch = env.state.workspaces[&ws_id].lease.epoch().get();

        // 1. Stale epoch must return DeliverySemantics::not_delivered_safe().
        let stale_err = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 1,
                method: "vseat.click".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "expected_epoch": epoch.saturating_add(99),
                    "x": 10.0,
                    "y": 10.0,
                }),
            },
            Vec::new(),
        )
        .expect_err("stale epoch rejected");
        assert_eq!(stale_err.symbol, koma_proto::ErrorSymbol::ErrStaleEpoch);
        assert_eq!(
            stale_err.disposition,
            Some(koma_proto::DeliverySemantics::not_delivered_safe())
        );

        // 2. Unobstructed hit_test reaches target; once open animation settles, geometry_stable is true.
        *env.state.window_animations.borrow_mut() = koma_model::WindowAnimationTable::default();
        let (ht1_val, _) = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 2,
                method: "vseat.hit_test".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "target": { "anchor": "top-left", "dx": 32.0, "dy": 32.0 },
                }),
            },
            Vec::new(),
        )
        .expect("hit_test succeeds");
        let ht1: koma_proto::rpc::HitTestResult =
            serde_json::from_value(ht1_val).expect("HitTestResult deserializes");
        assert!(ht1.reaches_target);
        assert_eq!(ht1.target_win_id, Some(win_base));
        assert_eq!(ht1.blocking_win_id, None);
        assert!(ht1.geometry_stable);
        assert_eq!(ht1.sampled_points.len(), 5);

        // 3. Spawn a floating blocker covering win_base's active surface while win_base keeps focus.
        let (_s2, _xs2, _tl2, win_blocker) =
            env.spawn_window("AuthPrompt", "org.koma.blocker", false);
        let base_rect = env
            .state
            .mapped_windows
            .iter()
            .find(|m| m.win_id == win_base)
            .expect("win_base mapped")
            .rect;
        {
            let ws = env.state.workspaces.get_mut(&ws_id).expect("ws");
            ws.remove_window(win_blocker, 0);
            let blocker_rect = koma_model::LogicalRect::new(base_rect.x, base_rect.y, 80, 80);
            ws.floating.push_top(win_blocker, blocker_rect);
            ws.focused_window = Some(win_base);
        }
        env.state.sync_workspace_geometries(&ws_id);

        let (ht2_val, _) = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 3,
                method: "vseat.hit_test".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "target": { "anchor": "top-left", "dx": 32.0, "dy": 32.0 },
                }),
            },
            Vec::new(),
        )
        .expect("occluded hit_test succeeds");
        let ht2: koma_proto::rpc::HitTestResult =
            serde_json::from_value(ht2_val).expect("HitTestResult deserializes");
        assert!(!ht2.reaches_target);
        assert_eq!(ht2.target_win_id, Some(win_base));
        assert_eq!(ht2.blocking_win_id, Some(win_blocker));
        assert_eq!(ht2.blocking_app_id.as_deref(), Some("org.koma.blocker"));
        assert_eq!(ht2.blocking_title.as_deref(), Some("AuthPrompt"));
        assert!(ht2.blocking_rect.is_some());

        // 4. Active window animation flips geometry_stable to false.
        let anim_cfg = env.state.lua_sandbox.animations_snapshot();
        let now_ms = env.state.elapsed_ms();
        env.state.window_animations.borrow_mut().retarget_geometry(
            win_base,
            koma_model::LogicalRect::new(base_rect.x + 20, base_rect.y, base_rect.w, base_rect.h),
            now_ms,
            &anim_cfg,
            false,
        );
        let (ht3_val, _) = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 4,
                method: "vseat.hit_test".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "target": { "anchor": "center" },
                }),
            },
            Vec::new(),
        )
        .expect("animated hit_test succeeds");
        let ht3: koma_proto::rpc::HitTestResult = serde_json::from_value(ht3_val).unwrap();
        assert!(!ht3.geometry_stable);

        // 5. vseat.cursor_label sets & clears label_pill + caret_rect.
        let _ = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 5,
                method: "vseat.cursor_label".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "label": "TYPE QUERY",
                    "caret_rect": [100.0, 200.0, 14.0, 22.0],
                }),
            },
            Vec::new(),
        )
        .expect("cursor_label set succeeds");
        let labeled_snap = env
            .state
            .step_agent_cursor_for_workspace(&ws_id)
            .expect("labeled snapshot");
        assert_eq!(
            labeled_snap.label_pill.as_ref().map(|p| p.text.as_str()),
            Some("TYPE QUERY")
        );
        assert_eq!(
            labeled_snap.caret_rect,
            Some(koma_model::LogicalRect::new(100, 200, 14, 22))
        );

        let _ = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 6,
                method: "vseat.cursor_label".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "label": serde_json::Value::Null,
                    "caret_rect": serde_json::Value::Null,
                }),
            },
            Vec::new(),
        )
        .expect("cursor_label clear succeeds");
        if let Some(snap_after_clear) = env.state.step_agent_cursor_for_workspace(&ws_id) {
            assert!(snap_after_clear.label_pill.is_none());
            assert!(snap_after_clear.caret_rect.is_none());
        }

        // 6. Verify Human Takeover via ws.set_mode flushes held inputs and returns ErrHumanPreempted.
        let (drag2_res, _) = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 7,
                method: "vseat.drag".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "expected_epoch": epoch,
                    "from": [10.0, 10.0],
                    "to": [30.0, 30.0],
                    "duration_ms": 200,
                    "modifiers": ["Shift"],
                }),
            },
            Vec::new(),
        )
        .expect("second drag succeeds");
        let active_epoch = drag2_res["epoch"].as_u64().expect("epoch");
        env.pump();
        env.client.keyboard_key_events.clear();
        env.client.pointer_buttons.clear();

        let _ = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 8,
                method: "ws.set_mode".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "mode": "takeover",
                }),
            },
            Vec::new(),
        )
        .expect("ws.set_mode takeover succeeds");
        env.pump();
        assert_eq!(
            env.client.pointer_buttons,
            vec![(0x110, wl_pointer::ButtonState::Released)]
        );
        assert_eq!(
            env.client.keyboard_key_events,
            vec![(42, wl_keyboard::KeyState::Released)]
        );
        let preempt_err = crate::ipc_server::dispatch_rpc(
            &mut env.state,
            conn_id,
            koma_proto::IpcRequest {
                id: 9,
                method: "vseat.click".to_owned(),
                params: serde_json::json!({
                    "ws_id": ws_id.as_str(),
                    "expected_epoch": active_epoch,
                    "x": 10.0,
                    "y": 10.0,
                }),
            },
            Vec::new(),
        )
        .expect_err("takeover blocks vseat.click");
        assert_eq!(
            preempt_err.symbol,
            koma_proto::ErrorSymbol::ErrHumanPreempted
        );
        assert_eq!(
            preempt_err.disposition,
            Some(koma_proto::DeliverySemantics::not_delivered_safe())
        );
    }
}

/// Drives the real `ext-session-lock-v1` wire protocol (`Locked` -> `Confirm` -> `Unlock`) against
/// the headless seat and asserts every phase reaches a render pass (komawm/koma#37).
///
/// Reproduces the KOM-0043 hang: `SessionLockHandler::new_surface` reconciles keyboard focus while
/// the session is locked, Smithay calls `SeatHandler::focus_changed` while holding the keyboard
/// mutex, and the locked branch re-entered `KeyboardHandle::current_focus()` -> self-deadlock.
#[test]
fn session_lock_acquire_confirm_unlock_cycles_schedule_rendering() {
    use crate::state::{SessionLockPhase, is_surface_mapped, output_logical_viewport};

    let mut env = ClientEnv::new();
    let qh = env.queue.handle();
    let manager = env
        .client
        .bind::<ext_session_lock_manager_v1::ExtSessionLockManagerV1>(
            &env.registry,
            &qh,
            "ext_session_lock_manager_v1",
        );
    let compositor =
        env.client
            .bind::<wl_compositor::WlCompositor>(&env.registry, &qh, "wl_compositor");
    let shm = env
        .client
        .bind::<wl_shm::WlShm>(&env.registry, &qh, "wl_shm");
    let wl_out = env.client.outputs[0].clone();

    let out = env.state.test_outputs[0].clone();
    let (width, height, scale) = output_logical_viewport(&out);
    assert_eq!(scale, 1.0, "test output must be integer-scaled");
    let stride = width * 4;
    let shm_fd = create_portable_shm_fd((stride * height) as usize, None);
    let pool = shm.create_pool(shm_fd.as_fd(), (stride * height) as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );

    for cycle in 0..3 {
        // --- Acquire ------------------------------------------------------------
        env.state.redraw_queued = false;
        let lock = manager.lock(&qh, ());
        env.pump();
        assert_eq!(
            env.client.session_lock_locked,
            cycle + 1,
            "client got Locked"
        );
        assert!(
            matches!(env.state.session_lock.phase, SessionLockPhase::Locked(_)),
            "compositor must transition to Locked"
        );
        assert!(
            env.state.redraw_queued,
            "acquire must schedule a blackout render pass"
        );

        // --- Confirm: lock surface create + configure + first commit ------------
        let surface = compositor.create_surface(&qh, ());
        let lock_surface = lock.get_lock_surface(&surface, &wl_out, &qh, ());
        env.state.redraw_queued = false;
        env.pump();
        assert_eq!(
            env.state.session_lock.surfaces.len(),
            1,
            "LockSurface must be registered for the output"
        );
        let configure = *env
            .client
            .session_lock_configures
            .last()
            .expect("lock surface must receive a configure");
        assert_eq!((configure.1, configure.2), (width, height));

        lock_surface.ack_configure(configure.0);
        surface.attach(Some(&buffer), 0, 0);
        env.state.redraw_queued = false;
        surface.commit();
        env.pump();
        let lock_surface_ref = env
            .state
            .session_lock
            .active_lock_surface_for_output(&out)
            .cloned()
            .expect("lock surface stays active");
        assert!(
            is_surface_mapped(lock_surface_ref.wl_surface()),
            "committed lock surface must be mapped so the locked frame draws it"
        );
        // Session-lock hit-testing keeps the lock surface's output origin unchanged.
        let (lock_hit_surface, lock_hit_origin) = env
            .state
            .surface_under(Point::from((100.0, 100.0)))
            .expect("committed lock surface must be hit-testable while locked");
        assert_eq!(&lock_hit_surface, lock_surface_ref.wl_surface());
        assert_eq!(
            lock_hit_origin,
            Point::from((
                f64::from(out.current_location().x),
                f64::from(out.current_location().y)
            )),
            "session-lock surface origin must stay at the output origin"
        );
        assert!(
            env.state.redraw_queued,
            "LockSurface commit must schedule a render pass while locked"
        );

        // --- Unlock -------------------------------------------------------------
        env.state.redraw_queued = false;
        lock.unlock_and_destroy();
        env.pump();
        assert_eq!(
            env.state.session_lock.phase,
            SessionLockPhase::Unlocked,
            "unlock must return to Unlocked"
        );
        assert!(
            env.state.session_lock.surfaces.is_empty(),
            "unlock must not leak LockSurface handles"
        );
        assert!(
            env.state.redraw_queued,
            "unlock must schedule a render pass of the unlocked scene"
        );

        surface.destroy();
        env.pump();
    }

    assert_eq!(env.client.session_lock_finished, 0);
    buffer.destroy();
    pool.destroy();
}

/// `komawm/koma#33`: a split must never configure a client below the `xdg_toplevel.set_min_size`
/// it advertised. Chromium/Firefox cannot shrink past that minimum, keep committing a cropped,
/// un-reflowed buffer, and the compositor then masks it to the too-small tile.
#[test]
fn split_resize_configures_tiled_clients_above_their_minimum_size() {
    let mut env = ClientEnv::new();
    for ws in env.state.workspaces.values_mut() {
        ws.dwindle.config.size_limits_tiled = true;
    }
    let (_surf_a, _xs_a, toplevel_a) = env.spawn_window("Browser", "org.koma.browser");
    let browser_id = toplevel_a.id().protocol_id();

    // A CSD browser advertising a 1100 px minimum content width (wider than half of the
    // 1920 px test workspace), exactly like Chromium's `set_min_size(500, 87)`.
    toplevel_a.set_min_size(1100, 200);
    _surf_a.commit();
    env.pump();
    let configures_before = env
        .client
        .toplevel_configures
        .get(&browser_id)
        .map_or(0, Vec::len);

    let (_surf_b, _xs_b, _toplevel_b) = env.spawn_window("Terminal", "org.koma.terminal");
    env.pump();

    let configures = env
        .client
        .toplevel_configures
        .get(&browser_id)
        .cloned()
        .unwrap_or_default();
    assert!(
        configures.len() > configures_before,
        "the existing tiled client must be re-configured after the split"
    );

    let border = env.state.lua_sandbox.config_snapshot().appearance.border;
    let border_inset = if border.enabled {
        (border.width.max(0.0) * 2.0).round() as i32
    } else {
        0
    };

    let browser = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.app_id == "org.koma.browser")
        .expect("browser mapped");
    assert_eq!(
        browser.rect.w, 1100,
        "the tiled rect must be clamped up to the client minimum"
    );

    let (width, height) = *configures.last().expect("configure size");
    assert_eq!(
        (width, height),
        (
            browser.rect.w as i32 - border_inset,
            browser.rect.h as i32 - border_inset
        ),
        "the configure must carry the clamped, client-honourable size"
    );

    // An unconstrained client (a terminal) still receives its plain half-width tile.
    let terminal = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.app_id == "org.koma.terminal")
        .expect("terminal mapped");
    let terminal_id = _toplevel_b.id().protocol_id();
    let terminal_configure = *env.client.toplevel_configures[&terminal_id]
        .last()
        .expect("terminal configure");
    assert!(
        terminal.rect.w < 1100,
        "the unconstrained terminal keeps the raw half-width box"
    );
    assert_eq!(
        terminal_configure.0,
        terminal.rect.w as i32 - border_inset,
        "unconstrained clients are still configured at their tile size"
    );
}

#[test]
fn issue_113_default_dwindle_resizes_and_partitions_large_min_size_clients_without_overlap() {
    let mut env = ClientEnv::new();
    let (_surf_qq, _xs_qq, toplevel_qq) = env.spawn_window("QQ", "QQ");
    toplevel_qq.set_min_size(1100, 600);
    _surf_qq.commit();
    env.pump();

    let (_surf_wc, _xs_wc, toplevel_wc) = env.spawn_window("WeChat", "wechat");
    toplevel_wc.set_min_size(1000, 600);
    _surf_wc.commit();
    env.pump();

    let qq_id = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.app_id == "QQ")
        .unwrap()
        .win_id;
    let wc_id = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.app_id == "wechat")
        .unwrap()
        .win_id;
    let ws_id = env.state.active_ws_id.clone();
    assert!(
        env.state
            .workspaces
            .get_mut(&ws_id)
            .unwrap()
            .activate_window(qq_id, 1)
    );
    env.state.focus_active_deck_window();

    let rect_of = |st: &KomaState, id| {
        st.mapped_windows
            .iter()
            .find(|m| m.win_id == id)
            .unwrap()
            .rect
    };
    let init_qq = rect_of(&env.state, qq_id);
    let init_wc = rect_of(&env.state, wc_id);
    assert!(init_qq.x + init_qq.w as i32 <= init_wc.x);
    assert!(init_qq.w < 1100 && init_wc.w < 1000);

    // Resize left (`WindowResizeDelta { width: -120, height: 0 }`) shrinks QQ smoothly below min_w
    for _ in 0..4 {
        execute_action_descriptor(
            &mut env.state,
            ActionDescriptor::WindowResizeDelta {
                width: -30,
                height: 0,
            },
            false,
        );
    }
    env.pump();
    let shrunk_qq = rect_of(&env.state, qq_id);
    let grown_wc = rect_of(&env.state, wc_id);
    assert!(
        shrunk_qq.w <= init_qq.w - 90,
        "QQ failed to shrink left: {shrunk_qq:?} vs {init_qq:?}"
    );
    assert!(shrunk_qq.x + shrunk_qq.w as i32 <= grown_wc.x);

    // Resize right (`WindowResizeDelta { width: +240, height: 0 }`) expands QQ smoothly
    for _ in 0..8 {
        execute_action_descriptor(
            &mut env.state,
            ActionDescriptor::WindowResizeDelta {
                width: 30,
                height: 0,
            },
            false,
        );
    }
    env.pump();
    let grown_qq = rect_of(&env.state, qq_id);
    let shrunk_wc = rect_of(&env.state, wc_id);
    assert!(
        grown_qq.w >= init_qq.w + 90,
        "QQ failed to grow right: {grown_qq:?} vs {init_qq:?}"
    );
    assert!(grown_qq.x + grown_qq.w as i32 <= shrunk_wc.x);
}

#[test]
fn issue_113_clipped_oversized_surface_never_steals_neighbor_pointer_hits() {
    let mut env = ClientEnv::new();

    let (surf_a, _xs_a, tl_a) = env.spawn_window("Overflowing", "org.koma.overflow");
    let qh = env.queue.handle();
    let comp = env.client.compositor.clone().unwrap();
    let wm = env.client.xdg_wm_base.clone().unwrap();
    let shm = env.client.shm.clone().unwrap();

    tl_a.set_min_size(1400, 600);
    // Commit an oversized 1400x400 buffer inside the left tile (`~960 px` wide after split).
    let fd = create_portable_shm_fd(1400 * 400 * 4, None);
    let pool = shm.create_pool(fd.as_fd(), 1400 * 400 * 4, &qh, ());
    let buf = pool.create_buffer(0, 1400, 400, 1400 * 4, wl_shm::Format::Argb8888, &qh, ());
    surf_a.attach(Some(&buf), 0, 0);
    surf_a.commit();
    env.pump();

    let (surf_b, xs_b, _tl_b) = env.spawn_window("Neighbor", "org.koma.neighbor");
    let fd_b = create_portable_shm_fd(400 * 400 * 4, None);
    let pool_b = shm.create_pool(fd_b.as_fd(), 400 * 400 * 4, &qh, ());
    let buf_b = pool_b.create_buffer(0, 400, 400, 400 * 4, wl_shm::Format::Argb8888, &qh, ());
    surf_b.attach(Some(&buf_b), 0, 0);
    surf_b.commit();
    env.pump();

    let win_a = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.app_id == "org.koma.overflow")
        .unwrap()
        .win_id;
    let win_b = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.app_id == "org.koma.neighbor")
        .unwrap()
        .win_id;
    let rect_b = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win_b)
        .unwrap()
        .rect;

    // Focus win_a (making it later/front-most if iterated last, then click inside win_b's tile):
    // even if win_a's committed buffer extends past x=960 into win_b's rect, hits in win_b's
    // tile must land on win_b, never win_a!
    let ws_id = env.state.active_ws_id.clone();
    assert!(
        env.state
            .workspaces
            .get_mut(&ws_id)
            .unwrap()
            .activate_window(win_a, 1)
    );
    let probe_b = Point::from((f64::from(rect_b.x) + 40.0, f64::from(rect_b.y) + 40.0));
    let (hit_win, _, _) = env
        .state
        .surface_under_in_workspace(&ws_id, probe_b)
        .expect("hit neighbor");
    assert_eq!(
        hit_win, win_b,
        "oversized surface in left tile must not steal pointer hits inside right tile"
    );

    // Popups extending beyond a toplevel's clipped rect must remain hit-testable.
    let pos = wm.create_positioner(&qh, ());
    pos.set_size(120, 80);
    pos.set_anchor_rect(-60, 10, 20, 20);
    let pop_surf = comp.create_surface(&qh, ());
    let pop_xdg = wm.get_xdg_surface(&pop_surf, &qh, ());
    let _pop = pop_xdg.get_popup(Some(&xs_b), &pos, &qh, ());
    pop_surf.commit();
    env.pump();
    let pop_fd = create_portable_shm_fd(120 * 80 * 4, None);
    let pop_pool = shm.create_pool(pop_fd.as_fd(), 120 * 80 * 4, &qh, ());
    let pop_buf = pop_pool.create_buffer(0, 120, 80, 120 * 4, wl_shm::Format::Argb8888, &qh, ());
    pop_surf.attach(Some(&pop_buf), 0, 0);
    pop_surf.commit();
    env.pump();

    let popup_probe = Point::from((f64::from(rect_b.x) - 20.0, f64::from(rect_b.y) + 40.0));
    let (pop_hit_win, _, _) = env
        .state
        .surface_under_in_workspace(&ws_id, popup_probe)
        .expect("hit popup");
    assert_eq!(
        pop_hit_win, win_b,
        "unclipped popup overhanging neighbor tile must stay hit-testable"
    );

    pop_buf.destroy();
    pop_pool.destroy();
    buf_b.destroy();
    pool_b.destroy();
    buf.destroy();
    pool.destroy();
}

#[test]
fn issue_115_interactive_resize_sets_resizing_throttles_bursts_and_flushes_on_commit_and_ungrab() {
    use crate::pointer_grabs::BTN_LEFT;
    use smithay::backend::input::ButtonState;

    let mut env = ClientEnv::new();
    let (surf_a, _xs_a, tl_a) = env.spawn_window("QQ", "QQ");
    let (_surf_b, _xs_b, _tl_b) = env.spawn_window("WeChat", "wechat");
    env.pump();

    let a_id = tl_a.id().protocol_id();
    let win_a = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.app_id == "QQ")
        .unwrap()
        .win_id;
    let rect_a = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win_a)
        .unwrap()
        .rect;

    let ws_id = env.state.active_ws_id.clone();
    assert!(
        env.state
            .workspaces
            .get_mut(&ws_id)
            .unwrap()
            .activate_window(win_a, 1)
    );
    env.state.focus_active_deck_window();

    // Hover right edge of left tile and press BTN_LEFT to start frame-edge ResizeWindowGrab.
    let edge_x = f64::from(rect_a.x + rect_a.w as i32) - 1.0;
    let edge_y = f64::from(rect_a.y) + f64::from(rect_a.h) * 0.5;
    env.state
        .handle_pointer_absolute_motion(Point::from((edge_x, edge_y)), InputTime::from_millis(10));
    env.pump();
    let cfg_before = env.client.toplevel_configures[&a_id].len();

    crate::keybind_dispatcher::process_pointer_button_event(
        &mut env.state,
        BTN_LEFT,
        ButtonState::Pressed,
        InputTime::from_millis(20),
        true,
    );
    env.pump();

    assert_eq!(env.state.active_resize_ws.as_ref(), Some(&ws_id));
    let resizing_code = xdg_toplevel::State::Resizing as u32;
    assert!(
        env.client.toplevel_states[&a_id]
            .last()
            .is_some_and(|st| st.contains(&resizing_code)),
        "interactive resize grab must emit xdg_toplevel::State::Resizing"
    );

    // Rapidly move 6 times without client surface commits -> configures must coalesce/throttle!
    for step in 1..=6i32 {
        env.state.handle_pointer_absolute_motion(
            Point::from((edge_x - f64::from(step * 25), edge_y)),
            InputTime::from_millis(20 + step as u32),
        );
    }
    env.pump();
    let cfg_after_burst = env.client.toplevel_configures[&a_id].len();
    assert!(
        cfg_after_burst <= cfg_before + 2,
        "burst of 6 motion steps before commit must throttle configures (before={cfg_before}, after={cfg_after_burst})"
    );

    // Committing the client surface flushes the latest throttled target width in one configure.
    surf_a.commit();
    env.pump();
    let cfg_after_commit = env.client.toplevel_configures[&a_id].len();
    assert_eq!(cfg_after_commit, cfg_after_burst + 1);

    // Releasing BTN_LEFT ends ResizeWindowGrab, unsets Resizing, and flushes settled state.
    crate::keybind_dispatcher::process_pointer_button_event(
        &mut env.state,
        BTN_LEFT,
        ButtonState::Released,
        InputTime::from_millis(40),
        true,
    );
    env.pump();
    assert!(env.state.active_resize_ws.is_none());
    assert!(
        !env.client.toplevel_states[&a_id]
            .last()
            .unwrap()
            .contains(&resizing_code),
        "releasing resize grab must unset xdg_toplevel::State::Resizing"
    );
}

/// Client `wl_data_device` drag-and-drop grab coverage (`komawm/koma#68`).
mod dnd_tests {
    use super::*;
    use smithay::{
        backend::input::{ButtonState, KeyState},
        input::{
            dnd::{DndAction, GrabType, Source, SourceMetadata},
            keyboard::Keycode,
        },
        utils::{IsAlive, SERIAL_COUNTER},
    };

    use crate::pointer_grabs::BTN_LEFT;

    /// Linux evdev code for Escape; XKB keycodes are `evdev + 8` in this harness.
    const EVDEV_ESC: u32 = 1;
    const MIME: &str = "text/plain;charset=utf-8";

    /// Counts source lifecycle events so grab preconditions can be asserted without a client.
    #[derive(Debug, Default)]
    struct RecordingSource {
        cancelled: Arc<AtomicU64>,
        finished: Arc<AtomicU64>,
        dropped: Arc<AtomicU64>,
    }

    impl IsAlive for RecordingSource {
        fn alive(&self) -> bool {
            true
        }
    }

    impl Source for RecordingSource {
        fn metadata(&self) -> Option<SourceMetadata> {
            Some(SourceMetadata {
                mime_types: vec![MIME.to_owned()],
                ..SourceMetadata::default()
            })
        }

        fn choose_action(&self, _action: DndAction) {}

        fn send(&self, _mime_type: &str, _fd: OwnedFd) {}

        fn drop_performed(&self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }

        fn cancel(&self) {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
        }

        fn finished(&self) {
            self.finished.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn pointer_grabbed(state: &KomaState) -> bool {
        state.seat.get_pointer().is_some_and(|p| p.is_grabbed())
    }

    fn source_events(
        client: &ClientHarness,
        source: &wl_data_source::WlDataSource,
    ) -> CapturedDataSource {
        client
            .data_source_events
            .get(&source.id().protocol_id())
            .cloned()
            .unwrap_or_default()
    }

    /// Presses `button` through the real pointer-button dispatcher and returns the serial the
    /// compositor delivered to the client, which is the serial `wl_data_device.start_drag` needs.
    fn press_button(env: &mut ClientEnv, button: u32) -> u32 {
        crate::keybind_dispatcher::process_pointer_button_event(
            &mut env.state,
            button,
            ButtonState::Pressed,
            InputTime::from_millis(10),
            false,
        );
        env.pump();
        env.client
            .pointer_buttons
            .iter()
            .rev()
            .find(|(code, _, _)| *code == button)
            .map(|(_, _, serial)| *serial)
            .expect("client must observe the implicit-grab button press")
    }

    fn release_button(env: &mut ClientEnv, button: u32) {
        crate::keybind_dispatcher::process_pointer_button_event(
            &mut env.state,
            button,
            ButtonState::Released,
            InputTime::from_millis(20),
            false,
        );
        env.pump();
    }

    fn data_device_manager(env: &mut ClientEnv) -> wl_data_device_manager::WlDataDeviceManager {
        let qh = env.queue.handle();
        let manager = env
            .client
            .bind::<wl_data_device_manager::WlDataDeviceManager>(
                &env.registry,
                &qh,
                "wl_data_device_manager",
            );
        env.pump();
        manager
    }

    fn create_source(
        env: &mut ClientEnv,
        manager: &wl_data_device_manager::WlDataDeviceManager,
        mime_types: &[&str],
    ) -> wl_data_source::WlDataSource {
        let qh = env.queue.handle();
        let source = manager.create_data_source(&qh, ());
        // Sources advertise the actions they are willing to perform; without this the target's
        // `wl_data_offer.set_actions` negotiation cannot select an action and the drop is
        // rejected even though the target accepted it.
        source.set_actions(
            wl_data_device_manager::DndAction::Copy | wl_data_device_manager::DndAction::Move,
        );
        for mime in mime_types {
            source.offer((*mime).to_owned());
        }
        env.client
            .data_source_events
            .entry(source.id().protocol_id())
            .or_default();
        source
    }

    /// Creates and commits the client drag icon surface (role `dnd_icon` on `start_drag`).
    fn create_drag_icon(env: &mut ClientEnv) -> wl_surface::WlSurface {
        let qh = env.queue.handle();
        let compositor = env
            .client
            .compositor
            .clone()
            .expect("spawn_window bound wl_compositor");
        let shm = env.client.shm.clone().expect("spawn_window bound wl_shm");
        let icon = compositor.create_surface(&qh, ());
        let fd = create_portable_shm_fd(24 * 24 * 4, None);
        let pool = shm.create_pool(fd.as_fd(), 24 * 24 * 4, &qh, ());
        let buffer = pool.create_buffer(0, 24, 24, 24 * 4, wl_shm::Format::Argb8888, &qh, ());
        icon.attach(Some(&buffer), 0, 0);
        icon.commit();
        env.pump();
        buffer.destroy();
        pool.destroy();
        icon
    }

    /// Moves the pointer onto `win_id`'s visible geometry and returns the probe point.
    fn place_pointer_on_window(
        env: &mut ClientEnv,
        win_id: koma_proto::WindowId,
    ) -> Point<f64, smithay::utils::Logical> {
        let ws_id = env.state.active_ws_id.clone();
        let probe = probe_inside(env, &ws_id, win_id);
        move_pointer(env, probe);
        probe
    }

    fn spawn_origin_window(env: &mut ClientEnv) -> (wl_surface::WlSurface, koma_proto::WindowId) {
        let (surface, _xdg_surface, _toplevel) = env.spawn_window("Browser", "org.koma.browser");
        let win_id = env
            .state
            .mapped_windows
            .last()
            .expect("mapped window")
            .win_id;
        (surface, win_id)
    }

    fn start_client_drag_request(
        env: &mut ClientEnv,
        origin: &wl_surface::WlSurface,
        icon: Option<&wl_surface::WlSurface>,
        source: &wl_data_source::WlDataSource,
        serial: u32,
    ) {
        let qh = env.queue.handle();
        let manager = data_device_manager(env);
        let seat = env.client.seats.first().expect("seat").clone();
        let device = manager.get_data_device(&seat, &qh, ());
        device.start_drag(Some(source), origin, icon, serial);
        env.pump();
        env.client.data_device = Some(device);
    }

    #[test]
    fn start_drag_is_refused_while_locked_and_cancels_the_source() {
        let (_event_loop, _display, mut state) = make_headless_test_state();
        assert!(state.session_lock.try_lock(None));
        let seat = state.seat.clone();
        let source = RecordingSource::default();
        let cancelled = source.cancelled.clone();

        let installed = crate::dnd::start_client_drag(
            &mut state,
            source,
            None,
            seat,
            SERIAL_COUNTER.next_serial(),
            GrabType::Pointer,
        );

        assert!(!installed, "a locked session must not install a DnD grab");
        assert_eq!(
            cancelled.load(Ordering::SeqCst),
            1,
            "the refused source must be cancelled so the client can tear down its drag state"
        );
        assert!(state.dnd_drag.is_none());
        assert!(!pointer_grabbed(&state));
    }

    #[test]
    fn start_drag_requires_the_serial_of_the_live_implicit_grab() {
        let (_event_loop, _display, mut state) = make_headless_test_state();
        let seat = state.seat.clone();
        let source = RecordingSource::default();
        let cancelled = source.cancelled.clone();

        let installed = crate::dnd::start_client_drag(
            &mut state,
            source,
            None,
            seat,
            SERIAL_COUNTER.next_serial(),
            GrabType::Pointer,
        );

        assert!(!installed, "no implicit grab exists, so no drag may start");
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        assert!(state.dnd_drag.is_none());
        assert!(!pointer_grabbed(&state));
    }

    #[test]
    fn start_drag_installs_a_grab_tracks_the_icon_and_does_not_cancel_the_source() {
        let mut env = ClientEnv::new();
        let (origin, win_id) = spawn_origin_window(&mut env);
        place_pointer_on_window(&mut env, win_id);
        let serial = press_button(&mut env, BTN_LEFT);

        let icon = create_drag_icon(&mut env);
        let manager = data_device_manager(&mut env);
        let source = create_source(&mut env, &manager, &[MIME]);
        start_client_drag_request(&mut env, &origin, Some(&icon), &source, serial);

        assert!(
            pointer_grabbed(&env.state),
            "`wl_data_device.start_drag` must replace the click grab with a DnD grab"
        );
        let drag = env
            .state
            .dnd_drag
            .as_ref()
            .expect("the drag must be tracked while the grab runs");
        let tracked_icon = drag.icon().expect("the requested icon must be tracked");
        assert_eq!(tracked_icon.offset(), Point::from((0, 0)));
        assert_eq!(
            source_events(&env.client, &source).cancelled,
            0,
            "installing the grab must not cancel the client source"
        );
        assert_eq!(source_events(&env.client, &source).drop_performed, 0);

        // A second `start_drag` while the first still owns the seat is refused and cancelled.
        let other = create_source(&mut env, &manager, &[MIME]);
        start_client_drag_request(&mut env, &origin, None, &other, serial);
        assert_eq!(
            source_events(&env.client, &other).cancelled,
            1,
            "a second concurrent drag must be refused instead of replacing the active grab"
        );
        assert!(
            env.state.dnd_drag.is_some(),
            "refusing the second drag must keep the first drag alive"
        );

        release_button(&mut env, BTN_LEFT);
        assert!(!pointer_grabbed(&env.state));
        assert!(env.state.dnd_drag.is_none());
    }

    #[test]
    fn motion_over_a_toplevel_sets_an_offer_and_drop_finishes_the_source() {
        let mut env = ClientEnv::new();
        let (origin, win_id) = spawn_origin_window(&mut env);
        let probe = place_pointer_on_window(&mut env, win_id);
        let serial = press_button(&mut env, BTN_LEFT);

        let icon = create_drag_icon(&mut env);
        let manager = data_device_manager(&mut env);
        let source = create_source(&mut env, &manager, &[MIME]);
        start_client_drag_request(&mut env, &origin, Some(&icon), &source, serial);
        assert!(pointer_grabbed(&env.state));
        // Motion after `start_drag` is what offers the drag to the surface under the cursor.
        move_pointer(&mut env, probe + Point::from((8.0, 8.0)));
        env.pump();

        assert_eq!(
            env.client.data_device_enters, 1,
            "the toplevel under the cursor must receive `wl_data_device.enter`"
        );
        let offer_id = *env
            .client
            .data_offer_proxies
            .keys()
            .next()
            .expect("the target must receive a data offer");
        assert!(
            env.client.data_offer_mimes[&offer_id].contains(&MIME.to_owned()),
            "the offered mime types must be forwarded to the target"
        );
        let offer = env.client.data_offer_proxies[&offer_id].clone();
        let enter_serial = env
            .client
            .data_device_enter_serial
            .expect("enter carries the serial used to accept the offer");
        offer.accept(enter_serial, Some(MIME.to_owned()));
        offer.set_actions(
            wl_data_device_manager::DndAction::Copy,
            wl_data_device_manager::DndAction::Copy,
        );
        env.pump();

        release_button(&mut env, BTN_LEFT);

        assert_eq!(
            env.client.data_device_drops, 1,
            "releasing the button must deliver `wl_data_device.drop` to the accepted target"
        );
        assert_eq!(
            source_events(&env.client, &source).drop_performed,
            1,
            "an accepted drop must reach the source as `wl_data_source.dnd_drop_performed`"
        );
        assert_eq!(source_events(&env.client, &source).cancelled, 0);
        assert!(!pointer_grabbed(&env.state));
        assert!(
            env.state.dnd_drag.is_none(),
            "the drag icon must disappear once the drop completes"
        );

        // The target acknowledges the transfer, which is the only path to `dnd_finished`.
        offer.finish();
        env.pump();
        assert_eq!(
            source_events(&env.client, &source).finished,
            1,
            "`wl_data_offer.finish` must finish the source, not cancel it"
        );
        assert_eq!(source_events(&env.client, &source).cancelled, 0);
    }

    #[test]
    fn escape_cancels_the_source_and_leaves_no_grab_or_icon() {
        let mut env = ClientEnv::new();
        let (origin, win_id) = spawn_origin_window(&mut env);
        let probe = place_pointer_on_window(&mut env, win_id);
        let serial = press_button(&mut env, BTN_LEFT);

        let icon = create_drag_icon(&mut env);
        let manager = data_device_manager(&mut env);
        let source = create_source(&mut env, &manager, &[MIME]);
        start_client_drag_request(&mut env, &origin, Some(&icon), &source, serial);
        move_pointer(&mut env, probe + Point::from((4.0, 4.0)));
        env.pump();
        assert!(env.state.dnd_drag.is_some());

        // Real `Escape` key path: `process_keyboard_key_event` intercepts it while a grab owns
        // the pointer and routes it to `cancel_active_pointer_grab`.
        crate::keybind_dispatcher::process_keyboard_key_event(
            &mut env.state,
            Keycode::from(EVDEV_ESC + 8),
            KeyState::Pressed,
            InputTime::from_millis(30),
            false,
        );
        env.pump();

        assert_eq!(
            source_events(&env.client, &source).cancelled,
            1,
            "an aborted drag must cancel the source"
        );
        assert_eq!(source_events(&env.client, &source).finished, 0);
        assert!(!pointer_grabbed(&env.state));
        assert!(env.state.dnd_drag.is_none());
    }

    #[test]
    fn acquiring_the_session_lock_cancels_the_active_drag() {
        let mut env = ClientEnv::new();
        let (origin, win_id) = spawn_origin_window(&mut env);
        let probe = place_pointer_on_window(&mut env, win_id);
        let serial = press_button(&mut env, BTN_LEFT);

        let icon = create_drag_icon(&mut env);
        let manager = data_device_manager(&mut env);
        let source = create_source(&mut env, &manager, &[MIME]);
        start_client_drag_request(&mut env, &origin, Some(&icon), &source, serial);
        move_pointer(&mut env, probe + Point::from((4.0, 4.0)));
        env.pump();
        assert!(env.state.dnd_drag.is_some());

        // Mirrors `SessionLockHandler::lock`, which acquires the lock and then revokes all input.
        assert!(env.state.session_lock.try_lock(None));
        env.state.clear_inputs_for_lock();
        env.pump();

        assert_eq!(
            source_events(&env.client, &source).cancelled,
            1,
            "locking the session must cancel the drag source"
        );
        assert!(!pointer_grabbed(&env.state));
        assert!(env.state.dnd_drag.is_none());
    }

    #[test]
    fn jailed_sandbox_clients_cannot_start_a_host_drag() {
        // A `security-context-v1` sandbox client never receives host data-device focus, so it must
        // not be able to originate a drag that offers its data to host clients either.
        let mut env = ClientEnv::with_client_state(|state| ClientState {
            jailed_workspace: Some(state.active_ws_id.clone()),
            ..ClientState::default()
        });
        let (origin, win_id) = spawn_origin_window(&mut env);
        place_pointer_on_window(&mut env, win_id);
        let serial = press_button(&mut env, BTN_LEFT);

        let icon = create_drag_icon(&mut env);
        let manager = data_device_manager(&mut env);
        let source = create_source(&mut env, &manager, &[MIME]);
        start_client_drag_request(&mut env, &origin, Some(&icon), &source, serial);

        assert!(
            env.state.dnd_drag.is_none(),
            "a jailed client must not install a drag grab"
        );
        assert_eq!(
            source_events(&env.client, &source).cancelled,
            1,
            "the refused jailed source must be cancelled"
        );
        assert!(
            env.state.dnd_icon().is_none(),
            "no drag icon may be tracked"
        );

        // The plain implicit click grab is still installed until the button is released, but it
        // must not turn into a drag.
        release_button(&mut env, BTN_LEFT);
        assert!(!pointer_grabbed(&env.state));
        assert!(env.state.dnd_drag.is_none());
    }

    #[test]
    fn icon_offset_follows_committed_surface_offset_requests() {
        let mut env = ClientEnv::new();
        let (origin, win_id) = spawn_origin_window(&mut env);
        place_pointer_on_window(&mut env, win_id);
        let serial = press_button(&mut env, BTN_LEFT);

        let icon = create_drag_icon(&mut env);
        let manager = data_device_manager(&mut env);
        let source = create_source(&mut env, &manager, &[MIME]);
        start_client_drag_request(&mut env, &origin, Some(&icon), &source, serial);
        assert_eq!(
            env.state.dnd_icon().map(|i| i.offset()),
            Some(Point::from((0, 0)))
        );

        let qh = env.queue.handle();
        let shm = env.client.shm.clone().expect("wl_shm");
        let fd = create_portable_shm_fd(24 * 24 * 4, None);
        let pool = shm.create_pool(fd.as_fd(), 24 * 24 * 4, &qh, ());
        let buffer = pool.create_buffer(0, 24, 24, 24 * 4, wl_shm::Format::Argb8888, &qh, ());
        icon.offset(-6, -10);
        icon.attach(Some(&buffer), 0, 0);
        icon.commit();
        env.pump();

        assert_eq!(
            env.state.dnd_icon().map(|i| i.offset()),
            Some(Point::from((-6, -10))),
            "`wl_surface.offset` must move the icon hot spot off the cursor"
        );

        // Toolkits re-offset their icon several times per drag (centring, then per-target hints);
        // every commit is a delta relative to the previous buffer, not an absolute position.
        icon.offset(-2, 4);
        icon.attach(Some(&buffer), 0, 0);
        icon.commit();
        env.pump();
        assert_eq!(
            env.state.dnd_icon().map(|i| i.offset()),
            Some(Point::from((-8, -6))),
            "successive offsets accumulate instead of replacing the hot spot"
        );

        release_button(&mut env, BTN_LEFT);
        buffer.destroy();
        pool.destroy();
    }
}

mod kom_0072;

#[test]
fn issue_118_fullscreen_window_is_configured_to_the_full_output_without_border_inset() {
    let mut env = ClientEnv::new();
    let (_surf, _xs, toplevel) = env.spawn_window("Player", "org.koma.player");
    env.pump();
    let tl_id = toplevel.id().protocol_id();

    let border = env.state.lua_sandbox.config_snapshot().appearance.border;
    assert!(
        border.enabled && border.width > 0.0,
        "the default config must draw a border, otherwise this test proves nothing"
    );
    let border_inset = (border.width.max(0.0) * 2.0).round() as i32;

    let windowed = *env.client.toplevel_configures[&tl_id]
        .last()
        .expect("windowed configure");

    let win_id = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.app_id == "org.koma.player")
        .expect("player mapped")
        .win_id;
    env.state.set_window_fullscreen(win_id, true, None);
    env.pump();

    let rect = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win_id)
        .expect("player mapped")
        .rect;
    let viewport = env
        .state
        .workspaces
        .get(&env.state.active_ws_id)
        .expect("active workspace")
        .viewport;
    assert_eq!(
        (rect.w, rect.h),
        (viewport.width, viewport.height),
        "fullscreen rect is the whole output"
    );

    let fullscreen = *env.client.toplevel_configures[&tl_id]
        .last()
        .expect("fullscreen configure");
    assert_eq!(
        fullscreen,
        (viewport.width as i32, viewport.height as i32),
        "a fullscreen window has no border, so it is configured to the full output \
         (windowed configure was {windowed:?}, border inset {border_inset})"
    );

    // Leaving fullscreen restores the border-inset size.
    env.state.set_window_fullscreen(win_id, false, None);
    env.pump();
    let restored = *env.client.toplevel_configures[&tl_id]
        .last()
        .expect("restored configure");
    let rect = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win_id)
        .expect("player mapped")
        .rect;
    assert_eq!(
        restored,
        (rect.w as i32 - border_inset, rect.h as i32 - border_inset),
        "windowed configure subtracts the border inset again"
    );
}
