//! `zwlr_foreign_toplevel_manager_v1` (v3) + `ext_foreign_toplevel_list_v1` coordination.
//!
//! Broadcasts managed toplevel metadata (`title`, `app_id`, `state`, `output_enter`/`leave`,
//! `parent`, `done`, `closed`) and services taskbar/dock control requests while respecting
//! Koma's fail-closed session-lock invariant (`is_locked_or_abandoned()`).

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

use koma_model::LogicalRect;
use koma_proto::WindowId;
use smithay::{
    output::Output,
    reexports::{
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
            zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, Weak, backend::ClientId,
            protocol::wl_surface::WlSurface,
        },
    },
    wayland::{Dispatch2, GlobalDispatch2, foreign_toplevel_list::ForeignToplevelHandle},
};
use tracing::debug;

use crate::{security_context::client_is_unrestricted, state::KomaState};

const WLR_FOREIGN_TOPLEVEL_VERSION: u32 = 3;

#[derive(Debug)]
pub struct WlrForeignToplevelGlobalData;

#[derive(Debug)]
pub struct WlrForeignToplevelManagerUserData;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToplevelSnapshot {
    pub title: String,
    pub app_id: String,
    pub maximized: bool,
    pub minimized: bool,
    pub activated: bool,
    pub fullscreen: bool,
    pub output_names: Vec<String>,
    pub parent_win_id: Option<WindowId>,
}

#[derive(Debug)]
pub struct WlrToplevelHandleRecord {
    pub handle: ZwlrForeignToplevelHandleV1,
    pub last_sent: Option<ToplevelSnapshot>,
    pub sent_outputs: HashSet<String>,
}

#[derive(Debug)]
pub struct WlrForeignToplevelHandleUserData {
    pub win_id: WindowId,
    pub inert: Mutex<bool>,
}

/// Manages both `zwlr_foreign_toplevel_manager_v1` and per-window `ext_foreign_toplevel_list_v1`
/// synchronization from a single authoritative snapshot pipeline.
#[derive(Debug, Default)]
pub struct WlrForeignToplevelState {
    pub managers: Vec<ZwlrForeignToplevelManagerV1>,
    pub wlr_handles: HashMap<WindowId, Vec<WlrToplevelHandleRecord>>,
    pub ext_handles: HashMap<WindowId, ForeignToplevelHandle>,
    pub client_rectangles: HashMap<(u32, WindowId), (Weak<WlSurface>, LogicalRect)>,
}

impl WlrForeignToplevelState {
    #[must_use]
    pub fn new(display_handle: &DisplayHandle) -> Self {
        display_handle.create_global::<KomaState, ZwlrForeignToplevelManagerV1, _>(
            WLR_FOREIGN_TOPLEVEL_VERSION,
            WlrForeignToplevelGlobalData,
        );
        Self::default()
    }
}

/// Encodes `zwlr_foreign_toplevel_handle_v1::State` bitflags into a Wayland `wl_array` byte vector.
#[must_use]
pub fn encode_wlr_toplevel_states(
    maximized: bool,
    minimized: bool,
    activated: bool,
    fullscreen: bool,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16);
    if maximized {
        bytes.extend_from_slice(
            &(zwlr_foreign_toplevel_handle_v1::State::Maximized as u32).to_ne_bytes(),
        );
    }
    if minimized {
        bytes.extend_from_slice(
            &(zwlr_foreign_toplevel_handle_v1::State::Minimized as u32).to_ne_bytes(),
        );
    }
    if activated {
        bytes.extend_from_slice(
            &(zwlr_foreign_toplevel_handle_v1::State::Activated as u32).to_ne_bytes(),
        );
    }
    if fullscreen {
        bytes.extend_from_slice(
            &(zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32).to_ne_bytes(),
        );
    }
    bytes
}

impl GlobalDispatch2<ZwlrForeignToplevelManagerV1, KomaState> for WlrForeignToplevelGlobalData {
    fn can_view(&self, client: &Client) -> bool {
        client_is_unrestricted(client)
    }

    fn bind(
        &self,
        state: &mut KomaState,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        let manager = data_init.init(resource, WlrForeignToplevelManagerUserData);
        state.wlr_foreign_toplevel_state.managers.push(manager);
        sync_all_foreign_toplevels(state);
    }
}

impl Dispatch2<ZwlrForeignToplevelManagerV1, KomaState> for WlrForeignToplevelManagerUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            manager.finished();
            state
                .wlr_foreign_toplevel_state
                .managers
                .retain(|m| m.id() != manager.id());
        }
    }

    fn destroyed(
        &self,
        state: &mut KomaState,
        _client: ClientId,
        manager: &ZwlrForeignToplevelManagerV1,
    ) {
        state
            .wlr_foreign_toplevel_state
            .managers
            .retain(|m| m.id() != manager.id());
    }
}

impl Dispatch2<ZwlrForeignToplevelHandleV1, KomaState> for WlrForeignToplevelHandleUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        if let zwlr_foreign_toplevel_handle_v1::Request::Destroy = request {
            remove_wlr_handle(state, self.win_id, handle);
            return;
        }

        if *self.inert.lock().unwrap() {
            return;
        }

        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                state.set_window_maximized(self.win_id, true);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                state.set_window_maximized(self.win_id, false);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => {
                state.set_window_minimized(self.win_id, true);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {
                state.set_window_minimized(self.win_id, false);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
                state.reconcile_session_lock();
                if state.is_locked_or_abandoned() {
                    debug!(win_id = ?self.win_id, "blocked foreign_toplevel activate during lock");
                    return;
                }
                let _ = state.activate_window_by_id(self.win_id);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                if let Some(m) = state
                    .mapped_windows
                    .iter()
                    .find(|m| m.win_id == self.win_id)
                    && let Some(toplevel) = m.window.toplevel()
                {
                    toplevel.send_close();
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle {
                surface,
                x,
                y,
                width,
                height,
            } => {
                if width < 0 || height < 0 {
                    handle.post_error(
                        zwlr_foreign_toplevel_handle_v1::Error::InvalidRectangle,
                        "foreign toplevel rectangle cannot have negative width or height",
                    );
                    return;
                }
                let key = (handle.id().protocol_id(), self.win_id);
                if width == 0 && height == 0 {
                    state
                        .wlr_foreign_toplevel_state
                        .client_rectangles
                        .remove(&key);
                } else {
                    state.wlr_foreign_toplevel_state.client_rectangles.insert(
                        key,
                        (
                            surface.downgrade(),
                            LogicalRect::new(x, y, width as u32, height as u32),
                        ),
                    );
                }
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { output } => {
                let target_output = output.as_ref().and_then(Output::from_resource);
                state.set_window_fullscreen(self.win_id, true, target_output);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.set_window_fullscreen(self.win_id, false, None);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut KomaState,
        _client: ClientId,
        handle: &ZwlrForeignToplevelHandleV1,
    ) {
        remove_wlr_handle(state, self.win_id, handle);
    }
}

fn remove_wlr_handle(
    state: &mut KomaState,
    win_id: WindowId,
    handle: &ZwlrForeignToplevelHandleV1,
) {
    state
        .wlr_foreign_toplevel_state
        .client_rectangles
        .remove(&(handle.id().protocol_id(), win_id));
    if let Some(list) = state
        .wlr_foreign_toplevel_state
        .wlr_handles
        .get_mut(&win_id)
    {
        list.retain(|entry| entry.handle.id() != handle.id());
        if list.is_empty() {
            state.wlr_foreign_toplevel_state.wlr_handles.remove(&win_id);
        }
    }
}

/// Closes both `zwlr_foreign_toplevel_handle_v1` and `ext_foreign_toplevel_handle_v1` for an unmapped window.
pub fn close_foreign_toplevel(state: &mut KomaState, win_id: WindowId) {
    if let Some(ext) = state.wlr_foreign_toplevel_state.ext_handles.remove(&win_id) {
        state.foreign_toplevel_list_state.remove_toplevel(&ext);
    }
    if let Some(records) = state.wlr_foreign_toplevel_state.wlr_handles.remove(&win_id) {
        for record in records {
            if let Some(ud) = record.handle.data::<WlrForeignToplevelHandleUserData>() {
                *ud.inert.lock().unwrap() = true;
            }
            if record.handle.is_alive() {
                record.handle.closed();
            }
        }
    }
    state
        .wlr_foreign_toplevel_state
        .client_rectangles
        .retain(|(_, id), _| *id != win_id);
}

/// Synchronizes all active foreign-toplevel handles (`zwlr` and `ext`) with current `KomaState`.
pub fn sync_all_foreign_toplevels(state: &mut KomaState) {
    state
        .wlr_foreign_toplevel_state
        .managers
        .retain(|m| m.is_alive());

    let locked = state.is_locked_or_abandoned();
    let active_ws = state.effective_interactive_ws_id();
    let focused_win = if locked {
        None
    } else {
        state
            .workspaces
            .get(&active_ws)
            .and_then(|ws| ws.active_window())
    };
    let outputs = state.all_outputs();
    let snapshots = build_snapshots(state, focused_win, &outputs);

    // Ensure every mapped window has an `ext_foreign_toplevel_list_v1` handle & stays synced.
    for (win_id, snap) in &snapshots {
        sync_ext_handle(state, *win_id, snap);
    }

    // Ensure every active `zwlr` manager has a handle for every mapped window & emits updates.
    for manager in state.wlr_foreign_toplevel_state.managers.clone() {
        let Some(client) = manager.client() else {
            continue;
        };
        for (win_id, _snap) in &snapshots {
            let handles = state
                .wlr_foreign_toplevel_state
                .wlr_handles
                .entry(*win_id)
                .or_default();
            let exists = handles.iter().any(|rec| {
                rec.handle.is_alive()
                    && rec.handle.client().as_ref().map(Client::id) == Some(client.id())
            });
            if !exists
                && let Ok(new_handle) = client
                    .create_resource::<ZwlrForeignToplevelHandleV1, _, KomaState>(
                        &state.display_handle,
                        manager.version(),
                        WlrForeignToplevelHandleUserData {
                            win_id: *win_id,
                            inert: Mutex::new(false),
                        },
                    )
            {
                manager.toplevel(&new_handle);
                handles.push(WlrToplevelHandleRecord {
                    handle: new_handle,
                    last_sent: None,
                    sent_outputs: HashSet::new(),
                });
            }
        }
    }

    // Emit changed fields + `done()` across all handles (after all handles exist so parent lookups work).
    let handle_lookup = collect_client_handle_map(&state.wlr_foreign_toplevel_state.wlr_handles);
    for (win_id, snap) in &snapshots {
        let Some(records) = state.wlr_foreign_toplevel_state.wlr_handles.get_mut(win_id) else {
            continue;
        };
        records.retain(|rec| rec.handle.is_alive());
        for rec in records.iter_mut() {
            send_wlr_handle_diff(rec, snap, &outputs, &handle_lookup);
        }
    }
}

fn build_snapshots(
    state: &KomaState,
    focused_win: Option<WindowId>,
    outputs: &[Output],
) -> Vec<(WindowId, ToplevelSnapshot)> {
    let mut parent_map: HashMap<smithay::reexports::wayland_server::backend::ObjectId, WindowId> =
        HashMap::with_capacity(state.mapped_windows.len());
    for m in &state.mapped_windows {
        if let Some(t) = m.window.toplevel() {
            parent_map.insert(t.wl_surface().id(), m.win_id);
        }
    }

    let output_names: Vec<String> = outputs.iter().map(Output::name).collect();
    state
        .mapped_windows
        .iter()
        .map(|m| {
            let ws = state.workspaces.get(&m.ws_id);
            let fullscreen = ws.is_some_and(|w| w.fullscreen_window == Some(m.win_id));
            let parent_win_id = m
                .window
                .toplevel()
                .and_then(|t| t.parent())
                .and_then(|p| parent_map.get(&p.id()).copied());
            let visible_outputs = if m.ws_id == state.active_ws_id
                || state.active_special_ws_id.as_ref() == Some(&m.ws_id)
            {
                output_names.clone()
            } else {
                Vec::new()
            };
            (
                m.win_id,
                ToplevelSnapshot {
                    title: m.title.clone(),
                    app_id: m.app_id.clone(),
                    maximized: m.is_maximized,
                    minimized: m.is_minimized,
                    activated: focused_win == Some(m.win_id) && !m.is_minimized,
                    fullscreen,
                    output_names: visible_outputs,
                    parent_win_id,
                },
            )
        })
        .collect()
}

fn sync_ext_handle(state: &mut KomaState, win_id: WindowId, snap: &ToplevelSnapshot) {
    let handle = state
        .wlr_foreign_toplevel_state
        .ext_handles
        .entry(win_id)
        .or_insert_with(|| {
            state
                .foreign_toplevel_list_state
                .new_toplevel::<KomaState>(snap.title.clone(), snap.app_id.clone())
        });
    let mut changed = false;
    if handle.title() != snap.title {
        handle.send_title(&snap.title);
        changed = true;
    }
    if handle.app_id() != snap.app_id {
        handle.send_app_id(&snap.app_id);
        changed = true;
    }
    if changed {
        handle.send_done();
    }
}

fn collect_client_handle_map(
    wlr_handles: &HashMap<WindowId, Vec<WlrToplevelHandleRecord>>,
) -> HashMap<(ClientId, WindowId), ZwlrForeignToplevelHandleV1> {
    let mut map = HashMap::new();
    for (wid, list) in wlr_handles {
        for rec in list {
            if rec.handle.is_alive()
                && let Some(client) = rec.handle.client()
            {
                map.insert((client.id(), *wid), rec.handle.clone());
            }
        }
    }
    map
}

fn send_wlr_handle_diff(
    rec: &mut WlrToplevelHandleRecord,
    snap: &ToplevelSnapshot,
    outputs: &[Output],
    handle_lookup: &HashMap<(ClientId, WindowId), ZwlrForeignToplevelHandleV1>,
) {
    if rec.last_sent.as_ref() == Some(snap) {
        return;
    }
    let Some(client) = rec.handle.client() else {
        return;
    };
    let prev = rec.last_sent.as_ref();

    if prev.is_none_or(|p| p.title != snap.title) {
        rec.handle.title(snap.title.clone());
    }
    if prev.is_none_or(|p| p.app_id != snap.app_id) {
        rec.handle.app_id(snap.app_id.clone());
    }
    if prev.is_none_or(|p| {
        p.maximized != snap.maximized
            || p.minimized != snap.minimized
            || p.activated != snap.activated
            || p.fullscreen != snap.fullscreen
    }) {
        rec.handle.state(encode_wlr_toplevel_states(
            snap.maximized,
            snap.minimized,
            snap.activated,
            snap.fullscreen,
        ));
    }

    let next_outputs: HashSet<String> = snap.output_names.iter().cloned().collect();
    for out in outputs {
        let name = out.name();
        let should_have = next_outputs.contains(&name);
        let currently_has = rec.sent_outputs.contains(&name);
        if should_have && !currently_has {
            for wl_out in out.client_outputs(&client) {
                rec.handle.output_enter(&wl_out);
            }
            rec.sent_outputs.insert(name);
        } else if !should_have && currently_has {
            for wl_out in out.client_outputs(&client) {
                rec.handle.output_leave(&wl_out);
            }
            rec.sent_outputs.remove(&name);
        }
    }

    if rec.handle.version() >= 3 && prev.is_none_or(|p| p.parent_win_id != snap.parent_win_id) {
        let parent_handle = snap
            .parent_win_id
            .and_then(|pid| handle_lookup.get(&(client.id(), pid)));
        rec.handle.parent(parent_handle);
    }

    rec.handle.done();
    rec.last_sent = Some(snap.clone());
}
