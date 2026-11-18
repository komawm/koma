//! Smithay Wayland protocol trait implementations & sandbox privilege guards for `KomaState`
//! (modeled on `niri/src/handlers/{mod,compositor,xdg_shell,virtual_keyboard}.rs`).

use std::{fs::File, io::Write as _, os::fd::OwnedFd, sync::Arc, thread};

use rustix::fs::{OFlags, fcntl_setfl};
use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        input::{
            AbsolutePositionEvent as _, Event as _, InputEvent, PointerButtonEvent as _,
            PointerMotionEvent as _, TabletToolDescriptor,
        },
        renderer::utils::on_commit_buffer_handler,
    },
    desktop::{
        LayerSurface, PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window,
        WindowSurfaceType, find_popup_root_surface, layer_map_for_output,
    },
    input::{
        Seat, SeatHandler, SeatState,
        dnd::{DndGrabHandler, DndTarget, GrabType, Source},
        pointer::{CursorImageStatus, Focus},
        tablet::TabletSeatHandler,
    },
    output::Output,
    reexports::{
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
            shell::server::xdg_toplevel,
        },
        wayland_server::{
            Client, Resource as _,
            protocol::{
                wl_buffer::WlBuffer, wl_output::WlOutput, wl_seat::WlSeat, wl_surface::WlSurface,
            },
        },
    },
    utils::{IsAlive as _, SERIAL_COUNTER, Serial},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface, with_states,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::{FractionalScaleHandler, with_fractional_scale},
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState,
            KeyboardShortcutsInhibitor,
        },
        output::OutputHandler,
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
        virtual_pointer::{VirtualPointerBackend, VirtualPointerHandler},
    },
};
use tracing::{info, warn};

use crate::{
    macro_runtime::notify_workspace_surface_damage,
    security_context::is_surface_from_restricted_client,
    state::{
        ClientState, KomaState, UnmappedToplevel, is_surface_mapped, output_logical_viewport,
        read_toplevel_metadata, read_toplevel_min_max_size,
    },
};

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
        self.popups.cleanup();
        self.cleanup_ime_popups();
        if let Some(PopupKind::Xdg(ref popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
        {
            self.unconstrain_xdg_popup(popup);
            let _ = popup.send_configure();
        }

        if let Some(ime_popup) = self
            .ime_popups
            .iter()
            .find(|p| p.wl_surface() == &root_surface)
            .cloned()
        {
            if self.is_locked_or_abandoned() {
                self.clear_ime_state_for_lock();
            } else {
                self.unconstrain_ime_popup(&ime_popup);
                self.queue_redraw();
            }
            return;
        }

        // 2a. Apply committed cursor hotspot buffer_delta & redraw when active cursor commits
        if self
            .cursor_manager
            .on_surface_commit(surface, &root_surface)
        {
            self.queue_redraw();
            return;
        }

        // 2a2. Move the active drag icon by its committed `wl_surface` attach/offset delta
        if self
            .dnd_icon()
            .is_some_and(|icon| icon.surface() == &root_surface)
        {
            let delta = crate::dnd::take_buffer_delta(&root_surface);
            if let Some(delta) = delta
                && let Some(icon) = self.dnd_drag.as_mut().and_then(|drag| drag.icon_mut())
            {
                icon.apply_buffer_delta(delta);
            }
            self.queue_redraw();
            return;
        }

        let active_ws = self.active_ws_id.clone();
        notify_workspace_surface_damage(self, &active_ws);

        // 2b. Reconcile active LockSurface commits
        if let Some(lock_out) = self
            .session_lock
            .surfaces
            .iter()
            .find(|(_, ls)| ls.wl_surface() == &root_surface)
            .map(|(o, _)| o.clone())
        {
            let (w, h, _) = output_logical_viewport(&lock_out);
            if let Some(ls) = self.session_lock.surfaces.get(&lock_out) {
                let needs_resize = ls.with_pending_state(|st| {
                    let expected = Some((w, h).into());
                    if st.size != expected {
                        st.size = expected;
                        true
                    } else {
                        false
                    }
                });
                if needs_resize {
                    ls.send_configure();
                }
            }
            self.reconcile_keyboard_focus();
            self.reconcile_keyboard_shortcuts_inhibitors();
            self.reconcile_idle_inhibitors();
            self.maybe_activate_pointer_constraint();
            self.queue_redraw();
            return;
        }

        // 2c. Arrange wlr-layer-shell surfaces (`koma-overview`, `waybar`, `mako`) on commit
        let mut matched_layer_output: Option<Output> = None;
        for output in self.all_outputs() {
            let mut map = layer_map_for_output(&output);
            if let Some(layer) = map
                .layer_for_surface(&root_surface, WindowSurfaceType::ALL)
                .cloned()
            {
                if surface == &root_surface {
                    map.arrange();
                    layer.layer_surface().send_pending_configure();
                }
                drop(map);
                matched_layer_output = Some(output);
                break;
            }
        }
        if let Some(output) = matched_layer_output {
            if surface == &root_surface {
                self.sync_output_workarea(&output);
            }
            self.reconcile_keyboard_focus();
            self.reconcile_keyboard_shortcuts_inhibitors();
            self.reconcile_idle_inhibitors();
            self.maybe_activate_pointer_constraint();
            self.queue_redraw();
            return;
        }

        // 3. Two-phase Unmapped -> Mapped window transition (`niri/src/handlers/compositor.rs`)
        if surface == &root_surface
            && let Some(mut unmapped) = self.unmapped_windows.remove(&root_surface)
        {
            if is_surface_mapped(&root_surface) {
                unmapped.window.on_commit();
                unmapped.icon =
                    crate::desktop_protocols::extract_validated_toplevel_icon(&root_surface);
                self.promote_unmapped_to_mapped(unmapped);
                self.reconcile_idle_inhibitors();
                self.maybe_activate_pointer_constraint();
                self.sync_foreign_toplevels();
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
                crate::foreign_toplevel::close_foreign_toplevel(self, removed.win_id);
                // The client already released its buffer on the unmap commit, so this only keeps
                // a fading frame when a snapshot is still possible; otherwise state is freed now.
                self.begin_close_animation(
                    removed.win_id,
                    &removed.ws_id,
                    &removed.window,
                    removed.rect,
                );
                self.window_frames.remove(&removed.win_id);
                let now_ms = self.elapsed_ms();
                if let Some(ws) = self.workspaces.get_mut(&removed.ws_id) {
                    ws.remove_window(removed.win_id, now_ms);
                }
                self.sync_workspace_geometries(&removed.ws_id);
                // Losing a window can empty the workspace; a bar with `ignore-hidden` must drop it.
                crate::ext_workspace::sync_ext_workspaces(self);
                self.unmapped_windows.insert(
                    root_surface,
                    UnmappedToplevel {
                        win_id: removed.win_id,
                        window: removed.window,
                        initial_configure_sent: false,
                        decoration_mode: removed.decoration_mode,
                        icon: removed.icon,
                    },
                );
                self.break_active_pointer_constraints();
                self.reconcile_idle_inhibitors();
                self.sync_foreign_toplevels();
                let is_offscreen = self.workspaces.get(&removed.ws_id).is_some_and(|ws| {
                    matches!(
                        ws.presentation,
                        koma_model::PresentationTarget::Offscreen { .. }
                    )
                });
                if !is_offscreen {
                    self.emit_lua_event(
                        "window.closed",
                        serde_json::json!({
                            "id": removed.win_id.slot,
                            "app_id": removed.app_id,
                            "workspace": removed.ws_id.to_string(),
                        }),
                    );
                }
                if removed.ws_id == self.active_ws_id
                    || Some(&removed.ws_id) == self.active_special_ws_id.as_ref()
                {
                    self.focus_active_deck_window();
                    self.queue_redraw();
                }
                return;
            }

            self.mapped_windows[idx].window.on_commit();
            self.refresh_toplevel_icon(&root_surface);
            let (app_id, title) = read_toplevel_metadata(&root_surface);
            self.mapped_windows[idx].app_id = app_id;
            self.mapped_windows[idx].title = title;
            let win_id = self.mapped_windows[idx].win_id;
            let ws_id = self.mapped_windows[idx].ws_id.clone();
            // Clients may (re)advertise `xdg_toplevel` size constraints at any commit; Chromium
            // sends `set_min_size` right after every settled configure. A tiled box below that
            // minimum can never be honoured, so refresh the hints and re-run the geometry sync.
            let (min_sz, max_sz) = read_toplevel_min_max_size(&root_surface);
            let hints_changed = self.workspaces.get_mut(&ws_id).is_some_and(|ws| {
                ws.set_size_hints(win_id, koma_model::SizeHints::from_min_max(min_sz, max_sz))
            });
            let now = self.elapsed_ms();
            if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                ws.frame_clock.record_activity(now);
            }
            if hints_changed {
                self.sync_workspace_geometries(&ws_id);
            }
            // Cache the frame while the client's buffer is still readable; a later unmap commit
            // attaches a null buffer and destroy releases the pixels entirely.
            self.refresh_window_frame(win_id);
            notify_workspace_surface_damage(self, &ws_id);
            self.reconcile_idle_inhibitors();
            self.maybe_activate_pointer_constraint();
            self.sync_foreign_toplevels();
            if ws_id == self.active_ws_id || Some(&ws_id) == self.active_special_ws_id.as_ref() {
                self.queue_redraw();
            }
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.idle_inhibitors.remove(surface);
        if self
            .dnd_icon()
            .is_some_and(|icon| icon.surface() == surface)
        {
            // A drag icon is never mapped, so its destruction can only mean the drag source gave
            // up (or the client vanished): release the grab instead of leaving the seat wedged.
            self.cancel_client_drag("drag icon surface destroyed");
        }
        if matches!(
            self.cursor_manager.status(),
            CursorImageStatus::Surface(active) if active == surface
        ) {
            self.set_cursor_status(CursorImageStatus::default_named());
        }
        if self.session_lock.remove_wl_surface(surface) {
            self.reconcile_session_lock();
            self.reconcile_keyboard_focus();
            self.queue_redraw();
        }
        if self.focused_ondemand_layer.as_ref() == Some(surface) {
            self.focused_ondemand_layer = None;
            self.reconcile_keyboard_focus();
        }
        self.reconcile_idle_inhibitors();
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
                decoration_mode: DecorationMode::ServerSide,
                icon: None,
            },
        );
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_xdg_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            warn!("failed tracking xdg_popup: {err:?}");
        }
        self.queue_redraw();
    }

    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let root_in_tree = self
            .mapped_windows
            .iter()
            .any(|m| m.window.toplevel().is_some_and(|t| *t.wl_surface() == root))
            || self.all_outputs().into_iter().any(|o| {
                layer_map_for_output(&o)
                    .layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)
                    .is_some()
            });
        if !root_in_tree {
            return;
        }
        if let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) {
            if let Some(keyboard) = self.kb() {
                if keyboard.is_grabbed()
                    && !(keyboard.has_grab(serial)
                        || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
                {
                    grab.ungrab(PopupUngrabStrategy::All);
                    return;
                }
                keyboard.set_focus(self, grab.current_grab(), serial);
                keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
            }
            if let Some(pointer) = self.ptr() {
                if pointer.is_grabbed()
                    && !(pointer.has_grab(serial)
                        || pointer
                            .has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
                {
                    grab.ungrab(PopupUngrabStrategy::All);
                    return;
                }
                pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
            }
        }
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: WlSeat, serial: Serial) {
        let wl_surface = surface.wl_surface();
        // `start_csd_*_grab` re-acquires the pointer itself, so this clone must not hold the
        // pointer scope across the call.
        let Some(pointer) = self.pointer_handle() else {
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
        // `start_csd_*_grab` re-acquires the pointer itself, so this clone must not hold the
        // pointer scope across the call.
        let Some(pointer) = self.pointer_handle() else {
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
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|s| s.positioner = positioner);
        self.unconstrain_xdg_popup(&surface);
        surface.send_repositioned(token);
        let _ = surface.send_configure();
        self.queue_redraw();
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        if let Some(win_id) = find_mapped_window_id(self, wl_surface) {
            self.set_window_maximized(win_id, true);
        } else {
            surface.with_pending_state(|s| s.states.set(xdg_toplevel::State::Maximized));
            surface.send_configure();
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        if let Some(win_id) = find_mapped_window_id(self, wl_surface) {
            self.set_window_maximized(win_id, false);
        } else {
            surface.with_pending_state(|s| s.states.unset(xdg_toplevel::State::Maximized));
            surface.send_configure();
        }
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, output: Option<WlOutput>) {
        let wl_surface = surface.wl_surface();
        let target_output = output.and_then(|o| Output::from_resource(&o));
        if let Some(win_id) = find_mapped_window_id(self, wl_surface) {
            self.set_window_fullscreen(win_id, true, target_output);
        } else {
            surface.with_pending_state(|s| s.states.set(xdg_toplevel::State::Fullscreen));
            surface.send_configure();
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let wl_surface = surface.wl_surface();
        if let Some(win_id) = find_mapped_window_id(self, wl_surface) {
            self.set_window_fullscreen(win_id, false, None);
        } else {
            surface.with_pending_state(|s| s.states.unset(xdg_toplevel::State::Fullscreen));
            surface.send_configure();
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(win_id) = find_mapped_window_id(self, surface.wl_surface()) {
            self.set_window_minimized(win_id, true);
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        sync_toplevel_metadata_change(self, surface.wl_surface());
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        sync_toplevel_metadata_change(self, surface.wl_surface());
    }

    fn parent_changed(&mut self, _surface: ToplevelSurface) {
        self.reconcile_keyboard_shortcuts_inhibitors();
        self.sync_foreign_toplevels();
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
            crate::foreign_toplevel::close_foreign_toplevel(self, removed.win_id);
            // Smithay dispatches `toplevel_destroyed` before it clears the surface's cached state,
            // so the client's last committed buffer can still be captured here.
            self.begin_close_animation(
                removed.win_id,
                &removed.ws_id,
                &removed.window,
                removed.rect,
            );
            self.window_frames.remove(&removed.win_id);
            let now_ms = self.elapsed_ms();
            if let Some(ws) = self.workspaces.get_mut(&removed.ws_id) {
                ws.remove_window(removed.win_id, now_ms);
            }
            self.sync_workspace_geometries(&removed.ws_id);
            // A destroyed toplevel can leave its workspace empty and therefore hidden in bars.
            crate::ext_workspace::sync_ext_workspaces(self);
            self.break_active_pointer_constraints();
            self.reconcile_idle_inhibitors();
            self.sync_foreign_toplevels();
            let is_offscreen = self.workspaces.get(&removed.ws_id).is_some_and(|ws| {
                matches!(
                    ws.presentation,
                    koma_model::PresentationTarget::Offscreen { .. }
                )
            });
            if !is_offscreen {
                self.emit_lua_event(
                    "window.closed",
                    serde_json::json!({
                        "id": removed.win_id.slot,
                        "app_id": removed.app_id,
                        "workspace": removed.ws_id.to_string(),
                    }),
                );
            }
            if removed.ws_id == self.active_ws_id
                || Some(&removed.ws_id) == self.active_special_ws_id.as_ref()
            {
                self.focus_active_deck_window();
                self.queue_redraw();
            }
        }
    }
}

fn find_mapped_window_id(
    state: &KomaState,
    wl_surface: &WlSurface,
) -> Option<koma_proto::WindowId> {
    state
        .mapped_windows
        .iter()
        .find(|m| {
            m.window
                .toplevel()
                .is_some_and(|t| t.wl_surface() == wl_surface)
        })
        .map(|m| m.win_id)
}

fn sync_toplevel_metadata_change(state: &mut KomaState, wl_surface: &WlSurface) {
    let (app_id, title) = read_toplevel_metadata(wl_surface);
    if let Some(mapped) = state.mapped_windows.iter_mut().find(|m| {
        m.window
            .toplevel()
            .is_some_and(|t| t.wl_surface() == wl_surface)
    }) {
        mapped.app_id = app_id;
        mapped.title = title;
        state.sync_foreign_toplevels();
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
        // Smithay delivers this from `PointerInnerHandle` while holding the pointer mutex.
        let _held = crate::seat_guard::pointer_callback();
        self.set_cursor_status(image);
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        // Smithay calls this from `KeyboardGrab::set_focus` with the keyboard mutex held, so any
        // `self.kb()` below would self-deadlock; the guard turns that into a logged soft failure.
        let _held = crate::seat_guard::keyboard_callback();
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

        if self.is_locked_or_abandoned() {
            // `focus_changed` runs while Smithay holds the keyboard mutex: only the IME half of
            // the lock barrier is legal here. Shortcut inhibitors are reconciled with the explicit
            // `focused` argument below, which avoids re-entering `KeyboardHandle::current_focus()`
            // (komawm/koma#37).
            self.clear_ime_focus_for_lock();
        } else if !self.ime_popups.is_empty() {
            let popups = self.ime_popups.clone();
            for popup in &popups {
                self.unconstrain_ime_popup(popup);
            }
            self.queue_redraw();
        }

        // Smithay calls this while it holds the keyboard mutex, so the seat guard is tracking the
        // callback scope: the pointer focus must be reduced to its root in its own acquisition
        // scope, because `break_active_pointer_constraints` acquires the pointer again. Nesting
        // that acquisition used to be swallowed by the guard, which also meant a focus change
        // never actually broke a locked pointer constraint (komawm/koma#64).
        let pointer_root = self
            .ptr()
            .and_then(|pointer| pointer.current_focus())
            .map(|ptr_surf| {
                let mut root = ptr_surf;
                while let Some(parent) = get_parent(&root) {
                    root = parent;
                }
                root
            });
        if let Some(ptr_root) = pointer_root {
            let same_root = focused.is_some_and(|f| {
                let mut kb_root = f.clone();
                while let Some(p) = get_parent(&kb_root) {
                    kb_root = p;
                }
                kb_root == ptr_root
            });
            if !same_root {
                self.break_active_pointer_constraints();
            }
        }
        self.reconcile_keyboard_shortcuts_inhibitors_with_focus(focused);
        self.sync_foreign_toplevels();
    }

    fn led_state_changed(
        &mut self,
        _seat: &Seat<Self>,
        led_state: smithay::input::keyboard::LedState,
    ) {
        #[cfg(feature = "smithay-tty")]
        if let Some(crate::state::ActiveBackend::Tty(tty)) = self.backend.as_mut() {
            tty.sync_keyboard_leds(led_state);
        }
        #[cfg(not(feature = "smithay-tty"))]
        let _ = led_state;
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

impl WaylandDndGrabHandler for KomaState {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        // `wl_data_device.start_drag` must never reach Smithay's default handler: it cancels the
        // source immediately, which is what broke tab reordering and every other client DnD.
        let _ = crate::dnd::start_client_drag(self, source, icon, seat, serial, type_);
    }
}

impl DndGrabHandler for KomaState {
    fn dropped(
        &mut self,
        target: Option<DndTarget<'_, Self>>,
        validated: bool,
        seat: Seat<Self>,
        location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        let _ = (target, seat);
        tracing::debug!(
            validated,
            x = location.x,
            y = location.y,
            "client drag-and-drop dropped"
        );
        // Smithay already reported the outcome to the source: an accepted offer received
        // `drop_performed` (and `finished` once the target sends `wl_data_offer.finish`), a
        // rejected one was cancelled. Only the local icon state is Koma's to clear.
        crate::dnd::finish_client_drag(self, "drop completed");
    }

    fn cancelled(
        &mut self,
        seat: Seat<Self>,
        location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        let _ = seat;
        tracing::debug!(
            x = location.x,
            y = location.y,
            "client drag-and-drop cancelled"
        );
        crate::dnd::finish_client_drag(self, "grab cancelled");
    }
}

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
        let ext_lock = confirmation.ext_session_lock().clone();
        if self.session_lock.try_lock(Some(ext_lock)) {
            self.clear_inputs_for_lock();
            confirmation.lock();
            info!("host Wayland session locked via ext-session-lock-v1");
            self.reconcile_keyboard_focus();
            self.reconcile_keyboard_shortcuts_inhibitors();
            self.reconcile_idle_inhibitors();
            self.sync_foreign_toplevels();
            self.queue_redraw();
        } else {
            warn!("rejected concurrent ext-session-lock-v1 acquire while lock active");
        }
    }

    fn unlock(&mut self) {
        if self.session_lock.unlock() {
            self.focused_ondemand_layer = None;
            info!("host Wayland session unlocked");
            self.reconcile_keyboard_focus();
            self.reconcile_keyboard_shortcuts_inhibitors();
            self.flush_deferred_xdg_activations();
            self.reconcile_idle_inhibitors();
            self.sync_foreign_toplevels();
            crate::keybind_dispatcher::refresh_pointer_hover_focus(
                self,
                smithay::backend::input::InputTime::now(),
            );
            self.queue_redraw();
        } else {
            warn!("ignored ext-session-lock-v1 unlock request when not actively Locked");
        }
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        if !self
            .session_lock
            .lock_matches_active(surface.ext_session_lock())
        {
            warn!("ignored ext-session-lock-v1 LockSurface from inactive or defunct locker");
            return;
        }
        let Some(out) = Output::from_resource(&wl_output).or_else(|| self.primary_output()) else {
            return;
        };
        let (w, h, _) = output_logical_viewport(&out);
        surface.with_pending_state(|states| {
            states.size = Some((w, h).into());
        });
        surface.send_configure();
        self.session_lock.insert_surface(out, surface);
        self.reconcile_keyboard_focus();
        self.queue_redraw();
    }
}

impl VirtualKeyboardHandler for KomaState {
    fn process_virtual_keyboard_event(&mut self, event: InputEvent<VirtualKeyboardBackend>) {
        use smithay::backend::input::{Event as _, KeyboardKeyEvent as _};
        use smithay::input::keyboard::FilterResult;
        use smithay::wayland::virtual_keyboard::VirtualKeyboardSpecialEvent;

        self.reconcile_session_lock();
        if self.is_locked_or_abandoned() {
            if let InputEvent::DeviceRemoved { .. } = event
                && self.virtual_keyboard_keymap_active
            {
                self.force_reapply_configured_xkb_config();
            }
            return;
        }

        // This handler re-applies XKB state through `force_reapply_configured_xkb_config`, which
        // acquires the keyboard itself, so the handle is cloned without holding a scope.
        let Some(keyboard) = self.keyboard_handle() else {
            return;
        };

        let sync_vk_keymap =
            |state: &mut Self,
             device: &smithay::wayland::virtual_keyboard::VirtualKeyboardDevice| {
                if let Some(keymap) = device.keymap() {
                    if let Err(err) = keyboard.set_keymap_from_keymap_file(state, keymap) {
                        warn!("failed applying virtual keyboard keymap: {err:?}");
                    } else {
                        state.virtual_keyboard_keymap_active = true;
                    }
                } else if state.virtual_keyboard_keymap_active {
                    state.force_reapply_configured_xkb_config();
                }
            };

        match event {
            InputEvent::DeviceAdded { .. } => {}
            InputEvent::DeviceRemoved { device } => {
                if device.keymap().is_some() && self.virtual_keyboard_keymap_active {
                    self.force_reapply_configured_xkb_config();
                }
            }
            InputEvent::Keyboard { event } => {
                let device = event.device();
                sync_vk_keymap(self, &device);

                let grab_client = self
                    .input_method()
                    .and_then(|input_method| input_method.keyboard_grab_client());
                let from_active_ime_grab = grab_client.is_some() && grab_client == device.client();
                let serial = SERIAL_COUNTER.next_serial();
                if from_active_ime_grab {
                    keyboard.input_forward_bypassing_grab(
                        self,
                        event.key_code(),
                        event.state(),
                        serial,
                        event.time(),
                        true,
                    );
                } else {
                    let _ = keyboard.input_from_source(
                        device.source(),
                        self,
                        event.key_code(),
                        event.state(),
                        serial,
                        event.time(),
                        |_, _, _| FilterResult::Forward::<()>,
                    );
                }
            }
            InputEvent::Special(VirtualKeyboardSpecialEvent::KeymapChanged { device }) => {
                sync_vk_keymap(self, &device);
            }
            InputEvent::Special(VirtualKeyboardSpecialEvent::Modifiers {
                device,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            }) => {
                sync_vk_keymap(self, &device);
                keyboard.with_xkb_state(self, |mut ctx| {
                    ctx.set_modifier_mask(mods_depressed, mods_latched, mods_locked, group);
                });
            }
            _ => {}
        }
    }
}

impl OutputHandler for KomaState {
    fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {
        // A bar that binds `wl_output` after `ext_workspace_manager_v1` still needs its
        // `workspace_group.output_enter`; the sync is a no-op while no manager is bound.
        crate::ext_workspace::sync_ext_workspaces(self);
    }
}

impl FractionalScaleHandler for KomaState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.preferred_scale_for_surface(&surface);
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
        wl_output: Option<WlOutput>,
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

        let Some(output) = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.primary_output())
        else {
            surface.send_close();
            return;
        };
        {
            let mut map = layer_map_for_output(&output);
            let _ = map.map_layer(&LayerSurface::new(surface, namespace));
            map.arrange();
        }
        self.sync_output_workarea(&output);
        self.sync_output_layer_scales(&output);
        self.reconcile_idle_inhibitors();
        self.reconcile_keyboard_focus();
        self.queue_redraw();
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let mut removed_output: Option<Output> = None;
        for output in self.all_outputs() {
            let mut map = layer_map_for_output(&output);
            let found = map
                .layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = found {
                if self.focused_ondemand_layer.as_ref() == Some(layer.wl_surface()) {
                    self.focused_ondemand_layer = None;
                }
                map.unmap_layer(&layer);
                map.arrange();
                drop(map);
                removed_output = Some(output);
                break;
            }
        }
        if let Some(output) = removed_output {
            self.sync_output_workarea(&output);
        }
        self.reconcile_idle_inhibitors();
        self.reconcile_keyboard_focus();
        self.queue_redraw();
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        if crate::security_context::is_surface_from_restricted_client(self, popup.wl_surface()) {
            popup.send_popup_done();
            return;
        }
        self.unconstrain_xdg_popup(&popup);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(popup)) {
            warn!("failed tracking layer xdg_popup: {err:?}");
        }
        self.queue_redraw();
    }
}

impl TabletSeatHandler for KomaState {
    type ToolFocus = WlSurface;

    fn tablet_tool_image(&mut self, _tool: &TabletToolDescriptor, image: CursorImageStatus) {
        self.set_cursor_status(image);
    }
}

// Unified Smithay `Dispatch2` / `GlobalDispatch2` delegation (`niri/src/niri.rs` line 488,
// pinned Smithay rev `65aa86fa5236e41775c313408102a5d5c2d1e2ca`).
impl smithay::wayland::input_method::InputMethodHandler for KomaState {
    fn new_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        // Reached from `InputMethodHandle::activate_input_method`, which holds the input-method
        // mutex while calling us.
        let _held = crate::seat_guard::input_method_callback();
        if self.is_locked_or_abandoned() {
            return;
        }
        self.cleanup_ime_popups();
        self.unconstrain_ime_popup(&surface);
        self.ime_popups.retain(|p| p != &surface);
        self.ime_popups.push(surface);
        self.queue_redraw();
    }

    fn dismiss_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        let _held = crate::seat_guard::input_method_callback();
        self.ime_popups.retain(|p| p != &surface);
        self.queue_redraw();
    }

    fn popup_repositioned(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        // Reached from `InputMethodHandle::set_text_input_rectangle` with the mutex held.
        let _held = crate::seat_guard::input_method_callback();
        if self.is_locked_or_abandoned() {
            self.clear_ime_state_for_lock();
            return;
        }
        self.unconstrain_ime_popup(&surface);
        self.queue_redraw();
    }

    fn parent_geometry(
        &self,
        parent: &WlSurface,
    ) -> smithay::utils::Rectangle<i32, smithay::utils::Logical> {
        let _held = crate::seat_guard::input_method_callback();
        self.ime_parent_geometry(parent)
    }
}

impl smithay::wayland::text_input::TextInputActivation for KomaState {
    fn activated(
        &mut self,
        _content_type: Option<(
            smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::ContentHint,
            smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::ContentPurpose,
        )>,
    ) {
        if self.is_locked_or_abandoned() {
            self.clear_ime_state_for_lock();
            return;
        }
        let popups = self.ime_popups.clone();
        for popup in &popups {
            self.unconstrain_ime_popup(popup);
        }
        self.queue_redraw();
    }

    fn deactivated(&mut self) {
        if self.is_locked_or_abandoned() {
            self.clear_ime_state_for_lock();
        }
        self.queue_redraw();
    }
}

impl VirtualPointerHandler for KomaState {
    fn process_virtual_pointer_event(&mut self, event: InputEvent<VirtualPointerBackend>) {
        self.reconcile_session_lock();
        if self.is_locked_or_abandoned() {
            return;
        }
        let is_bare_metal_tty = self.is_bare_metal_tty();
        match event {
            InputEvent::PointerMotion { event } => {
                self.handle_pointer_relative_motion(
                    event.delta(),
                    event.delta_unaccel(),
                    event.time(),
                );
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let target_output = event
                    .device()
                    .output()
                    .or_else(|| {
                        self.focused_output_name.as_deref().and_then(|name| {
                            self.all_outputs().into_iter().find(|o| o.name() == name)
                        })
                    })
                    .or_else(|| self.primary_output());
                let out_rect = target_output
                    .as_ref()
                    .map(|o| self.output_logical_rect(o))
                    .unwrap_or_else(|| smithay::utils::Rectangle::from_size((1920, 1080).into()));
                let target_pos = event.position_transformed(out_rect.size) + out_rect.loc.to_f64();
                self.handle_pointer_absolute_motion(target_pos, event.time());
            }
            InputEvent::PointerButton { event } => {
                crate::keybind_dispatcher::process_pointer_button_event(
                    self,
                    event.button_code(),
                    event.state(),
                    event.time(),
                    is_bare_metal_tty,
                );
            }
            InputEvent::PointerAxis { event } => {
                crate::keybind_dispatcher::process_pointer_axis_event::<VirtualPointerBackend>(
                    self,
                    &event,
                    is_bare_metal_tty,
                );
            }
            _ => {}
        }
    }
}

impl KeyboardShortcutsInhibitHandler for KomaState {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        if crate::security_context::is_surface_from_restricted_client(self, inhibitor.wl_surface())
        {
            warn!("ignored zwp_keyboard_shortcuts_inhibitor_v1 from restricted sandbox client");
            return;
        }
        if !self.active_shortcuts_inhibitors.contains(&inhibitor) {
            self.active_shortcuts_inhibitors.push(inhibitor);
        }
        self.reconcile_keyboard_shortcuts_inhibitors();
    }

    fn inhibitor_destroyed(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        self.active_shortcuts_inhibitors
            .retain(|i| i != &inhibitor && i.wl_surface().alive());
        self.reconcile_keyboard_shortcuts_inhibitors();
    }
}

smithay::delegate_dispatch2!(KomaState);
