//! Wayland client bridge for `xdg-desktop-portal-koma`:
//! - Output discovery via `wl_output` + `zxdg_output_manager_v1`
//! - Frame capture via `zwlr_screencopy_manager_v1` (SHM memfd)
//! - Virtual pointer & keyboard injection via `zwlr_virtual_pointer_manager_v1` and
//!   `zwp_virtual_keyboard_manager_v1`

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
#[cfg(target_os = "linux")]
use wayland_client::protocol::wl_keyboard;
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};
#[cfg(target_os = "linux")]
use xkbcommon::xkb;

#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub name: String,
    pub description: String,
    pub logical_x: i32,
    pub logical_y: i32,
    pub logical_w: i32,
    pub logical_h: i32,
    pub pixel_w: u32,
    pub pixel_h: u32,
}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    #[cfg_attr(not(target_os = "linux"), expect(dead_code))]
    pub width: u32,
    #[cfg_attr(not(target_os = "linux"), expect(dead_code))]
    pub height: u32,
    #[cfg_attr(not(target_os = "linux"), expect(dead_code))]
    pub stride: u32,
    #[expect(dead_code)]
    pub format: u32,
    #[cfg_attr(not(target_os = "linux"), expect(dead_code))]
    pub pixels: Vec<u8>,
}

#[derive(Debug)]
pub enum WaylandCommand {
    ListOutputs(mpsc::Sender<Vec<OutputInfo>>),
    #[cfg_attr(not(target_os = "linux"), expect(dead_code))]
    CaptureFrame {
        output_name: Option<String>,
        overlay_cursor: bool,
        reply: mpsc::Sender<anyhow::Result<CapturedFrame>>,
    },
    PointerMotion {
        dx: f64,
        dy: f64,
    },
    PointerMotionAbsolute {
        output_name: Option<String>,
        x: f64,
        y: f64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
    PointerAxis {
        dx: f64,
        dy: f64,
    },
    PointerAxisDiscrete {
        axis: u32,
        steps: i32,
    },
    KeyboardKeycode {
        keycode: u32,
        pressed: bool,
    },
    KeyboardKeysym {
        keysym: u32,
        pressed: bool,
    },
}

#[derive(Clone)]
pub struct WaylandBridge {
    tx: mpsc::Sender<WaylandCommand>,
}

impl WaylandBridge {
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<WaylandCommand>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx }, rx)
    }

    pub fn connect() -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env().context("failed to connect to Wayland display")?;
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        std::thread::Builder::new()
            .name("koma-portal-wayland".to_owned())
            .spawn(move || {
                if let Err(err) = run_wayland_thread(conn, rx, ready_tx) {
                    tracing::error!(error = %err, "wayland bridge thread exited with error");
                }
            })
            .context("failed to spawn wayland bridge thread")?;

        ready_rx
            .recv()
            .context("wayland bridge initialization aborted")??;

        Ok(Self { tx })
    }

    pub fn list_outputs(&self) -> anyhow::Result<Vec<OutputInfo>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(WaylandCommand::ListOutputs(reply_tx))
            .context("wayland bridge closed")?;
        reply_rx.recv().context("wayland bridge dropped reply")
    }

    pub fn capture_frame(
        &self,
        output_name: Option<String>,
        overlay_cursor: bool,
    ) -> anyhow::Result<CapturedFrame> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(WaylandCommand::CaptureFrame {
                output_name,
                overlay_cursor,
                reply: reply_tx,
            })
            .context("wayland bridge closed")?;
        reply_rx.recv().context("wayland bridge dropped reply")?
    }

    pub fn send(&self, cmd: WaylandCommand) {
        let _ = self.tx.send(cmd);
    }
}

struct OutputSlot {
    wl_output: wl_output::WlOutput,
    _xdg_output: Option<zxdg_output_v1::ZxdgOutputV1>,
    info: OutputInfo,
}

#[derive(Default)]
struct CaptureBufferSpec {
    format: Option<wl_shm::Format>,
    width: u32,
    height: u32,
    stride: u32,
    buffer_done: bool,
    ready: bool,
    failed: bool,
}

struct WaylandClientState {
    start_time: Instant,
    outputs: HashMap<u32, OutputSlot>,
    xdg_output_mgr: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    screencopy_mgr: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    virtual_pointer_mgr: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    virtual_keyboard_mgr: Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    virtual_pointer: Option<zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1>,
    virtual_keyboard: Option<zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1>,
    #[cfg(target_os = "linux")]
    xkb_keymap: Option<xkb::Keymap>,
    xkb_mods_depressed: u32,
    capture: CaptureBufferSpec,
}

impl WaylandClientState {
    fn time_ms(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    fn ensure_virtual_pointer(
        &mut self,
        qh: &QueueHandle<Self>,
    ) -> Option<&zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1> {
        if self.virtual_pointer.is_none()
            && let Some(mgr) = &self.virtual_pointer_mgr
        {
            self.virtual_pointer = Some(mgr.create_virtual_pointer(self.seat.as_ref(), qh, ()));
        }
        self.virtual_pointer.as_ref()
    }

    #[cfg(target_os = "linux")]
    fn ensure_virtual_keyboard(
        &mut self,
        qh: &QueueHandle<Self>,
    ) -> anyhow::Result<&zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1> {
        if self.virtual_keyboard.is_none() {
            let mgr = self
                .virtual_keyboard_mgr
                .as_ref()
                .context("compositor missing zwp_virtual_keyboard_manager_v1")?;
            let seat = self.seat.as_ref().context("compositor missing wl_seat")?;
            let kbd = mgr.create_virtual_keyboard(seat, qh, ());

            let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let keymap = xkb::Keymap::new_from_names(
                &ctx,
                "",
                "",
                "us",
                "",
                None,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .context("failed to compile US XKB keymap")?;
            let keymap_str = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
            let bytes = keymap_str.as_bytes();
            let fd = create_memfd("koma-portal-keymap", bytes.len() + 1)?;
            let mut file = File::from(fd);
            file.write_all(bytes)?;
            file.write_all(&[0])?;
            file.seek(SeekFrom::Start(0))?;
            kbd.keymap(
                wl_keyboard::KeymapFormat::XkbV1.into(),
                file.as_fd(),
                (bytes.len() + 1) as u32,
            );

            self.xkb_keymap = Some(keymap);
            self.virtual_keyboard = Some(kbd);
        }
        Ok(self.virtual_keyboard.as_ref().expect("virtual keyboard"))
    }

    #[cfg(not(target_os = "linux"))]
    fn ensure_virtual_keyboard(
        &mut self,
        _qh: &QueueHandle<Self>,
    ) -> anyhow::Result<&zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1> {
        bail!("virtual keyboard keymap requires Linux")
    }
}

fn create_memfd(name: &str, size: usize) -> anyhow::Result<OwnedFd> {
    let fd = memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)
        .context("memfd_create failed")?;
    ftruncate(&fd, size as u64).context("ftruncate memfd failed")?;
    Ok(fd)
}

fn run_wayland_thread(
    conn: Connection,
    rx: mpsc::Receiver<WaylandCommand>,
    ready_tx: mpsc::Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let (globals, mut event_queue) = match registry_queue_init::<WaylandClientState>(&conn) {
        Ok(res) => res,
        Err(err) => {
            let _ = ready_tx.send(Err(err.into()));
            return Ok(());
        }
    };
    let qh = event_queue.handle();

    let mut state = WaylandClientState {
        start_time: Instant::now(),
        outputs: HashMap::new(),
        xdg_output_mgr: globals.bind(&qh, 1..=3, ()).ok(),
        shm: globals.bind(&qh, 1..=1, ()).ok(),
        seat: globals.bind(&qh, 1..=9, ()).ok(),
        screencopy_mgr: globals.bind(&qh, 1..=3, ()).ok(),
        virtual_pointer_mgr: globals.bind(&qh, 1..=2, ()).ok(),
        virtual_keyboard_mgr: globals.bind(&qh, 1..=1, ()).ok(),
        virtual_pointer: None,
        virtual_keyboard: None,
        #[cfg(target_os = "linux")]
        xkb_keymap: None,
        xkb_mods_depressed: 0,
        capture: CaptureBufferSpec::default(),
    };

    for g in globals.contents().clone_list() {
        if g.interface == "wl_output" {
            let wl_out: wl_output::WlOutput =
                globals
                    .registry()
                    .bind(g.name, g.version.min(4), &qh, g.name);
            let xdg_out = state
                .xdg_output_mgr
                .as_ref()
                .map(|m| m.get_xdg_output(&wl_out, &qh, g.name));
            state.outputs.insert(
                g.name,
                OutputSlot {
                    wl_output: wl_out,
                    _xdg_output: xdg_out,
                    info: OutputInfo {
                        name: format!("output-{}", g.name),
                        description: String::new(),
                        logical_x: 0,
                        logical_y: 0,
                        logical_w: 1920,
                        logical_h: 1080,
                        pixel_w: 1920,
                        pixel_h: 1080,
                    },
                },
            );
        }
    }

    event_queue.roundtrip(&mut state)?;
    event_queue.roundtrip(&mut state)?;
    let _ = ready_tx.send(Ok(()));

    while let Ok(cmd) = rx.recv() {
        match cmd {
            WaylandCommand::ListOutputs(reply) => {
                let _ = event_queue.roundtrip(&mut state);
                let mut list: Vec<OutputInfo> =
                    state.outputs.values().map(|s| s.info.clone()).collect();
                list.sort_by(|a, b| a.name.cmp(&b.name));
                let _ = reply.send(list);
            }
            WaylandCommand::CaptureFrame {
                output_name,
                overlay_cursor,
                reply,
            } => {
                let res = capture_output_shm(
                    &mut event_queue,
                    &mut state,
                    &qh,
                    output_name.as_deref(),
                    overlay_cursor,
                );
                let _ = reply.send(res);
            }
            WaylandCommand::PointerMotion { dx, dy } => {
                let t = state.time_ms();
                if let Some(vptr) = state.ensure_virtual_pointer(&qh) {
                    vptr.motion(t, dx, dy);
                    vptr.frame();
                    let _ = conn.flush();
                }
            }
            WaylandCommand::PointerMotionAbsolute { output_name, x, y } => {
                let t = state.time_ms();
                let (w, h) = state
                    .outputs
                    .values()
                    .find(|o| output_name.as_deref().is_none_or(|n| o.info.name == n))
                    .map(|o| {
                        (
                            o.info.logical_w.max(1) as u32,
                            o.info.logical_h.max(1) as u32,
                        )
                    })
                    .unwrap_or((1920, 1080));
                if let Some(vptr) = state.ensure_virtual_pointer(&qh) {
                    vptr.motion_absolute(
                        t,
                        x.max(0.0).round() as u32,
                        y.max(0.0).round() as u32,
                        w,
                        h,
                    );
                    vptr.frame();
                    let _ = conn.flush();
                }
            }
            WaylandCommand::PointerButton { button, pressed } => {
                let t = state.time_ms();
                let btn_state = if pressed {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                if let Some(vptr) = state.ensure_virtual_pointer(&qh) {
                    vptr.button(t, button, btn_state);
                    vptr.frame();
                    let _ = conn.flush();
                }
            }
            WaylandCommand::PointerAxis { dx, dy } => {
                let t = state.time_ms();
                if let Some(vptr) = state.ensure_virtual_pointer(&qh) {
                    if dy.abs() > f64::EPSILON {
                        vptr.axis(t, wl_pointer::Axis::VerticalScroll, dy);
                    }
                    if dx.abs() > f64::EPSILON {
                        vptr.axis(t, wl_pointer::Axis::HorizontalScroll, dx);
                    }
                    vptr.frame();
                    let _ = conn.flush();
                }
            }
            WaylandCommand::PointerAxisDiscrete { axis, steps } => {
                let t = state.time_ms();
                let wl_axis = if axis == 1 {
                    wl_pointer::Axis::HorizontalScroll
                } else {
                    wl_pointer::Axis::VerticalScroll
                };
                if let Some(vptr) = state.ensure_virtual_pointer(&qh) {
                    vptr.axis_discrete(t, wl_axis, f64::from(steps) * 15.0, steps);
                    vptr.frame();
                    let _ = conn.flush();
                }
            }
            WaylandCommand::KeyboardKeycode { keycode, pressed } => {
                let t = state.time_ms();
                let ks = if pressed { 1 } else { 0 };
                if let Ok(vkbd) = state.ensure_virtual_keyboard(&qh) {
                    vkbd.key(t, keycode, ks);
                    let _ = conn.flush();
                }
            }
            WaylandCommand::KeyboardKeysym { keysym, pressed } => {
                let t = state.time_ms();
                let ks = if pressed { 1 } else { 0 };
                if state.ensure_virtual_keyboard(&qh).is_ok()
                    && let Some((evdev_kc, needs_shift)) =
                        find_evdev_keycode_for_keysym(&state, keysym)
                    && let Some(vkbd) = &state.virtual_keyboard
                {
                    if needs_shift && pressed {
                        state.xkb_mods_depressed |= 1;
                        vkbd.modifiers(state.xkb_mods_depressed, 0, 0, 0);
                    }
                    vkbd.key(t, evdev_kc, ks);
                    if needs_shift && !pressed {
                        state.xkb_mods_depressed &= !1;
                        vkbd.modifiers(state.xkb_mods_depressed, 0, 0, 0);
                    }
                    let _ = conn.flush();
                }
            }
        }
        let _ = event_queue.dispatch_pending(&mut state);
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn find_evdev_keycode_for_keysym(
    state: &WaylandClientState,
    target_sym: u32,
) -> Option<(u32, bool)> {
    let km = state.xkb_keymap.as_ref()?;
    let target = xkb::Keysym::new(target_sym);
    let min_kc = km.min_keycode().raw();
    let max_kc = km.max_keycode().raw();
    for raw_kc in min_kc..=max_kc {
        let kc = xkb::Keycode::new(raw_kc);
        for level in 0..=1 {
            if km.key_get_syms_by_level(kc, 0, level).contains(&target) {
                return Some((raw_kc.saturating_sub(8), level == 1));
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn find_evdev_keycode_for_keysym(
    _state: &WaylandClientState,
    _target_sym: u32,
) -> Option<(u32, bool)> {
    None
}

fn dispatch_with_timeout(
    event_queue: &mut wayland_client::EventQueue<WaylandClientState>,
    state: &mut WaylandClientState,
    deadline: Instant,
) -> anyhow::Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("Wayland screencopy timed out");
    }
    event_queue.flush()?;
    if let Some(guard) = event_queue.prepare_read() {
        let fd = guard.connection_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let ts = Timespec {
            tv_sec: remaining.as_secs() as _,
            tv_nsec: remaining.subsec_nanos() as _,
        };
        let ready = poll(&mut fds, Some(&ts)).context("poll Wayland fd failed")?;
        if ready == 0 {
            drop(guard);
            bail!("Wayland screencopy timed out");
        }
        guard.read()?;
    }
    event_queue.dispatch_pending(state)?;
    Ok(())
}

fn capture_output_shm(
    event_queue: &mut wayland_client::EventQueue<WaylandClientState>,
    state: &mut WaylandClientState,
    qh: &QueueHandle<WaylandClientState>,
    output_name: Option<&str>,
    overlay_cursor: bool,
) -> anyhow::Result<CapturedFrame> {
    let mgr = state
        .screencopy_mgr
        .as_ref()
        .context("compositor missing zwlr_screencopy_manager_v1")?
        .clone();
    let shm = state
        .shm
        .as_ref()
        .context("compositor missing wl_shm")?
        .clone();
    let wl_out = state
        .outputs
        .values()
        .find(|o| output_name.is_none_or(|n| o.info.name == n))
        .map(|o| o.wl_output.clone())
        .context("no matching Wayland output for screencopy")?;

    state.capture = CaptureBufferSpec::default();
    let frame = mgr.capture_output(i32::from(overlay_cursor), &wl_out, qh, ());
    let deadline = Instant::now() + Duration::from_secs(2);

    while !state.capture.buffer_done && !state.capture.failed {
        if let Err(err) = dispatch_with_timeout(event_queue, state, deadline) {
            frame.destroy();
            return Err(err);
        }
    }
    if state.capture.failed {
        frame.destroy();
        bail!("screencopy buffer negotiation failed");
    }

    let fmt = state.capture.format.unwrap_or(wl_shm::Format::Argb8888);
    let w = state.capture.width.max(1);
    let h = state.capture.height.max(1);
    let stride = state.capture.stride.max(w * 4);
    let byte_len = (stride as usize) * (h as usize);

    let memfd = create_memfd("koma-portal-capture", byte_len)?;
    let pool = shm.create_pool(memfd.as_fd(), byte_len as i32, qh, ());
    let wl_buf = pool.create_buffer(0, w as i32, h as i32, stride as i32, fmt, qh, ());

    frame.copy(&wl_buf);
    while !state.capture.ready && !state.capture.failed {
        if let Err(err) = dispatch_with_timeout(event_queue, state, deadline) {
            wl_buf.destroy();
            pool.destroy();
            frame.destroy();
            return Err(err);
        }
    }

    wl_buf.destroy();
    pool.destroy();
    frame.destroy();

    if state.capture.failed {
        bail!("screencopy frame copy failed");
    }

    let mut file = File::from(memfd);
    file.seek(SeekFrom::Start(0))?;
    let mut pixels = vec![0_u8; byte_len];
    file.read_exact(&mut pixels)?;

    Ok(CapturedFrame {
        width: w,
        height: h,
        stride,
        format: u32::from(fmt),
        pixels,
    })
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandClientState {
    fn event(
        state: &mut Self,
        proxy: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_output" && !state.outputs.contains_key(&name) {
                    let wl_out: wl_output::WlOutput = proxy.bind(name, version.min(4), qh, name);
                    let xdg_out = state
                        .xdg_output_mgr
                        .as_ref()
                        .map(|m| m.get_xdg_output(&wl_out, qh, name));
                    state.outputs.insert(
                        name,
                        OutputSlot {
                            wl_output: wl_out,
                            _xdg_output: xdg_out,
                            info: OutputInfo {
                                name: format!("output-{name}"),
                                description: String::new(),
                                logical_x: 0,
                                logical_y: 0,
                                logical_w: 1920,
                                logical_h: 1080,
                                pixel_w: 1920,
                                pixel_h: 1080,
                            },
                        },
                    );
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.remove(&name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for WaylandClientState {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let Some(slot) = state.outputs.get_mut(id) {
            match event {
                wl_output::Event::Mode { width, height, .. } => {
                    slot.info.pixel_w = width.max(1) as u32;
                    slot.info.pixel_h = height.max(1) as u32;
                }
                wl_output::Event::Name { name } => {
                    slot.info.name = name;
                }
                wl_output::Event::Description { description } => {
                    slot.info.description = description;
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for WaylandClientState {
    fn event(
        state: &mut Self,
        _proxy: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        id: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let Some(slot) = state.outputs.get_mut(id) {
            match event {
                zxdg_output_v1::Event::LogicalPosition { x, y } => {
                    slot.info.logical_x = x;
                    slot.info.logical_y = y;
                }
                zxdg_output_v1::Event::LogicalSize { width, height } => {
                    slot.info.logical_w = width.max(1);
                    slot.info.logical_h = height.max(1);
                }
                zxdg_output_v1::Event::Name { name } => {
                    slot.info.name = name;
                }
                zxdg_output_v1::Event::Description { description } => {
                    slot.info.description = description;
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for WaylandClientState {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if let wayland_client::WEnum::Value(fmt) = format {
                    state.capture.format = Some(fmt);
                }
                state.capture.width = width;
                state.capture.height = height;
                state.capture.stride = stride;
                if _proxy.version() < 3 {
                    state.capture.buffer_done = true;
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                state.capture.buffer_done = true;
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.capture.ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.capture.failed = true;
            }
            _ => {}
        }
    }
}

delegate_noop!(WaylandClientState: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
delegate_noop!(WaylandClientState: ignore wl_shm::WlShm);
delegate_noop!(WaylandClientState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WaylandClientState: ignore wayland_client::protocol::wl_buffer::WlBuffer);
delegate_noop!(WaylandClientState: ignore wl_seat::WlSeat);
delegate_noop!(WaylandClientState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(WaylandClientState: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(WaylandClientState: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
delegate_noop!(WaylandClientState: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(WaylandClientState: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
