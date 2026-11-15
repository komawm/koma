//! `zwlr_output_manager_v1` (`wlr-output-management-unstable-v1` v4) protocol implementation.
//!
//! Exposes runtime multi-monitor topology discovery and atomic configuration (`test` / `apply`)
//! exclusively to unrestricted clients (`client_is_unrestricted`).

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

use koma_model::{OutputTransform, SavedOutputConfig, closest_representable_scale};
use smithay::{
    output::{Mode, Output, Scale},
    reexports::{
        wayland_protocols_wlr::output_management::v1::server::{
            zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
            zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
            zwlr_output_head_v1::{self, AdaptiveSyncState, ZwlrOutputHeadV1},
            zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
            zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, WEnum, Weak,
            backend::{ClientId, ObjectId},
            protocol::wl_output::Transform as WlTransform,
        },
    },
    utils::Transform,
    wayland::{Dispatch2, GlobalDispatch2},
};
use tracing::{debug, warn};

use crate::{
    security_context::client_is_unrestricted,
    state::{KomaState, output_logical_viewport, smithay_transform_to_koma},
};

const WLR_OUTPUT_MANAGER_VERSION: u32 = 4;
const WLR_OUTPUT_MODE_MAX_VERSION: u32 = 3;

#[derive(Debug)]
pub struct WlrOutputManagerGlobalData;

#[derive(Debug)]
pub struct OutputManagerUserData;

#[derive(Debug, Clone)]
pub struct OutputHeadUserData {
    pub output_name: String,
    pub manager_id: ObjectId,
}

#[derive(Debug, Clone)]
pub struct OutputModeUserData {
    pub output_name: String,
    pub mode: Mode,
}

#[derive(Debug, Clone, PartialEq)]
struct OutputHeadSnapshot {
    name: String,
    description: String,
    make: String,
    model: String,
    serial_number: String,
    phys_mm: (i32, i32),
    enabled: bool,
    position: (i32, i32),
    transform: WlTransform,
    scale: f64,
    current_mode: Option<Mode>,
    preferred_mode: Option<Mode>,
    modes: Vec<Mode>,
    adaptive_sync: bool,
}

#[derive(Debug)]
struct HeadInstance {
    head: Weak<ZwlrOutputHeadV1>,
    modes: Vec<(Weak<ZwlrOutputModeV1>, Mode)>,
    last_snapshot: OutputHeadSnapshot,
}

#[derive(Debug)]
struct ManagerInstance {
    manager: Weak<ZwlrOutputManagerV1>,
    stopped: bool,
    heads: HashMap<String, HeadInstance>,
}

/// Ledger for `zwlr_output_manager_v1` clients, per-manager heads/modes, and configuration serials.
#[derive(Debug)]
pub struct WlrOutputManagerState {
    display_handle: DisplayHandle,
    serial: u32,
    managers: Vec<ManagerInstance>,
    adaptive_sync_enabled: HashMap<String, bool>,
}

impl WlrOutputManagerState {
    #[must_use]
    pub fn new(display_handle: &DisplayHandle) -> Self {
        display_handle.create_global::<KomaState, ZwlrOutputManagerV1, _>(
            WLR_OUTPUT_MANAGER_VERSION,
            WlrOutputManagerGlobalData,
        );
        Self {
            display_handle: display_handle.clone(),
            serial: 1,
            managers: Vec::new(),
            adaptive_sync_enabled: HashMap::new(),
        }
    }

    #[must_use]
    pub fn current_serial(&self) -> u32 {
        self.serial
    }

    #[must_use]
    pub fn adaptive_sync_for(&self, output_name: &str) -> bool {
        self.adaptive_sync_enabled
            .get(output_name)
            .copied()
            .unwrap_or(false)
    }

    fn live_head_names_for_manager(&self, manager_id: &ObjectId) -> Vec<String> {
        let Some(inst) = self
            .managers
            .iter()
            .find(|m| !m.stopped && m.manager.id() == *manager_id)
        else {
            return Vec::new();
        };
        inst.heads
            .iter()
            .filter_map(|(name, h)| {
                h.head
                    .upgrade()
                    .ok()
                    .filter(Resource::is_alive)
                    .map(|_| name.clone())
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct PendingHeadChanges {
    mode: Option<Mode>,
    mode_invalid: bool,
    mode_already_set: bool,
    custom_mode: Option<Mode>,
    position: Option<(i32, i32)>,
    transform: Option<WlTransform>,
    scale: Option<f64>,
    adaptive_sync: Option<bool>,
}

#[derive(Debug)]
enum ConfiguredHeadAction {
    Enable(ZwlrOutputConfigurationHeadV1),
    Disable,
}

#[derive(Debug)]
pub struct OutputConfigurationUserData {
    manager_id: ObjectId,
    serial: u32,
    state: Mutex<PendingConfigurationState>,
}

#[derive(Debug, Default)]
struct PendingConfigurationState {
    used: bool,
    heads: HashMap<ObjectId, (String, ConfiguredHeadAction)>,
}

#[derive(Debug)]
pub struct OutputConfigurationHeadUserData {
    config_id: ObjectId,
    output_name: String,
    changes: Mutex<PendingHeadChanges>,
}

#[derive(Debug)]
struct ResolvedHeadUpdate {
    output_name: String,
    enabled: bool,
    mode: Option<Mode>,
    is_custom_mode: bool,
    position: Option<(i32, i32)>,
    transform: Transform,
    scale: f64,
    adaptive_sync: Option<bool>,
}

fn capture_output_snapshots(state: &KomaState) -> Vec<OutputHeadSnapshot> {
    let outputs = state.all_outputs();
    outputs
        .into_iter()
        .map(|out| {
            let name = out.name();
            let props = out.physical_properties();
            let current_mode = out.current_mode();
            let preferred_mode = out.preferred_mode().or(current_mode);
            let mut modes = out.modes();
            if modes.is_empty()
                && let Some(cur) = current_mode
            {
                modes.push(cur);
            }
            let enabled = state
                .output_layout
                .entry(&name)
                .is_none_or(|entry| entry.enabled);
            let position = state
                .output_layout
                .entry(&name)
                .map(|entry| entry.resolved_pos)
                .unwrap_or_else(|| {
                    let loc = out.current_location();
                    (loc.x, loc.y)
                });
            OutputHeadSnapshot {
                description: out.description(),
                make: props.make,
                model: props.model,
                serial_number: props.serial_number,
                phys_mm: (props.size.w, props.size.h),
                enabled,
                position,
                transform: WlTransform::from(out.current_transform()),
                scale: out.current_scale().fractional_scale(),
                current_mode,
                preferred_mode,
                modes,
                adaptive_sync: state.wlr_output_manager_state.adaptive_sync_for(&name),
                name,
            }
        })
        .collect()
}

fn create_mode_resource(
    dh: &DisplayHandle,
    client: &Client,
    head: &ZwlrOutputHeadV1,
    output_name: &str,
    mode: Mode,
    is_preferred: bool,
) -> Option<ZwlrOutputModeV1> {
    let mode_ver = head.version().min(WLR_OUTPUT_MODE_MAX_VERSION);
    let mode_res = client
        .create_resource::<ZwlrOutputModeV1, _, KomaState>(
            dh,
            mode_ver,
            OutputModeUserData {
                output_name: output_name.to_owned(),
                mode,
            },
        )
        .ok()?;
    head.mode(&mode_res);
    mode_res.size(mode.size.w, mode.size.h);
    if mode.refresh > 0 {
        mode_res.refresh(mode.refresh);
    }
    if is_preferred {
        mode_res.preferred();
    }
    Some(mode_res)
}

fn find_mode_resource(
    modes: &[(Weak<ZwlrOutputModeV1>, Mode)],
    target: Mode,
) -> Option<ZwlrOutputModeV1> {
    modes.iter().find_map(|(weak, m)| {
        if *m == target {
            weak.upgrade().ok().filter(Resource::is_alive)
        } else {
            None
        }
    })
}

fn emit_new_head(
    dh: &DisplayHandle,
    client: &Client,
    manager: &ZwlrOutputManagerV1,
    snap: &OutputHeadSnapshot,
) -> Option<HeadInstance> {
    let head = client
        .create_resource::<ZwlrOutputHeadV1, _, KomaState>(
            dh,
            manager.version().min(WLR_OUTPUT_MANAGER_VERSION),
            OutputHeadUserData {
                output_name: snap.name.clone(),
                manager_id: manager.id(),
            },
        )
        .ok()?;

    manager.head(&head);
    head.name(snap.name.clone());
    head.description(snap.description.clone());
    if snap.phys_mm.0 > 0 && snap.phys_mm.1 > 0 {
        head.physical_size(snap.phys_mm.0, snap.phys_mm.1);
    }
    if head.version() >= 2 {
        if !snap.make.is_empty() {
            head.make(snap.make.clone());
        }
        if !snap.model.is_empty() {
            head.model(snap.model.clone());
        }
        if !snap.serial_number.is_empty() {
            head.serial_number(snap.serial_number.clone());
        }
    }

    let mut mode_entries = Vec::with_capacity(snap.modes.len());
    for &mode in &snap.modes {
        if let Some(res) = create_mode_resource(
            dh,
            client,
            &head,
            &snap.name,
            mode,
            snap.preferred_mode == Some(mode),
        ) {
            mode_entries.push((res.downgrade(), mode));
        }
    }

    head.enabled(i32::from(snap.enabled));
    if snap.enabled {
        if let Some(cur) = snap.current_mode
            && let Some(cur_res) = find_mode_resource(&mode_entries, cur)
        {
            head.current_mode(&cur_res);
        }
        head.position(snap.position.0, snap.position.1);
        head.transform(snap.transform);
        head.scale(snap.scale);
        if head.version() >= 4 {
            head.adaptive_sync(if snap.adaptive_sync {
                AdaptiveSyncState::Enabled
            } else {
                AdaptiveSyncState::Disabled
            });
        }
    }

    Some(HeadInstance {
        head: head.downgrade(),
        modes: mode_entries,
        last_snapshot: snap.clone(),
    })
}

fn sync_existing_head(
    dh: &DisplayHandle,
    client: &Client,
    head_inst: &mut HeadInstance,
    snap: &OutputHeadSnapshot,
) -> bool {
    let Ok(head) = head_inst.head.upgrade() else {
        return false;
    };
    if !head.is_alive() {
        return false;
    }

    let prev = &head_inst.last_snapshot;
    if *prev == *snap {
        return false;
    }

    let mut changed = false;

    if prev.modes != snap.modes || prev.preferred_mode != snap.preferred_mode {
        head_inst.modes.retain(|(weak, old_mode)| {
            if !snap.modes.contains(old_mode) {
                if let Ok(m_res) = weak.upgrade()
                    && m_res.is_alive()
                {
                    m_res.finished();
                }
                changed = true;
                false
            } else {
                true
            }
        });
        for &mode in &snap.modes {
            if find_mode_resource(&head_inst.modes, mode).is_none()
                && let Some(res) = create_mode_resource(
                    dh,
                    client,
                    &head,
                    &snap.name,
                    mode,
                    snap.preferred_mode == Some(mode),
                )
            {
                head_inst.modes.push((res.downgrade(), mode));
                changed = true;
            }
        }
    }

    let became_enabled = !prev.enabled && snap.enabled;
    if prev.enabled != snap.enabled {
        head.enabled(i32::from(snap.enabled));
        changed = true;
    }

    if snap.enabled {
        if (became_enabled || prev.current_mode != snap.current_mode)
            && let Some(cur) = snap.current_mode
            && let Some(cur_res) = find_mode_resource(&head_inst.modes, cur)
        {
            head.current_mode(&cur_res);
            changed = true;
        }
        if became_enabled || prev.position != snap.position {
            head.position(snap.position.0, snap.position.1);
            changed = true;
        }
        if became_enabled || prev.transform != snap.transform {
            head.transform(snap.transform);
            changed = true;
        }
        if became_enabled || (prev.scale - snap.scale).abs() > 1e-6 {
            head.scale(snap.scale);
            changed = true;
        }
        if head.version() >= 4 && (became_enabled || prev.adaptive_sync != snap.adaptive_sync) {
            head.adaptive_sync(if snap.adaptive_sync {
                AdaptiveSyncState::Enabled
            } else {
                AdaptiveSyncState::Disabled
            });
            changed = true;
        }
    }

    head_inst.last_snapshot = snap.clone();
    changed
}

fn finish_head(mut head_inst: HeadInstance) {
    for (weak, _) in head_inst.modes.drain(..) {
        if let Ok(mode_res) = weak.upgrade()
            && mode_res.is_alive()
        {
            mode_res.finished();
        }
    }
    if let Ok(head) = head_inst.head.upgrade()
        && head.is_alive()
    {
        head.finished();
    }
}

/// Synchronizes all active `zwlr_output_manager_v1` clients with current output state and emits
/// `done(serial)` when any advertised head, mode, position, scale, transform, or enable bit changed.
pub fn sync_output_manager(state: &mut KomaState) {
    let snapshots = capture_output_snapshots(state);
    let dh = state.wlr_output_manager_state.display_handle.clone();
    let serial = state.wlr_output_manager_state.serial;
    let next_serial = serial.wrapping_add(1).max(1);

    let snapshot_map: HashMap<&str, &OutputHeadSnapshot> =
        snapshots.iter().map(|s| (s.name.as_str(), s)).collect();

    let mut dirty_managers = Vec::new();
    state.wlr_output_manager_state.managers.retain_mut(|inst| {
        if inst.stopped {
            return false;
        }
        let Ok(manager) = inst.manager.upgrade() else {
            return false;
        };
        if !manager.is_alive() {
            return false;
        }
        let Some(client) = manager.client() else {
            return false;
        };

        let mut changed = false;

        let removed_names: Vec<String> = inst
            .heads
            .keys()
            .filter(|name| !snapshot_map.contains_key(name.as_str()))
            .cloned()
            .collect();
        for name in removed_names {
            if let Some(removed) = inst.heads.remove(&name) {
                finish_head(removed);
                changed = true;
            }
        }

        for snap in &snapshots {
            if let Some(existing) = inst.heads.get_mut(&snap.name) {
                if sync_existing_head(&dh, &client, existing, snap) {
                    changed = true;
                }
            } else if let Some(new_head) = emit_new_head(&dh, &client, &manager, snap) {
                inst.heads.insert(snap.name.clone(), new_head);
                changed = true;
            }
        }

        if changed {
            dirty_managers.push(manager);
        }
        true
    });

    if !dirty_managers.is_empty() {
        state.wlr_output_manager_state.serial = next_serial;
        for manager in dirty_managers {
            if manager.is_alive() {
                manager.done(next_serial);
            }
        }
    }
}

fn initialize_manager_client(
    state: &mut KomaState,
    client: &Client,
    manager: &ZwlrOutputManagerV1,
) {
    let snapshots = capture_output_snapshots(state);
    let dh = state.wlr_output_manager_state.display_handle.clone();
    let serial = state.wlr_output_manager_state.serial;
    let mut heads = HashMap::with_capacity(snapshots.len());
    for snap in &snapshots {
        if let Some(head_inst) = emit_new_head(&dh, client, manager, snap) {
            heads.insert(snap.name.clone(), head_inst);
        }
    }
    manager.done(serial);
    state
        .wlr_output_manager_state
        .managers
        .push(ManagerInstance {
            manager: manager.downgrade(),
            stopped: false,
            heads,
        });
}

impl GlobalDispatch2<ZwlrOutputManagerV1, KomaState> for WlrOutputManagerGlobalData {
    fn can_view(&self, client: &Client) -> bool {
        client_is_unrestricted(client)
    }

    fn bind(
        &self,
        state: &mut KomaState,
        _dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        let manager = data_init.init(resource, OutputManagerUserData);
        initialize_manager_client(state, client, &manager);
    }
}

impl Dispatch2<ZwlrOutputManagerV1, KomaState> for OutputManagerUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    OutputConfigurationUserData {
                        manager_id: manager.id(),
                        serial,
                        state: Mutex::new(PendingConfigurationState::default()),
                    },
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                if let Some(inst) = state
                    .wlr_output_manager_state
                    .managers
                    .iter_mut()
                    .find(|m| m.manager.id() == manager.id())
                {
                    inst.stopped = true;
                }
                if manager.is_alive() {
                    manager.finished();
                }
            }
            _ => {}
        }
    }

    fn destroyed(&self, state: &mut KomaState, _client: ClientId, manager: &ZwlrOutputManagerV1) {
        state
            .wlr_output_manager_state
            .managers
            .retain(|m| m.manager.id() != manager.id());
    }
}

impl Dispatch2<ZwlrOutputHeadV1, KomaState> for OutputHeadUserData {
    fn request(
        &self,
        _state: &mut KomaState,
        _client: &Client,
        _head: &ZwlrOutputHeadV1,
        request: zwlr_output_head_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        if let zwlr_output_head_v1::Request::Release = request {
            // Wayland resource destruction removes the handle automatically.
        }
    }
}

impl Dispatch2<ZwlrOutputModeV1, KomaState> for OutputModeUserData {
    fn request(
        &self,
        _state: &mut KomaState,
        _client: &Client,
        _mode: &ZwlrOutputModeV1,
        request: zwlr_output_mode_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        if let zwlr_output_mode_v1::Request::Release = request {
            // Wayland resource destruction removes the mode automatically.
        }
    }
}

impl Dispatch2<ZwlrOutputConfigurationV1, KomaState> for OutputConfigurationUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        config: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let head_ud = head.data::<OutputHeadUserData>();
                let output_name = head_ud.map(|u| u.output_name.clone()).unwrap_or_default();
                let wrong_manager = head_ud.is_none_or(|u| u.manager_id != self.manager_id);
                let cfg_head = data_init.init(
                    id,
                    OutputConfigurationHeadUserData {
                        config_id: config.id(),
                        output_name: output_name.clone(),
                        changes: Mutex::new(PendingHeadChanges::default()),
                    },
                );
                let mut guard = self.state.lock().unwrap();
                if guard.used {
                    config.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration has already been applied or tested",
                    );
                    return;
                }
                if wrong_manager || guard.heads.contains_key(&head.id()) {
                    config.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                        format!("head `{output_name}` is invalid or already configured"),
                    );
                    return;
                }
                guard.heads.insert(
                    head.id(),
                    (output_name, ConfiguredHeadAction::Enable(cfg_head)),
                );
            }
            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                let head_ud = head.data::<OutputHeadUserData>();
                let output_name = head_ud.map(|u| u.output_name.clone()).unwrap_or_default();
                let wrong_manager = head_ud.is_none_or(|u| u.manager_id != self.manager_id);
                let mut guard = self.state.lock().unwrap();
                if guard.used {
                    config.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration has already been applied or tested",
                    );
                    return;
                }
                if wrong_manager || guard.heads.contains_key(&head.id()) {
                    config.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                        format!("head `{output_name}` is invalid or already configured"),
                    );
                    return;
                }
                guard
                    .heads
                    .insert(head.id(), (output_name, ConfiguredHeadAction::Disable));
            }
            zwlr_output_configuration_v1::Request::Apply => {
                handle_test_or_apply_configuration(
                    state,
                    config,
                    &self.manager_id,
                    self.serial,
                    &self.state,
                    false,
                );
            }
            zwlr_output_configuration_v1::Request::Test => {
                handle_test_or_apply_configuration(
                    state,
                    config,
                    &self.manager_id,
                    self.serial,
                    &self.state,
                    true,
                );
            }
            zwlr_output_configuration_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch2<ZwlrOutputConfigurationHeadV1, KomaState> for OutputConfigurationHeadUserData {
    fn request(
        &self,
        _state: &mut KomaState,
        _client: &Client,
        head_cfg: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        let mut pending = self.changes.lock().unwrap();
        let _ = self.config_id;
        match request {
            zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
                if pending.mode_already_set {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "mode or custom mode already set",
                    );
                    return;
                }
                pending.mode_already_set = true;
                match mode.data::<OutputModeUserData>() {
                    Some(ud) if ud.output_name == self.output_name => {
                        pending.mode = Some(ud.mode);
                    }
                    _ => {
                        pending.mode_invalid = true;
                        head_cfg.post_error(
                            zwlr_output_configuration_head_v1::Error::InvalidMode,
                            "mode does not belong to configured head",
                        );
                    }
                }
            }
            zwlr_output_configuration_head_v1::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                if pending.mode_already_set {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "mode or custom mode already set",
                    );
                    return;
                }
                if width <= 0 || height <= 0 || refresh < 0 {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidCustomMode,
                        format!("invalid custom mode {width}x{height}@{refresh}"),
                    );
                    return;
                }
                pending.mode_already_set = true;
                pending.custom_mode = Some(Mode {
                    size: (width, height).into(),
                    refresh,
                });
            }
            zwlr_output_configuration_head_v1::Request::SetPosition { x, y } => {
                if pending.position.is_some() {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "position already set",
                    );
                    return;
                }
                pending.position = Some((x, y));
            }
            zwlr_output_configuration_head_v1::Request::SetTransform { transform } => {
                if pending.transform.is_some() {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "transform already set",
                    );
                    return;
                }
                match transform {
                    WEnum::Value(val) => {
                        pending.transform = Some(val);
                    }
                    WEnum::Unknown(raw) => {
                        head_cfg.post_error(
                            zwlr_output_configuration_head_v1::Error::InvalidTransform,
                            format!("invalid transform value {raw}"),
                        );
                    }
                }
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                if pending.scale.is_some() {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "scale already set",
                    );
                    return;
                }
                if !scale.is_finite() || scale <= 0.0 {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::InvalidScale,
                        format!("invalid non-positive scale {scale}"),
                    );
                    return;
                }
                pending.scale = Some(scale);
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { state: sync_state } => {
                if pending.adaptive_sync.is_some() {
                    head_cfg.post_error(
                        zwlr_output_configuration_head_v1::Error::AlreadySet,
                        "adaptive sync already set",
                    );
                    return;
                }
                match sync_state {
                    WEnum::Value(AdaptiveSyncState::Disabled) => {
                        pending.adaptive_sync = Some(false);
                    }
                    WEnum::Value(AdaptiveSyncState::Enabled) => {
                        pending.adaptive_sync = Some(true);
                    }
                    _ => {
                        head_cfg.post_error(
                            zwlr_output_configuration_head_v1::Error::InvalidAdaptiveSyncState,
                            "invalid adaptive sync state",
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

fn validate_configuration(
    state: &KomaState,
    configured_entries: Vec<(String, Option<PendingHeadSnapshot>)>,
) -> Result<Vec<ResolvedHeadUpdate>, String> {
    let outputs = state.all_outputs();
    let by_name: HashMap<String, Output> = outputs.into_iter().map(|o| (o.name(), o)).collect();

    let mut seen_names = HashSet::new();
    let mut updates = Vec::with_capacity(configured_entries.len());

    for (output_name, opt_changes) in configured_entries {
        if !seen_names.insert(output_name.clone()) {
            return Err(format!("duplicate head configuration for `{output_name}`"));
        }
        let Some(output) = by_name.get(&output_name) else {
            return Err(format!("unknown or disconnected output `{output_name}`"));
        };

        let cur_transform = output.current_transform();
        let cur_scale = output.current_scale().fractional_scale();
        let existing_explicit_pos = state
            .output_layout
            .entry(&output_name)
            .and_then(|e| e.explicit_pos);

        match opt_changes {
            None => {
                updates.push(ResolvedHeadUpdate {
                    output_name,
                    enabled: false,
                    mode: None,
                    is_custom_mode: false,
                    position: existing_explicit_pos,
                    transform: cur_transform,
                    scale: cur_scale,
                    adaptive_sync: None,
                });
            }
            Some(ch) => {
                if ch.mode_invalid {
                    return Err(format!("invalid mode requested for `{output_name}`"));
                }

                let (mode, is_custom_mode) = if let Some(m) = ch.mode {
                    (Some(m), false)
                } else if let Some(cm) = ch.custom_mode {
                    (Some(cm), true)
                } else {
                    (
                        output
                            .current_mode()
                            .or_else(|| output.preferred_mode())
                            .or_else(|| output.modes().into_iter().next()),
                        false,
                    )
                };

                let Some(resolved_mode) = mode else {
                    return Err(format!("output `{output_name}` has no usable mode"));
                };
                if resolved_mode.size.w <= 0 || resolved_mode.size.h <= 0 {
                    return Err(format!(
                        "invalid mode dimensions {}x{} for `{output_name}`",
                        resolved_mode.size.w, resolved_mode.size.h
                    ));
                }

                #[cfg(feature = "smithay-tty")]
                if let Some(crate::state::ActiveBackend::Tty(tty)) = state.backend.as_ref() {
                    let available = tty.available_modes_for_output(output);
                    let supported = available.iter().any(|m| {
                        *m == resolved_mode
                            || (m.size == resolved_mode.size
                                && (resolved_mode.refresh <= 0
                                    || (m.refresh - resolved_mode.refresh).abs() <= 500))
                    });
                    if !supported {
                        return Err(format!(
                            "mode {}x{}@{}mHz unsupported on DRM output `{output_name}`",
                            resolved_mode.size.w, resolved_mode.size.h, resolved_mode.refresh
                        ));
                    }
                }

                let scale = match ch.scale {
                    Some(s) => {
                        if !s.is_finite() || !(0.5..=4.0).contains(&s) {
                            return Err(format!("scale {s} outside supported [0.5, 4.0] range"));
                        }
                        closest_representable_scale(s)
                    }
                    None => cur_scale,
                };
                let transform = ch.transform.map(Transform::from).unwrap_or(cur_transform);
                let position = ch.position.or(existing_explicit_pos);

                updates.push(ResolvedHeadUpdate {
                    output_name,
                    enabled: true,
                    mode: Some(resolved_mode),
                    is_custom_mode,
                    position,
                    transform,
                    scale,
                    adaptive_sync: ch.adaptive_sync,
                });
            }
        }
    }

    Ok(updates)
}

#[derive(Debug)]
struct PendingHeadSnapshot {
    mode: Option<Mode>,
    mode_invalid: bool,
    custom_mode: Option<Mode>,
    position: Option<(i32, i32)>,
    transform: Option<WlTransform>,
    scale: Option<f64>,
    adaptive_sync: Option<bool>,
}

fn handle_test_or_apply_configuration(
    state: &mut KomaState,
    config: &ZwlrOutputConfigurationV1,
    manager_id: &ObjectId,
    config_serial: u32,
    pending_mutex: &Mutex<PendingConfigurationState>,
    test_only: bool,
) {
    let entries_snapshot: Vec<(String, Option<PendingHeadSnapshot>)> = {
        let mut guard = pending_mutex.lock().unwrap();
        if guard.used {
            config.post_error(
                zwlr_output_configuration_v1::Error::AlreadyUsed,
                "configuration has already been applied or tested",
            );
            return;
        }
        guard.used = true;

        if config_serial != state.wlr_output_manager_state.current_serial() {
            config.cancelled();
            return;
        }

        let live_names = state
            .wlr_output_manager_state
            .live_head_names_for_manager(manager_id);
        let configured_names: HashSet<&str> =
            guard.heads.values().map(|(n, _)| n.as_str()).collect();
        if let Some(missing) = live_names
            .iter()
            .find(|name| !configured_names.contains(name.as_str()))
        {
            config.post_error(
                zwlr_output_configuration_v1::Error::UnconfiguredHead,
                format!("head `{missing}` was omitted from configuration"),
            );
            return;
        }

        guard
            .heads
            .values()
            .map(|(name, action)| {
                let snap = match action {
                    ConfiguredHeadAction::Disable => None,
                    ConfiguredHeadAction::Enable(head_cfg) => head_cfg
                        .data::<OutputConfigurationHeadUserData>()
                        .map(|ud| {
                            let c = ud.changes.lock().unwrap();
                            PendingHeadSnapshot {
                                mode: c.mode,
                                mode_invalid: c.mode_invalid,
                                custom_mode: c.custom_mode,
                                position: c.position,
                                transform: c.transform,
                                scale: c.scale,
                                adaptive_sync: c.adaptive_sync,
                            }
                        }),
                };
                (name.clone(), snap)
            })
            .collect()
    };

    let updates = match validate_configuration(state, entries_snapshot) {
        Ok(u) => u,
        Err(reason) => {
            debug!("zwlr_output_configuration validation failed: {reason}");
            config.failed();
            return;
        }
    };

    if test_only {
        config.succeeded();
        return;
    }

    if let Err(err) = apply_validated_configuration(state, updates) {
        warn!("zwlr_output_configuration apply failed: {err}");
        config.failed();
        return;
    }

    config.succeeded();
    sync_output_manager(state);
}

fn apply_validated_configuration(
    state: &mut KomaState,
    updates: Vec<ResolvedHeadUpdate>,
) -> Result<(), String> {
    let outputs = state.all_outputs();
    let by_name: HashMap<String, Output> = outputs.into_iter().map(|o| (o.name(), o)).collect();

    #[cfg(feature = "smithay-tty")]
    if let Some(crate::state::ActiveBackend::Tty(mut tty)) = state
        .backend
        .take()
        .filter(|b| matches!(b, crate::state::ActiveBackend::Tty(_)))
    {
        for upd in &updates {
            if upd.enabled
                && let Some(mode) = upd.mode
                && let Some(output) = by_name.get(&upd.output_name)
                && output.current_mode() != Some(mode)
                && let Err(err) = tty.apply_output_mode(output, mode)
            {
                state.backend = Some(crate::state::ActiveBackend::Tty(tty));
                return Err(err);
            }
        }
        state.backend = Some(crate::state::ActiveBackend::Tty(tty));
    }

    for upd in &updates {
        let Some(output) = by_name.get(&upd.output_name) else {
            continue;
        };
        if let Some(adaptive) = upd.adaptive_sync {
            state
                .wlr_output_manager_state
                .adaptive_sync_enabled
                .insert(upd.output_name.clone(), adaptive);
        }

        if upd.enabled {
            if upd.is_custom_mode
                && let Some(cm) = upd.mode
                && !output.modes().contains(&cm)
            {
                output.add_mode(cm);
            }
            output.change_current_state(
                upd.mode,
                Some(upd.transform),
                Some(Scale::Fractional(upd.scale)),
                None,
            );
            let (lw, lh, scale) = output_logical_viewport(output);
            let koma_transform: OutputTransform = smithay_transform_to_koma(upd.transform);
            state.output_layout.upsert_output(
                upd.output_name.clone(),
                koma_model::OutputLayoutSpec {
                    logical_w: lw,
                    logical_h: lh,
                    scale,
                    transform: koma_transform,
                    explicit_pos: upd.position,
                    enabled: true,
                },
            );
        } else {
            let (lw, lh, scale) = output_logical_viewport(output);
            let koma_transform = smithay_transform_to_koma(output.current_transform());
            state.output_layout.upsert_output(
                upd.output_name.clone(),
                koma_model::OutputLayoutSpec {
                    logical_w: lw,
                    logical_h: lh,
                    scale,
                    transform: koma_transform,
                    explicit_pos: upd.position,
                    enabled: false,
                },
            );
            state.output_workspaces.remove(&upd.output_name);
            state.output_layout.record_hotplug_state(
                &upd.output_name,
                SavedOutputConfig {
                    active_ws_id: state
                        .output_layout
                        .saved_hotplug_state(&upd.output_name)
                        .and_then(|s| s.active_ws_id.clone()),
                    position: upd.position,
                    scale: Some(scale),
                    transform: Some(koma_transform),
                    mode: output
                        .current_mode()
                        .map(|m| (m.size.w, m.size.h, m.refresh.max(0) as u32)),
                    enabled: false,
                },
            );
        }
    }

    state.apply_output_layout_positions();

    // Ensure every enabled output owns a workspace and `focused_output_name` points at one.
    let mut enabled_outputs = Vec::new();
    for upd in &updates {
        if let Some(output) = by_name.get(&upd.output_name) {
            if upd.enabled {
                enabled_outputs.push(output.clone());
                let ws_id = state.ensure_initial_workspace_for_output(output);
                let resolved_pos = state
                    .output_layout
                    .entry(&upd.output_name)
                    .map(|e| e.explicit_pos.or(Some(e.resolved_pos)))
                    .unwrap_or(upd.position);
                let cur_mode = output
                    .current_mode()
                    .map(|m| (m.size.w, m.size.h, m.refresh.max(0) as u32));
                let transform = smithay_transform_to_koma(output.current_transform());
                state.output_layout.record_hotplug_state(
                    &upd.output_name,
                    SavedOutputConfig {
                        active_ws_id: Some(ws_id.clone()),
                        position: resolved_pos,
                        scale: Some(output.current_scale().fractional_scale()),
                        transform: Some(transform),
                        mode: cur_mode,
                        enabled: true,
                    },
                );
                state.sync_output_workarea(output);
                state.sync_window_output_membership(&ws_id);
                state.sync_output_layer_scales(output);
            } else if state.focused_output_name.as_deref() == Some(upd.output_name.as_str()) {
                state.focused_output_name = None;
            }
        }
    }

    if state.focused_output_name.is_none()
        && let Some(first) = enabled_outputs.first()
    {
        let first_name = first.name();
        state.focused_output_name = Some(first_name.clone());
        state.active_ws_id = state.active_workspace_for_output(&first_name);
    }

    state.clamp_and_update_pointer_bounds();
    state.reconcile_keyboard_focus();
    state.sync_foreign_toplevels();
    state.queue_redraw();
    Ok(())
}
