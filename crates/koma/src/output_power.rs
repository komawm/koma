//! `zwlr_output_power_management_v1` (DPMS) protocol implementation.
//!
//! Exposes exclusive per-output power control (`On` / `Off`), driving DRM connector DPMS
//! properties on bare-metal TTY (Linux and FreeBSD/OpenBSD DRM drivers) while maintaining
//! deterministic logical power state in nested Winit and headless sessions.

use std::{collections::HashMap, sync::Mutex};

use smithay::{
    output::{Output, WeakOutput},
    reexports::{
        wayland_protocols_wlr::output_power_management::v1::server::{
            zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
            zwlr_output_power_v1::{self, Mode, ZwlrOutputPowerV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, WEnum, Weak, backend::ClientId,
        },
    },
    wayland::{Dispatch2, GlobalDispatch2},
};
use tracing::{debug, warn};

use crate::{security_context::client_is_unrestricted, state::KomaState};

const OUTPUT_POWER_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputPowerMode {
    Off,
    #[default]
    On,
}

impl OutputPowerMode {
    #[must_use]
    pub const fn to_protocol(self) -> Mode {
        match self {
            Self::Off => Mode::Off,
            Self::On => Mode::On,
        }
    }
}

#[derive(Debug)]
pub struct OutputPowerGlobalData;

#[derive(Debug)]
pub struct OutputPowerManagerUserData;

#[derive(Debug)]
pub struct OutputPowerUserData {
    pub output_name: String,
    pub output: WeakOutput,
    pub failed: Mutex<bool>,
}

/// Exclusive per-output DPMS / power state ledger stored on `KomaState`.
#[derive(Debug, Default)]
pub struct OutputPowerState {
    pub modes: HashMap<String, OutputPowerMode>,
    pub controllers: HashMap<String, Weak<ZwlrOutputPowerV1>>,
}

impl OutputPowerState {
    #[must_use]
    pub fn new(display_handle: &DisplayHandle) -> Self {
        display_handle.create_global::<KomaState, ZwlrOutputPowerManagerV1, _>(
            OUTPUT_POWER_VERSION,
            OutputPowerGlobalData,
        );
        Self::default()
    }

    #[must_use]
    pub fn mode_for(&self, output_name: &str) -> OutputPowerMode {
        self.modes
            .get(output_name)
            .copied()
            .unwrap_or(OutputPowerMode::On)
    }

    #[cfg_attr(not(feature = "smithay-tty"), allow(dead_code))]
    #[must_use]
    pub fn is_output_off(&self, output_name: &str) -> bool {
        self.mode_for(output_name) == OutputPowerMode::Off
    }

    /// Transitions an output's power mode directly (used by backends/tests/DPMS handlers).
    #[allow(dead_code)]
    pub fn set_logical_mode(&mut self, output_name: &str, mode: OutputPowerMode) {
        self.modes.insert(output_name.to_owned(), mode);
        if let Some(ctrl) = self
            .controllers
            .get(output_name)
            .and_then(|w| w.upgrade().ok())
            && ctrl.is_alive()
        {
            ctrl.mode(mode.to_protocol());
        }
    }

    /// Marks any active power controller for an unplugged/destroyed output as failed.
    #[cfg_attr(not(feature = "smithay-tty"), allow(dead_code))]
    pub fn notify_output_removed(&mut self, output_name: &str) {
        self.modes.remove(output_name);
        if let Some(ctrl) = self
            .controllers
            .remove(output_name)
            .and_then(|w| w.upgrade().ok())
            && ctrl.is_alive()
        {
            if let Some(ud) = ctrl.data::<OutputPowerUserData>() {
                *ud.failed.lock().unwrap() = true;
            }
            ctrl.failed();
        }
    }
}

impl GlobalDispatch2<ZwlrOutputPowerManagerV1, KomaState> for OutputPowerGlobalData {
    fn can_view(&self, client: &Client) -> bool {
        client_is_unrestricted(client)
    }

    fn bind(
        &self,
        _state: &mut KomaState,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputPowerManagerV1>,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        data_init.init(resource, OutputPowerManagerUserData);
    }
}

impl Dispatch2<ZwlrOutputPowerManagerV1, KomaState> for OutputPowerManagerUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        _resource: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        if let zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } = request {
            let Some(resolved) = Output::from_resource(&output) else {
                let ctrl = data_init.init(
                    id,
                    OutputPowerUserData {
                        output_name: String::new(),
                        output: output_dummy_weak(),
                        failed: Mutex::new(true),
                    },
                );
                ctrl.failed();
                return;
            };

            let name = resolved.name();
            let output_active = state.all_outputs().iter().any(|o| o.name() == name);
            let already_controlled = state
                .output_power_state
                .controllers
                .get(&name)
                .and_then(|w| w.upgrade().ok())
                .is_some_and(|c| c.is_alive());

            let ctrl = data_init.init(
                id,
                OutputPowerUserData {
                    output_name: name.clone(),
                    output: resolved.downgrade(),
                    failed: Mutex::new(!output_active || already_controlled),
                },
            );

            if !output_active || already_controlled {
                ctrl.failed();
                return;
            }

            let current = state.output_power_state.mode_for(&name);
            state
                .output_power_state
                .controllers
                .insert(name, ctrl.downgrade());
            ctrl.mode(current.to_protocol());
        }
    }
}

fn output_dummy_weak() -> WeakOutput {
    Output::new(
        "<invalid>".to_owned(),
        smithay::output::PhysicalProperties {
            size: (0, 0).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: String::new(),
            model: String::new(),
            serial_number: String::new(),
        },
    )
    .downgrade()
}

impl Dispatch2<ZwlrOutputPowerV1, KomaState> for OutputPowerUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        ctrl: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        match request {
            zwlr_output_power_v1::Request::SetMode { mode } => {
                let parsed_mode = match mode {
                    WEnum::Value(val) => match val {
                        Mode::Off => OutputPowerMode::Off,
                        Mode::On => OutputPowerMode::On,
                        _ => {
                            ctrl.post_error(
                                zwlr_output_power_v1::Error::InvalidMode,
                                "unknown output power mode",
                            );
                            return;
                        }
                    },
                    WEnum::Unknown(raw) => {
                        ctrl.post_error(
                            zwlr_output_power_v1::Error::InvalidMode,
                            format!("invalid output power mode {raw}"),
                        );
                        return;
                    }
                };

                if *self.failed.lock().unwrap() {
                    return;
                }
                let Some(output) = self.output.upgrade() else {
                    *self.failed.lock().unwrap() = true;
                    ctrl.failed();
                    return;
                };
                if !state
                    .all_outputs()
                    .iter()
                    .any(|o| o.name() == self.output_name)
                {
                    *self.failed.lock().unwrap() = true;
                    ctrl.failed();
                    return;
                }

                if let Err(err) = state.apply_output_power_mode(&output, parsed_mode) {
                    warn!(output = %self.output_name, "DPMS set_mode failed: {err}");
                    *self.failed.lock().unwrap() = true;
                    state
                        .output_power_state
                        .controllers
                        .remove(&self.output_name);
                    ctrl.failed();
                    return;
                }

                let prev = state.output_power_state.mode_for(&self.output_name);
                state
                    .output_power_state
                    .modes
                    .insert(self.output_name.clone(), parsed_mode);
                if prev != parsed_mode {
                    ctrl.mode(parsed_mode.to_protocol());
                    debug!(output = %self.output_name, ?parsed_mode, "updated output power mode");
                    if parsed_mode == OutputPowerMode::On {
                        state.queue_redraw();
                    }
                }
            }
            zwlr_output_power_v1::Request::Destroy => {
                release_controller(state, &self.output_name, ctrl);
            }
            _ => {}
        }
    }

    fn destroyed(&self, state: &mut KomaState, _client: ClientId, ctrl: &ZwlrOutputPowerV1) {
        release_controller(state, &self.output_name, ctrl);
    }
}

fn release_controller(state: &mut KomaState, output_name: &str, ctrl: &ZwlrOutputPowerV1) {
    if let Some(existing) = state.output_power_state.controllers.get(output_name)
        && existing.id() == ctrl.id()
    {
        state.output_power_state.controllers.remove(output_name);
    }
}
