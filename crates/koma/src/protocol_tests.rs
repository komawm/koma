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
    xdg_surface, xdg_toplevel, xdg_wm_base,
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
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region,
    wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
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

#[derive(Default)]
struct ClientHarness {
    sync_done: bool,
    globals: BTreeMap<String, (u32, u32)>,
    outputs: Vec<wl_output::WlOutput>,
    seats: Vec<wl_seat::WlSeat>,
    pointers: Vec<wl_pointer::WlPointer>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    xdg_wm_base: Option<xdg_wm_base::XdgWmBase>,
    xdg_surface_configures: usize,
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
}

impl ClientHarness {
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
        let (mut event_loop, mut display, mut state) = make_headless_test_state();
        let output = test_output("DP-1");
        let output_global = output.create_global::<KomaState>(&state.display_handle);
        state.test_outputs.push(output.clone());
        let (conn, mut queue, mut client, registry) = ClientHarness::connect(&display, false);
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
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
        if matches!(event, xdg_toplevel::Event::Close) {
            state.toplevel_close_count += 1;
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
delegate_noop!(ClientHarness: ignore wl_pointer::WlPointer);
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
    editor_handle.set_fullscreen(None);
    editor_handle.set_rectangle(&surf1, 10, 20, 300, 200);
    env.pump();
    let m1 = env
        .state
        .mapped_windows
        .iter()
        .find(|m| m.win_id == win1)
        .expect("win1");
    assert!(m1.is_maximized);
    assert!(m1.is_minimized);
    assert_eq!(env.state.workspaces[&ws1].fullscreen_window, Some(win1));

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
    assert!(!m1.is_maximized);
    assert!(!m1.is_minimized);
    assert_eq!(env.state.workspaces[&ws1].fullscreen_window, None);

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
        pointer_motions: usize,
        pointer_buttons: Vec<(u32, wl_pointer::ButtonState)>,
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
            if let wl_keyboard::Event::Key { key, .. } = event {
                state.keyboard_keys.push(key);
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
                    ..
                } => state.pointer_buttons.push((button, btn_state)),
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
}
