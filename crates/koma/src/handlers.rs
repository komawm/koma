//! Smithay Wayland protocol trait implementations & sandbox privilege guards for `KomaState`
//! (modeled on `niri/src/handlers/{mod,compositor,xdg_shell,virtual_keyboard}.rs`).

use std::{fs::File, io::Write as _, os::fd::OwnedFd, sync::Arc, thread};

use rustix::fs::{OFlags, fcntl_setfl};
use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        input::InputEvent,
        renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state},
    },
    desktop::{LayerSurface, PopupKind, Window, WindowSurfaceType, layer_map_for_output},
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Resource as _,
            protocol::{
                wl_buffer::WlBuffer, wl_output::WlOutput, wl_seat::WlSeat, wl_surface::WlSurface,
            },
        },
    },
    utils::{SERIAL_COUNTER, Serial},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface, with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::{FractionalScaleHandler, with_fractional_scale},
        output::OutputHandler,
        pointer_constraints::PointerConstraintsHandler,
        selection::{
            SelectionHandler, SelectionTarget,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            ext_data_control::{
                DataControlHandler as ExtDataControlHandler,
                DataControlState as ExtDataControlState,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
            wlr_data_control::{
                DataControlHandler as WlrDataControlHandler,
                DataControlState as WlrDataControlState,
            },
        },
        session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker},
        shell::{
            wlr_layer::{
                Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState},
        },
        shm::{ShmHandler, ShmState},
        virtual_keyboard::{VirtualKeyboardBackend, VirtualKeyboardHandler},
    },
};
use tracing::{info, warn};

use crate::{
    macro_runtime::notify_workspace_surface_damage,
    security_context::is_surface_from_restricted_client,
    state::{ClientState, KomaState, UnmappedToplevel, read_toplevel_metadata},
};

/// Checks whether a surface currently holds a committed buffer in Smithay's renderer state.
///
/// Must be queried after `on_commit_buffer_handler`, which consumes `SurfaceAttributes::buffer`.
fn is_surface_mapped(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

impl BufferHandler for KomaState {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl CompositorHandler for KomaState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("ClientState initialized on client connect")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // 1. Import committed wl_shm / linux-dmabuf buffer into Smithay's RendererSurfaceState
        on_commit_buffer_handler::<Self>(surface);
        if is_sync_subsurface(surface) {
            return;
        }

        // Resolve child `wl_subsurface` commits (e.g., `foot` CSD borders/titlebars) to their root.
        let mut root_surface = surface.clone();
        while let Some(parent) = get_parent(&root_surface) {
            root_surface = parent;
        }

        // 2. Track popup state commits & reset active workspace damage-settle timer
        self.popups.commit(surface);
        let active_ws = self.active_ws_id.clone();
        notify_workspace_surface_damage(self, &active_ws);

        // 2b. Arrange wlr-layer-shell surfaces (`koma-overview`, `waybar`, `mako`) on commit
        if let Some(output) = self.primary_output() {
            let is_layer = {
                let mut map = layer_map_for_output(&output);
                if map
                    .layer_for_surface(&root_surface, WindowSurfaceType::TOPLEVEL)
                    .is_some()
                {
                    if surface == &root_surface {
                        map.arrange();
                        if let Some(l) =
                            map.layer_for_surface(&root_surface, WindowSurfaceType::TOPLEVEL)
                        {
                            l.layer_surface().send_pending_configure();
                        }
                    }
                    true
                } else {
                    false
                }
            };
            if is_layer {
                self.queue_redraw();
                return;
            }
        }

        // 3. Two-phase Unmapped -> Mapped window transition (`niri/src/handlers/compositor.rs`)
        if surface == &root_surface
            && let Some(mut unmapped) = self.unmapped_windows.remove(&root_surface)
        {
            if is_surface_mapped(&root_surface) {
                unmapped.window.on_commit();
                self.promote_unmapped_to_mapped(unmapped);
                return;
            }

            if !unmapped.initial_configure_sent {
                if let Some(toplevel) = unmapped.window.toplevel() {
                    self.send_initial_configure(toplevel);
                }
                unmapped.initial_configure_sent = true;
            }
            self.unmapped_windows.insert(root_surface, unmapped);
            return;
        }

        // 4. Refresh live metadata, handle explicit unmap, & schedule redraw for mapped windows
        if let Some(idx) = self.mapped_windows.iter().position(|m| {
            m.window
                .toplevel()
                .is_some_and(|t| t.wl_surface() == &root_surface)
        }) {
            if surface == &root_surface && !is_surface_mapped(&root_surface) {
                let removed = self.mapped_windows.remove(idx);
                self.window_animations
                    .borrow_mut()
                    .remove_immediately(removed.win_id);
                let now_ms = self.elapsed_ms();
                if let Some(ws) = self.workspaces.get_mut(&removed.ws_id) {
                    ws.remove_window(removed.win_id, now_ms);
                }
                self.sync_workspace_geometries(&removed.ws_id);
                self.unmapped_windows.insert(
                    root_surface,
                    UnmappedToplevel {
                        win_id: removed.win_id,
                        window: removed.window,
                        initial_configure_sent: false,
                    },
                );
                if removed.ws_id == self.active_ws_id
                    || Some(&removed.ws_id) == self.active_special_ws_id.as_ref()
                {
                    self.focus_active_deck_window();
                    self.queue_redraw();
                }
                return;
            }

            self.mapped_windows[idx].window.on_commit();
            let (app_id, title) = read_toplevel_metadata(&root_surface);
            self.mapped_windows[idx].app_id = app_id;
            self.mapped_windows[idx].title = title;
            let ws_id = self.mapped_windows[idx].ws_id.clone();
            let now = self.elapsed_ms();
            if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                ws.frame_clock.record_activity(now);
            }
            notify_workspace_surface_damage(self, &ws_id);
            if ws_id == self.active_ws_id || Some(&ws_id) == self.active_special_ws_id.as_ref() {
                self.queue_redraw();
            }
        }
    }
}

impl ShmHandler for KomaState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for KomaState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self.import_client_dmabuf(&dmabuf) {
            let _ = notifier.successful::<Self>();
        } else {
            notifier.failed();
        }
    }
}

impl XdgShellHandler for KomaState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let win_id = self.allocate_window_id();
        let wl_surface = surface.wl_surface().clone();
        let window = Window::new_wayland_window(surface);
        self.unmapped_windows.insert(
            wl_surface,
            UnmappedToplevel {
                win_id,
                window,
                initial_configure_sent: false,
            },
        );
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            warn!("failed tracking xdg_popup: {err:?}");
        }
        self.queue_redraw();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn move_request(&mut self, surface: ToplevelSurface, _seat: WlSeat, serial: Serial) {
        let wl_surface = surface.wl_surface();
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let same_client = start_data
            .focus
            .as_ref()
            .is_some_and(|(focus, _)| focus.id().same_client_as(&wl_surface.id()));
        if !same_client {
            return;
        }
        let Some(win_id) = self
            .mapped_windows
            .iter()
            .find(|m| {
                m.window
                    .toplevel()
                    .is_some_and(|t| t.wl_surface() == wl_surface)
            })
            .map(|m| m.win_id)
        else {
            return;
        };
        let _ = crate::pointer_grabs::start_csd_move_grab(self, win_id, start_data, serial);
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let wl_surface = surface.wl_surface();
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        if !pointer.has_grab(serial) {
            return;
        }
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let same_client = start_data
            .focus
            .as_ref()
            .is_some_and(|(focus, _)| focus.id().same_client_as(&wl_surface.id()));
        if !same_client {
            return;
        }
        let Some(win_id) = self
            .mapped_windows
            .iter()
            .find(|m| {
                m.window
                    .toplevel()
                    .is_some_and(|t| t.wl_surface() == wl_surface)
            })
            .map(|m| m.win_id)
        else {
            return;
        };
        let _ =
            crate::pointer_grabs::start_csd_resize_grab(self, win_id, start_data, serial, edges);
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        _positioner: PositionerState,
        token: u32,
    ) {
        surface.send_repositioned(token);
        self.queue_redraw();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        self.unmapped_windows.remove(wl_surface);

        if let Some(pos) = self.mapped_windows.iter().position(|m| {
            m.window
                .toplevel()
                .is_some_and(|t| t.wl_surface() == wl_surface)
        }) {
            let removed = self.mapped_windows.remove(pos);
            self.window_animations
                .borrow_mut()
                .remove_immediately(removed.win_id);
            let now_ms = self.elapsed_ms();
            if let Some(ws) = self.workspaces.get_mut(&removed.ws_id) {
                ws.remove_window(removed.win_id, now_ms);
            }
            self.sync_workspace_geometries(&removed.ws_id);
            if removed.ws_id == self.active_ws_id
                || Some(&removed.ws_id) == self.active_special_ws_id.as_ref()
            {
                self.focus_active_deck_window();
                self.queue_redraw();
            }
        }
    }
}

impl SeatHandler for KomaState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.queue_redraw();
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());

        // Jailed background container clients never receive host physical clipboard focus;
        // unrestricted and interactive Flatpak clients receive standard focused `wl_data_device`,
        // while `primary_selection` is strictly withheld from all `restricted` clients!
        let allow_data_device = client.as_ref().is_some_and(|c| {
            c.get_data::<ClientState>()
                .is_some_and(|cs| cs.jailed_workspace.is_none())
        });
        let allow_primary = client.as_ref().is_some_and(|c| {
            c.get_data::<ClientState>()
                .is_some_and(|cs| !cs.restricted && !cs.primary_selection_disabled)
        });

        set_data_device_focus(
            dh,
            seat,
            allow_data_device.then(|| client.clone()).flatten(),
        );
        set_primary_focus(dh, seat, allow_primary.then_some(client).flatten());
    }
}

impl SelectionHandler for KomaState {
    type SelectionUserData = Arc<[u8]>;

    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        // Clear O_NONBLOCK and write selection payload on a worker thread (`niri/src/handlers/mod.rs`)
        // so slow or wedged clients can never block Koma's 240 Hz calloop!
        let buf = user_data.clone();
        thread::spawn(move || {
            let _ = fcntl_setfl(&fd, OFlags::empty());
            let _ = File::from(fd).write_all(&buf);
        });
    }
}

impl DataDeviceHandler for KomaState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl WaylandDndGrabHandler for KomaState {}

impl PrimarySelectionHandler for KomaState {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl WlrDataControlHandler for KomaState {
    fn data_control_state(&mut self) -> &mut WlrDataControlState {
        &mut self.wlr_data_control_state
    }
}

impl ExtDataControlHandler for KomaState {
    fn data_control_state(&mut self) -> &mut ExtDataControlState {
        &mut self.ext_data_control_state
    }
}

impl SessionLockHandler for KomaState {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        self.is_session_locked = true;
        confirmation.lock();
        info!("host Wayland session locked via ext-session-lock-v1");
        self.queue_redraw();
    }

    fn unlock(&mut self) {
        self.is_session_locked = false;
        info!("host Wayland session unlocked");
        self.queue_redraw();
    }

    fn new_surface(&mut self, surface: LockSurface, _output: WlOutput) {
        if let Some(out) = self.primary_output()
            && let Some(mode) = out.current_mode()
        {
            surface.with_pending_state(|states| {
                states.size = Some((mode.size.w as u32, mode.size.h as u32).into());
            });
            surface.send_configure();
        }
    }
}

impl VirtualKeyboardHandler for KomaState {
    fn process_virtual_keyboard_event(&mut self, event: InputEvent<VirtualKeyboardBackend>) {
        use smithay::backend::input::{Event as _, KeyboardKeyEvent as _};
        use smithay::input::keyboard::FilterResult;

        if let InputEvent::Keyboard { event } = event
            && let Some(keyboard) = self.seat.get_keyboard()
        {
            let serial = SERIAL_COUNTER.next_serial();
            let _ = keyboard.input::<(), _>(
                self,
                event.key_code(),
                event.state(),
                serial,
                event.time(),
                |_, _, _| FilterResult::Forward,
            );
        }
    }
}

impl OutputHandler for KomaState {
    fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {}
}

impl FractionalScaleHandler for KomaState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self
            .primary_output()
            .map(|o| o.current_scale().fractional_scale())
            .unwrap_or(1.0);
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

impl WlrLayerShellHandler for KomaState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        _wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // Defense-in-depth double gate: even though `WlrLayerShellState::new_with_filter` hides
        // `zwlr_layer_shell_v1` from `restricted` clients, immediately reject any restricted surface.
        if is_surface_from_restricted_client(self, surface.wl_surface()) {
            warn!(
                namespace = %namespace,
                "rejected zwlr_layer_shell_v1 surface from restricted sandbox client"
            );
            surface.send_close();
            return;
        }

        let Some(output) = self.primary_output() else {
            surface.send_close();
            return;
        };
        let mut map = layer_map_for_output(&output);
        let _ = map.map_layer(&LayerSurface::new(surface, namespace));
        map.arrange();
        self.queue_redraw();
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        if let Some(output) = self.primary_output() {
            let mut map = layer_map_for_output(&output);
            let found = map
                .layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = found {
                map.unmap_layer(&layer);
                map.arrange();
            }
        }
        self.queue_redraw();
    }
}

impl PointerConstraintsHandler for KomaState {}

// Unified Smithay `Dispatch2` / `GlobalDispatch2` delegation (`niri/src/niri.rs` line 488,
// pinned Smithay rev `65aa86fa5236e41775c313408102a5d5c2d1e2ca`).
smithay::delegate_dispatch2!(KomaState);
