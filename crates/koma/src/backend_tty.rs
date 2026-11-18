//! Bare-metal `libseat` + `udev` + `libinput` + `DRM/KMS` display & input backend
//! (modeled directly on `niri/src/backend/tty.rs`).
//!
//! Key architectural invariants preserved from Niri:
//! 1. **Unprivileged Seat Management (`LibSeatSession`)**: Runs rootless under `seatd` or
//!    `systemd-logind`, cleanly suspending `libinput` and `DrmDevice` on `SessionEvent::PauseSession`
//!    (`Ctrl + Alt + F1..F12` VT switch) and re-activating + hotplug-rescanning on `ActivateSession`.
//! 2. **Hybrid iGPU + NVIDIA dGPU Multi-GPU Pipeline (`GpuManager<GbmGlesBackend>`)**: Selects the
//!    primary render node via `KOMA_DRM_DEVICE` or `udev::primary_gpu(seat)` and negotiates
//!    `[Fourcc::Abgr2101010, Fourcc::Argb8888, Fourcc::Abgr8888]` so NVIDIA dGPU <-> Intel/AMD iGPU
//!    cross-device scanout copies never fail on `X/Argb`-only restrictions.
//! 3. **Atomic KMS VBlank Loop (`DrmCompositor`)**: Filters problematic high-bandwidth CCS modifiers,
//!    falls back to `Modifier::Invalid` for legacy drivers, enables primary & cursor plane direct
//!    scanout, and gates frames strictly on `DrmEvent::VBlank(crtc)` -> `frame_submitted()`.

use std::{
    collections::HashMap,
    env, mem,
    os::fd::{AsFd, BorrowedFd},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, anyhow, bail, ensure};
use calloop::{Dispatcher, LoopHandle, RegistrationToken};
use koma_lua::schema::{MonitorScaleSpec, MonitorSpec};
use koma_model::{LogicalViewport, OutputTransform, closest_representable_scale};
use koma_proto::OverlayKind;
use koma_render::{
    RenderTargetKind,
    elements::KomaRenderElement,
    frame_composer::{
        DEFAULT_BACKDROP_CLEAR, FrameBlurContext, append_scratchpad_dim_scrim,
        append_tab_indicator_elements, assemble_workspace_elements, prepend_agent_cursor_elements,
        prepend_config_error_banner,
    },
};
use libc::dev_t;
use smithay::{
    backend::{
        allocator::{
            Fourcc,
            dmabuf::Dmabuf,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType, VrrSupport,
            compositor::{DrmCompositor, FrameFlags, PrimaryPlaneElement},
            exporter::gbm::GbmFramebufferExporter,
        },
        egl::{EGLDevice, EGLDisplay, context::ContextPriority},
        input::{
            AbsolutePositionEvent as _, Event as _, InputBackend, InputEvent,
            KeyboardKeyEvent as _, PointerButtonEvent as _, PointerMotionEvent as _,
        },
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            Color32F, ImportDma as _,
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{
            Event as SessionEvent, Session as _,
            libseat::{Error as LibSeatError, LibSeatSession},
        },
        udev::{self, UdevBackend, UdevEvent},
    },
    input::keyboard::KeyboardSource,
    output::{Mode, Output, OutputModeSource, PhysicalProperties, Scale},
    reexports::{
        drm::control::{self, Device as _, ModeFlags, ModeTypeFlags, connector, crtc, property},
        gbm::Modifier,
        input::Libinput,
        rustix::{fs::OFlags, io::Errno},
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Point, SERIAL_COUNTER, Scale as MathScale},
    wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal},
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, info, trace, warn};

use crate::{
    backend_winit::{
        append_ime_popup_elements, append_layer_tier_elements, assemble_locked_frame_elements,
        dispatch_output_frame_callbacks_filtered,
    },
    edid::ParsedEdid,
    state::{ActiveBackend, KomaState, koma_transform_to_smithay},
};

/// Hybrid NVIDIA dGPU -> Intel/AMD iGPU cross-device copies require `Abgr` formats alongside `Argb`
/// (`niri/src/backend/tty.rs` lines 74-80).
const SUPPORTED_COLOR_FORMATS_10BIT: [Fourcc; 3] =
    [Fourcc::Abgr2101010, Fourcc::Argb8888, Fourcc::Abgr8888];

const SUPPORTED_COLOR_FORMATS_8BIT: [Fourcc; 4] = [
    Fourcc::Argb8888,
    Fourcc::Abgr8888,
    Fourcc::Xrgb8888,
    Fourcc::Xbgr8888,
];

/// Multi-GPU OpenGL ES renderer spanning primary render node and target scanout node.
pub type TtyRenderer<'render> = MultiRenderer<
    'render,
    'render,
    GbmGlesBackend<GlesRenderer, DeviceFd>,
    GbmGlesBackend<GlesRenderer, DeviceFd>,
>;

type GbmDrmCompositor =
    DrmCompositor<GbmAllocator<DeviceFd>, GbmFramebufferExporter<DeviceFd>, (), DeviceFd>;

/// Attached to each `smithay::output::Output::user_data()` for O(1) `(DrmNode, crtc::Handle)` lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtyOutputTag {
    pub node: DrmNode,
    pub crtc: crtc::Handle,
}

/// Per-CRTC VBlank state machine preventing double-queued atomic page flips (`EBUSY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRedrawState {
    Idle,
    Queued,
    WaitingForVBlank { redraw_needed: bool },
}

/// Active KMS CRTC scanout surface managed by `DrmCompositor`.
pub struct DrmOutputSurface {
    // Physical connector handle retained for DRM hotplug, DPMS, and output lifecycle tracking.
    pub connector: connector::Handle,
    pub connector_name: String,
    pub make: String,
    pub model: String,
    pub serial_number: String,
    pub phys_mm: (i32, i32),
    pub panel_orientation: Option<OutputTransform>,
    pub available_modes: Vec<Mode>,
    pub drm_modes: Vec<control::Mode>,
    pub output: Output,
    pub wl_global: Option<GlobalId>,
    pub compositor: GbmDrmCompositor,
    // RAII per-surface DMA-BUF feedback handle kept alive for the surface lifetime.
    #[allow(dead_code)]
    pub dmabuf_feedback: Option<DmabufFeedback>,
    pub redraw_state: SurfaceRedrawState,
    pub refresh_mhz: i32,
}

/// Physical DRM card (`/dev/dri/cardX`) opened unprivileged via `LibSeatSession::open`.
pub struct DrmOutputDevice {
    pub token: RegistrationToken,
    pub render_node: Option<DrmNode>,
    /// Rendering through Mesa's software rasterizer on a display-only device.
    pub software_rendering: bool,
    pub drm_scanner: DrmScanner,
    pub surfaces: HashMap<crtc::Handle, DrmOutputSurface>,
    // Drop `drm` and `gbm` after `surfaces` (`https://github.com/Smithay/smithay/issues/1102`).
    pub drm: DrmDevice,
    pub gbm: GbmDevice<DeviceFd>,
    pub allocator: GbmAllocator<DeviceFd>,
}

/// Tracks split display/render DRM nodes, hardware render-node promotion (Apple Silicon
/// `apple-drm` + `asahi` AGX, dual-GPU `i915` + `amdgpu`), and shared `render_node` refcounting
/// across hot-unplug cycles.
#[derive(Debug, Clone)]
pub(crate) struct RenderNodeRegistry<N = DrmNode>
where
    N: Copy + Eq + std::hash::Hash,
{
    pub primary_render_node: N,
    pub has_hardware_primary: bool,
    pub primary_uses_software: bool,
    pub explicit_render_override: Option<N>,
    pub render_refcounts: HashMap<N, usize>,
    pub card_render_target: HashMap<N, N>,
    pub card_hardware_render: HashMap<N, Option<N>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderRegistrationOutcome<N = DrmNode> {
    pub target_render_node: N,
    pub promoted_from: Option<N>,
    pub nodes_to_remove: Vec<N>,
}

impl<N> RenderNodeRegistry<N>
where
    N: Copy + Eq + std::hash::Hash,
{
    pub(crate) fn new(initial_render_node: N, explicit_render_override: Option<N>) -> Self {
        let mut render_refcounts = HashMap::new();
        let has_hardware_primary = explicit_render_override.is_some();
        if let Some(rn) = explicit_render_override {
            render_refcounts.insert(rn, 1);
        }
        Self {
            primary_render_node: explicit_render_override.unwrap_or(initial_render_node),
            has_hardware_primary,
            primary_uses_software: false,
            explicit_render_override,
            render_refcounts,
            card_render_target: HashMap::new(),
            card_hardware_render: HashMap::new(),
        }
    }

    pub(crate) fn register_card(
        &mut self,
        card: N,
        hardware_render: Option<N>,
        software_render: Option<N>,
    ) -> RenderRegistrationOutcome<N> {
        let mut promoted_from = None;
        let mut nodes_to_remove = Vec::new();

        if let Some(prev_target) = self.card_render_target.remove(&card) {
            Self::dec_refcount(
                &mut self.render_refcounts,
                prev_target,
                &mut nodes_to_remove,
            );
        }

        let target = if let Some(hw_rn) = hardware_render {
            *self.render_refcounts.entry(hw_rn).or_insert(0) += 1;
            self.card_render_target.insert(card, hw_rn);
            self.card_hardware_render.insert(card, Some(hw_rn));

            if !self.has_hardware_primary && self.explicit_render_override.is_none() {
                let old_primary = self.primary_render_node;
                self.primary_render_node = hw_rn;
                self.has_hardware_primary = true;
                self.primary_uses_software = false;
                if old_primary != hw_rn {
                    promoted_from = Some(old_primary);
                }

                let borrowed_cards: Vec<N> = self
                    .card_hardware_render
                    .iter()
                    .filter(|(c, own)| **c != card && own.is_none())
                    .map(|(c, _)| *c)
                    .collect();
                for c in borrowed_cards {
                    if let Some(&old_target) = self.card_render_target.get(&c)
                        && old_target != hw_rn
                    {
                        Self::dec_refcount(
                            &mut self.render_refcounts,
                            old_target,
                            &mut nodes_to_remove,
                        );
                        *self.render_refcounts.entry(hw_rn).or_insert(0) += 1;
                        self.card_render_target.insert(c, hw_rn);
                    }
                }
            }
            hw_rn
        } else if let Some(sw_rn) = software_render {
            *self.render_refcounts.entry(sw_rn).or_insert(0) += 1;
            self.card_render_target.insert(card, sw_rn);
            self.card_hardware_render.insert(card, None);
            if !self.has_hardware_primary && self.explicit_render_override.is_none() {
                self.primary_render_node = sw_rn;
                self.primary_uses_software = true;
            }
            sw_rn
        } else {
            let borrowed = self.primary_render_node;
            *self.render_refcounts.entry(borrowed).or_insert(0) += 1;
            self.card_render_target.insert(card, borrowed);
            self.card_hardware_render.insert(card, None);
            borrowed
        };

        nodes_to_remove
            .retain(|rn| *rn != target && self.render_refcounts.get(rn).copied().unwrap_or(0) == 0);
        RenderRegistrationOutcome {
            target_render_node: target,
            promoted_from,
            nodes_to_remove,
        }
    }

    pub(crate) fn unregister_card(&mut self, card: N) -> Option<N> {
        self.card_hardware_render.remove(&card);
        let rn = self.card_render_target.remove(&card)?;
        let mut removed = Vec::new();
        Self::dec_refcount(&mut self.render_refcounts, rn, &mut removed);
        if removed.contains(&rn) {
            if self.primary_render_node == rn
                && self.explicit_render_override.is_none()
                && let Some(replacement) =
                    self.card_hardware_render.values().flatten().copied().next()
            {
                self.primary_render_node = replacement;
            }
            Some(rn)
        } else {
            None
        }
    }

    fn dec_refcount(refcounts: &mut HashMap<N, usize>, rn: N, zeroed: &mut Vec<N>) {
        if let Some(count) = refcounts.get_mut(&rn) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                refcounts.remove(&rn);
                zeroed.push(rn);
            }
        }
    }
}

/// Bare-metal TTY display & input backend state owned by `KomaState`.
pub struct TtyBackendState {
    pub session: LibSeatSession,
    pub seat_name: String,
    udev_dispatcher: Dispatcher<'static, UdevBackend, KomaState>,
    libinput: Libinput,
    pub libinput_devices: Vec<smithay::reexports::input::Device>,
    pub gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DeviceFd>>,
    pub primary_node: DrmNode,
    pub primary_render_node: DrmNode,
    pub(crate) render_registry: RenderNodeRegistry,
    explicit_render_gbm: Option<GbmDevice<DeviceFd>>,
    pub devices: HashMap<DrmNode, DrmOutputDevice>,
    pub dmabuf_global: Option<DmabufGlobal>,
    /// Connector configuration failures since the last enumeration, reported if no output works.
    connector_errors: Vec<String>,
}

impl TtyBackendState {
    /// Initializes `LibSeatSession`, `UdevBackend`, `LibinputInputBackend`, and `GpuManager`,
    /// and selects the primary render GPU (`niri/src/backend/tty.rs` `Tty::new`).
    pub fn new(event_loop: &LoopHandle<'static, KomaState>) -> anyhow::Result<Self> {
        let (session, notifier) = LibSeatSession::new().context(
            "failed creating LibSeatSession (ensure seatd or systemd-logind is running and              Koma is launched from an unoccupied Linux TTY)",
        )?;
        let seat_name = session.seat();

        // 1. Register `UdevBackend` hotplug dispatcher onto `calloop`
        let udev_backend =
            UdevBackend::new(&seat_name).context("failed creating udev DRM monitor")?;
        let udev_dispatcher =
            Dispatcher::new(udev_backend, move |event, _, state: &mut KomaState| {
                on_udev_event_callback(state, event);
            });
        event_loop
            .register_dispatcher(udev_dispatcher.clone())
            .map_err(|e| anyhow!("failed registering udev dispatcher on calloop: {e}"))?;

        // 2. Initialize `libinput` bound to `LibSeatSession`
        let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        libinput
            .udev_assign_seat(&seat_name)
            .map_err(|()| anyhow!("failed assigning seat `{seat_name}` to libinput"))?;

        // If started while another VT is active, suspend `libinput` immediately so `ActivateSession`
        // triggers a clean full device enumeration (`niri/src/backend/tty.rs` lines 469-472).
        if !session.is_active() {
            debug!("LibSeatSession initially inactive; starting libinput suspended");
            libinput.suspend();
        }

        let input_backend = LibinputInputBackend::new(libinput.clone());
        event_loop
            .insert_source(input_backend, |event, _, state| {
                handle_libinput_device_event(state, &event);
                process_tty_input_event(state, event);
            })
            .map_err(|e| anyhow!("failed inserting LibinputInputBackend into calloop: {e}"))?;

        // 3. Register VT pause/resume `LibSeatSession` notifier onto `calloop`
        event_loop
            .insert_source(notifier, move |event, _, state| {
                on_session_event_callback(state, event);
            })
            .map_err(|e| anyhow!("failed inserting LibSeatSession notifier into calloop: {e}"))?;

        // 4. Initialize high-priority EGL context & multi-GPU manager
        let gbm_api = GbmGlesBackend::with_context_priority(ContextPriority::High);
        let gpu_manager =
            GpuManager::new(gbm_api).context("failed initializing Smithay GpuManager")?;

        // 5. Discover primary GPU (`KOMA_DRM_DEVICE` override -> `udev::primary_gpu(&seat_name)`)
        let PrimaryGpuDiscovery {
            primary_node,
            primary_render_node,
            explicit_render_override,
        } = discover_primary_gpu(&seat_name)?;
        let render_registry = RenderNodeRegistry::new(
            primary_render_node,
            explicit_render_override.as_ref().map(|(_, node)| *node),
        );
        info!(
            seat = %seat_name,
            primary_node = ?primary_node,
            primary_render_node = ?render_registry.primary_render_node,
            "initialized bare-metal TTY session & primary render GPU"
        );

        Ok(Self {
            session,
            seat_name,
            udev_dispatcher,
            libinput,
            libinput_devices: Vec::new(),
            gpu_manager,
            primary_node,
            primary_render_node: render_registry.primary_render_node,
            render_registry,
            explicit_render_gbm: None,
            devices: HashMap::new(),
            dmabuf_global: None,
            connector_errors: Vec::new(),
        })
    }

    /// Re-applies declarative `koma.input` and `koma.device` rules to every connected `libinput` device.
    pub fn apply_libinput_config_to_all(
        &mut self,
        input: &koma_lua::schema::InputConfigSpec,
        devices: &[koma_lua::schema::DeviceOverrideSpec],
    ) {
        for dev in &mut self.libinput_devices {
            configure_libinput_device(dev, input, devices);
        }
    }

    /// Synchronizes keyboard NumLock/CapsLock/ScrollLock hardware LEDs with Smithay's `LedState`.
    pub fn sync_keyboard_leds(&mut self, led_state: smithay::input::keyboard::LedState) {
        use smithay::reexports::input::DeviceCapability;

        let leds = led_state.into();
        for dev in &mut self.libinput_devices {
            if dev.has_capability(DeviceCapability::Keyboard) {
                dev.led_update(leds);
            }
        }
    }

    /// Enumerates DRM nodes when the VT session is active, always adding `primary_node` first so
    /// secondary/dGPU outputs can share the primary `DmabufGlobal` and `GbmAllocator`.
    ///
    /// # Errors
    ///
    /// Fails when the session is active but no KMS output could be driven, carrying every
    /// per-device failure so the cause is visible without debug logging (komawm/koma#2).
    pub fn enumerate_initial_devices(&mut self, state: &mut KomaState) -> anyhow::Result<()> {
        if !self.session.is_active() {
            info!("TTY session not yet active; deferring DRM enumeration to ActivateSession");
            return Ok(());
        }

        if let Some((render_path, explicit_rn)) = env::var_os("KOMA_RENDER_DRM_DEVICE")
            .map(PathBuf::from)
            .zip(self.render_registry.explicit_render_override)
            && self.explicit_render_gbm.is_none()
        {
            self.init_explicit_render_device(&render_path, explicit_rn)?;
        }

        let snapshot: Vec<(dev_t, PathBuf)> = self
            .udev_dispatcher
            .as_source_ref()
            .device_list()
            .map(|(id, p)| (id, p.to_owned()))
            .collect();

        self.connector_errors.clear();
        let mut failures: Vec<String> = Vec::new();
        if let Some((prim_id, prim_path)) = snapshot
            .iter()
            .find(|(id, _)| *id == self.primary_node.dev_id())
            && let Err(err) = self.add_device_internal(*prim_id, prim_path, state, false)
        {
            warn!("failed initializing primary DRM node {prim_path:?}: {err:#}");
            failures.push(format!("{}: {err:#}", prim_path.display()));
        }

        for (dev_id, path) in &snapshot {
            if *dev_id == self.primary_node.dev_id() {
                continue;
            }
            if let Err(err) = self.add_device_internal(*dev_id, path, state, false) {
                warn!("failed initializing secondary DRM node {path:?}: {err:#}");
                failures.push(format!("{}: {err:#}", path.display()));
            }
        }

        if !self.render_registry.has_hardware_primary
            && !self.render_registry.primary_uses_software
            && self.devices.contains_key(&self.primary_node)
        {
            self.enable_software_fallback_on_primary()?;
        }

        self.ensure_hardware_dmabuf_global(state)?;

        if let Some((prim_id, _)) = snapshot
            .iter()
            .find(|(id, _)| *id == self.primary_node.dev_id())
        {
            self.device_changed(*prim_id, state);
        }
        for (dev_id, _) in &snapshot {
            if *dev_id == self.primary_node.dev_id() {
                continue;
            }
            self.device_changed(*dev_id, state);
        }

        failures.append(&mut self.connector_errors);
        let output_count = self.output_count();
        if output_count == 0 {
            let detail = if failures.is_empty() {
                "no connected connector with a usable mode was found".to_owned()
            } else {
                failures.join("; ")
            };
            bail!(
                "no KMS output could be initialized on seat `{}` ({} DRM device(s) seen): {detail}",
                self.seat_name,
                snapshot.len()
            );
        }
        info!(outputs = output_count, "initialized KMS outputs");
        Ok(())
    }

    /// Keeps the most recent connector failures for the fatal no-output report (bounded).
    fn record_connector_error(&mut self, message: String) {
        const MAX_CONNECTOR_ERRORS: usize = 16;
        if self.connector_errors.len() >= MAX_CONNECTOR_ERRORS {
            self.connector_errors.remove(0);
        }
        self.connector_errors.push(message);
    }

    /// Number of active KMS scanout surfaces across all devices.
    #[must_use]
    pub fn output_count(&self) -> usize {
        self.devices.values().map(|d| d.surfaces.len()).sum()
    }

    /// Invokes `f` with the primary GPU's `GlesRenderer` (used for `Headless` `LazyOffscreenSurface`
    /// `SCM_RIGHTS` zero-copy agent screenshot capture even on bare-metal KMS!).
    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer) -> T,
    ) -> Option<T> {
        let mut renderer = self
            .gpu_manager
            .single_renderer(&self.primary_render_node)
            .ok()?;
        Some(f(renderer.as_mut()))
    }

    /// Imports a client `zwp_linux_dmabuf_v1` buffer on the primary render GPU.
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        let Ok(mut renderer) = self.gpu_manager.single_renderer(&self.primary_render_node) else {
            return false;
        };
        match renderer.import_dmabuf(dmabuf, None) {
            Ok(_tex) => {
                if dmabuf.node().is_none() {
                    dmabuf.set_node(Some(self.primary_render_node));
                }
                true
            }
            Err(err) => {
                debug!("TTY primary GPU dmabuf import failed: {err:?}");
                false
            }
        }
    }

    /// Returns the primary connected `Output` (preferring `primary_node` connectors such as `eDP-1`).
    #[must_use]
    pub fn primary_output(&self) -> Option<Output> {
        if let Some(prim_dev) = self.devices.get(&self.primary_node)
            && let Some(surf) = prim_dev.surfaces.values().next()
        {
            return Some(surf.output.clone());
        }
        self.devices
            .values()
            .flat_map(|d| d.surfaces.values())
            .next()
            .map(|s| s.output.clone())
    }

    /// Returns all active KMS `Output`s across all DRM devices.
    #[must_use]
    pub fn all_outputs(&self) -> Vec<Output> {
        self.devices
            .values()
            .flat_map(|d| d.surfaces.values())
            .map(|s| s.output.clone())
            .collect()
    }

    /// Marks all active KMS surfaces dirty so the next `calloop` iteration renders or schedules after VBlank.
    pub fn queue_redraw_all(&mut self) {
        for device in self.devices.values_mut() {
            for surface in device.surfaces.values_mut() {
                surface.redraw_state = match surface.redraw_state {
                    SurfaceRedrawState::Idle | SurfaceRedrawState::Queued => {
                        SurfaceRedrawState::Queued
                    }
                    SurfaceRedrawState::WaitingForVBlank { .. } => {
                        SurfaceRedrawState::WaitingForVBlank {
                            redraw_needed: true,
                        }
                    }
                };
            }
        }
    }

    /// Live-updates scale, transform, 2D position, and `safe_area` on connected KMS outputs during
    /// `init.lua` hot-reload without dropping client surfaces or resetting DRM devices.
    pub fn apply_hot_reloaded_monitors(
        &mut self,
        registry: &koma_lua::KomaConfigRegistry,
    ) -> Vec<HotReloadedMonitorUpdate> {
        let mut updated = Vec::new();
        for device in self.devices.values_mut() {
            for surface in device.surfaces.values_mut() {
                let spec = registry.find_monitor_for_output(
                    &surface.connector_name,
                    Some(&surface.make),
                    Some(&surface.model),
                    Some(&surface.serial_number),
                );
                if spec.is_some_and(|s| s.off) {
                    continue;
                }
                let wl_mode = surface.output.current_mode().unwrap_or(Mode {
                    size: (1920, 1080).into(),
                    refresh: 60_000,
                });
                let new_scale = resolve_connector_scale(
                    spec,
                    wl_mode.size.w.max(1) as u32,
                    wl_mode.size.h.max(1) as u32,
                    surface.phys_mm.0,
                    surface.phys_mm.1,
                );
                let new_transform = koma_transform_to_smithay(resolve_connector_transform(
                    spec,
                    surface.panel_orientation,
                ));
                let explicit_pos = spec.and_then(MonitorSpec::resolved_position);
                let safe_area = spec.map(|s| s.safe_area).unwrap_or_default();

                let cur_scale = surface.output.current_scale().fractional_scale();
                let cur_transform = surface.output.current_transform();
                if (cur_scale - new_scale).abs() > 1e-4 || cur_transform != new_transform {
                    surface.output.change_current_state(
                        None,
                        Some(new_transform),
                        Some(Scale::Fractional(new_scale)),
                        None,
                    );
                    surface.redraw_state = SurfaceRedrawState::Queued;
                }
                let phys_transformed = new_transform.transform_size(wl_mode.size);
                let lw = (f64::from(phys_transformed.w) / new_scale)
                    .round()
                    .max(64.0) as u32;
                let lh = (f64::from(phys_transformed.h) / new_scale)
                    .round()
                    .max(64.0) as u32;
                updated.push(HotReloadedMonitorUpdate {
                    output: surface.output.clone(),
                    logical_w: lw,
                    logical_h: lh,
                    scale: new_scale,
                    explicit_pos,
                    safe_area,
                });
            }
        }
        updated
    }

    /// Returns the DRM connector modes discovered for `output`.
    #[must_use]
    pub fn available_modes_for_output(&self, output: &Output) -> Vec<Mode> {
        let name = output.name();
        for device in self.devices.values() {
            for surf in device.surfaces.values() {
                if surf.output == *output || surf.output.name() == name {
                    return surf.available_modes.clone();
                }
            }
        }
        Vec::new()
    }

    /// Requests a live KMS modeset on `output` for the given Wayland `Mode` (`zwlr_output_manager_v1`).
    pub fn apply_output_mode(&mut self, output: &Output, mode: Mode) -> Result<(), String> {
        let name = output.name();
        for device in self.devices.values_mut() {
            if let Some(surface) = device
                .surfaces
                .values_mut()
                .find(|s| s.output == *output || s.output.name() == name)
            {
                let target_w =
                    u16::try_from(mode.size.w).map_err(|_| "mode width out of range".to_owned())?;
                let target_h = u16::try_from(mode.size.h)
                    .map_err(|_| "mode height out of range".to_owned())?;
                let drm_mode = surface
                    .drm_modes
                    .iter()
                    .copied()
                    .find(|m| Mode::from(*m) == mode)
                    .or_else(|| {
                        surface
                            .drm_modes
                            .iter()
                            .copied()
                            .filter(|m| m.size() == (target_w, target_h))
                            .filter(|m| {
                                mode.refresh <= 0
                                    || (Mode::from(*m).refresh - mode.refresh).abs() <= 500
                            })
                            .max_by_key(|m| m.vrefresh())
                    })
                    .ok_or_else(|| {
                        format!(
                            "mode {}x{}@{}mHz is not supported by DRM connector {}",
                            mode.size.w, mode.size.h, mode.refresh, surface.connector_name
                        )
                    })?;

                surface.compositor.use_mode(drm_mode).map_err(|err| {
                    format!("DRM modeset failed on {}: {err:?}", surface.connector_name)
                })?;
                surface.refresh_mhz = Mode::from(drm_mode).refresh;
                surface.redraw_state = SurfaceRedrawState::Queued;
                return Ok(());
            }
        }
        Err(format!("unknown TTY output `{name}`"))
    }

    /// Switches Linux virtual terminal (`Ctrl + Alt + F1..F12` -> `VT 1..12`).
    pub fn change_vt(&mut self, vt: i32) {
        info!(vt, "switching Linux virtual terminal via libseat");
        if let Err(err) = self.session.change_vt(vt) {
            match vt_switch_failure_hint(&err) {
                Some(hint) => warn!(vt, "VT switch failed: {err}. {hint}"),
                None => warn!(vt, "LibSeatSession::change_vt failed: {err}"),
            }
        }
    }

    fn init_explicit_render_device(
        &mut self,
        render_path: &Path,
        explicit_rn: DrmNode,
    ) -> anyhow::Result<()> {
        let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let fd = self
            .session
            .open(render_path, open_flags)
            .with_context(|| format!("LibSeatSession::open({render_path:?}) failed"))?;
        let raw_device_fd = DeviceFd::from(fd);
        let gbm = GbmDevice::new(raw_device_fd).context("GbmDevice::new(render) failed")?;
        let rn = try_register_egl_render_node(&mut self.gpu_manager, &gbm, explicit_rn, false)
            .with_context(|| {
                format!("failed initializing KOMA_RENDER_DRM_DEVICE EGL on {render_path:?}")
            })?;
        self.render_registry.explicit_render_override = Some(rn);
        self.render_registry.primary_render_node = rn;
        self.render_registry.has_hardware_primary = true;
        self.render_registry.primary_uses_software = false;
        self.render_registry.render_refcounts.insert(rn, 1);
        self.primary_render_node = rn;
        self.explicit_render_gbm = Some(gbm);
        info!(?rn, path = ?render_path, "initialized KOMA_RENDER_DRM_DEVICE hardware renderer");
        Ok(())
    }

    fn enable_software_fallback_on_primary(&mut self) -> anyhow::Result<()> {
        if !software_rendering_allowed() {
            bail!(
                "primary DRM device {:?} has no hardware renderer and software rendering is disabled",
                self.primary_node
            );
        }
        let primary_gbm = self
            .devices
            .get(&self.primary_node)
            .map(|d| d.gbm.clone())
            .ok_or_else(|| anyhow!("missing primary DRM device {:?}", self.primary_node))?;
        warn!(
            node = ?self.primary_node,
            "no hardware EGL renderer found across DRM devices; falling back to software rendering (llvmpipe). Expect high CPU use; set KOMA_DISABLE_SOFTWARE_RENDERING=1 to fail instead"
        );
        let rn = try_register_egl_render_node(
            &mut self.gpu_manager,
            &primary_gbm,
            self.primary_node,
            true,
        )
        .context("software (llvmpipe) EGL fallback on display-only primary DRM device failed")?;
        let outcome = self
            .render_registry
            .register_card(self.primary_node, None, Some(rn));
        self.primary_render_node = self.render_registry.primary_render_node;
        for dead in outcome.nodes_to_remove {
            self.gpu_manager.as_mut().remove_node(&dead);
        }
        if let Some(dev) = self.devices.get_mut(&self.primary_node) {
            dev.render_node = Some(rn);
            dev.software_rendering = true;
        }
        Ok(())
    }

    fn ensure_hardware_dmabuf_global(&mut self, state: &mut KomaState) -> anyhow::Result<()> {
        if self.render_registry.primary_uses_software
            || !self.render_registry.has_hardware_primary
            || self.dmabuf_global.is_some()
        {
            return Ok(());
        }
        let renderer = self
            .gpu_manager
            .single_renderer(&self.primary_render_node)
            .context("failed obtaining primary GlesRenderer")?;
        let primary_formats = renderer.dmabuf_formats();
        let default_feedback =
            DmabufFeedbackBuilder::new(self.primary_render_node.dev_id(), primary_formats)
                .build()
                .context("failed building default DmabufFeedback")?;
        let global = state
            .dmabuf_state
            .create_global_with_default_feedback::<KomaState>(
                &state.display_handle,
                &default_feedback,
            );
        self.dmabuf_global = Some(global);
        Ok(())
    }

    fn device_added(
        &mut self,
        device_id: dev_t,
        path: &Path,
        state: &mut KomaState,
    ) -> anyhow::Result<()> {
        self.add_device_internal(device_id, path, state, true)
    }

    fn add_device_internal(
        &mut self,
        device_id: dev_t,
        path: &Path,
        state: &mut KomaState,
        scan_connectors: bool,
    ) -> anyhow::Result<()> {
        let node = DrmNode::from_dev_id(device_id)?;
        if node.ty() != NodeType::Primary {
            trace!(?path, "skipping non-primary DRM node on udev event");
            return Ok(());
        }
        if self.devices.contains_key(&node) {
            return Ok(());
        }

        let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let fd = self
            .session
            .open(path, open_flags)
            .with_context(|| format!("LibSeatSession::open({path:?}) failed"))?;
        let raw_device_fd = DeviceFd::from(fd);
        log_drm_master_status(path, &raw_device_fd, self.session.is_active());
        let device_fd = DrmDeviceFd::new(raw_device_fd);

        let (drm, drm_notifier) =
            DrmDevice::new(device_fd.clone(), false).context("DrmDevice::new failed")?;
        let gbm = GbmDevice::new(device_fd.device_fd()).context("GbmDevice::new failed")?;

        // Attempt hardware EGL first. During initial boot enumeration (`!scan_connectors`) or when
        // split display+render cards exist (`apple-drm` + `asahi` AGX, `i915` + `amdgpu`), a
        // display-only primary card defers software fallback until any companion render node is
        // checked and will promote cleanly when the render card is registered.
        let is_primary = node == self.primary_node;
        let (hw_render_node, sw_render_node, render_node, software_rendering) =
            match try_register_egl_render_node(&mut self.gpu_manager, &gbm, node, false) {
                Ok(rn) => {
                    debug!(?node, ?rn, "registered hardware EGL render node");
                    (Some(rn), None, Some(rn), false)
                }
                Err(err)
                    if scan_connectors
                        && is_primary
                        && !self.render_registry.has_hardware_primary
                        && software_rendering_allowed() =>
                {
                    warn!(
                        ?node,
                        "primary DRM device has no hardware renderer ({err:#}); falling back to software rendering (llvmpipe). Expect high CPU use; set KOMA_DISABLE_SOFTWARE_RENDERING=1 to fail instead"
                    );
                    let rn =
                        try_register_egl_render_node(&mut self.gpu_manager, &gbm, node, true)
                            .context(
                                "software (llvmpipe) EGL fallback on display-only primary DRM device failed",
                            )?;
                    (None, Some(rn), Some(rn), true)
                }
                Err(err) => {
                    if scan_connectors && is_primary && !self.render_registry.has_hardware_primary {
                        bail!(
                            "primary DRM device {node:?} has no hardware renderer and software rendering is disabled: {err:#}"
                        );
                    }
                    debug!(
                        ?node,
                        "display-only DRM device; borrowing primary render node: {err:#}"
                    );
                    (None, None, None, false)
                }
            };

        let outcome = self
            .render_registry
            .register_card(node, hw_render_node, sw_render_node);
        self.primary_render_node = self.render_registry.primary_render_node;
        if let Some(old_rn) = outcome.promoted_from {
            info!(
                old = ?old_rn,
                new = ?self.primary_render_node,
                "promoted primary render node to newly discovered hardware GPU"
            );
            for dev in self.devices.values_mut() {
                if dev.render_node == Some(old_rn) && dev.software_rendering {
                    dev.render_node = None;
                    dev.software_rendering = false;
                }
            }
        }
        for dead in outcome.nodes_to_remove {
            self.gpu_manager.as_mut().remove_node(&dead);
        }

        if scan_connectors && let Err(err) = self.ensure_hardware_dmabuf_global(state) {
            warn!("failed initializing dmabuf global after render node promotion: {err:#}");
        }

        // A display-only primary must allocate scanout buffers from its own GBM device or an
        // explicit render override (`komawm/koma#2`, Apple Silicon `apple-drm` + `asahi`).
        let allocator_gbm = if render_node.is_some() || is_primary {
            gbm.clone()
        } else if let Some(explicit_gbm) = &self.explicit_render_gbm {
            explicit_gbm.clone()
        } else if let Some(primary_dev) = self.devices.get(&self.primary_node) {
            primary_dev.gbm.clone()
        } else {
            gbm.clone()
        };
        let allocator = GbmAllocator::new(
            allocator_gbm,
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );

        let loop_handle = state
            .loop_handle
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("missing calloop LoopHandle on KomaState"))?;

        let token = loop_handle
            .insert_source(drm_notifier, move |event, meta, state| match event {
                DrmEvent::VBlank(crtc) => {
                    on_drm_vblank_callback(state, node, crtc, meta);
                }
                DrmEvent::Error(err) => {
                    warn!(?node, "DRM kernel event error: {err}");
                }
            })
            .map_err(|e| anyhow!("failed registering DrmDevice notifier on calloop: {e}"))?;

        self.devices.insert(
            node,
            DrmOutputDevice {
                token,
                render_node,
                software_rendering,
                drm_scanner: DrmScanner::new(),
                surfaces: HashMap::new(),
                drm,
                gbm,
                allocator,
            },
        );

        if scan_connectors {
            self.device_changed(device_id, state);
        }
        Ok(())
    }

    fn device_changed(&mut self, device_id: dev_t, state: &mut KomaState) {
        let Ok(node) = DrmNode::from_dev_id(device_id) else {
            return;
        };
        if node.ty() != NodeType::Primary {
            return;
        }

        if !self.devices.contains_key(&node) {
            if let Some(path) = node.dev_path() {
                let _ = self.device_added(device_id, &path, state);
            }
            return;
        }

        let scan_events: Vec<DrmScanEvent> = {
            let Some(device) = self.devices.get_mut(&node) else {
                return;
            };
            match device.drm_scanner.scan_connectors(&device.drm) {
                Ok(res) => res.into_iter().collect(),
                Err(err) => {
                    warn!(?node, "DrmScanner::scan_connectors failed: {err:?}");
                    self.record_connector_error(format!(
                        "{node}: scanning connectors failed: {err:?}"
                    ));
                    return;
                }
            }
        };

        for ev in scan_events {
            match ev {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    if let Err(err) = self.connector_connected(node, connector, crtc, state) {
                        warn!(
                            ?node,
                            ?crtc,
                            "failed configuring connected KMS connector: {err:#}"
                        );
                        self.record_connector_error(format!("{node}: {err:#}"));
                    }
                }
                DrmScanEvent::Disconnected {
                    crtc: Some(crtc), ..
                } => {
                    self.connector_disconnected(node, crtc, state);
                }
                DrmScanEvent::Changed {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_disconnected(node, crtc, state);
                    if let Err(err) = self.connector_connected(node, connector, crtc, state) {
                        warn!(
                            ?node,
                            ?crtc,
                            "failed reconfiguring changed KMS connector: {err:#}"
                        );
                        self.record_connector_error(format!("{node}: {err:#}"));
                    }
                }
                _ => {}
            }
        }
    }

    fn device_removed(&mut self, device_id: dev_t, state: &mut KomaState) {
        let Ok(node) = DrmNode::from_dev_id(device_id) else {
            return;
        };
        let Some(mut device) = self.devices.remove(&node) else {
            return;
        };
        for (_, surf) in device.surfaces.drain() {
            let output_name = surf.output.name();
            state.output_power_state.notify_output_removed(&output_name);
            state
                .gamma_control_state
                .notify_output_removed(&output_name);
            state.unregister_output(&surf.output);
            if let Some(global) = surf.wl_global {
                state.display_handle.remove_global::<KomaState>(global);
            }
        }
        if let Some(unused_rn) = self.render_registry.unregister_card(node) {
            self.gpu_manager.as_mut().remove_node(&unused_rn);
        }
        self.primary_render_node = self.render_registry.primary_render_node;
        if let Some(lh) = &state.loop_handle {
            lh.remove(device.token);
        }
        info!(?node, "removed hot-unplugged DRM device");
    }

    fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
        state: &mut KomaState,
    ) -> anyhow::Result<()> {
        let connector_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        let fallback_serial = format!("{node:?}-{crtc:?}");
        let meta = self
            .devices
            .get(&node)
            .map(|dev| query_connector_metadata(&dev.drm, connector.handle()))
            .unwrap_or_default();

        let (make, model, serial_number, phys_mm) = if let Some(edid) = &meta.edid {
            let edid_mm = edid.physical_size_mm();
            let fallback_mm = connector
                .size()
                .map(|(w, h)| (w as i32, h as i32))
                .unwrap_or((0, 0));
            let mm = if edid_mm.0 > 0 && edid_mm.1 > 0 {
                edid_mm
            } else {
                fallback_mm
            };
            let sn = edid.display_serial();
            (
                edid.display_make().to_owned(),
                edid.display_model(&connector_name),
                if sn.is_empty() { fallback_serial } else { sn },
                mm,
            )
        } else {
            let fallback_mm = connector
                .size()
                .map(|(w, h)| (w as i32, h as i32))
                .unwrap_or((0, 0));
            (
                "DRM".to_owned(),
                connector_name.clone(),
                fallback_serial,
                fallback_mm,
            )
        };

        // Consult declarative `koma.monitor` rules matching connector name or EDID identity
        let lua_out_cfg = state
            .lua_sandbox
            .config_snapshot()
            .find_monitor_for_output(
                &connector_name,
                Some(&make),
                Some(&model),
                Some(&serial_number),
            )
            .cloned();
        if lua_out_cfg.as_ref().is_some_and(|c| c.off) {
            info!(connector = %connector_name, "connector disabled (`off = true`) by init.lua koma.monitor rule");
            return Ok(());
        }
        if connector_should_be_skipped(meta.non_desktop, lua_out_cfg.as_ref()) {
            info!(
                connector = %connector_name,
                make = %make,
                model = %model,
                "skipping DRM non-desktop connector (Touch Bar / HMD); set `non_desktop = true` in koma.monitor to opt in"
            );
            return Ok(());
        }

        let requested_mode = lua_out_cfg
            .as_ref()
            .map(|c| c.mode.as_str())
            .filter(|m| !m.is_empty() && *m != "preferred");
        let drm_mode = pick_connector_mode(&connector, requested_mode).ok_or_else(|| {
            anyhow!("connector `{connector_name}` advertised zero usable KMS modes")
        })?;
        let drm_modes = connector.modes().to_vec();
        let wl_mode = Mode::from(drm_mode);
        let available_modes: Vec<Mode> = drm_modes.iter().copied().map(Mode::from).collect();
        let scale_factor = resolve_connector_scale(
            lua_out_cfg.as_ref(),
            wl_mode.size.w.max(1) as u32,
            wl_mode.size.h.max(1) as u32,
            phys_mm.0,
            phys_mm.1,
        );
        let output_transform =
            resolve_connector_transform(lua_out_cfg.as_ref(), meta.panel_orientation);
        let smithay_transform = koma_transform_to_smithay(output_transform);
        let explicit_pos = lua_out_cfg
            .as_ref()
            .and_then(MonitorSpec::resolved_position);
        let safe_area = lua_out_cfg
            .as_ref()
            .map(|c| c.safe_area)
            .unwrap_or_default();

        let target_render_node = self
            .devices
            .get(&node)
            .and_then(|d| d.render_node)
            .unwrap_or(self.primary_render_node);

        let render_formats: FormatSet = {
            let renderer = self.gpu_manager.single_renderer(&target_render_node)?;
            let raw_formats = renderer.as_ref().egl_context().dmabuf_render_formats();
            let (is_display_only, is_software) =
                self.devices.get(&node).map_or((false, false), |d| {
                    (d.render_node.is_none(), d.software_rendering)
                });

            raw_formats
                .iter()
                .copied()
                .filter(|fmt| {
                    if is_display_only {
                        return fmt.modifier == Modifier::Linear;
                    }
                    if is_software {
                        // `kms_swrast` allocates linear dumb buffers, often without modifier support.
                        return matches!(fmt.modifier, Modifier::Linear | Modifier::Invalid);
                    }
                    // Exclude high-bandwidth Intel CCS modifiers (`niri/src/backend/tty.rs` lines 1440-1458)
                    !matches!(
                        fmt.modifier,
                        Modifier::I915_y_tiled_ccs
                            | Modifier::Unrecognized(0x100000000000005)
                            | Modifier::I915_y_tiled_gen12_rc_ccs
                            | Modifier::I915_y_tiled_gen12_mc_ccs
                            | Modifier::Unrecognized(0x100000000000008)
                            | Modifier::Unrecognized(0x10000000000000a)
                            | Modifier::Unrecognized(0x10000000000000b)
                            | Modifier::Unrecognized(0x10000000000000c)
                    )
                })
                .collect()
        };

        let output = Output::new(
            connector_name.clone(),
            PhysicalProperties {
                size: phys_mm.into(),
                subpixel: connector.subpixel().into(),
                make: make.clone(),
                model: model.clone(),
                serial_number: serial_number.clone(),
            },
        );
        output.change_current_state(
            Some(wl_mode),
            Some(smithay_transform),
            Some(Scale::Fractional(scale_factor)),
            Some((0, 0).into()),
        );
        output.set_preferred(wl_mode);
        for &m in &available_modes {
            if m != wl_mode {
                output.add_mode(m);
            }
        }
        output
            .user_data()
            .insert_if_missing(|| TtyOutputTag { node, crtc });

        let compositor = {
            let device = self
                .devices
                .get_mut(&node)
                .ok_or_else(|| anyhow!("missing DrmOutputDevice for {node:?}"))?;
            let drm_surface = device
                .drm
                .create_surface(crtc, drm_mode, &[connector.handle()])
                .context("DrmDevice::create_surface failed")?;

            // Honor `vrr` setting when supported, otherwise explicitly clear `VRR_ENABLED`
            let want_vrr = lua_out_cfg.as_ref().is_some_and(|c| {
                c.vrr.as_bool() == Some(true)
                    || matches!(c.vrr.as_str(), Some("on" | "always" | "on_demand"))
            });
            if let Ok(VrrSupport::Supported | VrrSupport::RequiresModeset) =
                drm_surface.vrr_supported(connector.handle())
            {
                let _ = drm_surface.use_vrr(want_vrr);
            } else {
                let _ = drm_surface.use_vrr(false);
            }

            let color_formats = SUPPORTED_COLOR_FORMATS_10BIT.iter().copied();
            let exporter =
                GbmFramebufferExporter::new(device.gbm.clone(), device.render_node.into());
            match DrmCompositor::new(
                OutputModeSource::Auto(output.downgrade()),
                drm_surface,
                None,
                device.allocator.clone(),
                exporter,
                color_formats,
                render_formats.clone(),
                device.drm.cursor_size(),
                Some(device.gbm.clone()),
            ) {
                Ok(comp) => comp,
                Err(err) => {
                    warn!(
                        connector = %connector_name,
                        "DrmCompositor::new failed with explicit modifiers ({err:?}); retrying with Modifier::Invalid"
                    );
                    let invalid_mod_formats: FormatSet = render_formats
                        .iter()
                        .copied()
                        .filter(|f| f.modifier == Modifier::Invalid)
                        .collect();
                    let retry_surface =
                        device
                            .drm
                            .create_surface(crtc, drm_mode, &[connector.handle()])?;
                    DrmCompositor::new(
                        OutputModeSource::Auto(output.downgrade()),
                        retry_surface,
                        None,
                        device.allocator.clone(),
                        GbmFramebufferExporter::new(device.gbm.clone(), device.render_node.into()),
                        SUPPORTED_COLOR_FORMATS_8BIT.iter().copied(),
                        invalid_mod_formats,
                        device.drm.cursor_size(),
                        Some(device.gbm.clone()),
                    )
                    .context("DrmCompositor::new fallback failed")?
                }
            }
        };

        let dmabuf_feedback = self
            .gpu_manager
            .single_renderer(&self.primary_render_node)
            .ok()
            .and_then(|r| {
                DmabufFeedbackBuilder::new(self.primary_render_node.dev_id(), r.dmabuf_formats())
                    .build()
                    .ok()
            });

        let wl_global = Some(output.create_global::<KomaState>(&state.display_handle));

        info!(
            connector = %connector_name,
            make = %make,
            model = %model,
            mode = ?wl_mode.size,
            refresh_hz = (f64::from(wl_mode.refresh) / 1000.0),
            scale = scale_factor,
            transform = ?output_transform,
            "initialized atomic KMS DrmCompositor surface"
        );

        if let Some(device) = self.devices.get_mut(&node) {
            device.surfaces.insert(
                crtc,
                DrmOutputSurface {
                    connector: connector.handle(),
                    connector_name,
                    make,
                    model,
                    serial_number,
                    phys_mm,
                    panel_orientation: meta.panel_orientation,
                    available_modes,
                    drm_modes,
                    output: output.clone(),
                    wl_global,
                    compositor,
                    dmabuf_feedback,
                    redraw_state: SurfaceRedrawState::Queued,
                    refresh_mhz: wl_mode.refresh,
                },
            );
        }

        state.register_or_update_output(&output, explicit_pos, safe_area, true);
        Ok(())
    }

    fn connector_disconnected(&mut self, node: DrmNode, crtc: crtc::Handle, state: &mut KomaState) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };
        if let Some(removed) = device.surfaces.remove(&crtc) {
            let output_name = removed.output.name();
            state.output_power_state.notify_output_removed(&output_name);
            state
                .gamma_control_state
                .notify_output_removed(&output_name);
            state.unregister_output(&removed.output);
            if let Some(global) = removed.wl_global {
                state.display_handle.remove_global::<KomaState>(global);
            }
            info!(
                connector = %removed.connector_name,
                "disconnected KMS connector & removed wl_output global"
            );
        }
    }

    pub fn set_output_dpms(
        &mut self,
        output: &Output,
        mode: crate::output_power::OutputPowerMode,
    ) -> Result<(), String> {
        let mut target = None;
        for (node, dev) in &self.devices {
            for (crtc, surf) in &dev.surfaces {
                if surf.output == *output {
                    target = Some((*node, *crtc, surf.connector));
                    break;
                }
            }
            if target.is_some() {
                break;
            }
        }
        let Some((node, _crtc, connector)) = target else {
            return Err(format!(
                "output {} is not bound to any active DRM connector",
                output.name()
            ));
        };
        let device = self
            .devices
            .get(&node)
            .ok_or_else(|| format!("missing DRM device for {}", output.name()))?;
        let props = device
            .drm
            .get_properties(connector)
            .map_err(|err| format!("failed to query connector properties: {err}"))?;
        for &prop_handle in props.as_props_and_values().0 {
            let Ok(info) = device.drm.get_property(prop_handle) else {
                continue;
            };
            if info.name().to_bytes() != b"DPMS" {
                continue;
            }
            let target_val = match info.value_type() {
                control::property::ValueType::Enum(enum_vals) => {
                    let wanted: &[u8] = match mode {
                        crate::output_power::OutputPowerMode::On => b"On",
                        crate::output_power::OutputPowerMode::Off => b"Off",
                    };
                    enum_vals
                        .values()
                        .1
                        .iter()
                        .find(|ev| ev.name().to_bytes().eq_ignore_ascii_case(wanted))
                        .map(|ev| ev.value())
                        .unwrap_or(match mode {
                            crate::output_power::OutputPowerMode::On => 0,
                            crate::output_power::OutputPowerMode::Off => 3,
                        })
                }
                _ => match mode {
                    crate::output_power::OutputPowerMode::On => 0,
                    crate::output_power::OutputPowerMode::Off => 3,
                },
            };
            device
                .drm
                .set_property(connector, prop_handle, target_val)
                .map_err(|err| format!("failed setting DRM DPMS property: {err}"))?;
            return Ok(());
        }
        Err(format!(
            "connector {} does not expose a DRM DPMS property",
            output.name()
        ))
    }

    #[must_use]
    pub fn query_output_gamma_info(
        &self,
        output: &Output,
    ) -> Option<(u32, Option<crate::gamma_control::GammaLut>)> {
        for dev in self.devices.values() {
            for (crtc, surf) in &dev.surfaces {
                if surf.output == *output {
                    let crtc_info = dev.drm.get_crtc(*crtc).ok()?;
                    let gamma_len = crtc_info.gamma_length();
                    if gamma_len == 0 {
                        return Some((0, None));
                    }
                    let len = gamma_len as usize;
                    let mut red = vec![0_u16; len];
                    let mut green = vec![0_u16; len];
                    let mut blue = vec![0_u16; len];
                    let original_lut = dev
                        .drm
                        .get_gamma(*crtc, &mut red, &mut green, &mut blue)
                        .ok()
                        .map(|()| crate::gamma_control::GammaLut { red, green, blue });
                    return Some((gamma_len, original_lut));
                }
            }
        }
        None
    }

    pub fn apply_output_gamma(
        &self,
        output: &Output,
        lut: &crate::gamma_control::GammaLut,
    ) -> Result<(), String> {
        for dev in self.devices.values() {
            for (crtc, surf) in &dev.surfaces {
                if surf.output == *output {
                    return dev
                        .drm
                        .set_gamma(*crtc, &lut.red, &lut.green, &lut.blue)
                        .map_err(|err| format!("DRM set_gamma failed: {err}"));
                }
            }
        }
        Err(format!(
            "output {} is not bound to an active DRM CRTC",
            output.name()
        ))
    }
}

/// Updated geometry, transform/scale, position, and notch safe-area snapshot produced when
/// hot-reloading `MonitorSpec` rules onto live KMS outputs.
pub struct HotReloadedMonitorUpdate {
    pub output: Output,
    pub logical_w: u32,
    pub logical_h: u32,
    pub scale: f64,
    pub explicit_pos: Option<(i32, i32)>,
    pub safe_area: koma_model::SafeAreaInsets,
}

struct PrimaryGpuDiscovery {
    primary_node: DrmNode,
    primary_render_node: DrmNode,
    explicit_render_override: Option<(PathBuf, DrmNode)>,
}

/// Discovers the primary DRM card + render node via `KOMA_DRM_DEVICE` /
/// `KOMA_RENDER_DRM_DEVICE` or `udev::primary_gpu`.
fn discover_primary_gpu(seat_name: &str) -> anyhow::Result<PrimaryGpuDiscovery> {
    let primary_path = match env::var_os("KOMA_DRM_DEVICE") {
        Some(override_path) => PathBuf::from(override_path),
        None => udev::primary_gpu(seat_name)
            .context("udev::primary_gpu query failed")?
            .context("no usable DRM/KMS GPU detected on seat")?,
    };

    let primary_node = DrmNode::from_path(&primary_path)
        .with_context(|| format!("failed opening primary DRM node {primary_path:?}"))?;

    let explicit_render_override = match env::var_os("KOMA_RENDER_DRM_DEVICE") {
        Some(override_path) => {
            let render_path = PathBuf::from(override_path);
            let explicit_node = DrmNode::from_path(&render_path).with_context(|| {
                format!("failed opening KOMA_RENDER_DRM_DEVICE {render_path:?}")
            })?;
            let render_node = explicit_node
                .node_with_type(NodeType::Render)
                .and_then(Result::ok)
                .unwrap_or(explicit_node);
            Some((render_path, render_node))
        }
        None => None,
    };

    let primary_render_node = explicit_render_override
        .as_ref()
        .map(|(_, rn)| *rn)
        .unwrap_or_else(|| {
            primary_node
                .node_with_type(NodeType::Render)
                .and_then(Result::ok)
                .unwrap_or_else(|| {
                    warn!(
                        ?primary_node,
                        "primary GPU has no dedicated renderD* node; reusing card node"
                    );
                    primary_node
                })
        });
    Ok(PrimaryGpuDiscovery {
        primary_node,
        primary_render_node,
        explicit_render_override,
    })
}

#[allow(unsafe_code)]
fn create_egl_display_from_gbm(gbm: &GbmDevice<DeviceFd>) -> anyhow::Result<EGLDisplay> {
    // SAFETY: the GBM device wraps a DRM fd that stays open for the device lifetime.
    Ok(unsafe { EGLDisplay::new(gbm.clone())? })
}

fn try_register_egl_render_node(
    gpu_manager: &mut GpuManager<GbmGlesBackend<GlesRenderer, DeviceFd>>,
    gbm: &GbmDevice<DeviceFd>,
    fallback_node: DrmNode,
    allow_software: bool,
) -> anyhow::Result<DrmNode> {
    let display = create_egl_display_from_gbm(gbm)?;
    let egl_device = EGLDevice::device_for_display(&display)?;
    let is_software = egl_device.is_software();
    ensure!(
        allow_software || !is_software,
        "rejecting software llvmpipe/softpipe EGL device"
    );
    let render_node = egl_device
        .try_get_render_node()
        .ok()
        .flatten()
        .unwrap_or(fallback_node);
    gpu_manager
        .as_mut()
        .add_node(render_node, gbm.clone())
        .context("GpuManager::add_node failed")?;
    if is_software {
        info!(
            ?render_node,
            "registered software EGL renderer (llvmpipe) on display-only device"
        );
    }
    Ok(render_node)
}

/// Software rendering on a display-only primary device is on unless
/// `KOMA_DISABLE_SOFTWARE_RENDERING=1` (or `true`) is set.
fn software_rendering_allowed() -> bool {
    !is_truthy_env_value(env::var("KOMA_DISABLE_SOFTWARE_RENDERING").ok().as_deref())
}

fn is_truthy_env_value(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// `DRM_IOCTL_AUTH_MAGIC` (`_IOW('d', 0x11, struct drm_auth)`).
///
/// Linux encodes `_IOC_WRITE` as `1 << 30` (`0x4000_0000`), whereas BSD `<sys/ioccom.h>` defines
/// `IOC_IN` as `0x8000_0000`.
#[cfg(target_os = "linux")]
const DRM_IOCTL_AUTH_MAGIC: libc::c_ulong = 0x4004_6411;
/// `DRM_IOCTL_AUTH_MAGIC` (`_IOW('d', 0x11, struct drm_auth)`) for BSD targets (`IOC_IN`).
#[cfg(any(
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
))]
const DRM_IOCTL_AUTH_MAGIC: libc::c_ulong = 0x8004_6411;

/// Mirrors libdrm's `drmIsMaster`: authenticating magic `0` fails with `EINVAL` for a master
/// (unknown token) and with `EACCES` for a non-master, without changing any state.
#[allow(unsafe_code)]
fn drm_fd_is_master(fd: BorrowedFd<'_>) -> std::io::Result<bool> {
    let mut magic: u32 = 0;
    // SAFETY: `fd` is a valid open DRM fd and `magic` matches `struct drm_auth` (one `u32`).
    let rc = unsafe {
        libc::ioctl(
            std::os::fd::AsRawFd::as_raw_fd(&fd),
            DRM_IOCTL_AUTH_MAGIC as _,
            &raw mut magic,
        )
    };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EACCES) => Ok(false),
        Some(libc::EINVAL) => Ok(true),
        _ => Err(err),
    }
}

/// Explains Smithay's generic "Unable to become drm master" warning (komawm/koma#2).
///
/// The kernel refuses `DRM_IOCTL_SET_MASTER` from an unprivileged process for an fd it did not
/// open itself, even when that fd already *is* master. Seat managers (seatd, systemd-logind)
/// open the device and grant master on our behalf, so the warning is harmless whenever the fd is
/// already master. This checks the real state like libdrm's `drmIsMaster` (only a master can
/// authenticate a magic token) and logs the actual cause.
fn log_drm_master_status(path: &Path, fd: &DeviceFd, session_active: bool) {
    match drm_fd_is_master(fd.as_fd()) {
        Ok(true) => info!(
            ?path,
            "seat manager granted DRM master; a following \"Unable to become drm master\" warning only means SET_MASTER is refused because the seat manager, not Koma, opened the fd, and is harmless"
        ),
        Ok(false) => warn!(
            ?path,
            session_active,
            "Koma does not hold DRM master on this device, so modesetting will fail. Likely causes: the session's VT is not in the foreground (start Koma from the VT it will own, not over SSH or from another VT), another compositor or display server holds master, or the seat manager (seatd / systemd-logind) did not grant the device to this session"
        ),
        Err(err) => debug!(?path, "could not query DRM master state: {err}"),
    }
}

/// Explains libseat VT-switch failures (komawm/koma#5).
fn vt_switch_failure_hint(err: &LibSeatError) -> Option<&'static str> {
    match err {
        LibSeatError::FailedToChangeVt(errno) => vt_switch_errno_hint(*errno),
        _ => None,
    }
}

fn vt_switch_errno_hint(errno: Errno) -> Option<&'static str> {
    if errno == Errno::ACCESS || errno == Errno::PERM {
        Some(
            "The seat manager denied the switch: systemd-logind only allows SwitchTo for an unprivileged session when polkit authorizes it (org.freedesktop.login1.chvt). Install and run polkit, or run Koma under seatd (`LIBSEAT_BACKEND=seatd`, user in the `seat` group), which permits VT switching for the active session",
        )
    } else if errno == Errno::NOSYS || errno == Errno::OPNOTSUPP {
        Some(
            "This seat has no virtual terminals (e.g. seat1 or a VT-less kernel); VT switching is unavailable",
        )
    } else {
        None
    }
}

/// Selects either the user-requested `"WIDTHxHEIGHT@HZ"` mode from `init.lua` or the connector's
/// highest-refresh non-interlaced `PREFERRED` mode (`niri/src/backend/tty.rs` `pick_mode`).
fn pick_connector_mode(
    connector: &connector::Info,
    requested_spec: Option<&str>,
) -> Option<control::Mode> {
    if let Some(spec) = requested_spec
        && let Some((target_w, target_h, target_mhz)) = parse_mode_spec(spec)
    {
        let mut best_match: Option<control::Mode> = None;
        for m in connector.modes() {
            if m.flags().contains(ModeFlags::INTERLACE) || m.size() != (target_w, target_h) {
                continue;
            }
            if let Some(mhz) = target_mhz {
                let wl_m = Mode::from(*m);
                if (wl_m.refresh - mhz).abs() <= 500 {
                    best_match = Some(*m);
                    break;
                }
            } else if best_match.is_none_or(|curr| curr.vrefresh() < m.vrefresh()) {
                best_match = Some(*m);
            }
        }
        if best_match.is_some() {
            return best_match;
        }
        warn!(
            requested = %spec,
            "configured output mode not advertised by EDID; falling back to preferred mode"
        );
    }

    // Prefer `ModeTypeFlags::PREFERRED` with the highest refresh rate (e.g. 2560x1600@240Hz)
    connector
        .modes()
        .iter()
        .copied()
        .filter(|m| !m.flags().contains(ModeFlags::INTERLACE))
        .max_by_key(|m| {
            let is_pref = m.mode_type().contains(ModeTypeFlags::PREFERRED);
            let (w, h) = m.size();
            (is_pref, u32::from(w) * u32::from(h), m.vrefresh())
        })
}

fn parse_mode_spec(spec: &str) -> Option<(u16, u16, Option<i32>)> {
    let (res_part, hz_part) = match spec.split_once('@') {
        Some((r, hz)) => (r, Some(hz)),
        None => (spec, None),
    };
    let (w_str, h_str) = res_part.split_once('x')?;
    let w: u16 = w_str.trim().parse().ok()?;
    let h: u16 = h_str.trim().parse().ok()?;
    let mhz = match hz_part {
        Some(s) => {
            let hz: f64 = s.trim().trim_end_matches("Hz").trim().parse().ok()?;
            Some((hz * 1000.0).round() as i32)
        }
        None => None,
    };
    Some((w, h, mhz))
}

#[derive(Debug, Default)]
struct ConnectorDrmMetadata {
    edid: Option<ParsedEdid>,
    panel_orientation: Option<OutputTransform>,
    non_desktop: bool,
}

fn panel_orientation_from_prop_name(name: &str) -> Option<OutputTransform> {
    match name.trim() {
        "Normal" => Some(OutputTransform::Normal),
        "Upside Down" => Some(OutputTransform::Rotate180),
        "Left Side Up" => Some(OutputTransform::Rotate90),
        "Right Side Up" => Some(OutputTransform::Rotate270),
        _ => None,
    }
}

fn query_connector_metadata(drm: &DrmDevice, conn: connector::Handle) -> ConnectorDrmMetadata {
    let mut meta = ConnectorDrmMetadata::default();
    let Ok(props) = drm.get_properties(conn) else {
        return meta;
    };
    let (handles, values) = props.as_props_and_values();
    for (&handle, &raw_val) in handles.iter().zip(values.iter()) {
        let Ok(info) = drm.get_property(handle) else {
            continue;
        };
        let prop_name = info.name().to_bytes();
        let value_type = info.value_type();
        let val = value_type.convert_value(raw_val);
        if prop_name.eq_ignore_ascii_case(b"EDID") {
            if let property::Value::Blob(blob_id) = val
                && blob_id != 0
                && let Ok(bytes) = drm.get_property_blob(blob_id)
            {
                meta.edid = ParsedEdid::parse(&bytes).ok();
            }
        } else if prop_name.eq_ignore_ascii_case(b"non-desktop") {
            meta.non_desktop = match val {
                property::Value::Boolean(b) => b,
                property::Value::UnsignedRange(v) => v != 0,
                property::Value::SignedRange(v) => v != 0,
                property::Value::Enum(Some(ev)) => ev.value() != 0,
                _ => false,
            };
        } else if prop_name.eq_ignore_ascii_case(b"panel orientation") {
            meta.panel_orientation = match val {
                property::Value::Enum(Some(ev)) => std::str::from_utf8(ev.name().to_bytes())
                    .ok()
                    .and_then(panel_orientation_from_prop_name)
                    .or_else(|| match ev.value() {
                        0 => Some(OutputTransform::Normal),
                        1 => Some(OutputTransform::Rotate180),
                        2 => Some(OutputTransform::Rotate90),
                        3 => Some(OutputTransform::Rotate270),
                        _ => None,
                    }),
                property::Value::UnsignedRange(v) => match v {
                    0 => Some(OutputTransform::Normal),
                    1 => Some(OutputTransform::Rotate180),
                    2 => Some(OutputTransform::Rotate90),
                    3 => Some(OutputTransform::Rotate270),
                    _ => None,
                },
                _ => None,
            };
        }
    }
    meta
}

#[must_use]
fn connector_should_be_skipped(non_desktop: bool, spec: Option<&MonitorSpec>) -> bool {
    non_desktop && !spec.is_some_and(|s| s.allow_non_desktop)
}

#[must_use]
fn resolve_connector_transform(
    spec: Option<&MonitorSpec>,
    panel_orientation: Option<OutputTransform>,
) -> OutputTransform {
    if let Some(spec) = spec
        && spec.has_explicit_transform
    {
        return spec.parsed_transform().unwrap_or_default();
    }
    panel_orientation.unwrap_or(OutputTransform::Normal)
}

#[must_use]
fn resolve_connector_scale(
    spec: Option<&MonitorSpec>,
    phys_w_px: u32,
    phys_h_px: u32,
    phys_w_mm: i32,
    phys_h_mm: i32,
) -> f64 {
    let resolved = match spec {
        Some(s) => s.scale.resolve(phys_w_px, phys_h_px, phys_w_mm, phys_h_mm),
        None => MonitorScaleSpec::Auto.resolve(phys_w_px, phys_h_px, phys_w_mm, phys_h_mm),
    };
    closest_representable_scale(resolved)
}

fn on_udev_event_callback(state: &mut KomaState, event: UdevEvent) {
    let Some(ActiveBackend::Tty(mut tty)) = state.backend.take() else {
        return;
    };
    if tty.session.is_active() {
        match event {
            UdevEvent::Added { device_id, path } => {
                if let Err(err) = tty.device_added(device_id, &path, state) {
                    warn!(?path, "udev hotplug device_added failed: {err:#}");
                }
            }
            UdevEvent::Changed { device_id } => {
                tty.device_changed(device_id, state);
            }
            UdevEvent::Removed { device_id } => {
                tty.device_removed(device_id, state);
            }
        }
    }
    state.backend = Some(ActiveBackend::Tty(tty));
    render_pending_tty_outputs(state);
}

fn on_session_event_callback(state: &mut KomaState, event: SessionEvent) {
    let Some(ActiveBackend::Tty(mut tty)) = state.backend.take() else {
        return;
    };

    match event {
        SessionEvent::PauseSession => {
            info!("pausing Koma TTY session (VT switch away)");
            // Release physical & virtual held keys immediately because release scancodes will land
            // on the destination VT (`niri/src/backend/tty.rs` lines 622-638).
            if let Some(keyboard) = state.seat.get_keyboard() {
                keyboard.release_source(state, KeyboardSource::MAIN);
            }
            crate::macro_runtime::release_all_vseat_held_inputs_everywhere(state);
            for vseat in state.vseats.values_mut() {
                let _ = vseat.flush_all_held();
            }

            let _ = crate::pointer_grabs::cancel_active_pointer_grab(
                state,
                SERIAL_COUNTER.next_serial(),
                smithay::backend::input::InputTime::now(),
            );
            state.pointer_animations.clear();
            let now_ms = state.elapsed_ms();
            for ac in state.agent_cursors.values_mut() {
                ac.visual.on_preempted(now_ms);
            }
            state.keybinds.reset_scroll_trackers();
            state.gestures.reset_active_sessions();
            tty.libinput.suspend();
            for device in tty.devices.values_mut() {
                device.drm.pause();
            }
        }
        SessionEvent::ActivateSession => {
            info!("activating Koma TTY session (VT switch return)");
            if tty.libinput.resume().is_err() {
                warn!("libinput::resume reported an error on VT activation");
            }
            let reg = state.lua_sandbox.config_snapshot();
            tty.apply_libinput_config_to_all(&reg.input, &reg.devices);
            if let Some(keyboard) = state.seat.get_keyboard() {
                tty.sync_keyboard_leds(keyboard.led_state());
            }

            let mut current_udev: HashMap<dev_t, PathBuf> = tty
                .udev_dispatcher
                .as_source_ref()
                .device_list()
                .map(|(id, p)| (id, p.to_owned()))
                .collect();

            let existing_nodes: Vec<DrmNode> = tty.devices.keys().copied().collect();
            for node in existing_nodes {
                if current_udev.remove(&node.dev_id()).is_some() {
                    if let Some(device) = tty.devices.get_mut(&node) {
                        if let Err(err) = device.drm.activate(false) {
                            warn!(?node, "DrmDevice::activate failed on resume: {err:?}");
                        }
                        for surf in device.surfaces.values_mut() {
                            surf.compositor.reset_buffers();
                            surf.redraw_state = SurfaceRedrawState::Queued;
                        }
                    }
                    tty.device_changed(node.dev_id(), state);
                } else {
                    tty.device_removed(node.dev_id(), state);
                }
            }

            let prim_id = tty.primary_node.dev_id();
            let prim_entry = current_udev.remove(&prim_id).map(|p| (prim_id, p));
            for (dev_id, path) in prim_entry.into_iter().chain(current_udev) {
                if let Err(err) = tty.device_added(dev_id, &path, state) {
                    warn!(
                        ?path,
                        "failed adding hotplugged DRM node on VT resume: {err:#}"
                    );
                }
            }
            tty.queue_redraw_all();
        }
    }

    state.backend = Some(ActiveBackend::Tty(tty));
    render_pending_tty_outputs(state);
}

fn on_drm_vblank_callback(
    state: &mut KomaState,
    node: DrmNode,
    crtc: crtc::Handle,
    _meta: &mut Option<DrmEventMetadata>,
) {
    let mut output_for_callbacks: Option<(Output, Duration)> = None;
    let mut needs_immediate_redraw = false;

    if let Some(ActiveBackend::Tty(tty)) = state.backend.as_mut()
        && let Some(device) = tty.devices.get_mut(&node)
        && let Some(surface) = device.surfaces.get_mut(&crtc)
    {
        if let Err(err) = surface.compositor.frame_submitted() {
            warn!(
                connector = %surface.connector_name,
                "DrmCompositor::frame_submitted error: {err}"
            );
        }

        let redraw_again = match mem::replace(&mut surface.redraw_state, SurfaceRedrawState::Idle) {
            SurfaceRedrawState::WaitingForVBlank { redraw_needed } => redraw_needed,
            SurfaceRedrawState::Queued => true,
            SurfaceRedrawState::Idle => false,
        };

        if redraw_again {
            surface.redraw_state = SurfaceRedrawState::Queued;
            needs_immediate_redraw = true;
        }

        let refresh_interval =
            Duration::from_secs_f64(1_000.0 / f64::from(surface.refresh_mhz.max(1_000)));
        output_for_callbacks = Some((surface.output.clone(), refresh_interval));
    }

    // Dispatch `wl_surface.frame` callbacks synced to hardware VBlank so 240 Hz clients pace cleanly
    if let Some((output_ref, refresh_interval)) = output_for_callbacks {
        dispatch_output_frame_callbacks(state, &output_ref, refresh_interval);
    }

    if needs_immediate_redraw {
        render_pending_tty_outputs(state);
    }
}

/// Renders every `SurfaceRedrawState::Queued` KMS surface that is not currently awaiting VBlank.
pub fn render_pending_tty_outputs(state: &mut KomaState) {
    state.redraw_queued = false;
    state.reconcile_session_lock();

    let Some(ActiveBackend::Tty(mut tty)) = state.backend.take() else {
        return;
    };
    if !tty.session.is_active() {
        state.backend = Some(ActiveBackend::Tty(tty));
        return;
    }

    let queued_targets: Vec<(DrmNode, crtc::Handle)> = tty
        .devices
        .iter()
        .filter(|(_, dev)| dev.drm.is_active())
        .flat_map(|(&node, dev)| {
            dev.surfaces
                .iter()
                .filter(|(_, s)| s.redraw_state == SurfaceRedrawState::Queued)
                .map(move |(&crtc, _)| (node, crtc))
        })
        .collect();

    let mut no_damage_outputs: Vec<(Output, Duration)> = Vec::new();
    for (node, crtc) in queued_targets {
        if let Some(cb) = render_single_drm_surface(&mut tty, state, node, crtc) {
            no_damage_outputs.push(cb);
        }
    }

    state.backend = Some(ActiveBackend::Tty(tty));

    for (output_ref, interval) in no_damage_outputs {
        dispatch_output_frame_callbacks(state, &output_ref, interval);
    }
}

fn render_single_drm_surface(
    tty: &mut TtyBackendState,
    state: &mut KomaState,
    node: DrmNode,
    crtc: crtc::Handle,
) -> Option<(Output, Duration)> {
    let device = tty.devices.get_mut(&node)?;
    if !device.drm.is_active() {
        return None;
    }
    let target_render_node = device.render_node.unwrap_or(tty.primary_render_node);
    let surface = device.surfaces.get_mut(&crtc)?;
    let out_name = surface.output.name();
    if state.output_power_state.is_output_off(&out_name) || !state.is_output_enabled(&out_name) {
        surface.redraw_state = SurfaceRedrawState::Idle;
        return None;
    }
    let out_ws_id = state.active_workspace_for_output(&out_name);
    let ws = state.workspaces.get(&out_ws_id)?;
    let viewport: LogicalViewport = state.viewport_for_output(&surface.output);
    let lease_state = ws.lease.state().clone();
    let output_scale = MathScale::from(surface.output.current_scale().fractional_scale());
    let output_origin = surface.output.current_location().to_f64();

    let mut renderer: TtyRenderer<'_> = match tty.gpu_manager.renderer(
        &tty.primary_render_node,
        &target_render_node,
        surface.compositor.format(),
    ) {
        Ok(r) => r,
        Err(err) => {
            warn!(
                connector = %surface.connector_name,
                "GpuManager::renderer failed: {err:?}"
            );
            surface.redraw_state = SurfaceRedrawState::Idle;
            return None;
        }
    };

    let locked = state.is_locked_or_abandoned();
    if !locked {
        state.tick_frame_animations_and_edge_scroll();
    }
    let pointer_loc = state.pointer_location - output_origin;
    let now_ms = state.elapsed_ms();
    let cursor_elems =
        state
            .cursor_manager
            .render_elements(&mut renderer, pointer_loc, output_scale, now_ms);
    if let Some(delay_ms) = state
        .cursor_manager
        .next_frame_delay_ms(output_scale.x, now_ms)
    {
        state.schedule_cursor_animation_redraw(delay_ms);
    }
    let agent_cursor_snap = (!locked)
        .then(|| state.step_agent_cursor_for_output(&out_name, &out_ws_id))
        .flatten();

    let (elements, clear_color): (Vec<KomaRenderElement<TtyRenderer<'_>>>, Color32F) = if locked {
        (
            assemble_locked_frame_elements(
                &mut renderer,
                state,
                &surface.output,
                output_scale,
                cursor_elems,
            ),
            Color32F::new(0.0, 0.0, 0.0, 1.0),
        )
    } else {
        let (base_scene_items, special_overlay) = state.collect_output_scene_items(&out_name);
        let layer_tiers = state.output_layer_tiers(&out_ws_id);
        let blur_cfg = state.lua_sandbox.config_snapshot().appearance.blur;
        let any_window_needs_blur = blur_cfg.enabled
            && base_scene_items
                .iter()
                .chain(special_overlay.iter().flat_map(|(_, v)| v.iter()))
                .any(|it| it.wants_blur && (blur_cfg.ignore_opacity || it.opacity < 0.999));

        let frame_blur_ctx = if any_window_needs_blur {
            let phys_sz = surface
                .output
                .current_mode()
                .map(|m| surface.output.current_transform().transform_size(m.size))
                .unwrap_or_else(|| (1920, 1080).into());
            let mut gles_bg_elems: Vec<KomaRenderElement<GlesRenderer>> = Vec::new();
            append_layer_tier_elements(
                renderer.as_mut(),
                &surface.output,
                output_scale,
                layer_tiers.below_toplevels,
                &mut gles_bg_elems,
            );
            state
                .backdrop_blur_cache
                .borrow_mut()
                .update_and_blur(
                    renderer.as_mut(),
                    phys_sz,
                    output_scale,
                    DEFAULT_BACKDROP_CLEAR,
                    &gles_bg_elems,
                    blur_cfg,
                )
                .map(|texture| FrameBlurContext {
                    texture,
                    config: blur_cfg,
                })
        } else {
            None
        };

        // Assemble front-to-back elements: IME Popups -> Overlay/Top layer-shell -> Scratchpad -> Base Space -> Bottom/Bg layers
        let mut elements: Vec<KomaRenderElement<TtyRenderer<'_>>> = Vec::new();
        append_ime_popup_elements(
            &mut renderer,
            state,
            &surface.output,
            output_scale,
            &mut elements,
        );
        append_layer_tier_elements(
            &mut renderer,
            &surface.output,
            output_scale,
            layer_tiers.above_toplevels,
            &mut elements,
        );

        if let Some((sp_lease, sp_items)) = &special_overlay {
            let sp_drop = state
                .active_special_ws_id
                .as_ref()
                .and_then(|sp_id| state.drop_preview_for_workspace(sp_id));
            let sp_elems = assemble_workspace_elements(
                &mut renderer,
                viewport,
                output_scale,
                sp_lease,
                RenderTargetKind::PhysicalOutput,
                OverlayKind::None,
                frame_blur_ctx.as_ref(),
                sp_drop,
                sp_items,
            );
            elements.extend(sp_elems);
            append_scratchpad_dim_scrim(viewport, &mut elements);
        }

        if let Some(ws) = state.workspaces.get(&out_ws_id)
            && ws.layout_mode == koma_model::LayoutMode::Scrolling
            && ws.presented_window_id().is_none()
        {
            let cam_x = ws.scrolling.peek_camera_x(state.elapsed_ms());
            let tab_segs = ws.scrolling.tab_indicator_segments_at(viewport, cam_x);
            append_tab_indicator_elements(&mut renderer, output_scale, &tab_segs, &mut elements);
        }
        let base_drop = state.drop_preview_for_workspace(&out_ws_id);
        let ws_elements = assemble_workspace_elements(
            &mut renderer,
            viewport,
            output_scale,
            &lease_state,
            RenderTargetKind::PhysicalOutput,
            OverlayKind::None,
            frame_blur_ctx.as_ref(),
            base_drop,
            &base_scene_items,
        );
        elements.extend(ws_elements);
        if state.config_error_banner.is_some() {
            prepend_config_error_banner(viewport, &mut elements);
        }

        append_layer_tier_elements(
            &mut renderer,
            &surface.output,
            output_scale,
            layer_tiers.below_toplevels,
            &mut elements,
        );

        prepend_agent_cursor_elements(
            &mut renderer,
            RenderTargetKind::PhysicalOutput,
            output_scale,
            agent_cursor_snap.as_ref(),
            &mut elements,
        );
        elements.splice(0..0, cursor_elems);
        (elements, DEFAULT_BACKDROP_CLEAR)
    };

    let scanout_flags =
        FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY | FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT;

    match surface
        .compositor
        .render_frame(&mut renderer, &elements, clear_color, scanout_flags)
    {
        Ok(res) => {
            if res.needs_sync()
                && let PrimaryPlaneElement::Swapchain(elem) = res.primary_element
                && let Err(err) = elem.sync.wait()
            {
                warn!("EGL swapchain sync fence wait error: {err:?}");
            }

            if !res.is_empty {
                match surface.compositor.queue_frame(()) {
                    Ok(()) => {
                        surface.redraw_state = SurfaceRedrawState::WaitingForVBlank {
                            redraw_needed: !locked && state.has_active_animations(),
                        };
                        return None;
                    }
                    Err(err) => {
                        warn!(
                            connector = %surface.connector_name,
                            "DrmCompositor::queue_frame failed: {err}"
                        );
                        surface.redraw_state = SurfaceRedrawState::Idle;
                    }
                }
            } else {
                // Zero damage this tick; release frame callbacks so clients don't stall
                surface.redraw_state = SurfaceRedrawState::Idle;
                let interval =
                    Duration::from_secs_f64(1_000.0 / f64::from(surface.refresh_mhz.max(1_000)));
                return Some((surface.output.clone(), interval));
            }
        }
        Err(err) => {
            // Expected transiently if switching VTs mid-frame
            debug!(
                connector = %surface.connector_name,
                "DrmCompositor::render_frame skipped: {err}"
            );
            surface.redraw_state = SurfaceRedrawState::Idle;
        }
    }

    None
}

fn dispatch_output_frame_callbacks(state: &KomaState, output: &Output, refresh_interval: Duration) {
    dispatch_output_frame_callbacks_filtered(state, output, Some(refresh_interval));
}

/// Processes physical bare-metal `libinput` keyboard, pointer scroll (`Mod+WheelScroll*` /
/// `TouchpadScroll*`), multi-finger touchpad gestures (`GestureSwipe*` / `GesturePinch*` /
/// `GestureHold*`), and tablet events.
fn process_tty_input_event<B>(state: &mut KomaState, event: InputEvent<B>)
where
    B: InputBackend + 'static,
    B::Device: 'static,
{
    match event {
        InputEvent::Keyboard { event } => {
            crate::keybind_dispatcher::process_keyboard_key_event(
                state,
                event.key_code(),
                event.state(),
                event.time(),
                true,
            );
        }

        // Physical mouse & touchpad relative motion (`PointerMotionEvent::delta`)
        InputEvent::PointerMotion { event } => {
            state.handle_pointer_relative_motion(
                event.delta(),
                event.delta_unaccel(),
                event.time(),
            );
        }

        // Drawing tablets (e.g. Wacom CTL-472 / OpenTabletDriver) & touchscreens
        InputEvent::PointerMotionAbsolute { event } => {
            let rect = state
                .focused_output()
                .or_else(|| state.all_outputs().into_iter().next())
                .map(|out| state.output_logical_rect(&out))
                .unwrap_or_else(|| smithay::utils::Rectangle::from_size((1920, 1080).into()));
            let local_pos: Point<f64, _> = event.position_transformed(rect.size);
            let global_pos: Point<f64, _> = (
                local_pos.x + f64::from(rect.loc.x),
                local_pos.y + f64::from(rect.loc.y),
            )
                .into();
            state.handle_pointer_absolute_motion(global_pos, event.time());
        }

        InputEvent::PointerButton { event } => {
            crate::keybind_dispatcher::process_pointer_button_event(
                state,
                event.button_code(),
                event.state(),
                event.time(),
                true,
            );
            state.queue_redraw();
        }

        InputEvent::PointerAxis { event } => {
            crate::keybind_dispatcher::process_pointer_axis_event::<B>(state, &event, true);
            state.queue_redraw();
        }

        InputEvent::GestureSwipeBegin { event } => {
            crate::gesture_dispatcher::process_gesture_swipe_begin::<B>(state, &event);
        }

        InputEvent::GestureSwipeUpdate { event } => {
            crate::gesture_dispatcher::process_gesture_swipe_update::<B>(state, &event, true);
        }

        InputEvent::GestureSwipeEnd { event } => {
            crate::gesture_dispatcher::process_gesture_swipe_end::<B>(state, &event);
        }

        InputEvent::GesturePinchBegin { event } => {
            crate::gesture_dispatcher::process_gesture_pinch_begin::<B>(state, &event);
        }

        InputEvent::GesturePinchUpdate { event } => {
            crate::gesture_dispatcher::process_gesture_pinch_update::<B>(state, &event, true);
        }

        InputEvent::GesturePinchEnd { event } => {
            crate::gesture_dispatcher::process_gesture_pinch_end::<B>(state, &event);
        }

        InputEvent::GestureHoldBegin { event } => {
            crate::gesture_dispatcher::process_gesture_hold_begin::<B>(state, &event);
        }

        InputEvent::GestureHoldEnd { event } => {
            crate::gesture_dispatcher::process_gesture_hold_end::<B>(state, &event);
        }

        InputEvent::TouchDown { event } => {
            state.handle_touch_down::<B>(event);
        }

        InputEvent::TouchMotion { event } => {
            state.handle_touch_motion::<B>(event);
        }

        InputEvent::TouchUp { event } => {
            state.handle_touch_up::<B>(event);
        }

        InputEvent::TouchFrame { .. } => {
            state.handle_touch_frame();
        }

        InputEvent::TouchCancel { .. } => {
            state.handle_touch_cancel();
        }

        InputEvent::TabletToolAxis { .. }
        | InputEvent::TabletToolProximity { .. }
        | InputEvent::TabletToolTip { .. }
        | InputEvent::TabletToolButton { .. } => {
            state.notify_user_activity();
        }

        _ => {}
    }
}

fn handle_libinput_device_event(state: &mut KomaState, event: &InputEvent<LibinputInputBackend>) {
    use smithay::reexports::input::DeviceCapability;

    match event {
        InputEvent::DeviceAdded { device } => {
            let mut dev = device.clone();
            let reg = state.lua_sandbox.config_snapshot();
            configure_libinput_device(&mut dev, &reg.input, &reg.devices);
            if dev.has_capability(DeviceCapability::Keyboard)
                && let Some(keyboard) = state.seat.get_keyboard()
            {
                dev.led_update(keyboard.led_state().into());
            }
            if let Some(ActiveBackend::Tty(tty)) = state.backend.as_mut()
                && !tty.libinput_devices.contains(&dev)
            {
                tty.libinput_devices.push(dev);
            }
        }
        InputEvent::DeviceRemoved { device } => {
            if let Some(ActiveBackend::Tty(tty)) = state.backend.as_mut() {
                tty.libinput_devices.retain(|d| d != device);
            }
        }
        _ => {}
    }
}

fn probe_libinput_device(
    device: &smithay::reexports::input::Device,
) -> koma_lua::schema::LibinputDeviceProbe {
    use koma_lua::schema::ClickMethodSpec;
    use smithay::reexports::input::{ClickMethod, DeviceCapability, SendEventsMode};

    let is_pointer = device.has_capability(DeviceCapability::Pointer);
    let tap_finger_count = device.config_tap_finger_count();
    let click_methods = device.config_click_methods();
    let supports_clickfinger = click_methods.contains(&ClickMethod::Clickfinger);
    let default_click_method = device.config_click_default_method().and_then(|m| match m {
        ClickMethod::ButtonAreas => Some(ClickMethodSpec::ButtonAreas),
        ClickMethod::Clickfinger => Some(ClickMethodSpec::Clickfinger),
        _ => None,
    });
    // In libinput, `Clickfinger` is supported specifically on clickpads/buttonpads lacking
    // separate physical mouse buttons (e.g. Apple MacBook Force Touch trackpads).
    let has_physical_buttons = !supports_clickfinger;
    let send_modes = device.config_send_events_modes();

    koma_lua::schema::LibinputDeviceProbe {
        name: device.name().into_owned(),
        vendor_id: device.id_vendor(),
        product_id: device.id_product(),
        is_pointer,
        tap_finger_count,
        supports_clickfinger,
        has_physical_buttons,
        default_click_method,
        dwt_available: device.config_dwt_is_available(),
        dwtp_available: false,
        supports_disabled_on_external_mouse: send_modes
            .contains(SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE),
        is_trackball: false,
        is_trackpoint: false,
    }
}

fn configure_libinput_device(
    device: &mut smithay::reexports::input::Device,
    input: &koma_lua::schema::InputConfigSpec,
    devices: &[koma_lua::schema::DeviceOverrideSpec],
) {
    use koma_lua::schema::{AccelProfileSpec, ClickMethodSpec, ScrollMethodSpec};
    use smithay::reexports::input::{
        AccelProfile, ClickMethod, DragLockState, ScrollMethod, SendEventsMode,
    };

    let probe = probe_libinput_device(device);
    if !probe.is_pointer {
        return;
    }

    let eff = koma_lua::schema::resolve_effective_libinput_settings(input, devices, &probe);

    if let Some(speed) = eff.accel_speed {
        let _ = device.config_accel_set_speed(speed);
    }
    if let Some(profile) = eff.accel_profile {
        let _ = device.config_accel_set_profile(match profile {
            AccelProfileSpec::Adaptive => AccelProfile::Adaptive,
            AccelProfileSpec::Flat => AccelProfile::Flat,
        });
    }
    if let Some(natural) = eff.natural_scroll {
        let _ = device.config_scroll_set_natural_scroll_enabled(natural);
    }
    if let Some(left) = eff.left_handed {
        let _ = device.config_left_handed_set(left);
    }
    if let Some(middle) = eff.middle_emulation {
        let _ = device.config_middle_emulation_set_enabled(middle);
    }
    if let Some(method) = eff.scroll_method {
        let mapped = match method {
            ScrollMethodSpec::NoScroll => ScrollMethod::NoScroll,
            ScrollMethodSpec::TwoFinger => ScrollMethod::TwoFinger,
            ScrollMethodSpec::Edge => ScrollMethod::Edge,
            ScrollMethodSpec::OnButtonDown => ScrollMethod::OnButtonDown,
        };
        let _ = device.config_scroll_set_method(mapped);
    }

    if eff.is_touchpad {
        if let Some(tap) = eff.tap {
            let _ = device.config_tap_set_enabled(tap);
        }
        if let Some(drag) = eff.tap_and_drag {
            let _ = device.config_tap_set_drag_enabled(drag);
        }
        if let Some(lock) = eff.tap_drag_lock {
            let _ = device.config_tap_set_drag_lock_enabled(if lock {
                DragLockState::EnabledTimeout
            } else {
                DragLockState::Disabled
            });
        }
        if let Some(dwt) = eff.dwt
            && probe.dwt_available
        {
            let _ = device.config_dwt_set_enabled(dwt);
        }
        if let Some(click) = eff.click_method {
            match click {
                ClickMethodSpec::Clickfinger => {
                    let _ = device.config_click_set_method(ClickMethod::Clickfinger);
                }
                ClickMethodSpec::ButtonAreas => {
                    let _ = device.config_click_set_method(ClickMethod::ButtonAreas);
                }
                ClickMethodSpec::None => {
                    if let Some(default_method) = device.config_click_default_method() {
                        let _ = device.config_click_set_method(default_method);
                    }
                }
            }
        }
        if let Some(disabled_on_ext) = eff.disabled_on_external_mouse
            && probe.supports_disabled_on_external_mouse
        {
            let mode = if disabled_on_ext {
                SendEventsMode::DISABLED_ON_EXTERNAL_MOUSE
            } else {
                SendEventsMode::ENABLED
            };
            let _ = device.config_send_events_set_mode(mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_env_values() {
        for v in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(is_truthy_env_value(Some(v)), "{v:?}");
        }
        for v in [None, Some(""), Some("0"), Some("false"), Some("off")] {
            assert!(!is_truthy_env_value(v), "{v:?}");
        }
    }

    #[test]
    fn vt_switch_hints_name_cause_and_fix() {
        let denied = vt_switch_errno_hint(Errno::ACCESS).expect("EACCES hint");
        assert!(denied.contains("polkit") && denied.contains("seatd"));
        assert_eq!(vt_switch_errno_hint(Errno::PERM), Some(denied));
        assert!(vt_switch_errno_hint(Errno::NOSYS).is_some());
        assert!(vt_switch_errno_hint(Errno::IO).is_none());
        assert!(vt_switch_failure_hint(&LibSeatError::FailedToChangeVt(Errno::ACCESS)).is_some());
        assert!(vt_switch_failure_hint(&LibSeatError::SessionLost).is_none());
    }

    #[test]
    fn mode_spec_parsing() {
        assert_eq!(
            parse_mode_spec("2560x1600@240"),
            Some((2560, 1600, Some(240_000)))
        );
        assert_eq!(
            parse_mode_spec("1920x1080@59.94Hz"),
            Some((1920, 1080, Some(59_940)))
        );
        assert_eq!(parse_mode_spec("1280x720"), Some((1280, 720, None)));
        assert_eq!(parse_mode_spec("preferred"), None);
    }

    #[test]
    fn drm_master_query_rejects_non_drm_fd() {
        let file = std::fs::File::open("/dev/null").expect("open /dev/null");
        assert!(drm_fd_is_master(file.as_fd()).is_err());
    }

    #[test]
    fn render_node_registry_promotes_split_display_render_gpu() {
        const APPLE_DRM_CARD: u32 = 1;
        const ASAHI_CARD: u32 = 2;
        const ASAHI_RENDER: u32 = 128;

        let mut reg = RenderNodeRegistry::new(APPLE_DRM_CARD, None);
        let display_only = reg.register_card(APPLE_DRM_CARD, None, None);
        assert_eq!(display_only.target_render_node, APPLE_DRM_CARD);
        assert_eq!(display_only.promoted_from, None);
        assert!(!reg.has_hardware_primary);

        let companion = reg.register_card(ASAHI_CARD, Some(ASAHI_RENDER), None);
        assert_eq!(companion.target_render_node, ASAHI_RENDER);
        assert_eq!(companion.promoted_from, Some(APPLE_DRM_CARD));
        assert!(reg.has_hardware_primary);
        assert_eq!(reg.primary_render_node, ASAHI_RENDER);
        assert_eq!(
            reg.card_render_target.get(&APPLE_DRM_CARD),
            Some(&ASAHI_RENDER)
        );
        assert_eq!(reg.card_render_target.get(&ASAHI_CARD), Some(&ASAHI_RENDER));
        assert_eq!(reg.render_refcounts.get(&ASAHI_RENDER), Some(&2));
    }

    #[test]
    fn render_node_registry_refcounts_shared_render_node_on_hot_unplug() {
        const IGPU_CARD: u32 = 1;
        const DGPU_CARD: u32 = 2;
        const SHARED_RENDER: u32 = 128;
        const DGPU_RENDER: u32 = 129;

        let mut reg = RenderNodeRegistry::new(SHARED_RENDER, None);
        reg.register_card(IGPU_CARD, Some(SHARED_RENDER), None);
        reg.register_card(DGPU_CARD, None, None);
        assert_eq!(reg.render_refcounts.get(&SHARED_RENDER), Some(&2));

        // Removing iGPU card while dGPU display-only card still borrows `SHARED_RENDER` keeps refcount == 1.
        assert_eq!(reg.unregister_card(IGPU_CARD), None);
        assert_eq!(reg.render_refcounts.get(&SHARED_RENDER), Some(&1));

        // Removing the final borrower drops refcount to 0 and returns `Some(SHARED_RENDER)` for removal.
        assert_eq!(reg.unregister_card(DGPU_CARD), Some(SHARED_RENDER));
        assert!(!reg.render_refcounts.contains_key(&SHARED_RENDER));

        // Independent hardware card uses its own render node without affecting others.
        let other = reg.register_card(DGPU_CARD, Some(DGPU_RENDER), None);
        assert_eq!(other.target_render_node, DGPU_RENDER);
        assert_eq!(reg.unregister_card(DGPU_CARD), Some(DGPU_RENDER));
    }

    #[test]
    fn connector_policy_filters_non_desktop_unless_opted_in() {
        assert!(!connector_should_be_skipped(false, None));
        assert!(connector_should_be_skipped(true, None));

        let mut spec = MonitorSpec::default();
        assert!(connector_should_be_skipped(true, Some(&spec)));

        spec.allow_non_desktop = true;
        assert!(!connector_should_be_skipped(true, Some(&spec)));
    }

    #[test]
    fn panel_orientation_honored_when_monitor_spec_has_no_explicit_transform() {
        assert_eq!(
            resolve_connector_transform(None, Some(OutputTransform::Rotate270)),
            OutputTransform::Rotate270
        );

        let mut spec = MonitorSpec {
            has_explicit_transform: false,
            transform: "normal".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            resolve_connector_transform(Some(&spec), Some(OutputTransform::Rotate90)),
            OutputTransform::Rotate90
        );

        spec.has_explicit_transform = true;
        spec.transform = "180".to_owned();
        assert_eq!(
            resolve_connector_transform(Some(&spec), Some(OutputTransform::Rotate90)),
            OutputTransform::Rotate180
        );
    }

    #[test]
    fn auto_scale_resolves_retina_and_zero_mm_fallback() {
        let auto_spec = MonitorSpec {
            scale: MonitorScaleSpec::Auto,
            ..Default::default()
        };

        // 14" MacBook Pro Retina (3024x1964 in 302x196 mm, ~254 DPI) -> 2.0
        assert!(
            (resolve_connector_scale(Some(&auto_spec), 3024, 1964, 302, 196) - 2.0).abs() < 1e-6
        );
        // Zero / unknown physical mm (virtual/EDID-less) -> safe 1.0 fallback
        assert!((resolve_connector_scale(Some(&auto_spec), 2560, 1440, 0, 0) - 1.0).abs() < 1e-6);
        // Unconfigured Retina display -> automatic 2.0 out-of-the-box
        assert!((resolve_connector_scale(None, 2880, 1800, 331, 207) - 2.0).abs() < 1e-6);
        // Explicit fixed scale overrides auto DPI
        let fixed_spec = MonitorSpec {
            scale: MonitorScaleSpec::Fixed(1.5),
            ..Default::default()
        };
        assert!(
            (resolve_connector_scale(Some(&fixed_spec), 3024, 1964, 302, 196) - 1.5).abs() < 1e-6
        );
    }
}
