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
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, anyhow, bail, ensure};
use calloop::{Dispatcher, LoopHandle, RegistrationToken};
use koma_model::LogicalViewport;
use koma_proto::OverlayKind;
use koma_render::{
    RenderTargetKind,
    elements::KomaRenderElement,
    frame_composer::{
        DEFAULT_BACKDROP_CLEAR, FrameBlurContext, append_scratchpad_dim_scrim,
        append_tab_indicator_elements, assemble_workspace_elements, prepend_config_error_banner,
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
            ImportDma as _, ImportEgl as _,
            element::{
                Kind,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            },
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{Event as SessionEvent, Session as _, libseat::LibSeatSession},
        udev::{self, UdevBackend, UdevEvent},
    },
    desktop::{layer_map_for_output, utils::send_frames_surface_tree},
    input::{keyboard::KeyboardSource, pointer::MotionEvent},
    output::{Mode, Output, OutputModeSource, PhysicalProperties, Scale},
    reexports::{
        drm::control::{self, Device as _, ModeFlags, ModeTypeFlags, connector, crtc},
        gbm::Modifier,
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Point, SERIAL_COUNTER, Scale as MathScale},
    wayland::{
        dmabuf::{DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal},
        shell::wlr_layer::Layer,
    },
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, info, trace, warn};

use crate::{
    backend_winit::hit_test_active_deck_surface,
    state::{ActiveBackend, KomaState},
};

/// Hybrid NVIDIA dGPU -> Intel/AMD iGPU cross-device copies require `Abgr` formats alongside `Argb`
/// (`niri/src/backend/tty.rs` lines 74-80).
const SUPPORTED_COLOR_FORMATS_10BIT: [Fourcc; 3] =
    [Fourcc::Abgr2101010, Fourcc::Argb8888, Fourcc::Abgr8888];

const SUPPORTED_COLOR_FORMATS_8BIT: [Fourcc; 2] = [Fourcc::Argb8888, Fourcc::Abgr8888];

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
    pub connector: connector::Handle,
    pub connector_name: String,
    pub output: Output,
    pub wl_global: Option<GlobalId>,
    pub compositor: GbmDrmCompositor,
    pub dmabuf_feedback: Option<DmabufFeedback>,
    pub redraw_state: SurfaceRedrawState,
    pub refresh_mhz: i32,
}

/// Physical DRM card (`/dev/dri/cardX`) opened unprivileged via `LibSeatSession::open`.
pub struct DrmOutputDevice {
    pub token: RegistrationToken,
    pub render_node: Option<DrmNode>,
    pub drm_scanner: DrmScanner,
    pub surfaces: HashMap<crtc::Handle, DrmOutputSurface>,
    // Drop `drm` and `gbm` after `surfaces` (`https://github.com/Smithay/smithay/issues/1102`).
    pub drm: DrmDevice,
    pub gbm: GbmDevice<DeviceFd>,
    pub allocator: GbmAllocator<DeviceFd>,
}

/// Bare-metal TTY display & input backend state owned by `KomaState`.
pub struct TtyBackendState {
    pub session: LibSeatSession,
    pub seat_name: String,
    udev_dispatcher: Dispatcher<'static, UdevBackend, KomaState>,
    libinput: Libinput,
    pub gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DeviceFd>>,
    pub primary_node: DrmNode,
    pub primary_render_node: DrmNode,
    pub devices: HashMap<DrmNode, DrmOutputDevice>,
    pub dmabuf_global: Option<DmabufGlobal>,
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
        let (primary_node, primary_render_node) = discover_primary_gpu(&seat_name)?;
        info!(
            seat = %seat_name,
            primary_node = ?primary_node,
            primary_render_node = ?primary_render_node,
            "initialized bare-metal TTY session & primary render GPU"
        );

        Ok(Self {
            session,
            seat_name,
            udev_dispatcher,
            libinput,
            gpu_manager,
            primary_node,
            primary_render_node,
            devices: HashMap::new(),
            dmabuf_global: None,
        })
    }

    /// Enumerates DRM nodes when the VT session is active, always adding `primary_node` first so
    /// secondary/dGPU outputs can share the primary `DmabufGlobal` and `GbmAllocator`.
    pub fn enumerate_initial_devices(&mut self, state: &mut KomaState) {
        if !self.session.is_active() {
            debug!("TTY session not yet active; deferring DRM enumeration to ActivateSession");
            return;
        }

        let snapshot: Vec<(dev_t, PathBuf)> = self
            .udev_dispatcher
            .as_source_ref()
            .device_list()
            .map(|(id, p)| (id, p.to_owned()))
            .collect();

        if let Some((prim_id, prim_path)) = snapshot
            .iter()
            .find(|(id, _)| *id == self.primary_node.dev_id())
        {
            if let Err(err) = self.device_added(*prim_id, prim_path, state) {
                warn!("failed initializing primary DRM node {prim_path:?}: {err:#}");
            }
        }

        for (dev_id, path) in &snapshot {
            if *dev_id == self.primary_node.dev_id() {
                continue;
            }
            if let Err(err) = self.device_added(*dev_id, path, state) {
                warn!("failed initializing secondary DRM node {path:?}: {err:#}");
            }
        }
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
        if let Some(prim_dev) = self.devices.get(&self.primary_node) {
            if let Some(surf) = prim_dev.surfaces.values().next() {
                return Some(surf.output.clone());
            }
        }
        self.devices
            .values()
            .flat_map(|d| d.surfaces.values())
            .next()
            .map(|s| s.output.clone())
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

    /// Live-updates fractional scale & VRR on connected KMS outputs during `init.lua` hot-reload
    /// without dropping client surfaces or resetting DRM devices (`niri.rs` `reload_output_config`).
    pub fn apply_hot_reloaded_monitors(
        &mut self,
        registry: &koma_lua::KomaConfigRegistry,
    ) -> Vec<(Output, u32, u32, f64)> {
        let mut resized = Vec::new();
        for device in self.devices.values_mut() {
            for surface in device.surfaces.values_mut() {
                let Some(spec) = registry.find_monitor(&surface.connector_name) else {
                    continue;
                };
                if spec.off {
                    continue;
                }
                let new_scale = if (0.5..=4.0).contains(&spec.scale) {
                    spec.scale
                } else {
                    1.0
                };
                let cur_scale = surface.output.current_scale().fractional_scale();
                if (cur_scale - new_scale).abs() > 1e-4 {
                    surface.output.change_current_state(
                        None,
                        None,
                        Some(Scale::Fractional(new_scale)),
                        None,
                    );
                    if let Some(wl_mode) = surface.output.current_mode() {
                        let lw = (f64::from(wl_mode.size.w) / new_scale).round().max(64.0) as u32;
                        let lh = (f64::from(wl_mode.size.h) / new_scale).round().max(64.0) as u32;
                        resized.push((surface.output.clone(), lw, lh, new_scale));
                    }
                    surface.redraw_state = SurfaceRedrawState::Queued;
                }
            }
        }
        resized
    }

    /// Switches Linux virtual terminal (`Ctrl + Alt + F1..F12` -> `VT 1..12`).
    pub fn change_vt(&mut self, vt: i32) {
        info!(vt, "switching Linux virtual terminal via libseat");
        if let Err(err) = self.session.change_vt(vt) {
            warn!(vt, "LibSeatSession::change_vt failed: {err}");
        }
    }

    fn device_added(
        &mut self,
        device_id: dev_t,
        path: &Path,
        state: &mut KomaState,
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
        let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, drm_notifier) =
            DrmDevice::new(device_fd.clone(), false).context("DrmDevice::new failed")?;
        let gbm = GbmDevice::new(device_fd.device_fd()).context("GbmDevice::new failed")?;

        // Initialize hardware EGL render node (rejecting software `llvmpipe` per `niri/src/backend/tty.rs`)
        let render_node = match try_register_egl_render_node(&mut self.gpu_manager, &gbm, node) {
            Ok(rn) => {
                debug!(?node, ?rn, "registered hardware EGL render node");
                Some(rn)
            }
            Err(err) => {
                debug!(
                    ?node,
                    "display-only or software DRM device; falling back to primary GPU: {err:#}"
                );
                None
            }
        };

        // Advertise `zwp_linux_dmabuf_v1` global once the primary render node comes online
        if render_node == Some(self.primary_render_node) && self.dmabuf_global.is_none() {
            let mut renderer = self
                .gpu_manager
                .single_renderer(&self.primary_render_node)
                .context("failed obtaining primary GlesRenderer")?;

            if let Err(err) = renderer.bind_wl_display(&state.display_handle) {
                trace!("legacy wl_drm EGL bind skipped: {err}");
            }

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
        }

        let allocator_gbm = if render_node.is_some() {
            gbm.clone()
        } else if let Some(primary_dev) = self.devices.get(&self.primary_node) {
            primary_dev.gbm.clone()
        } else {
            bail!("no GBM allocator available for display-only DRM node {node:?}");
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
                drm_scanner: DrmScanner::new(),
                surfaces: HashMap::new(),
                drm,
                gbm,
                allocator,
            },
        );

        self.device_changed(device_id, state);
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
            if let Some(global) = surf.wl_global {
                state.display_handle.remove_global::<KomaState>(global);
            }
        }
        if let Some(rn) = device.render_node {
            self.gpu_manager.as_mut().remove_node(&rn);
        }
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

        // Consult declarative `koma.monitor({ output = "eDP-1", mode = "2560x1600@240", scale = 1.6, ... })`
        let lua_out_cfg = state
            .lua_sandbox
            .config_snapshot()
            .find_monitor(&connector_name)
            .cloned();
        if lua_out_cfg.as_ref().is_some_and(|c| c.off) {
            info!(connector = %connector_name, "connector disabled (`off = true`) by init.lua koma.monitor rule");
            return Ok(());
        }

        let requested_mode = lua_out_cfg
            .as_ref()
            .map(|c| c.mode.as_str())
            .filter(|m| !m.is_empty() && *m != "preferred");
        let drm_mode = pick_connector_mode(&connector, requested_mode).ok_or_else(|| {
            anyhow!("connector `{connector_name}` advertised zero usable KMS modes")
        })?;
        let wl_mode = Mode::from(drm_mode);
        let scale_factor = lua_out_cfg
            .as_ref()
            .map(|c| c.scale)
            .filter(|&s| (0.5..=4.0).contains(&s))
            .unwrap_or(1.0);

        let target_render_node = self
            .devices
            .get(&node)
            .and_then(|d| d.render_node)
            .unwrap_or(self.primary_render_node);

        let render_formats: FormatSet = {
            let renderer = self.gpu_manager.single_renderer(&target_render_node)?;
            let raw_formats = renderer.as_ref().egl_context().dmabuf_render_formats();
            let is_display_only = self
                .devices
                .get(&node)
                .is_some_and(|d| d.render_node.is_none());

            raw_formats
                .iter()
                .copied()
                .filter(|fmt| {
                    if is_display_only {
                        return fmt.modifier == Modifier::Linear;
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

        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            connector_name.clone(),
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: connector.subpixel().into(),
                make: "DRM".to_owned(),
                model: connector_name.clone(),
                serial_number: format!("{node:?}-{crtc:?}"),
            },
        );
        output.change_current_state(
            Some(wl_mode),
            None,
            Some(Scale::Fractional(scale_factor)),
            Some((0, 0).into()),
        );
        output.set_preferred(wl_mode);
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

        let logical_w = (f64::from(wl_mode.size.w) / scale_factor).round().max(64.0) as u32;
        let logical_h = (f64::from(wl_mode.size.h) / scale_factor).round().max(64.0) as u32;

        info!(
            connector = %connector_name,
            mode = ?wl_mode.size,
            refresh_hz = (f64::from(wl_mode.refresh) / 1000.0),
            scale = scale_factor,
            "initialized atomic KMS DrmCompositor surface"
        );

        if let Some(device) = self.devices.get_mut(&node) {
            device.surfaces.insert(
                crtc,
                DrmOutputSurface {
                    connector: connector.handle(),
                    connector_name,
                    output: output.clone(),
                    wl_global,
                    compositor,
                    dmabuf_feedback,
                    redraw_state: SurfaceRedrawState::Queued,
                    refresh_mhz: wl_mode.refresh,
                },
            );
        }

        state.handle_output_resized(&output, logical_w, logical_h, scale_factor);
        Ok(())
    }

    fn connector_disconnected(&mut self, node: DrmNode, crtc: crtc::Handle, state: &mut KomaState) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };
        if let Some(removed) = device.surfaces.remove(&crtc) {
            if let Some(global) = removed.wl_global {
                state.display_handle.remove_global::<KomaState>(global);
            }
            info!(
                connector = %removed.connector_name,
                "disconnected KMS connector & removed wl_output global"
            );
        }
    }
}

/// Discovers the primary DRM card + render node via `KOMA_DRM_DEVICE` or `udev::primary_gpu`.
fn discover_primary_gpu(seat_name: &str) -> anyhow::Result<(DrmNode, DrmNode)> {
    let primary_path = match env::var_os("KOMA_DRM_DEVICE") {
        Some(override_path) => PathBuf::from(override_path),
        None => udev::primary_gpu(seat_name)
            .context("udev::primary_gpu query failed")?
            .context("no usable DRM/KMS GPU detected on seat")?,
    };

    let primary_node = DrmNode::from_path(&primary_path)
        .with_context(|| format!("failed opening primary DRM node {primary_path:?}"))?;
    let primary_render_node = primary_node
        .node_with_type(NodeType::Render)
        .and_then(Result::ok)
        .unwrap_or_else(|| {
            warn!(
                ?primary_node,
                "primary GPU has no dedicated renderD* node; reusing card node"
            );
            primary_node
        });
    Ok((primary_node, primary_render_node))
}

fn try_register_egl_render_node(
    gpu_manager: &mut GpuManager<GbmGlesBackend<GlesRenderer, DeviceFd>>,
    gbm: &GbmDevice<DeviceFd>,
    fallback_node: DrmNode,
) -> anyhow::Result<DrmNode> {
    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_device = EGLDevice::device_for_display(&display)?;
    ensure!(
        !egl_device.is_software(),
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
    Ok(render_node)
}

/// Selects either the user-requested `"WIDTHxHEIGHT@HZ"` mode from `init.lua` or the connector's
/// highest-refresh non-interlaced `PREFERRED` mode (`niri/src/backend/tty.rs` `pick_mode`).
fn pick_connector_mode(
    connector: &connector::Info,
    requested_spec: Option<&str>,
) -> Option<control::Mode> {
    if let Some(spec) = requested_spec {
        if let Some((target_w, target_h, target_mhz)) = parse_mode_spec(spec) {
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
            for vseat in state.vseats.values_mut() {
                let _ = vseat.flush_all_held();
            }

            let _ = crate::pointer_grabs::cancel_active_pointer_grab(
                state,
                SERIAL_COUNTER.next_serial(),
                smithay::backend::input::InputTime::now(),
            );
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
                            let _ = surf.compositor.reset_buffers();
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

    if let Some(ActiveBackend::Tty(tty)) = state.backend.as_mut() {
        if let Some(device) = tty.devices.get_mut(&node) {
            if let Some(surface) = device.surfaces.get_mut(&crtc) {
                if let Err(err) = surface.compositor.frame_submitted() {
                    warn!(
                        connector = %surface.connector_name,
                        "DrmCompositor::frame_submitted error: {err}"
                    );
                }

                let redraw_again =
                    match mem::replace(&mut surface.redraw_state, SurfaceRedrawState::Idle) {
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
        }
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
    state: &KomaState,
    node: DrmNode,
    crtc: crtc::Handle,
) -> Option<(Output, Duration)> {
    let ws = state.workspaces.get(&state.active_ws_id)?;
    let viewport: LogicalViewport = ws.viewport;
    let lease_state = ws.lease.state().clone();
    let active_deck_id = ws.deck.active();

    let device = tty.devices.get_mut(&node)?;
    if !device.drm.is_active() {
        return None;
    }
    let target_render_node = device.render_node.unwrap_or(tty.primary_render_node);
    let surface = device.surfaces.get_mut(&crtc)?;
    let output_scale = MathScale::from(surface.output.current_scale().fractional_scale());

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

    state.tick_frame_animations_and_edge_scroll();
    let (base_scene_items, special_overlay) = state.collect_physical_scene_items();
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
            .map(|m| m.size)
            .unwrap_or_else(|| (1920, 1080).into());
        let mut gles_bg_elems: Vec<KomaRenderElement<GlesRenderer>> = Vec::new();
        {
            let map = layer_map_for_output(&surface.output);
            for tier in [Layer::Bottom, Layer::Background] {
                for layer in map.layers_on(tier) {
                    if let Some(geo) = map.layer_geometry(layer) {
                        let loc_phys = geo.loc.to_f64().to_physical_precise_round(output_scale);
                        let l_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                            render_elements_from_surface_tree(
                                renderer.as_mut(),
                                layer.wl_surface(),
                                loc_phys,
                                output_scale,
                                1.0,
                                Kind::Unspecified,
                            );
                        gles_bg_elems.extend(l_elems.into_iter().map(KomaRenderElement::from));
                    }
                }
            }
        }
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

    // Assemble front-to-back elements: Overlay/Top layer-shell -> Scratchpad -> Base Space -> Bottom/Bg layers
    let mut elements: Vec<KomaRenderElement<TtyRenderer<'_>>> = Vec::new();
    {
        let map = layer_map_for_output(&surface.output);
        for tier in [Layer::Overlay, Layer::Top] {
            for layer in map.layers_on(tier) {
                if let Some(geo) = map.layer_geometry(layer) {
                    let loc_phys = geo.loc.to_f64().to_physical_precise_round(output_scale);
                    let l_elems: Vec<WaylandSurfaceRenderElement<TtyRenderer<'_>>> =
                        render_elements_from_surface_tree(
                            &mut renderer,
                            layer.wl_surface(),
                            loc_phys,
                            output_scale,
                            1.0,
                            Kind::Unspecified,
                        );
                    elements.extend(l_elems.into_iter().map(KomaRenderElement::from));
                }
            }
        }
    }

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

    if let Some(ws) = state.workspaces.get(&state.active_ws_id) {
        if ws.layout_mode == koma_model::LayoutMode::Scrolling && ws.fullscreen_window.is_none() {
            let cam_x = ws.scrolling.peek_camera_x(state.elapsed_ms());
            let tab_segs = ws.scrolling.tab_indicator_segments_at(viewport, cam_x);
            append_tab_indicator_elements(&mut renderer, output_scale, &tab_segs, &mut elements);
        }
    }
    let base_drop = state.drop_preview_for_workspace(&state.active_ws_id);
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

    {
        let map = layer_map_for_output(&surface.output);
        for tier in [Layer::Bottom, Layer::Background] {
            for layer in map.layers_on(tier) {
                if let Some(geo) = map.layer_geometry(layer) {
                    let loc_phys = geo.loc.to_f64().to_physical_precise_round(output_scale);
                    let l_elems: Vec<WaylandSurfaceRenderElement<TtyRenderer<'_>>> =
                        render_elements_from_surface_tree(
                            &mut renderer,
                            layer.wl_surface(),
                            loc_phys,
                            output_scale,
                            1.0,
                            Kind::Unspecified,
                        );
                    elements.extend(l_elems.into_iter().map(KomaRenderElement::from));
                }
            }
        }
    }

    let scanout_flags =
        FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY | FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT;

    match surface.compositor.render_frame(
        &mut renderer,
        &elements,
        DEFAULT_BACKDROP_CLEAR,
        scanout_flags,
    ) {
        Ok(res) => {
            if res.needs_sync() {
                if let PrimaryPlaneElement::Swapchain(elem) = res.primary_element {
                    if let Err(err) = elem.sync.wait() {
                        warn!("EGL swapchain sync fence wait error: {err:?}");
                    }
                }
            }

            if !res.is_empty {
                match surface.compositor.queue_frame(()) {
                    Ok(()) => {
                        surface.redraw_state = SurfaceRedrawState::WaitingForVBlank {
                            redraw_needed: state.has_active_animations(),
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
    let elapsed = state.elapsed_duration();
    for wl_surf in state.visible_physical_toplevel_surfaces() {
        send_frames_surface_tree(&wl_surf, output, elapsed, Some(refresh_interval), |_, _| {
            Some(output.clone())
        });
    }

    let map = layer_map_for_output(output);
    for layer in map.layers() {
        send_frames_surface_tree(
            layer.wl_surface(),
            output,
            elapsed,
            Some(refresh_interval),
            |_, _| Some(output.clone()),
        );
    }
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
            let (max_w, max_h) = state
                .workspaces
                .get(&state.active_ws_id)
                .map(|ws| (f64::from(ws.viewport.width), f64::from(ws.viewport.height)))
                .unwrap_or((1920.0, 1080.0));

            let delta = event.delta();
            let next_x = (state.pointer_location.x + delta.x).clamp(0.0, (max_w - 1.0).max(0.0));
            let next_y = (state.pointer_location.y + delta.y).clamp(0.0, (max_h - 1.0).max(0.0));
            let pos = Point::from((next_x, next_y));
            state.pointer_location = pos;

            let under = hit_test_active_deck_surface(state, pos);
            if let Some(pointer) = state.seat.get_pointer() {
                let serial = SERIAL_COUNTER.next_serial();
                pointer.motion(
                    state,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time(),
                    },
                );
                pointer.frame(state);
            }
            state.queue_redraw();
        }

        // Drawing tablets (e.g. Wacom CTL-472 / OpenTabletDriver) & touchscreens
        InputEvent::PointerMotionAbsolute { event } => {
            let out_geo_size = state
                .workspaces
                .get(&state.active_ws_id)
                .map(|ws| (ws.viewport.width as i32, ws.viewport.height as i32))
                .unwrap_or((1920, 1080));
            let pos: Point<f64, _> = event.position_transformed(out_geo_size.into());
            state.pointer_location = pos;

            let under = hit_test_active_deck_surface(state, pos);
            if let Some(pointer) = state.seat.get_pointer() {
                let serial = SERIAL_COUNTER.next_serial();
                pointer.motion(
                    state,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time(),
                    },
                );
                pointer.frame(state);
            }
            state.queue_redraw();
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

        _ => {}
    }
}
