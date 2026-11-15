//! `zwlr_gamma_control_manager_v1` protocol implementation.
//!
//! Provides exclusive per-output gamma LUT ownership over DRM CRTCs across Linux and BSD
//! platforms, validates gamma table file descriptors without unsafe `mmap`, and restores
//! the original LUT when the control object is destroyed or disconnected.

use std::{
    collections::HashMap,
    fs::File,
    io::{Read as _, Seek as _, SeekFrom},
    os::fd::OwnedFd,
    sync::Mutex,
};

use smithay::{
    output::{Output, WeakOutput},
    reexports::{
        wayland_protocols_wlr::gamma_control::v1::server::{
            zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
            zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
        },
        wayland_server::{Client, DataInit, DisplayHandle, New, Resource, Weak, backend::ClientId},
    },
    wayland::{Dispatch2, GlobalDispatch2},
};
use tracing::{debug, warn};

use crate::{security_context::client_is_unrestricted, state::KomaState};

const GAMMA_CONTROL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GammaLut {
    pub red: Vec<u16>,
    pub green: Vec<u16>,
    pub blue: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GammaValidationError {
    UnsupportedGammaSize,
    InvalidFileSize { expected: usize, actual: u64 },
    IoError(String),
}

impl std::fmt::Display for GammaValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedGammaSize => write!(f, "output does not support gamma LUTs"),
            Self::InvalidFileSize { expected, actual } => {
                write!(
                    f,
                    "gamma LUT size mismatch: expected {expected} bytes, got {actual}"
                )
            }
            Self::IoError(msg) => write!(f, "failed reading gamma LUT fd: {msg}"),
        }
    }
}

/// Safely reads and validates `(red, green, blue)` `u16` ramps from a client-supplied FD
/// without `mmap` (avoiding `SIGBUS` UB on truncated files across Linux/FreeBSD/OpenBSD).
pub fn read_and_validate_gamma_lut(
    fd: OwnedFd,
    gamma_size: u32,
) -> Result<GammaLut, GammaValidationError> {
    if gamma_size == 0 {
        return Err(GammaValidationError::UnsupportedGammaSize);
    }
    let ramp_len = gamma_size as usize;
    let expected_bytes = ramp_len
        .checked_mul(3)
        .and_then(|elems| elems.checked_mul(std::mem::size_of::<u16>()))
        .ok_or(GammaValidationError::UnsupportedGammaSize)?;

    let mut file = File::from(fd);
    let metadata_len = file
        .metadata()
        .map_err(|err| GammaValidationError::IoError(err.to_string()))?
        .len();
    if metadata_len != expected_bytes as u64 {
        return Err(GammaValidationError::InvalidFileSize {
            expected: expected_bytes,
            actual: metadata_len,
        });
    }

    let _ = file.seek(SeekFrom::Start(0));
    let mut raw = vec![0_u8; expected_bytes];
    file.read_exact(&mut raw)
        .map_err(|err| GammaValidationError::IoError(err.to_string()))?;

    let mut trailing = [0_u8; 1];
    let extra = file
        .read(&mut trailing)
        .map_err(|err| GammaValidationError::IoError(err.to_string()))?;
    if extra != 0 {
        return Err(GammaValidationError::InvalidFileSize {
            expected: expected_bytes,
            actual: expected_bytes as u64 + extra as u64,
        });
    }

    let ramp_bytes = ramp_len * 2;
    let red = decode_u16_ramp(&raw[..ramp_bytes]);
    let green = decode_u16_ramp(&raw[ramp_bytes..ramp_bytes * 2]);
    let blue = decode_u16_ramp(&raw[ramp_bytes * 2..ramp_bytes * 3]);

    Ok(GammaLut { red, green, blue })
}

fn decode_u16_ramp(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| u16::from_ne_bytes(*chunk))
        .collect()
}

#[derive(Debug)]
pub struct GammaControlGlobalData;

#[derive(Debug)]
pub struct GammaControlManagerUserData;

#[derive(Debug)]
pub struct GammaControlUserData {
    pub output_name: String,
    pub output: WeakOutput,
    pub gamma_size: u32,
    pub failed: Mutex<bool>,
}

#[derive(Debug)]
pub struct ActiveGammaEntry {
    pub controller: Weak<ZwlrGammaControlV1>,
    pub output: WeakOutput,
    pub original_lut: Option<GammaLut>,
}

/// Exclusive per-output gamma control state and original LUT restoration ledger.
#[derive(Debug, Default)]
pub struct GammaControlState {
    pub active: HashMap<String, ActiveGammaEntry>,
}

impl GammaControlState {
    #[must_use]
    pub fn new(display_handle: &DisplayHandle) -> Self {
        display_handle.create_global::<KomaState, ZwlrGammaControlManagerV1, _>(
            GAMMA_CONTROL_VERSION,
            GammaControlGlobalData,
        );
        Self::default()
    }

    /// Marks any active controller on a removed output as failed.
    #[cfg_attr(not(feature = "smithay-tty"), allow(dead_code))]
    pub fn notify_output_removed(&mut self, output_name: &str) {
        if let Some(entry) = self.active.remove(output_name)
            && let Ok(ctrl) = entry.controller.upgrade()
            && ctrl.is_alive()
        {
            if let Some(ud) = ctrl.data::<GammaControlUserData>() {
                *ud.failed.lock().unwrap() = true;
            }
            ctrl.failed();
        }
    }
}

impl GlobalDispatch2<ZwlrGammaControlManagerV1, KomaState> for GammaControlGlobalData {
    fn can_view(&self, client: &Client) -> bool {
        client_is_unrestricted(client)
    }

    fn bind(
        &self,
        _state: &mut KomaState,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        data_init.init(resource, GammaControlManagerUserData);
    }
}

impl Dispatch2<ZwlrGammaControlManagerV1, KomaState> for GammaControlManagerUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        _resource: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        if let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } = request {
            let Some(resolved) = Output::from_resource(&output) else {
                let ctrl = init_failed_gamma_ctrl(id, data_init);
                ctrl.failed();
                return;
            };

            let name = resolved.name();
            let already_owned = state
                .gamma_control_state
                .active
                .get(&name)
                .and_then(|e| e.controller.upgrade().ok())
                .is_some_and(|c| c.is_alive());
            if already_owned {
                let ctrl = init_failed_gamma_ctrl(id, data_init);
                ctrl.failed();
                return;
            }

            let Some((gamma_size, original_lut)) = state.query_output_gamma_info(&resolved) else {
                let ctrl = init_failed_gamma_ctrl(id, data_init);
                ctrl.failed();
                return;
            };
            if gamma_size == 0 {
                let ctrl = init_failed_gamma_ctrl(id, data_init);
                ctrl.failed();
                return;
            }

            let ctrl = data_init.init(
                id,
                GammaControlUserData {
                    output_name: name.clone(),
                    output: resolved.downgrade(),
                    gamma_size,
                    failed: Mutex::new(false),
                },
            );
            state.gamma_control_state.active.insert(
                name,
                ActiveGammaEntry {
                    controller: ctrl.downgrade(),
                    output: resolved.downgrade(),
                    original_lut,
                },
            );
            ctrl.gamma_size(gamma_size);
        }
    }
}

fn init_failed_gamma_ctrl(
    id: New<ZwlrGammaControlV1>,
    data_init: &mut DataInit<'_, KomaState>,
) -> ZwlrGammaControlV1 {
    let dummy = Output::new(
        "<invalid>".to_owned(),
        smithay::output::PhysicalProperties {
            size: (0, 0).into(),
            subpixel: smithay::output::Subpixel::Unknown,
            make: String::new(),
            model: String::new(),
            serial_number: String::new(),
        },
    )
    .downgrade();
    data_init.init(
        id,
        GammaControlUserData {
            output_name: String::new(),
            output: dummy,
            gamma_size: 0,
            failed: Mutex::new(true),
        },
    )
}

impl Dispatch2<ZwlrGammaControlV1, KomaState> for GammaControlUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        ctrl: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        match request {
            zwlr_gamma_control_v1::Request::SetGamma { fd } => {
                if *self.failed.lock().unwrap() {
                    return;
                }
                let Some(output) = self.output.upgrade() else {
                    *self.failed.lock().unwrap() = true;
                    state.gamma_control_state.active.remove(&self.output_name);
                    ctrl.failed();
                    return;
                };

                let lut = match read_and_validate_gamma_lut(fd, self.gamma_size) {
                    Ok(lut) => lut,
                    Err(err) => {
                        *self.failed.lock().unwrap() = true;
                        restore_and_release_gamma(state, &self.output_name, ctrl);
                        ctrl.post_error(
                            zwlr_gamma_control_v1::Error::InvalidGamma,
                            err.to_string(),
                        );
                        return;
                    }
                };

                if let Err(err) = state.apply_output_gamma(&output, &lut) {
                    warn!(output = %self.output_name, "DRM set_gamma failed: {err}");
                    *self.failed.lock().unwrap() = true;
                    restore_and_release_gamma(state, &self.output_name, ctrl);
                    ctrl.failed();
                    return;
                }
                debug!(output = %self.output_name, size = self.gamma_size, "applied CRTC gamma LUT");
            }
            zwlr_gamma_control_v1::Request::Destroy => {
                restore_and_release_gamma(state, &self.output_name, ctrl);
            }
            _ => {}
        }
    }

    fn destroyed(&self, state: &mut KomaState, _client: ClientId, ctrl: &ZwlrGammaControlV1) {
        restore_and_release_gamma(state, &self.output_name, ctrl);
    }
}

fn restore_and_release_gamma(state: &mut KomaState, output_name: &str, ctrl: &ZwlrGammaControlV1) {
    let matches = state
        .gamma_control_state
        .active
        .get(output_name)
        .is_some_and(|e| e.controller.id() == ctrl.id());
    if !matches {
        return;
    }
    if let Some(entry) = state.gamma_control_state.active.remove(output_name)
        && let (Some(orig), Some(output)) = (entry.original_lut, entry.output.upgrade())
        && let Err(err) = state.apply_output_gamma(&output, &orig)
    {
        warn!(output = %output_name, "failed restoring original CRTC gamma LUT: {err}");
    }
}
