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
//! - the `activate` request queued per manager and applied on
//!   `ext_workspace_manager_v1.commit` (protocol v1 treats the requests preceding a commit as one
//!   atomic batch, so two activations before a commit activate only the last one), then routed
//!   through [`KomaState::focus_workspace`], the same single switch path used by keybinds,
//!   `ws.focus` and followed window moves.
//!
//! State bits and the deliberate, documented subset (see the `komawm/docs` `IPC_SPEC.md` follow-up):
//!
//! - `active` means "currently displayed": the workspace is assigned to an output, is the open
//!   `special:` overlay, or is the active workspace.
//! - `hidden` marks workspaces that have no on-screen representation at all: headless/agent
//!   workspaces (an offscreen FBO entry or an `Offscreen` presentation target). It is **not** set
//!   on ordinary inactive workspaces, because the protocol defines `hidden` as "clients attempting
//!   to visualize the compositor workspace state should not display such workspaces" — hiding an
//!   inactive workspace would remove the click target a status bar needs. Ordinary inactive
//!   workspaces therefore carry an empty state, and `ignore-hidden: true` keeps them visible while
//!   dropping agent/headless workspaces from the strip.
//! - `urgent` is never set: Koma has no window urgency notion yet, so no workspace can be urgent.
//! - `deactivate`, `assign`, `remove`, `create_workspace` and the matching capabilities are not
//!   advertised and are ignored when a client still sends them. Koma has no workspace destruction,
//!   no output reassignment and no group re-parenting; a request that cannot be honoured must not
//!   be advertised as supported.
//! - Group output membership is refreshed when a manager binds, when a `wl_output` is bound and on
//!   every workspace state change, so a bar that binds outputs after the manager converges on the
//!   next workspace event. The exact `wl_output` resources reported to a client are retained, so a
//!   later `output_leave` still reaches a client whose output was hot-unplugged and two bindings of
//!   one physical output are tracked separately.

use std::collections::HashMap;

use koma_model::PresentationTarget;
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
    /// Manager instance that owns this handle. A handle is destroyed only from its own
    /// manager's bookkeeping, never from another client's manager.
    pub manager: ExtWorkspaceManagerV1,
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
    /// Exact `wl_output` resources reported through `output_enter`, keyed by protocol id.
    ///
    /// Keeping the resources (instead of resolving outputs from `all_outputs()` later) lets
    /// `output_leave` reach a client after the output was hot-unplugged, and keeps two bindings of
    /// one physical output distinct.
    outputs: HashMap<u32, WlOutput>,
}

#[derive(Debug)]
struct ManagerRecord {
    manager: ExtWorkspaceManagerV1,
    group: Option<GroupRecord>,
    workspaces: HashMap<WorkspaceId, WorkspaceRecord>,
    /// Last `activate` requested since this manager's previous `commit`, applied on commit.
    ///
    /// `ext_workspace_manager_v1` defines the requests preceding a commit as one atomic batch, so
    /// an activation that is never committed has no effect and two activations in one batch land
    /// on the last workspace only.
    pending_activation: Option<WorkspaceId>,
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

    fn manager_mut(&mut self, manager: &ExtWorkspaceManagerV1) -> Option<&mut ManagerRecord> {
        self.managers
            .iter_mut()
            .find(|record| record.manager.id() == manager.id())
    }
}

/// Protocol state bits for one workspace.
///
/// `active` is the displayed workspace; `hidden` is reserved for workspaces that must not be
/// visualized at all (headless/agent workspaces). Ordinary inactive workspaces stay empty so a
/// status bar keeps their click targets.
fn workspace_state_bits(displayed: bool, headless: bool) -> WorkspaceStateBit {
    if displayed {
        WorkspaceStateBit::Active
    } else if headless {
        WorkspaceStateBit::Hidden
    } else {
        WorkspaceStateBit::empty()
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
            let displayed = state.output_workspaces.values().any(|id| *id == ws_id)
                || visible_special == Some(&ws_id)
                || ws_id == state.active_ws_id;
            // Headless/agent workspaces have no on-screen representation. A focused headless
            // workspace keeps its offscreen FBO entry, but `displayed` wins so the bar still
            // marks the workspace the human is actually looking at as active.
            let headless = state.offscreen_surfaces.contains_key(&ws_id)
                || matches!(ws.presentation, PresentationTarget::Offscreen { .. });
            Some(WorkspaceSnapshot {
                ws_id,
                name: ws.name.clone(),
                coordinates: vec![u32::try_from(index).unwrap_or(u32::MAX)],
                state: workspace_state_bits(displayed, headless),
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
                        manager: record.manager.clone(),
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
///
/// The `wl_output` resources are retained per group so a leave can still be delivered after the
/// compositor dropped the output, and so multiple bindings of one physical output stay distinct.
fn sync_group_outputs(group: &mut GroupRecord, client: &Client, outputs: &[Output]) {
    let mut desired: HashMap<u32, WlOutput> = HashMap::new();
    for output in outputs {
        for wl_output in output.client_outputs(client) {
            desired.insert(wl_output.id().protocol_id(), wl_output);
        }
    }

    let stale: Vec<u32> = group
        .outputs
        .keys()
        .filter(|id| !desired.contains_key(id))
        .copied()
        .collect();
    for protocol_id in stale {
        let Some(wl_output) = group.outputs.remove(&protocol_id) else {
            continue;
        };
        if wl_output.is_alive() {
            group.handle.output_leave(&wl_output);
        }
    }

    for (protocol_id, wl_output) in desired {
        if group.outputs.contains_key(&protocol_id) {
            continue;
        }
        group.handle.output_enter(&wl_output);
        group.outputs.insert(protocol_id, wl_output);
    }
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
            pending_activation: None,
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
            ext_workspace_manager_v1::Request::Commit => {
                // Protocol v1: the requests preceding a commit form one atomic batch. Apply the
                // last activation only, so intermediate workspaces never become visible or reach
                // `workspace.focused` hooks.
                let pending = state
                    .ext_workspace_state
                    .manager_mut(manager)
                    .and_then(|record| record.pending_activation.take());
                let Some(ws_id) = pending else {
                    return;
                };
                if state.is_locked_or_abandoned() || !state.workspaces.contains_key(&ws_id) {
                    return;
                }
                state.focus_workspace(&ws_id);
            }
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
                // Queued until the owning manager commits; the commit applies it atomically and
                // re-checks the lock gate, so an uncommitted or locked request never switches.
                if let Some(record) = state.ext_workspace_state.manager_mut(&self.manager) {
                    record.pending_activation = Some(self.ws_id.clone());
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
        // Remove only this manager instance's handle: another attached manager (or another
        // client) keeps its own object for the same workspace.
        if let Some(record) = state.ext_workspace_state.manager_mut(&self.manager) {
            record.workspaces.remove(&self.ws_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::make_headless_test_state;
    use koma_render::offscreen_fbo::LazyOffscreenSurface;

    #[test]
    fn workspace_state_bits_mark_only_displayed_and_headless_workspaces() {
        assert_eq!(
            workspace_state_bits(true, false),
            WorkspaceStateBit::Active,
            "a displayed workspace is active"
        );
        assert!(
            workspace_state_bits(false, false).is_empty(),
            "an ordinary inactive workspace must carry no state bits, so bars keep its click target"
        );
        assert_eq!(
            workspace_state_bits(false, true),
            WorkspaceStateBit::Hidden,
            "a headless/agent workspace has no on-screen representation"
        );
        assert_eq!(
            workspace_state_bits(true, true),
            WorkspaceStateBit::Active,
            "displaying a formerly headless workspace wins over hiding it"
        );
    }

    #[test]
    fn snapshot_marks_displayed_active_headless_hidden_and_plain_inactive_empty() {
        let (_event_loop, _display, mut state) = make_headless_test_state();
        let ws1 = state.active_ws_id.clone();
        let ws2 = state.ensure_workspace_by_name_or_id("2");
        let ws3 = state.ensure_workspace_by_name_or_id("3");
        let ws4 = state.ensure_workspace_by_name_or_id("4");

        // Two independent headless markers: an offscreen presentation and an offscreen FBO entry.
        state.workspaces.get_mut(&ws3).expect("ws3").presentation =
            PresentationTarget::Offscreen { lazy_fbo_slot: 0 };
        state
            .offscreen_surfaces
            .insert(ws4.clone(), LazyOffscreenSurface::new(1920, 1080, 1.0));

        let state_of = |snapshot: &[WorkspaceSnapshot], ws_id: &WorkspaceId| {
            snapshot
                .iter()
                .find(|entry| &entry.ws_id == ws_id)
                .map(|entry| entry.state)
                .expect("workspace in snapshot")
        };

        let snapshot = capture_snapshot(&state);
        assert_eq!(state_of(&snapshot, &ws1), WorkspaceStateBit::Active);
        assert!(
            state_of(&snapshot, &ws2).is_empty(),
            "an ordinary inactive workspace is neither active nor hidden"
        );
        assert_eq!(
            state_of(&snapshot, &ws3),
            WorkspaceStateBit::Hidden,
            "an offscreen presentation marks a headless workspace"
        );
        assert_eq!(
            state_of(&snapshot, &ws4),
            WorkspaceStateBit::Hidden,
            "an offscreen FBO entry marks an agent/headless workspace"
        );

        // Displayed headless workspaces become active instead of hidden.
        state
            .output_workspaces
            .insert("DP-1".to_owned(), ws4.clone());
        let snapshot = capture_snapshot(&state);
        assert_eq!(
            state_of(&snapshot, &ws4),
            WorkspaceStateBit::Active,
            "display wins over the headless marker"
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
