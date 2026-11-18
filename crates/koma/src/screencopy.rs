//! `zwlr_screencopy_manager_v1` protocol implementation (v3) with SHM and DMA-BUF support.
//!
//! Enforces Koma's fail-closed session-lock invariant (`is_locked_or_abandoned()`) and
//! `block_out_from` Protected Surface `#000000` blackout across `capture_output`,
//! `capture_output_region`, `copy`, `copy_with_damage`, and deferred damage completion.

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use koma_model::{AgentCursorFrameSnapshot, LayoutMode, LogicalViewport, WorkspaceLeaseState};
use koma_proto::{OverlayKind, WorkspaceId};
use koma_render::{
    RenderTargetKind,
    elements::KomaRenderElement,
    frame_composer::{
        append_closing_window_elements, append_tab_indicator_elements, assemble_workspace_elements,
        prepend_agent_cursor_elements, prepend_config_error_banner,
    },
    offscreen_fbo::LazyOffscreenSurface,
};
use smithay::{
    backend::{
        allocator::{Buffer as _, Fourcc, dmabuf::Dmabuf},
        renderer::{
            Bind as _, Color32F, Renderer,
            damage::OutputDamageTracker,
            element::utils::{Relocate, RelocateRenderElement},
            gles::GlesRenderer,
        },
    },
    output::{Output, WeakOutput},
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            Client, DataInit, DisplayHandle, New, Resource, Weak,
            backend::ClientId,
            protocol::{wl_buffer::WlBuffer, wl_shm},
        },
    },
    utils::{Logical, Physical, Point, Rectangle, Size, Transform},
    wayland::{Dispatch2, GlobalDispatch2, dmabuf, shm},
};
use tracing::{debug, warn};

use crate::{
    backend_winit::{append_ime_popup_elements, append_layer_tier_elements},
    keybind_overlay::prepend_keybind_overlay_elements,
    security_context::client_is_unrestricted,
    state::KomaState,
};

const SCREENCOPY_VERSION: u32 = 3;
const SCREENCOPY_BG_RGBA: [u8; 4] = [0x1e, 0x1e, 0x2e, 0xff];
const SCREENCOPY_BG_CLEAR: Color32F = Color32F::new(
    0x1e as f32 / 255.0,
    0x1e as f32 / 255.0,
    0x2e as f32 / 255.0,
    1.0,
);

static NEXT_MANAGER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct ScreencopyGlobalData;

#[derive(Debug, Clone, Copy)]
pub struct ScreencopyManagerUserData {
    pub manager_id: u64,
}

#[derive(Debug, Clone)]
pub struct ScreencopyFrameSpec {
    pub manager_id: u64,
    pub output: WeakOutput,
    pub overlay_cursor: bool,
    pub phys_origin: Point<i32, Physical>,
    pub phys_size: Size<i32, Physical>,
    pub output_phys_size: Size<i32, Physical>,
    pub scale: f64,
}

#[derive(Debug)]
pub struct ScreencopyFrameState {
    pub spec: Option<ScreencopyFrameSpec>,
    pub used: bool,
    pub failed: bool,
}

#[derive(Debug)]
pub struct ScreencopyFrameUserData {
    inner: Mutex<ScreencopyFrameState>,
}

#[derive(Debug)]
pub struct PendingDamageCopy {
    pub frame: Weak<ZwlrScreencopyFrameV1>,
    pub buffer: WlBuffer,
    pub spec: ScreencopyFrameSpec,
}

/// Compositor-wide `zwlr_screencopy_manager_v1` tracking and damage-triggered frame queue.
#[derive(Debug)]
pub struct ScreencopyState {
    pub damage_epoch: u64,
    pub manager_last_copied_epoch: HashMap<u64, u64>,
    pub pending_damage_copies: Vec<PendingDamageCopy>,
}

impl ScreencopyState {
    #[must_use]
    pub fn new(display_handle: &DisplayHandle) -> Self {
        display_handle.create_global::<KomaState, ZwlrScreencopyManagerV1, _>(
            SCREENCOPY_VERSION,
            ScreencopyGlobalData,
        );
        Self {
            damage_epoch: 1,
            manager_last_copied_epoch: HashMap::new(),
            pending_damage_copies: Vec::new(),
        }
    }

    /// Marks compositor output damage so waiting `copy_with_damage` requests can complete.
    pub fn mark_damage(&mut self) {
        self.damage_epoch = self.damage_epoch.wrapping_add(1).max(1);
    }

    #[must_use]
    pub fn manager_has_unseen_damage(&self, manager_id: u64) -> bool {
        self.manager_last_copied_epoch.get(&manager_id).copied() != Some(self.damage_epoch)
    }

    pub fn record_manager_copy(&mut self, manager_id: u64) {
        self.manager_last_copied_epoch
            .insert(manager_id, self.damage_epoch);
    }
}

/// Splits a monotonic duration into `(tv_sec_hi, tv_sec_lo, tv_nsec)` per `wlr-screencopy` spec.
#[must_use]
pub const fn timestamp_parts(elapsed: Duration) -> (u32, u32, u32) {
    let secs = elapsed.as_secs();
    let tv_sec_hi = (secs >> 32) as u32;
    let tv_sec_lo = (secs & 0xFFFF_FFFF) as u32;
    let tv_nsec = elapsed.subsec_nanos();
    (tv_sec_hi, tv_sec_lo, tv_nsec)
}

pub type ClampedCaptureRegion = (
    Rectangle<i32, Logical>,
    Point<i32, Physical>,
    Size<i32, Physical>,
);
type OutputCaptureDimensions = (Size<i32, Logical>, Size<i32, Physical>, f64);

/// Computes `(phys_origin, phys_size)` clamped cleanly within an output's physical pixel dimensions.
#[must_use]
pub fn compute_clamped_physical_region(
    output_logical_size: Size<i32, Logical>,
    output_phys_size: Size<i32, Physical>,
    scale: f64,
    requested_region: Option<Rectangle<i32, Logical>>,
) -> Option<ClampedCaptureRegion> {
    if output_logical_size.w <= 0
        || output_logical_size.h <= 0
        || output_phys_size.w <= 0
        || output_phys_size.h <= 0
    {
        return None;
    }

    let output_rect = Rectangle::from_size(output_logical_size);
    let logical_region = match requested_region {
        Some(req) => {
            if req.size.w <= 0 || req.size.h <= 0 {
                return None;
            }
            req.intersection(output_rect)?
        }
        None => output_rect,
    };

    if logical_region.size.w <= 0 || logical_region.size.h <= 0 {
        return None;
    }

    let eff_scale = if scale > 0.0 { scale } else { 1.0 };
    if requested_region.is_none() {
        return Some((logical_region, (0, 0).into(), output_phys_size));
    }

    let rx = ((logical_region.loc.x as f64) * eff_scale)
        .floor()
        .clamp(0.0, f64::from(output_phys_size.w.saturating_sub(1))) as i32;
    let ry = ((logical_region.loc.y as f64) * eff_scale)
        .floor()
        .clamp(0.0, f64::from(output_phys_size.h.saturating_sub(1))) as i32;
    let right = (((logical_region.loc.x + logical_region.size.w) as f64) * eff_scale)
        .ceil()
        .clamp(f64::from(rx + 1), f64::from(output_phys_size.w)) as i32;
    let bottom = (((logical_region.loc.y + logical_region.size.h) as f64) * eff_scale)
        .ceil()
        .clamp(f64::from(ry + 1), f64::from(output_phys_size.h)) as i32;

    Some((
        logical_region,
        Point::from((rx, ry)),
        Size::from((right - rx, bottom - ry)),
    ))
}

impl GlobalDispatch2<ZwlrScreencopyManagerV1, KomaState> for ScreencopyGlobalData {
    fn can_view(&self, client: &Client) -> bool {
        client_is_unrestricted(client)
    }

    fn bind(
        &self,
        _state: &mut KomaState,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        let manager_id = NEXT_MANAGER_ID.fetch_add(1, Ordering::Relaxed);
        data_init.init(resource, ScreencopyManagerUserData { manager_id });
    }
}

impl Dispatch2<ZwlrScreencopyManagerV1, KomaState> for ScreencopyManagerUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, KomaState>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                init_screencopy_frame(
                    state,
                    self.manager_id,
                    frame,
                    overlay_cursor != 0,
                    &output,
                    None,
                    data_init,
                );
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                init_screencopy_frame(
                    state,
                    self.manager_id,
                    frame,
                    overlay_cursor != 0,
                    &output,
                    Some(Rectangle::new((x, y).into(), (width, height).into())),
                    data_init,
                );
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut KomaState,
        _client: ClientId,
        _resource: &ZwlrScreencopyManagerV1,
    ) {
        state
            .screencopy_state
            .manager_last_copied_epoch
            .remove(&self.manager_id);
        state
            .screencopy_state
            .pending_damage_copies
            .retain(|pending| pending.spec.manager_id != self.manager_id);
    }
}

fn init_screencopy_frame(
    state: &mut KomaState,
    manager_id: u64,
    frame_new: New<ZwlrScreencopyFrameV1>,
    overlay_cursor: bool,
    wl_output: &smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    requested_region: Option<Rectangle<i32, Logical>>,
    data_init: &mut DataInit<'_, KomaState>,
) {
    state.reconcile_session_lock();
    let frame = data_init.init(
        frame_new,
        ScreencopyFrameUserData {
            inner: Mutex::new(ScreencopyFrameState {
                spec: None,
                used: false,
                failed: false,
            }),
        },
    );

    // Fail-closed security invariant: never leak unlocked desktop geometry or contents when locked.
    if state.is_locked_or_abandoned() {
        mark_frame_failed(&frame);
        return;
    }

    let Some(output) = Output::from_resource(wl_output) else {
        mark_frame_failed(&frame);
        return;
    };
    if !state.all_outputs().iter().any(|active| active == &output) {
        mark_frame_failed(&frame);
        return;
    }

    let Some((out_log_size, out_phys_size, scale)) = output_capture_dimensions(&output) else {
        mark_frame_failed(&frame);
        return;
    };

    let Some((_, phys_origin, phys_size)) =
        compute_clamped_physical_region(out_log_size, out_phys_size, scale, requested_region)
    else {
        mark_frame_failed(&frame);
        return;
    };

    let spec = ScreencopyFrameSpec {
        manager_id,
        output: output.downgrade(),
        overlay_cursor,
        phys_origin,
        phys_size,
        output_phys_size: out_phys_size,
        scale,
    };

    {
        let data = frame.data::<ScreencopyFrameUserData>().unwrap();
        let mut guard = data.inner.lock().unwrap();
        guard.spec = Some(spec);
    }

    let width = phys_size.w as u32;
    let height = phys_size.h as u32;
    let stride = width.saturating_mul(4);

    // wlr-screencopy requires `buffer` and `linux_dmabuf` to be emitted once per frame;
    // multiple SHM events make v1/v2 clients (such as grim) issue a second `copy`.
    frame.buffer(wl_shm::Format::Argb8888, width, height, stride);

    if frame.version() >= 3 {
        if state.backend.is_some() {
            frame.linux_dmabuf(Fourcc::Argb8888 as u32, width, height);
        }
        frame.buffer_done();
    }
}

fn mark_frame_failed(frame: &ZwlrScreencopyFrameV1) {
    if let Some(data) = frame.data::<ScreencopyFrameUserData>()
        && let Ok(mut guard) = data.inner.lock()
    {
        guard.failed = true;
    }
    frame.failed();
}

fn output_capture_dimensions(output: &Output) -> Option<OutputCaptureDimensions> {
    let mode = output.current_mode()?;
    let scale = output.current_scale().fractional_scale();
    let phys_size = output.current_transform().transform_size(mode.size);
    let logical_size = phys_size.to_f64().to_logical(scale).to_i32_round();
    Some((logical_size, phys_size, scale))
}

impl Dispatch2<ZwlrScreencopyFrameV1, KomaState> for ScreencopyFrameUserData {
    fn request(
        &self,
        state: &mut KomaState,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, KomaState>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                handle_copy_request(state, frame, &buffer, false);
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                handle_copy_request(state, frame, &buffer, true);
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {
                state
                    .screencopy_state
                    .pending_damage_copies
                    .retain(|pending| pending.frame.id() != frame.id());
            }
            _ => {}
        }
    }

    fn destroyed(&self, state: &mut KomaState, _client: ClientId, frame: &ZwlrScreencopyFrameV1) {
        state
            .screencopy_state
            .pending_damage_copies
            .retain(|pending| pending.frame.id() != frame.id());
    }
}

fn handle_copy_request(
    state: &mut KomaState,
    frame: &ZwlrScreencopyFrameV1,
    buffer: &WlBuffer,
    with_damage: bool,
) {
    let spec = {
        let data = frame.data::<ScreencopyFrameUserData>().unwrap();
        let mut guard = data.inner.lock().unwrap();
        if guard.used {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "screencopy frame already copied",
            );
            return;
        }
        guard.used = true;
        if guard.failed {
            return;
        }
        let Some(spec) = guard.spec.clone() else {
            guard.failed = true;
            frame.failed();
            return;
        };
        spec
    };

    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        mark_frame_failed(frame);
        return;
    }

    if !validate_target_buffer(frame, buffer, spec.phys_size) {
        return;
    }

    if with_damage
        && !state
            .screencopy_state
            .manager_has_unseen_damage(spec.manager_id)
    {
        state
            .screencopy_state
            .pending_damage_copies
            .push(PendingDamageCopy {
                frame: frame.downgrade(),
                buffer: buffer.clone(),
                spec,
            });
        return;
    }

    execute_screencopy_copy(state, frame, buffer, &spec, with_damage);
}

/// Completes or fails-closed any pending `copy_with_damage` requests after damage occurs.
pub fn flush_pending_screencopy_frames(state: &mut KomaState) {
    if state.screencopy_state.pending_damage_copies.is_empty() {
        return;
    }
    let pending = std::mem::take(&mut state.screencopy_state.pending_damage_copies);
    state.reconcile_session_lock();
    for item in pending {
        let Ok(frame) = item.frame.upgrade() else {
            continue;
        };
        if !frame.is_alive() || !item.buffer.is_alive() {
            continue;
        }
        if state.is_locked_or_abandoned() {
            mark_frame_failed(&frame);
            continue;
        }
        if !state
            .screencopy_state
            .manager_has_unseen_damage(item.spec.manager_id)
        {
            state.screencopy_state.pending_damage_copies.push(item);
            continue;
        }
        execute_screencopy_copy(state, &frame, &item.buffer, &item.spec, true);
    }
}

fn validate_target_buffer(
    frame: &ZwlrScreencopyFrameV1,
    buffer: &WlBuffer,
    expected_size: Size<i32, Physical>,
) -> bool {
    if let Ok(dmabuf) = dmabuf::get_dmabuf(buffer) {
        let valid_fmt = is_supported_fourcc(dmabuf.format().code);
        if !valid_fmt
            || dmabuf.width() != expected_size.w as u32
            || dmabuf.height() != expected_size.h as u32
        {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                "dmabuf format or dimensions mismatch",
            );
            return false;
        }
        return true;
    }

    let shm_check = shm::with_buffer_contents(buffer, |_ptr, pool_len, data| {
        validate_shm_metadata(&data, pool_len, expected_size)
    });
    match shm_check {
        Ok(Ok(())) => true,
        Ok(Err(reason)) => {
            frame.post_error(zwlr_screencopy_frame_v1::Error::InvalidBuffer, reason);
            false
        }
        Err(_) => {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                "unsupported buffer type",
            );
            false
        }
    }
}

const fn is_supported_fourcc(fourcc: Fourcc) -> bool {
    matches!(
        fourcc,
        Fourcc::Argb8888 | Fourcc::Xrgb8888 | Fourcc::Abgr8888 | Fourcc::Xbgr8888
    )
}

const fn is_supported_shm_format(format: wl_shm::Format) -> bool {
    matches!(
        format,
        wl_shm::Format::Argb8888
            | wl_shm::Format::Xrgb8888
            | wl_shm::Format::Abgr8888
            | wl_shm::Format::Xbgr8888
    )
}

fn validate_shm_metadata(
    data: &shm::BufferData,
    pool_len: usize,
    expected_size: Size<i32, Physical>,
) -> Result<(), &'static str> {
    if !is_supported_shm_format(data.format) {
        return Err("unsupported wl_shm format");
    }
    if data.width != expected_size.w || data.height != expected_size.h {
        return Err("wl_shm buffer dimensions mismatch");
    }
    let min_stride = data.width.checked_mul(4).ok_or("wl_shm width overflow")?;
    if data.stride < min_stride || data.offset < 0 {
        return Err("invalid wl_shm stride or offset");
    }
    let needed = (data.offset as usize)
        .checked_add(
            (data.height as usize)
                .checked_mul(data.stride as usize)
                .ok_or("wl_shm byte length overflow")?,
        )
        .ok_or("wl_shm pool offset overflow")?;
    if needed > pool_len {
        return Err("wl_shm buffer exceeds pool size");
    }
    Ok(())
}

fn execute_screencopy_copy(
    state: &mut KomaState,
    frame: &ZwlrScreencopyFrameV1,
    buffer: &WlBuffer,
    spec: &ScreencopyFrameSpec,
    emit_damage: bool,
) {
    state.reconcile_session_lock();
    if state.is_locked_or_abandoned() {
        mark_frame_failed(frame);
        return;
    }

    let Some(output) = spec.output.upgrade() else {
        mark_frame_failed(frame);
        return;
    };

    if let Ok(dmabuf) = dmabuf::get_dmabuf(buffer) {
        if let Err(err) = copy_to_dmabuf(state, &output, dmabuf, spec) {
            warn!("screencopy dmabuf render failed: {err}");
            mark_frame_failed(frame);
            return;
        }
        finalize_ready_frame(state, frame, spec, emit_damage);
        return;
    }

    let rendered_rgba = if state.backend.is_some() {
        match render_output_rgba8_for_screencopy(state, &output, spec) {
            Ok(pixels) => Some(pixels),
            Err(err) => {
                warn!("screencopy GPU readout failed: {err}");
                mark_frame_failed(frame);
                return;
            }
        }
    } else {
        None
    };

    let write_res = shm::with_buffer_contents_mut(buffer, |ptr, pool_len, data| {
        validate_shm_metadata(&data, pool_len, spec.phys_size)?;
        write_pixels_to_shm(ptr, pool_len, &data, rendered_rgba.as_deref(), spec);
        Ok::<(), &'static str>(())
    });

    match write_res {
        Ok(Ok(())) => finalize_ready_frame(state, frame, spec, emit_damage),
        Ok(Err(reason)) => {
            frame.post_error(zwlr_screencopy_frame_v1::Error::InvalidBuffer, reason);
        }
        Err(_) => {
            mark_frame_failed(frame);
        }
    }
}

fn finalize_ready_frame(
    state: &mut KomaState,
    frame: &ZwlrScreencopyFrameV1,
    spec: &ScreencopyFrameSpec,
    emit_damage: bool,
) {
    state.screencopy_state.record_manager_copy(spec.manager_id);
    if emit_damage && frame.version() >= 2 {
        frame.damage(0, 0, spec.phys_size.w as u32, spec.phys_size.h as u32);
    }
    frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
    let (tv_sec_hi, tv_sec_lo, tv_nsec) = timestamp_parts(state.start_time.elapsed());
    frame.ready(tv_sec_hi, tv_sec_lo, tv_nsec);
    debug!(
        output = %output_name_for_log(&spec.output),
        w = spec.phys_size.w,
        h = spec.phys_size.h,
        cursor = spec.overlay_cursor,
        "completed zwlr_screencopy_frame_v1 capture"
    );
}

fn output_name_for_log(weak: &WeakOutput) -> String {
    weak.upgrade()
        .map_or_else(|| "<dropped>".to_owned(), |o| o.name())
}

fn render_output_rgba8_for_screencopy(
    state: &mut KomaState,
    output: &Output,
    spec: &ScreencopyFrameSpec,
) -> Result<Vec<u8>, String> {
    let mut backend = state.backend.take();
    let mut offscreen_surfaces = std::mem::take(&mut state.offscreen_surfaces);
    let now_ms = state.elapsed_ms();
    let ws_id = output_workspace_id(state, output);

    let result = if let Some(active) = backend.as_mut() {
        let cursor_pos = state.pointer_location - output.current_location().to_f64();
        let mut cursor_manager = std::mem::take(&mut state.cursor_manager);
        let agent_cursor_snap = spec
            .overlay_cursor
            .then(|| state.step_agent_cursor_for_output(&output.name(), &ws_id))
            .flatten();
        let base_items = state.collect_workspace_scene_items(&ws_id, true);
        let special_overlay =
            if state.focused_output_name().as_deref() == Some(output.name().as_str()) {
                state
                    .active_special_ws_id
                    .as_ref()
                    .filter(|sp_id| state.workspaces.contains_key(*sp_id))
                    .map(|sp_id| {
                        let sp_lease = state
                            .workspaces
                            .get(sp_id)
                            .map(|ws| ws.lease.state().clone())
                            .unwrap_or(WorkspaceLeaseState::Unheld);
                        (sp_lease, state.collect_workspace_scene_items(sp_id, true))
                    })
            } else {
                None
            };
        let base_lease = state
            .workspaces
            .get(&ws_id)
            .map(|ws| ws.lease.state().clone())
            .unwrap_or(WorkspaceLeaseState::Unheld);
        let viewport = state
            .workspaces
            .get(&ws_id)
            .map(|ws| ws.viewport)
            .unwrap_or_else(|| {
                LogicalViewport::new(
                    spec.output_phys_size.w as u32,
                    spec.output_phys_size.h as u32,
                    spec.scale,
                )
            });

        let gpu_res = active.with_primary_renderer(|renderer| {
            let elements = build_screencopy_elements(
                renderer,
                state,
                output,
                &ws_id,
                viewport,
                &base_items,
                &base_lease,
                special_overlay
                    .as_ref()
                    .map(|(l, items)| (l, items.as_slice())),
                spec.overlay_cursor
                    .then_some((&mut cursor_manager, cursor_pos)),
                agent_cursor_snap.as_ref(),
                now_ms,
            );
            let slot_name = format!("__screencopy_{}", output.name());
            let slot_key = WorkspaceId::from(slot_name.as_str());
            let offscreen = offscreen_surfaces.entry(slot_key).or_insert_with(|| {
                LazyOffscreenSurface::new(
                    spec.output_phys_size.w,
                    spec.output_phys_size.h,
                    spec.scale,
                )
            });
            if offscreen.size() != spec.output_phys_size
                || offscreen.scale() != smithay::utils::Scale::from(spec.scale)
            {
                *offscreen = LazyOffscreenSurface::new(
                    spec.output_phys_size.w,
                    spec.output_phys_size.h,
                    spec.scale,
                );
            }
            offscreen
                .capture_rgba8_pixels(renderer, &elements)
                .map(|(_, _, pixels)| pixels)
                .map_err(|err| format!("{err:#}"))
        });

        drop(base_items);
        drop(special_overlay);
        state.cursor_manager = cursor_manager;
        gpu_res.unwrap_or_else(|| Err("no primary GLES renderer available".to_owned()))
    } else {
        Err("no active backend".to_owned())
    };

    state.backend = backend;
    state.offscreen_surfaces = offscreen_surfaces;
    result
}

fn copy_to_dmabuf(
    state: &mut KomaState,
    output: &Output,
    dmabuf: &Dmabuf,
    spec: &ScreencopyFrameSpec,
) -> Result<(), String> {
    let mut dmabuf_target = dmabuf.clone();
    let mut backend = state.backend.take();
    let now_ms = state.elapsed_ms();
    let ws_id = output_workspace_id(state, output);

    let res = if let Some(active) = backend.as_mut() {
        let cursor_pos = state.pointer_location - output.current_location().to_f64();
        let mut cursor_manager = std::mem::take(&mut state.cursor_manager);
        let agent_cursor_snap = spec
            .overlay_cursor
            .then(|| state.step_agent_cursor_for_output(&output.name(), &ws_id))
            .flatten();
        let base_items = state.collect_workspace_scene_items(&ws_id, true);
        let special_overlay =
            if state.focused_output_name().as_deref() == Some(output.name().as_str()) {
                state.active_special_ws_id.as_ref().map(|sp_id| {
                    let sp_lease = state
                        .workspaces
                        .get(sp_id)
                        .map(|ws| ws.lease.state().clone())
                        .unwrap_or(WorkspaceLeaseState::Unheld);
                    (sp_lease, state.collect_workspace_scene_items(sp_id, true))
                })
            } else {
                None
            };
        let base_lease = state
            .workspaces
            .get(&ws_id)
            .map(|ws| ws.lease.state().clone())
            .unwrap_or(WorkspaceLeaseState::Unheld);
        let viewport = state
            .workspaces
            .get(&ws_id)
            .map(|ws| ws.viewport)
            .unwrap_or_else(|| {
                LogicalViewport::new(
                    spec.output_phys_size.w as u32,
                    spec.output_phys_size.h as u32,
                    spec.scale,
                )
            });

        let gpu_res = active.with_primary_renderer(|renderer| {
            let elements = build_screencopy_elements(
                renderer,
                state,
                output,
                &ws_id,
                viewport,
                &base_items,
                &base_lease,
                special_overlay
                    .as_ref()
                    .map(|(l, items)| (l, items.as_slice())),
                spec.overlay_cursor
                    .then_some((&mut cursor_manager, cursor_pos)),
                agent_cursor_snap.as_ref(),
                now_ms,
            );
            let shifted: Vec<RelocateRenderElement<KomaRenderElement<GlesRenderer>>> = elements
                .into_iter()
                .map(|elem| {
                    RelocateRenderElement::from_element(
                        elem,
                        (-spec.phys_origin.x, -spec.phys_origin.y),
                        Relocate::Relative,
                    )
                })
                .collect();
            let mut target = renderer
                .bind(&mut dmabuf_target)
                .map_err(|err| format!("dmabuf bind failed: {err}"))?;
            let mut tracker =
                OutputDamageTracker::new(spec.phys_size, spec.scale, Transform::Normal);
            let _ = tracker
                .render_output(renderer, &mut target, 0, &shifted, SCREENCOPY_BG_CLEAR)
                .map_err(|err| format!("dmabuf render_output failed: {err}"))?;
            renderer
                .wait(&smithay::backend::renderer::sync::SyncPoint::signaled())
                .map_err(|err| format!("renderer sync failed: {err}"))?;
            Ok(())
        });

        drop(base_items);
        drop(special_overlay);
        state.cursor_manager = cursor_manager;
        gpu_res.unwrap_or_else(|| Err("no primary GLES renderer".to_owned()))
    } else {
        Err("no active backend".to_owned())
    };

    state.backend = backend;
    res
}

fn output_workspace_id(state: &KomaState, output: &Output) -> WorkspaceId {
    state.active_workspace_for_output(&output.name())
}

#[allow(clippy::too_many_arguments)]
fn build_screencopy_elements<'a>(
    renderer: &mut GlesRenderer,
    state: &'a KomaState,
    output: &Output,
    ws_id: &WorkspaceId,
    viewport: LogicalViewport,
    base_items: &[koma_render::frame_composer::WindowSceneItem<'a>],
    base_lease: &WorkspaceLeaseState,
    special_overlay: Option<(
        &WorkspaceLeaseState,
        &[koma_render::frame_composer::WindowSceneItem<'a>],
    )>,
    cursor: Option<(&mut crate::cursor::CursorManager, Point<f64, Logical>)>,
    agent_cursor: Option<&AgentCursorFrameSnapshot>,
    now_ms: u64,
) -> Vec<KomaRenderElement<GlesRenderer>> {
    let output_scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());
    let layer_tiers = state.output_layer_tiers(ws_id);
    let mut elements = Vec::new();

    // Front-to-back order: human cursor -> agent cursor/trail -> overlay/top layers -> IME popups -> special overlay -> workspace -> bottom/background layers.
    append_layer_tier_elements(
        renderer,
        output,
        output_scale,
        layer_tiers.above_toplevels,
        &mut elements,
    );
    append_ime_popup_elements(renderer, state, output, output_scale, &mut elements);

    if let Some((sp_lease, sp_items)) = special_overlay {
        let sp_elems = assemble_workspace_elements(
            renderer,
            viewport,
            output_scale,
            sp_lease,
            RenderTargetKind::ScreencastPreview,
            OverlayKind::None,
            None,
            None,
            sp_items,
        );
        elements.extend(sp_elems);
    }

    if let Some(ws) = state.workspaces.get(ws_id)
        && ws.layout_mode == LayoutMode::Scrolling
        && ws.presented_window_id().is_none()
    {
        let cam_x = ws.scrolling.peek_camera_x(now_ms);
        let tab_segs = ws.scrolling.tab_indicator_segments_at(viewport, cam_x);
        append_tab_indicator_elements(renderer, output_scale, &tab_segs, &mut elements);
    }

    let ws_elems = assemble_workspace_elements(
        renderer,
        viewport,
        output_scale,
        base_lease,
        RenderTargetKind::ScreencastPreview,
        OverlayKind::None,
        None,
        None,
        base_items,
    );
    elements.extend(ws_elems);
    // A screencast mirrors the physical output, so captured closing frames must appear here too.
    let mut closing_items = state.closing_window_scene_items(ws_id);
    // Special-overlay items are assembled above; keep their closing frames in the same layer.
    if let Some(sp_id) = state.active_special_ws_id.as_ref()
        && sp_id != ws_id
    {
        closing_items.extend(state.closing_window_scene_items(sp_id));
    }
    append_closing_window_elements(renderer, &closing_items, &mut elements);

    append_layer_tier_elements(
        renderer,
        output,
        output_scale,
        layer_tiers.below_toplevels,
        &mut elements,
    );

    if state.config_error_banner.is_some() {
        prepend_config_error_banner(viewport, &mut elements);
    }
    if state.keybind_overlay_is_open() {
        prepend_keybind_overlay_elements(
            renderer,
            state,
            &output.name(),
            viewport,
            output_scale,
            &mut elements,
        );
    }

    prepend_agent_cursor_elements(
        renderer,
        RenderTargetKind::ScreencastPreview,
        output_scale,
        agent_cursor,
        &mut elements,
    );
    if let Some((cursor_manager, cursor_pos)) = cursor {
        let cursor_elems =
            cursor_manager.render_elements(renderer, cursor_pos, output_scale, now_ms);
        elements.splice(0..0, cursor_elems);
    }

    elements
}

#[allow(unsafe_code)]
fn write_pixels_to_shm(
    ptr: *mut u8,
    pool_len: usize,
    data: &shm::BufferData,
    full_output_rgba: Option<&[u8]>,
    spec: &ScreencopyFrameSpec,
) {
    let dst_w = data.width as usize;
    let dst_h = data.height as usize;
    let dst_stride = data.stride as usize;
    let base_offset = data.offset as usize;
    let required_end = base_offset + dst_h * dst_stride;
    assert!(required_end <= pool_len, "validated SHM bounds exceeded");

    // SAFETY: `with_buffer_contents_mut` protects `ptr..ptr+pool_len` with a SIGBUS guard
    // and we verified `base_offset + dst_h * dst_stride <= pool_len` above.
    let dst_slice = unsafe { std::slice::from_raw_parts_mut(ptr, pool_len) };
    let out_w = spec.output_phys_size.w.max(0) as usize;
    let rx = spec.phys_origin.x.max(0) as usize;
    let ry = spec.phys_origin.y.max(0) as usize;

    for row in 0..dst_h {
        let dst_row = &mut dst_slice[base_offset + row * dst_stride..][..dst_w * 4];
        match full_output_rgba {
            Some(src_rgba) if (ry + row) * out_w * 4 + (rx + dst_w) * 4 <= src_rgba.len() => {
                let src_start = ((ry + row) * out_w + rx) * 4;
                let src_row = &src_rgba[src_start..src_start + dst_w * 4];
                convert_rgba8_row(src_row, dst_row, data.format);
            }
            _ => {
                for px in dst_row.as_chunks_mut::<4>().0 {
                    convert_rgba8_pixel(SCREENCOPY_BG_RGBA, px, data.format);
                }
            }
        }
    }
}

fn convert_rgba8_row(src_rgba: &[u8], dst: &mut [u8], format: wl_shm::Format) {
    for (src_px, dst_px) in src_rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(dst.as_chunks_mut::<4>().0)
    {
        convert_rgba8_pixel(*src_px, dst_px, format);
    }
}

#[inline]
fn convert_rgba8_pixel(rgba: [u8; 4], dst: &mut [u8], format: wl_shm::Format) {
    let [r, g, b, a] = rgba;
    match format {
        wl_shm::Format::Argb8888 => dst.copy_from_slice(&[b, g, r, a]),
        wl_shm::Format::Xrgb8888 => dst.copy_from_slice(&[b, g, r, 0xff]),
        wl_shm::Format::Abgr8888 => dst.copy_from_slice(&[r, g, b, a]),
        wl_shm::Format::Xbgr8888 => dst.copy_from_slice(&[r, g, b, 0xff]),
        _ => dst.copy_from_slice(&[b, g, r, a]),
    }
}
