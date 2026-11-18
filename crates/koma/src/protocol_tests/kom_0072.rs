//! Regression harness for komawm/koma#64: an IME switch (fcitx5 `Ctrl+Space`) wedged the
//! compositor event loop.
//!
//! The freeze is the frame pass, not the seat re-entry the `seat_guard` reports:
//! `dispatch_output_frame_callbacks_filtered` iterated the output's layer map while holding its
//! non-reentrant `MutexGuard`, and the IME popup pass that follows resolves the popup's parent
//! geometry through the very same layer map. A popup whose parent root is not a mapped Wayland
//! toplevel (a `wlr-layer-shell` text field, an X11 window) takes that second lock and the event
//! loop parks on a mutex it already owns.
//!
//! Each scenario mirrors what fcitx5 does over `zwp_input_method_v2` / `zwp_text_input_v3` /
//! `zwp_virtual_keyboard_v1` while a text-input client holds keyboard focus: the app enables text
//! input, the input method creates its popup surface and grabs the keyboard, the app reports a
//! cursor rectangle, the switch chord arrives as a virtual-keyboard key that the compositor
//! forwards past the IME grab, and the IME commits the switched state.
//!
//! Everything runs in-process against a headless `KomaState`; no backend, seat or display is
//! touched and no external compositor is involved.

use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use smithay::backend::input::InputTime;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};
use smithay::reexports::wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_keyboard_grab_v2, zwp_input_method_manager_v2, zwp_input_method_v2,
    zwp_input_popup_surface_v2,
};
use smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};
use smithay::reexports::wayland_server::Display;
use wayland_client::protocol::{wl_callback, wl_keyboard, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::*;

/// XKB keycodes (`evdev` + 8) for `LeftCtrl` and `Space`; these are the codes a client sends on
/// `zwp_virtual_keyboard_v1.key`.
const KEY_LEFT_CTRL: u32 = 29 + 8;
const KEY_SPACE: u32 = 57 + 8;
/// How long a single stage may make no progress before the test declares a wedge.
const STAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// Each scenario exercises one shape of the IME seam on a fresh headless compositor.
#[derive(Clone, Copy, Debug)]
enum Scenario {
    /// `Ctrl+Space` forwarded through the IME's virtual keyboard, then the state commit.
    VirtualKeyboardSwitch,
    /// The IME session is active while the pointer rests over another client's window, so
    /// `SeatHandler::focus_changed` has to reconcile a foreign pointer focus.
    FocusSwitchWithPointerElsewhere,
    /// The text-input client leaves and re-enters the field; fcitx5 creates and destroys its
    /// popup surface around every activation.
    ActivateDeactivateCycle,
    /// The focused text-input client is a `wlr-layer-shell` surface (bar, launcher, overlay), so
    /// the IME popup's parent is not a mapped window.
    LayerShellTextInput,
}

impl Scenario {
    const ALL: [Scenario; 4] = [
        Scenario::VirtualKeyboardSwitch,
        Scenario::FocusSwitchWithPointerElsewhere,
        Scenario::ActivateDeactivateCycle,
        Scenario::LayerShellTextInput,
    ];

    fn label(self) -> &'static str {
        match self {
            Scenario::VirtualKeyboardSwitch => "virtual-keyboard-switch",
            Scenario::FocusSwitchWithPointerElsewhere => "focus-switch-with-pointer-elsewhere",
            Scenario::ActivateDeactivateCycle => "activate-deactivate-cycle",
            Scenario::LayerShellTextInput => "layer-shell-text-input",
        }
    }
}

/// A progress report: `(scenario, step)`.
type Stage = (&'static str, &'static str);

/// Reports each reached stage to the test thread so a wedge names the step that hung.
#[derive(Clone)]
struct Steps {
    tx: Sender<Stage>,
    scenario: &'static str,
}

impl Steps {
    fn reached(&self, step: &'static str) {
        let _ = self.tx.send((self.scenario, step));
    }
}

/// One in-process Wayland client connection used by the harness.
#[derive(Default)]
struct ProbeClient {
    sync_done: bool,
    globals: BTreeMap<String, (u32, u32)>,
    seats: Vec<wl_seat::WlSeat>,
    text_input_entered: usize,
    text_input_left: usize,
    text_input_done: usize,
    text_input_commits: Vec<String>,
    ime_activated: usize,
    ime_deactivated: usize,
    ime_done: usize,
    /// Keymap the compositor handed to `wl_keyboard`, reused for the input method's virtual
    /// keyboard exactly like fcitx5 does.
    keymap: Option<Vec<u8>>,
    keyboard_keys: Vec<(u32, Option<wl_keyboard::KeyState>)>,
    xdg_configures: usize,
    layer_configures: usize,
}

struct ProbeConn {
    conn: Connection,
    queue: EventQueue<ProbeClient>,
    data: ProbeClient,
    registry: wl_registry::WlRegistry,
}

impl ProbeConn {
    fn new(display: &Display<KomaState>) -> Self {
        let (server_sock, client_sock) = UnixStream::pair().expect("UnixStream::pair");
        display
            .handle()
            .insert_client(server_sock, Arc::new(ClientState::default()))
            .expect("insert probe client");
        let conn = Connection::from_socket(client_sock).expect("Connection::from_socket");
        let queue = conn.new_event_queue::<ProbeClient>();
        let registry = conn.display().get_registry(&queue.handle(), ());
        Self {
            conn,
            queue,
            data: ProbeClient::default(),
            registry,
        }
    }

    fn qh(&self) -> QueueHandle<ProbeClient> {
        self.queue.handle()
    }

    fn bind<I: wayland_client::Proxy + 'static>(&self, interface: &str) -> I
    where
        ProbeClient: Dispatch<I, ()>,
    {
        let (name, version) = self
            .data
            .globals
            .get(interface)
            .copied()
            .unwrap_or_else(|| panic!("missing global {interface}"));
        self.registry
            .bind::<I, (), ProbeClient>(name, version, &self.qh(), ())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ProbeClient {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_seat" {
                    let seat = registry.bind::<wl_seat::WlSeat, (), Self>(name, version, qh, ());
                    state.seats.push(seat);
                }
                state.globals.insert(interface, (name, version));
            }
            wl_registry::Event::GlobalRemove { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for ProbeClient {
    fn event(
        state: &mut Self,
        _cb: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.sync_done = true;
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ProbeClient {
    fn event(
        _state: &mut Self,
        _seat: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for ProbeClient {
    fn event(
        state: &mut Self,
        _keyboard: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { fd, size, .. } => {
                let mut file = std::fs::File::from(fd);
                let mut bytes = vec![0_u8; size as usize];
                if std::io::Read::read_exact(&mut file, &mut bytes).is_ok() {
                    state.keymap = Some(bytes);
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => state
                .keyboard_keys
                .push((key, key_state.into_result().ok())),
            _ => {}
        }
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for ProbeClient {
    fn event(
        state: &mut Self,
        _ti: &zwp_text_input_v3::ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { .. } => state.text_input_entered += 1,
            zwp_text_input_v3::Event::Leave { .. } => state.text_input_left += 1,
            zwp_text_input_v3::Event::CommitString { text } => {
                state.text_input_commits.extend(text);
            }
            zwp_text_input_v3::Event::Done { .. } => state.text_input_done += 1,
            _ => {}
        }
    }
}

impl Dispatch<zwp_input_method_v2::ZwpInputMethodV2, ()> for ProbeClient {
    fn event(
        state: &mut Self,
        _im: &zwp_input_method_v2::ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_v2::Event::Activate => state.ime_activated += 1,
            zwp_input_method_v2::Event::Deactivate => state.ime_deactivated += 1,
            zwp_input_method_v2::Event::Done => state.ime_done += 1,
            _ => {}
        }
    }
}

delegate_noop!(ProbeClient: ignore wl_compositor::WlCompositor);
delegate_noop!(ProbeClient: ignore wl_surface::WlSurface);
delegate_noop!(ProbeClient: ignore wl_region::WlRegion);
delegate_noop!(ProbeClient: ignore wl_shm::WlShm);
delegate_noop!(ProbeClient: ignore wl_shm_pool::WlShmPool);
delegate_noop!(ProbeClient: ignore wl_buffer::WlBuffer);
delegate_noop!(ProbeClient: ignore xdg_wm_base::XdgWmBase);
delegate_noop!(ProbeClient: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(ProbeClient: ignore zwp_text_input_manager_v3::ZwpTextInputManagerV3);
delegate_noop!(ProbeClient: ignore zwp_input_method_manager_v2::ZwpInputMethodManagerV2);
delegate_noop!(ProbeClient: ignore zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2);
delegate_noop!(ProbeClient: ignore zwp_input_method_keyboard_grab_v2::ZwpInputMethodKeyboardGrabV2);
delegate_noop!(ProbeClient: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(ProbeClient: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for ProbeClient {
    fn event(
        state: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            surface.ack_configure(serial);
            surface.set_size(width.max(1), height.max(1));
            state.layer_configures += 1;
        }
    }
}

delegate_noop!(ProbeClient: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

impl Dispatch<xdg_surface::XdgSurface, ()> for ProbeClient {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.xdg_configures += 1;
        }
    }
}

/// Headless server side of the probe.
struct ProbeServer {
    event_loop: calloop::EventLoop<'static, KomaState>,
    display: Display<KomaState>,
    state: KomaState,
    _output: Output,
}

impl ProbeServer {
    fn new() -> Self {
        let (event_loop, display, mut state) = make_headless_test_state();
        let output = test_output("DP-1");
        let _global = output.create_global::<KomaState>(&state.display_handle);
        state.test_outputs.push(output.clone());
        Self {
            event_loop,
            display,
            state,
            _output: output,
        }
    }

    /// Dispatches server and clients until every client observed a `wl_callback.done`.
    fn roundtrip(&mut self, clients: &mut [&mut ProbeConn]) {
        for client in clients.iter_mut() {
            client.data.sync_done = false;
            let _cb = client.conn.display().sync(&client.qh(), ());
        }
        for client in clients.iter_mut() {
            client.conn.flush().expect("flush client");
        }
        for _ in 0..64 {
            self.display
                .dispatch_clients(&mut self.state)
                .expect("dispatch server clients");
            self.event_loop
                .dispatch(Some(Duration::ZERO), &mut self.state)
                .expect("dispatch calloop");
            self.display.flush_clients().expect("flush server clients");
            for client in clients.iter_mut() {
                if let Some(guard) = client.queue.prepare_read() {
                    let _ = guard.read();
                }
                client
                    .queue
                    .dispatch_pending(&mut client.data)
                    .expect("dispatch client pending");
                client.conn.flush().expect("flush client post-dispatch");
            }
            if clients.iter().all(|c| c.data.sync_done) {
                return;
            }
        }
        panic!("client roundtrip did not complete");
    }
}

/// The surface a text-input client owns and that holds keyboard focus.
struct FocusWindow {
    surface: wl_surface::WlSurface,
    kind: FocusWindowKind,
}

enum FocusWindowKind {
    Toplevel {
        xdg_surface: xdg_surface::XdgSurface,
        toplevel: xdg_toplevel::XdgToplevel,
    },
    Layer {
        layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        _buffer: wl_buffer::WlBuffer,
        _pool: wl_shm_pool::WlShmPool,
    },
}

impl FocusWindow {
    fn destroy(self) {
        match self.kind {
            FocusWindowKind::Toplevel {
                xdg_surface,
                toplevel,
            } => {
                toplevel.destroy();
                xdg_surface.destroy();
            }
            FocusWindowKind::Layer { layer, .. } => layer.destroy(),
        }
        self.surface.destroy();
    }
}

fn spawn_window(app: &mut ProbeConn, server: &mut ProbeServer, title: &str) -> FocusWindow {
    let qh = app.qh();
    let compositor = app.bind::<wl_compositor::WlCompositor>("wl_compositor");
    let wm_base = app.bind::<xdg_wm_base::XdgWmBase>("xdg_wm_base");
    let shm = app.bind::<wl_shm::WlShm>("wl_shm");
    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_app_id("org.koma.probe".to_owned());
    toplevel.set_title(title.to_owned());
    surface.commit();
    server.roundtrip(&mut [app]);

    let shm_fd = create_portable_shm_fd(64 * 64 * 4, None);
    let pool = shm.create_pool(shm_fd.as_fd(), 64 * 64 * 4, &qh, ());
    let buffer = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    server.roundtrip(&mut [app]);

    buffer.destroy();
    pool.destroy();
    FocusWindow {
        surface,
        kind: FocusWindowKind::Toplevel {
            xdg_surface,
            toplevel,
        },
    }
}

/// Maps a `wlr-layer-shell` surface with exclusive keyboard interactivity, the shape a bar,
/// launcher or overlay text field has.
fn spawn_layer_text_field(app: &mut ProbeConn, server: &mut ProbeServer) -> FocusWindow {
    let qh = app.qh();
    let compositor = app.bind::<wl_compositor::WlCompositor>("wl_compositor");
    let shm = app.bind::<wl_shm::WlShm>("wl_shm");
    let shell = app.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1>("zwlr_layer_shell_v1");
    let surface = compositor.create_surface(&qh, ());
    let layer = shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Top,
        "koma-0072-probe".to_owned(),
        &qh,
        (),
    );
    layer.set_size(400, 48);
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
    layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
    surface.commit();
    server.roundtrip(&mut [app]);
    assert_eq!(
        app.data.layer_configures, 1,
        "the layer surface must be configured before it can be mapped"
    );

    let shm_fd = create_portable_shm_fd(64 * 64 * 4, None);
    let pool = shm.create_pool(shm_fd.as_fd(), 64 * 64 * 4, &qh, ());
    let buffer = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    server.roundtrip(&mut [app]);

    FocusWindow {
        surface,
        kind: FocusWindowKind::Layer {
            layer,
            _buffer: buffer,
            _pool: pool,
        },
    }
}

/// Sends `Ctrl+Space` through a `zwp_virtual_keyboard_v1` owned by the input method client, which
/// is how fcitx5 re-delivers the chord it consumed from the keyboard grab.
fn send_ctrl_space_via_virtual_keyboard(
    app: &mut ProbeConn,
    ime: &mut ProbeConn,
    server: &mut ProbeServer,
    keymap: &[u8],
) {
    let qh = ime.qh();
    let seat = ime
        .data
        .seats
        .first()
        .cloned()
        .expect("seat advertised to the input method client");
    let manager = ime.bind::<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>(
        "zwp_virtual_keyboard_manager_v1",
    );
    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
    // `zwp_virtual_keyboard_v1.keymap` wants a real file so the compositor can mmap it; the
    // temporary copy lives in the system temp dir and is removed immediately after the request.
    let path = std::env::temp_dir().join(format!("koma-0072-keymap-{}.xkb", std::process::id()));
    std::fs::write(&path, keymap).expect("write probe keymap");
    let file = std::fs::File::open(&path).expect("open probe keymap");
    keyboard.keymap(
        wl_keyboard::KeymapFormat::XkbV1 as u32,
        file.as_fd(),
        keymap.len() as u32,
    );
    drop(file);
    let _ = std::fs::remove_file(&path);
    server.roundtrip(&mut [app, ime]);

    keyboard.key(700, KEY_LEFT_CTRL, 1);
    keyboard.modifiers(4, 0, 0, 0);
    keyboard.key(701, KEY_SPACE, 1);
    keyboard.key(702, KEY_SPACE, 0);
    keyboard.key(703, KEY_LEFT_CTRL, 0);
    keyboard.modifiers(0, 0, 0, 0);
    server.roundtrip(&mut [app, ime]);

    keyboard.destroy();
    drop(manager);
    server.roundtrip(&mut [app, ime]);
}

/// Screen-space probe inside the most recently mapped window.
fn window_probe(server: &ProbeServer) -> smithay::utils::Point<f64, smithay::utils::Logical> {
    let ws_id = server.state.active_ws_id.clone();
    let win_id = server
        .state
        .mapped_windows
        .last()
        .expect("mapped window")
        .win_id;
    let rect = server
        .state
        .workspaces
        .get(&ws_id)
        .expect("workspace")
        .compute_visible_geometries_at(server.state.elapsed_ms())
        .into_iter()
        .find(|(id, _, _)| *id == win_id)
        .map(|(_, rect, _)| rect)
        .expect("visible geometry");
    smithay::utils::Point::from((f64::from(rect.x) + 24.0, f64::from(rect.y) + 24.0))
}

/// Runs one scenario from client registration to teardown.
fn run_scenario(server: &mut ProbeServer, steps: &Steps, scenario: Scenario) {
    let mut app = ProbeConn::new(&server.display);
    let mut ime = ProbeConn::new(&server.display);
    server.roundtrip(&mut [&mut app, &mut ime]);
    steps.reached("clients-connected");

    let window = if matches!(scenario, Scenario::LayerShellTextInput) {
        spawn_layer_text_field(&mut app, server)
    } else {
        spawn_window(&mut app, server, "Probe")
    };
    server.roundtrip(&mut [&mut app, &mut ime]);
    if !matches!(scenario, Scenario::LayerShellTextInput) {
        assert!(
            app.data.xdg_configures > 0,
            "the toplevel must be configured before it can be mapped"
        );
    }

    let app_qh = app.qh();
    let app_seat = app
        .data
        .seats
        .first()
        .cloned()
        .expect("seat advertised to the text-input client");
    let text_input = app
        .bind::<zwp_text_input_manager_v3::ZwpTextInputManagerV3>("zwp_text_input_manager_v3")
        .get_text_input(&app_seat, &app_qh, ());
    // The keyboard object records the compositor keymap, which the IME reuses below.
    let _app_keyboard = app_seat.get_keyboard(&app_qh, ());
    server.roundtrip(&mut [&mut app, &mut ime]);

    let ime_qh = ime.qh();
    let ime_seat = ime
        .data
        .seats
        .first()
        .cloned()
        .expect("seat advertised to the input method client");
    let input_method = ime
        .bind::<zwp_input_method_manager_v2::ZwpInputMethodManagerV2>("zwp_input_method_manager_v2")
        .get_input_method(&ime_seat, &ime_qh, ());
    server.roundtrip(&mut [&mut app, &mut ime]);
    steps.reached("ime-instance-created");

    // Focus the app window: Smithay hands it `wl_keyboard.enter`, focuses the text input and
    // bounces the input method around the focus switch.
    server.state.reconcile_keyboard_focus();
    server.roundtrip(&mut [&mut app, &mut ime]);
    assert_eq!(
        app.data.text_input_entered, 1,
        "the focused text-input client must receive zwp_text_input_v3.enter"
    );
    let keymap = app.data.keymap.clone().expect("seat keymap");
    steps.reached("window-focused");

    text_input.set_content_type(
        zwp_text_input_v3::ContentHint::empty(),
        zwp_text_input_v3::ContentPurpose::Normal,
    );
    text_input.enable();
    text_input.commit();
    server.roundtrip(&mut [&mut app, &mut ime]);
    assert_eq!(
        ime.data.ime_activated, 1,
        "enabling text input must activate the input method"
    );
    steps.reached("text-input-enabled");

    // fcitx5 creates its candidate window and grabs the keyboard.
    let ime_compositor = ime.bind::<wl_compositor::WlCompositor>("wl_compositor");
    let popup_surface = ime_compositor.create_surface(&ime_qh, ());
    let popup = input_method.get_input_popup_surface(&popup_surface, &ime_qh, ());
    let grab = input_method.grab_keyboard(&ime_qh, ());
    server.roundtrip(&mut [&mut app, &mut ime]);
    steps.reached("ime-popup-and-keyboard-grab");

    text_input.set_cursor_rectangle(8, 16, 2, 18);
    text_input.commit();
    server.roundtrip(&mut [&mut app, &mut ime]);
    steps.reached("cursor-rectangle-committed");

    // The frame pass is where a live IME popup meets the output's layer map.
    {
        let output = server._output.clone();
        steps.reached("frame-callbacks-before");
        crate::backend_winit::dispatch_output_frame_callbacks_filtered(
            &server.state,
            &output,
            Some(Duration::from_millis(7)),
        );
        steps.reached("frame-callbacks-after");
    }

    match scenario {
        Scenario::VirtualKeyboardSwitch => {
            send_ctrl_space_via_virtual_keyboard(&mut app, &mut ime, server, &keymap);
            assert!(
                !app.data.keyboard_keys.is_empty(),
                "the forwarded chord must reach the focused text-input client"
            );
            steps.reached("ctrl-space-via-virtual-keyboard");

            input_method.set_preedit_string("ni".to_owned(), 0, 2);
            input_method.commit(1);
            server.roundtrip(&mut [&mut app, &mut ime]);
            steps.reached("ime-preedit-committed");

            input_method.commit_string("你".to_owned());
            input_method.commit(2);
            server.roundtrip(&mut [&mut app, &mut ime]);
            assert_eq!(
                app.data.text_input_commits,
                vec!["你".to_owned()],
                "the input method must be able to commit into the focused text input"
            );
            assert!(
                app.data.text_input_done > 0 && ime.data.ime_done > 0,
                "the double-buffered IME state must round-trip in both directions"
            );
            steps.reached("ime-commit-string");

            popup.destroy();
            popup_surface.destroy();
            server.roundtrip(&mut [&mut app, &mut ime]);
            steps.reached("ime-popup-destroyed");
        }
        Scenario::FocusSwitchWithPointerElsewhere => {
            // The pointer rests over the app window, then another client's window takes keyboard
            // focus without the pointer moving: `focus_changed` sees a foreign pointer focus and
            // has to reconcile the pointer constraint state.
            let probe = window_probe(server);
            server
                .state
                .handle_pointer_absolute_motion(probe, InputTime::from_millis(5));
            steps.reached("pointer-over-app-window");

            let mut other = ProbeConn::new(&server.display);
            server.roundtrip(&mut [&mut app, &mut ime, &mut other]);
            let other_window = spawn_window(&mut other, server, "Other");
            server.roundtrip(&mut [&mut app, &mut ime, &mut other]);
            steps.reached("second-window-mapped");

            server.state.reconcile_keyboard_focus();
            server.roundtrip(&mut [&mut app, &mut ime, &mut other]);
            assert!(
                app.data.text_input_left > 0,
                "moving keyboard focus away must deliver zwp_text_input_v3.leave"
            );
            steps.reached("keyboard-focus-moved-to-second-window");

            // Move focus back while the input method still owns the keyboard grab.
            server
                .state
                .focus_workspace(&server.state.active_ws_id.clone());
            server.roundtrip(&mut [&mut app, &mut ime, &mut other]);
            steps.reached("keyboard-focus-restored");

            other_window.destroy();
            server.roundtrip(&mut [&mut app, &mut ime, &mut other]);
            steps.reached("second-window-destroyed");
        }
        Scenario::ActivateDeactivateCycle => {
            for round in 0..3 {
                text_input.disable();
                text_input.commit();
                server.roundtrip(&mut [&mut app, &mut ime]);
                steps.reached("text-input-disabled");
                assert_eq!(
                    ime.data.ime_deactivated,
                    round + 1,
                    "disabling text input must deactivate the input method"
                );

                text_input.enable();
                text_input.commit();
                server.roundtrip(&mut [&mut app, &mut ime]);
                steps.reached("text-input-re-enabled");

                // fcitx5 registers a fresh popup surface for every activation.
                let surface = ime_compositor.create_surface(&ime_qh, ());
                let fresh = input_method.get_input_popup_surface(&surface, &ime_qh, ());
                server.roundtrip(&mut [&mut app, &mut ime]);
                steps.reached("ime-popup-recreated");
                fresh.destroy();
                surface.destroy();
                server.roundtrip(&mut [&mut app, &mut ime]);
            }
            assert_eq!(ime.data.ime_activated, 4, "one activation per enable");
        }
        Scenario::LayerShellTextInput => {
            let focused = server
                .state
                .seat
                .get_keyboard()
                .and_then(|kb| kb.current_focus())
                .expect("keyboard focus");
            let layer_owns_focus = {
                let map = smithay::desktop::layer_map_for_output(&server._output);
                map.layers().any(|l| l.wl_surface() == &focused)
            };
            assert!(
                layer_owns_focus,
                "an exclusive layer surface must own keyboard focus"
            );
            assert_eq!(
                server.state.ime_popups.len(),
                1,
                "the active input method must have registered its popup"
            );
            steps.reached("layer-focus-and-popup-verified");
        }
    }

    grab.release();
    drop(popup);
    popup_surface.destroy();
    server.roundtrip(&mut [&mut app, &mut ime]);
    steps.reached("keyboard-grab-released");

    text_input.disable();
    text_input.commit();
    server.roundtrip(&mut [&mut app, &mut ime]);
    steps.reached("text-input-disabled-at-teardown");

    window.destroy();
    server.roundtrip(&mut [&mut app, &mut ime]);
    steps.reached("window-destroyed");
}

/// Runs every scenario on a worker thread and fails the test if any stage stops making progress,
/// so a reintroduced deadlock fails CI instead of hanging it.
#[test]
fn ime_switch_while_text_input_is_focused_does_not_wedge_the_event_loop() {
    let (tx, rx): (Sender<Stage>, Receiver<Stage>) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("kom-0072-probe".to_owned())
        .spawn(move || {
            for scenario in Scenario::ALL {
                let steps = Steps {
                    tx: tx.clone(),
                    scenario: scenario.label(),
                };
                let mut server = ProbeServer::new();
                run_scenario(&mut server, &steps, scenario);
                assert!(
                    server.state.mapped_windows.is_empty(),
                    "{}: the destroyed toplevel must leave no mapped window behind",
                    scenario.label()
                );
            }
        })
        .expect("spawn probe thread");

    let mut last = ("startup", "setup");
    loop {
        match rx.recv_timeout(STAGE_TIMEOUT) {
            Ok(stage) => last = stage,
            Err(RecvTimeoutError::Timeout) => panic!(
                "compositor event loop wedged during `{}` / `{}`: the probe stopped making progress",
                last.0, last.1
            ),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    worker.join().expect("probe thread completed");
}
