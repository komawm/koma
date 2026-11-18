//! [`ext-workspace-v1`](https://wayland.app/protocols/ext-workspace-v1) (staging) implementation.
//!
//! Status and task bars (`waybar` `ext/workspaces`, `niri`-style docks) need a workspace list with
//! per-workspace click targets. This module advertises `ext_workspace_manager_v1` and publishes
//! Koma's workspace model through it:
//!
//! - one `ext_workspace_group_handle_v1` holding **every** output, because Koma workspaces are
//!   global objects that are assigned to an output on demand instead of belonging to a fixed
//!   per-output set;
//! - one `ext_workspace_handle_v1` per live workspace, carrying its door id ([`WorkspaceId`]) as
//!   the protocol `id`, its human name, 1D coordinates in strip order, and the `active` /
//!   `hidden` state bits;
//! - the `activate` request routed through [`KomaState::focus_workspace`], the same single
//!   switch path used by keybinds, `ws.focus` and followed window moves.
//!
//! Deliberate, documented subset (see the `komawm/docs` `IPC_SPEC.md` follow-up):
//!
//! - `urgent` is never set: Koma has no window urgency notion yet, so no workspace can be urgent.
//! - `deactivate`, `assign`, `remove`, `create_workspace` and the matching capabilities are not
//!   advertised and are ignored when a client still sends them. Koma has no workspace destruction,
//!   no output reassignment and no group re-parenting; a request that cannot be honoured must not
//!   be advertised as supported.
//! - `active` means "currently displayed": the workspace is assigned to an output, is the open
//!   `special:` overlay, or is the active workspace. Everything else is `hidden`, matching the
//!   protocol definition ("not visible in its workspace group"). Bars that do not want
//!   agent/headless workspaces in the strip set `ignore-hidden: true`.
//! - Group output membership is refreshed when a manager binds, when a `wl_output` is bound and on
//!   every workspace state change, so a bar that binds outputs after the manager converges on the
//!   next workspace event.

use std::collections::HashMap;

use koma_proto::WorkspaceId;
use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::ext::workspace::v1::server::{
            ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
            ext_workspace_handle_v1::{
                self, ExtWorkspaceHandleV1, State as WorkspaceStateBit, WorkspaceCapabilities,
            },
            ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, backend::ClientId,
            protocol::wl_output::WlOutput,
        },
    },
    wayland::{Dispatch2, GlobalDispatch2},
};

use crate::{security_context::client_is_unrestricted, state::KomaState};

const EXT_WORKSPACE_MANAGER_VERSION: u32 = 1;

#[derive(Debug)]
pub struct ExtWorkspaceGlobalData;

#[derive(Debug)]
pub struct ExtWorkspaceManagerUserData;

#[derive(Debug)]
pub struct ExtWorkspaceGroupUserData;

#[derive(Debug)]
pub struct ExtWorkspaceHandleUserData {
    pub ws_id: WorkspaceId,
}

/// Property snapshot last sent to one client handle, so sync only emits changed properties.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SentWorkspaceProperties {
    name: String,
    coordinates: Vec<u32>,
    state: WorkspaceStateBit,
}

#[derive(Debug)]
struct WorkspaceRecord {
    handle: ExtWorkspaceHandleV1,
    sent: Option<SentWorkspaceProperties>,
    /// Set while the handle still has to be associated with the group via `workspace_enter`.
    pending_enter: bool,
}

#[derive(Debug)]
struct GroupRecord {
    handle: ExtWorkspaceGroupHandleV1,
    capabilities_sent: bool,
    /// `wl_output` protocol ids already reported through `output_enter`, with their output name.
    outputs: HashMap<u32, String>,
}

#[derive(Debug)]
struct ManagerRecord {
    manager: ExtWorkspaceManagerV1,
    group: Option<GroupRecord>,
    workspaces: HashMap<WorkspaceId, WorkspaceRecord>,
}

/// Per-client `ext-workspace-v1` object bookkeeping.
#[derive(Debug, Default)]
pub struct ExtWorkspaceState {
    managers: Vec<ManagerRecord>,
}

impl ExtWorkspaceState {
    #[must_use]
    pub fn new(display_handle: &DisplayHandle) -> Self {
        display_handle.create_global::<KomaState, ExtWorkspaceManagerV1, _>(
            EXT_WORKSPACE_MANAGER_VERSION,
            ExtWorkspaceGlobalData,
        );
        Self::default()
    }

    #[must_use]
    pub fn has_clients(&self) -> bool {
        !self.managers.is_empty()
    }

    fn remove_manager(&mut self, manager: &ExtWorkspaceManagerV1) {
        self.managers
            .retain(|record| record.manager.id() != manager.id());
    }

    fn remove_workspace_handle(&mut self, ws_id: &WorkspaceId) {
        for record in &mut self.managers {
            record.workspaces.remove(ws_id);
        }
    }
}

fn workspace_state_bits(is_visible: bool) -> WorkspaceStateBit {
    if is_visible {
        WorkspaceStateBit::Active
    } else {
        WorkspaceStateBit::Hidden
    }
}

/// One entry of the authoritative workspace snapshot pushed to clients.
#[derive(Debug, Clone)]
struct WorkspaceSnapshot {
    ws_id: WorkspaceId,
    name: String,
    coordinates: Vec<u32>,
    state: WorkspaceStateBit,
}

/// Workspaces in strip order: numeric names first (by value), then the remaining names.
fn ordered_workspace_ids(state: &KomaState) -> Vec<WorkspaceId> {
    let mut ids: Vec<WorkspaceId> = state.workspaces.keys().cloned().collect();
    ids.sort_by_key(|id| {
        let name = state
            .workspaces
            .get(id)
            .map(|ws| ws.name.clone())
            .unwrap_or_default();
        let numeric = name.parse::<u32>().ok();
        (
            numeric.is_none(),
            numeric.unwrap_or(u32::MAX),
            name,
            id.as_str().to_owned(),
        )
    });
    ids
}

fn capture_snapshot(state: &KomaState) -> Vec<WorkspaceSnapshot> {
    let visible_special = state.active_special_ws_id.as_ref();
    ordered_workspace_ids(state)
        .into_iter()
        .enumerate()
        .filter_map(|(index, ws_id)| {
            let ws = state.workspaces.get(&ws_id)?;
            let visible = state.output_workspaces.values().any(|id| *id == ws_id)
                || visible_special == Some(&ws_id)
                || ws_id == state.active_ws_id;
            Some(WorkspaceSnapshot {
                ws_id,
                name: ws.name.clone(),
                coordinates: vec![u32::try_from(index).unwrap_or(u32::MAX)],
                state: workspace_state_bits(visible),
            })
        })
        .collect()
}

fn encode_coordinates(coordinates: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(coordinates));
    for value in coordinates {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes
}

/// Pushes the current workspace model to every bound `ext_workspace_manager_v1` client.
///
/// Returns immediately while no status bar is connected, so the workspace switch path stays free
/// for agent-only sessions.
pub fn sync_ext_workspaces(state: &mut KomaState) {
    if !state.ext_workspace_state.has_clients() {
        return;
    }
    let snapshot = capture_snapshot(state);
    let outputs: Vec<Output> = state.all_outputs();

    let mut managers = std::mem::take(&mut state.ext_workspace_state.managers);
    managers.retain(|record| record.manager.is_alive());

    for record in &mut managers {
        let Some(client) = record.manager.client() else {
            continue;
        };
        let version = record.manager.version().min(EXT_WORKSPACE_MANAGER_VERSION);

        if record.group.is_none() {
            let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, KomaState>(
                &state.display_handle,
                version,
                ExtWorkspaceGroupUserData,
            ) else {
                continue;
            };
            record.manager.workspace_group(&group);
            record.group = Some(GroupRecord {
                handle: group,
                capabilities_sent: false,
                outputs: HashMap::new(),
            });
        }

        if let Some(group) = record.group.as_mut() {
            if !group.capabilities_sent {
                group.handle.capabilities(GroupCapabilities::empty());
                group.capabilities_sent = true;
            }
            sync_group_outputs(group, &client, &outputs);
        }

        // Handles for workspaces that no longer exist must be removed before the group shrinks.
        let live: Vec<WorkspaceId> = snapshot.iter().map(|entry| entry.ws_id.clone()).collect();
        let dead: Vec<WorkspaceId> = record
            .workspaces
            .keys()
            .filter(|id| !live.contains(id))
            .cloned()
            .collect();
        for ws_id in dead {
            if let Some(ws_record) = record.workspaces.remove(&ws_id)
                && ws_record.handle.is_alive()
            {
                if let Some(group) = record.group.as_ref() {
                    group.handle.workspace_leave(&ws_record.handle);
                }
                ws_record.handle.removed();
            }
        }

        // Create new handles, then emit only the properties that changed.
        for entry in &snapshot {
            if !record.workspaces.contains_key(&entry.ws_id) {
                let Ok(handle) = client.create_resource::<ExtWorkspaceHandleV1, _, KomaState>(
                    &state.display_handle,
                    version,
                    ExtWorkspaceHandleUserData {
                        ws_id: entry.ws_id.clone(),
                    },
                ) else {
                    continue;
                };
                record.manager.workspace(&handle);
                handle.id(entry.ws_id.as_str().to_owned());
                handle.capabilities(WorkspaceCapabilities::Activate);
                record.workspaces.insert(
                    entry.ws_id.clone(),
                    WorkspaceRecord {
                        handle,
                        sent: None,
                        pending_enter: true,
                    },
                );
            }

            let Some(ws_record) = record.workspaces.get_mut(&entry.ws_id) else {
                continue;
            };
            let changed = ws_record.sent.as_ref().is_none_or(|sent| {
                sent.name != entry.name
                    || sent.coordinates != entry.coordinates
                    || sent.state != entry.state
            });
            if changed {
                let name_changed = ws_record
                    .sent
                    .as_ref()
                    .is_none_or(|sent| sent.name != entry.name);
                let coordinates_changed = ws_record
                    .sent
                    .as_ref()
                    .is_none_or(|sent| sent.coordinates != entry.coordinates);
                let state_changed = ws_record
                    .sent
                    .as_ref()
                    .is_none_or(|sent| sent.state != entry.state);
                if name_changed {
                    ws_record.handle.name(entry.name.clone());
                }
                if coordinates_changed {
                    ws_record
                        .handle
                        .coordinates(encode_coordinates(&entry.coordinates));
                }
                if state_changed {
                    ws_record.handle.state(entry.state);
                }
                ws_record.sent = Some(SentWorkspaceProperties {
                    name: entry.name.clone(),
                    coordinates: entry.coordinates.clone(),
                    state: entry.state,
                });
            }

            if ws_record.pending_enter {
                if let Some(group) = record.group.as_ref() {
                    group.handle.workspace_enter(&ws_record.handle);
                }
                ws_record.pending_enter = false;
            }
        }

        record.manager.done();
    }

    state.ext_workspace_state.managers = managers;
}

/// Emits `output_enter` / `output_leave` so the single group tracks the client's live outputs.
fn sync_group_outputs(group: &mut GroupRecord, client: &Client, outputs: &[Output]) {
    let mut desired: HashMap<u32, String> = HashMap::new();
    for output in outputs {
        // `Output::client_outputs` keeps the output mutex locked for the iterator's lifetime, so
        // the name must be read before the iterator starts (a nested `output.name()` would
        // self-deadlock on the non-reentrant `std::sync::Mutex`).
        let name = output.name();
        for wl_output in output.client_outputs(client) {
            desired.insert(wl_output.id().protocol_id(), name.clone());
        }
    }

    let stale: Vec<u32> = group
        .outputs
        .keys()
        .filter(|id| !desired.contains_key(id))
        .copied()
        .collect();
    for protocol_id in stale {
        let Some(name) = group.outputs.remove(&protocol_id) else {
            continue;
        };
        if let Some(wl_output) = find_client_output(client, outputs, &name) {
            group.handle.output_leave(&wl_output);
        }
    }

    for (protocol_id, name) in &desired {
        if group.outputs.contains_key(protocol_id) {
            continue;
        }
        let Some(wl_output) = find_client_output(client, outputs, name) else {
            continue;
        };
        group.handle.output_enter(&wl_output);
        group.outputs.insert(*protocol_id, name.clone());
    }
}

fn find_client_output(client: &Client, outputs: &[Output], name: &str) -> Option<WlOutput> {
    let output = outputs.iter().find(|output| output.name() == name)?.clone();
    output.client_outputs(client).next()
}

impl GlobalDispatch2<ExtWorkspaceManagerV1, KomaState> for ExtWorkspaceGlobalData {
    fn can_view(&self, client: &Client) -> bool {
        client_is_unrestricted(client)
    }

    fn bind(
        &self,
        state: &mut KomaState,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        let manager = data_init.init(resource, ExtWorkspaceManagerUserData);
        state.ext_workspace_state.managers.push(ManagerRecord {
            manager,
            group: None,
            workspaces: HashMap::new(),
        });
        // Child objects must not be created from inside a `bind` dispatch: `DataInit` still
        // holds the client's object-map borrow, so `Client::create_resource` would self-deadlock.
        // Flushing the initial snapshot from a calloop idle callback keeps `bind` allocation-free.
        if let Some(handle) = state.loop_handle.clone() {
            handle.insert_idle(sync_ext_workspaces);
        }
    }
}

impl Dispatch2<ExtWorkspaceManagerV1, KomaState> for ExtWorkspaceManagerUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        match request {
            // Requests are applied as they arrive, so a commit has nothing left to batch.
            ext_workspace_manager_v1::Request::Commit => {}
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
                state.ext_workspace_state.remove_manager(manager);
            }
            _ => {}
        }
    }

    fn destroyed(&self, state: &mut KomaState, _client: ClientId, manager: &ExtWorkspaceManagerV1) {
        state.ext_workspace_state.remove_manager(manager);
    }
}

impl Dispatch2<ExtWorkspaceGroupHandleV1, KomaState> for ExtWorkspaceGroupUserData {
    fn request(
        &self,
        _state: &mut KomaState,
        _client: &Client,
        _group: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        // `create_workspace` is not advertised: Koma workspaces are created by config, keybinds
        // and `ws.create`, never by a status bar. `destroy` is handled by `Dispatch2::destroyed`.
        if matches!(
            request,
            ext_workspace_group_handle_v1::Request::CreateWorkspace { .. }
        ) {}
    }
}

impl Dispatch2<ExtWorkspaceHandleV1, KomaState> for ExtWorkspaceHandleUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        _handle: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                // Fail closed while the session is locked: a bar must not surface another
                // workspace over the lock screen.
                if state.is_locked_or_abandoned() {
                    return;
                }
                if state.workspaces.contains_key(&self.ws_id) {
                    state.focus_workspace(&self.ws_id);
                }
            }
            // Unadvertised capabilities: the protocol requires the compositor to ignore them.
            ext_workspace_handle_v1::Request::Deactivate
            | ext_workspace_handle_v1::Request::Assign { .. }
            | ext_workspace_handle_v1::Request::Remove
            | ext_workspace_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(&self, state: &mut KomaState, _client: ClientId, _handle: &ExtWorkspaceHandleV1) {
        state
            .ext_workspace_state
            .remove_workspace_handle(&self.ws_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::make_headless_test_state;

    #[test]
    fn workspace_state_bits_mark_visible_workspaces_active_and_others_hidden() {
        assert_eq!(workspace_state_bits(true), WorkspaceStateBit::Active);
        assert_eq!(workspace_state_bits(false), WorkspaceStateBit::Hidden);
        assert!(
            !workspace_state_bits(true).contains(WorkspaceStateBit::Hidden),
            "active and hidden are mutually exclusive state bits"
        );
    }

    #[test]
    fn snapshot_orders_numeric_workspaces_before_named_ones_and_marks_one_active() {
        let (_event_loop, _display, mut state) = make_headless_test_state();
        state.ensure_workspace_by_name_or_id("2");
        state.ensure_workspace_by_name_or_id("scratch");

        let snapshot = capture_snapshot(&state);
        assert_eq!(
            snapshot
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2", "scratch"],
            "numeric names must sort by value before named workspaces"
        );
        assert_eq!(
            snapshot
                .iter()
                .filter(|entry| entry.state.contains(WorkspaceStateBit::Active))
                .count(),
            1,
            "exactly one workspace is displayed on the only output"
        );
        assert_eq!(
            snapshot
                .iter()
                .map(|entry| entry.coordinates.clone())
                .collect::<Vec<_>>(),
            vec![vec![0], vec![1], vec![2]],
            "coordinates must stay unique inside the single group"
        );
    }

    #[test]
    fn encode_coordinates_uses_native_u32_words() {
        assert_eq!(
            encode_coordinates(&[3, 7]),
            [3_u32.to_ne_bytes(), 7_u32.to_ne_bytes()].concat()
        );
    }
}
