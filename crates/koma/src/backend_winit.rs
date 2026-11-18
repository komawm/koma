//! Nested Smithay Winit backend & first-frame `GlesRenderer` loop
//! (modeled directly on `niri/src/backend/winit.rs`).

use std::time::Duration;

use anyhow::Context as _;
use calloop::LoopHandle;
use koma_model::LogicalViewport;
use koma_proto::OverlayKind;
use koma_render::{
    RenderTargetKind,
    frame_composer::{
        DEFAULT_BACKDROP_CLEAR, FrameBlurContext, append_scratchpad_dim_scrim,
        append_tab_indicator_elements, assemble_workspace_elements, prepend_agent_cursor_elements,
        prepend_config_error_banner,
    },
};
use smithay::{
    backend::{
        drm::DrmNode,
        egl::EGLDevice,
        input::{
            AbsolutePositionEvent as _, Event as _, InputBackend, InputEvent, InputTime,
            KeyboardKeyEvent as _, PointerButtonEvent as _,
        },
        renderer::{Color32F, ImportDma as _, damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    desktop::{LayerMap, LayerSurface, layer_map_for_output, utils::send_frames_surface_tree},
    input::{
        keyboard::KeyboardSource,
        pointer::{CursorIcon, CursorImageStatus},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::winit::{
        cursor::Cursor as WinitCursor, dpi::LogicalSize,
        platform::wayland::WindowAttributesWayland, window::WindowAttributes,
    },
    utils::{IsAlive, Point, Rectangle, SERIAL_COUNTER, Transform},
    wayland::{
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal},
        shell::wlr_layer::Layer,
    },
};
use tracing::{debug, info, warn};

use crate::state::{ActiveBackend, KomaState};

/// Render transform applied to the nested output's damage tracker.
///
/// A nested Koma presents its frame through an EGL *window* surface. Smithay composes with the
/// same projection for every render target (element row `y = 0` lands in the lowest framebuffer
/// rows), which is the row order `wl_buffer` consumers expect from texture/DMA-BUF targets such
/// as KMS scanout and screencopy - but an EGL window surface hands the parent compositor a
/// vertically mirrored buffer, so the whole nested desktop is displayed upside down (bar at the
/// bottom, text mirrored, cursor at the mirrored position; komawm/koma#35).
///
/// Mirroring the compose step cancels that: the bytes the host receives are upright again, the
/// damage-tracker geometry is unchanged (`Transform::Flipped180` keeps the transformed size),
/// and input needs no remapping because the two mirrors cancel, so a host pointer row maps to
/// the same scene row (`presented_row` in the tests pins this down).
///
/// The flip is a property of the winit/EGL presentation path, not of a driver: it reproduces
/// identically with the NVIDIA EGL stack (620.70.06) and with Mesa `llvmpipe`, and a plain GL
/// client (`mpv`) drawn directly in a nested Koma is upright, so the scene composition and the
/// consumer path are not involved. The KMS backend and the screencopy path keep rendering with
/// `Transform::Normal` and stay byte-identical. niri compensates the same mirror by forcing
/// `Transform::Flipped180` on its winit output transform (`niri/src/niri.rs`).
pub(crate) const NESTED_RENDER_TRANSFORM: Transform = Transform::Flipped180;

pub struct WinitBackendState {
    pub output: Output,
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    pub damage_tracker: OutputDamageTracker,
    pub render_node: Option<DrmNode>,
    pub dmabuf_global: Option<DmabufGlobal>,
    last_host_cursor_visible: Option<bool>,
    last_host_cursor_icon: Option<CursorIcon>,
}

impl WinitBackendState {
    /// Creates the nested Winit Wayland/X11 host window, initializes `GlesRenderer`,
    /// registers the `WinitEvent` source into `calloop`, and prepares Frame 1.
    pub fn init(
        event_loop: &LoopHandle<'static, KomaState>,
    ) -> anyhow::Result<(Self, LogicalViewport)> {
        let builder = WindowAttributes::default()
            .with_surface_size(LogicalSize::new(1280.0, 800.0))
            .with_title("Koma Compositor (Nested Winit)")
            .with_platform_attributes(Box::new(
                WindowAttributesWayland::default().with_name("koma", ""),
            ));

        let (backend, winit_evt_loop) = winit::init_from_attributes::<GlesRenderer>(builder)
            .map_err(|e| anyhow::anyhow!("failed initializing Smithay Winit backend: {e:?}"))?;
        // Hide the host OS cursor inside Koma's window while rendering our own cursor element
        // at the front of the scene (clean interactive control + captured in screencasts).
        backend.window().set_cursor_visible(false);

        let win_size = backend.window_size();
        let scale_factor = backend.scale_factor();

        let output = Output::new(
            "winit".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Koma".into(),
                model: "Smithay-Winit".into(),
                serial_number: "koma-nested-0".into(),
            },
        );
        let mode = Mode {
            size: win_size,
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            None,
            Some(Scale::Fractional(scale_factor)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);

        // See `NESTED_RENDER_TRANSFORM`: the EGL window surface mirrors what we compose.
        let damage_tracker =
            OutputDamageTracker::new(win_size, scale_factor, NESTED_RENDER_TRANSFORM);

        let logical_w = (f64::from(win_size.w) / scale_factor).round().max(64.0) as u32;
        let logical_h = (f64::from(win_size.h) / scale_factor).round().max(64.0) as u32;
        let initial_viewport = LogicalViewport::new(logical_w, logical_h, scale_factor);

        event_loop
            .insert_source(winit_evt_loop, move |event, _, state| match event {
                WinitEvent::Resized { size, scale_factor } => {
                    let output_clone = match state.backend.as_mut() {
                        Some(ActiveBackend::Winit(winit_state)) => {
                            winit_state.output.change_current_state(
                                Some(Mode {
                                    size,
                                    refresh: 60_000,
                                }),
                                None,
                                Some(Scale::Fractional(scale_factor)),
                                None,
                            );
                            // The tracker no longer follows the output (it carries the nested
                            // render transform), so rebuild it for the new size/scale.
                            winit_state.damage_tracker = OutputDamageTracker::new(
                                size,
                                scale_factor,
                                NESTED_RENDER_TRANSFORM,
                            );
                            winit_state.output.clone()
                        }
                        _ => return,
                    };
                    let lw = (f64::from(size.w) / scale_factor).round().max(64.0) as u32;
                    let lh = (f64::from(size.h) / scale_factor).round().max(64.0) as u32;
                    state.handle_output_resized(&output_clone, lw, lh, scale_factor);
                }
                WinitEvent::Input(input_ev) => {
                    process_nested_input_event(state, input_ev);
                }
                WinitEvent::Focus(false) => {
                    // Release held modifiers, active pointer grabs & scroll/gesture trackers on host focus loss
                    let _ = crate::pointer_grabs::cancel_active_pointer_grab(
                        state,
                        SERIAL_COUNTER.next_serial(),
                        InputTime::now(),
                    );
                    state.keybinds.reset_scroll_trackers();
                    state.gestures.reset_active_sessions();
                    if let Some(keyboard) = state.seat.get_keyboard() {
                        keyboard.release_source(state, KeyboardSource::MAIN);
                    }
                }
                WinitEvent::Focus(true) => {
                    state.queue_redraw();
                }
                WinitEvent::Redraw => {
                    if let Err(err) = render_winit_frame(state) {
                        warn!("Winit frame render error: {err:?}");
                    }
                }
                WinitEvent::CloseRequested => {
                    info!("host window close requested; stopping Koma event loop");
                    state.loop_signal.stop();
                }
            })
            .map_err(|e| anyhow::anyhow!("failed inserting Winit source into calloop: {e}"))?;

        Ok((
            Self {
                output,
                backend,
                damage_tracker,
                render_node: None,
                dmabuf_global: None,
                last_host_cursor_visible: Some(false),
                last_host_cursor_icon: None,
            },
            initial_viewport,
        ))
    }

    /// Binds EGL/DMA-BUF globals onto `DisplayHandle` and advertises the `wl_output` global
    /// (`niri/src/backend/winit.rs` `Winit::init` & `create_dmabuf_global`).
    pub fn bind_wayland_globals(&mut self, state: &mut KomaState) {
        // `zwp_linux_dmabuf_v1` below handles modern Mesa DMA-BUF without legacy `wl_drm` (`use_system_lib`).
        let renderer = self.backend.renderer();

        self.render_node = EGLDevice::device_for_display(renderer.egl_context().display())
            .ok()
            .and_then(|dev| dev.try_get_render_node().ok().flatten());

        if let Some(node) = self.render_node {
            debug!("bound nested Winit EGL render node: {node:?}");
        }

        let formats = renderer.dmabuf_formats();
        let dmabuf_global = match self.render_node.and_then(|node| {
            DmabufFeedbackBuilder::new(node.dev_id(), formats.clone())
                .build()
                .ok()
        }) {
            Some(feedback) => state
                .dmabuf_state
                .create_global_with_default_feedback::<KomaState>(&state.display_handle, &feedback),
            None => state
                .dmabuf_state
                .create_global::<KomaState>(&state.display_handle, formats),
        };
        self.dmabuf_global = Some(dmabuf_global);

        self.output
            .create_global::<KomaState>(&state.display_handle);
        let mon_cfg = state
            .lua_sandbox
            .config_snapshot()
            .find_monitor(&self.output.name())
            .cloned();
        let safe_area = mon_cfg.as_ref().map(|c| c.safe_area).unwrap_or_default();
        let explicit_pos = mon_cfg.as_ref().and_then(|c| c.resolved_position());
        state.register_or_update_output(&self.output, explicit_pos, safe_area, true);
    }

    pub fn request_redraw(&self) {
        self.backend.window().request_redraw();
    }

    /// Keeps host Winit cursor hidden inside Koma's framebuffer while syncing the host icon shape.
    pub fn sync_host_cursor(&mut self, status: &CursorImageStatus) {
        if self.last_host_cursor_visible != Some(false) {
            self.backend.window().set_cursor_visible(false);
            self.last_host_cursor_visible = Some(false);
        }
        if let CursorImageStatus::Named(icon) = status
            && self.last_host_cursor_icon != Some(*icon)
        {
            self.backend.window().set_cursor(WinitCursor::Icon(*icon));
            self.last_host_cursor_icon = Some(*icon);
        }
    }
}

/// Layer tiers drawn above workspace toplevels (front-to-back), shared by every backend so the
/// three frame assemblers cannot drift.
pub(crate) const FOREGROUND_LAYER_TIERS: [Layer; 2] = [Layer::Overlay, Layer::Top];

/// Layer tiers drawn below workspace toplevels, immediately above the clear colour
/// (`assemble_workspace_elements` documents the front-to-back list). `Layer::Bottom` precedes
/// `Layer::Background` so a `Bottom` surface paints over a `Background` wallpaper.
pub(crate) const BACKGROUND_LAYER_TIERS: [Layer; 2] = [Layer::Bottom, Layer::Background];

/// Resolves the top-left of a layer surface in output-logical coordinates.
///
/// A missing cached geometry (for example because `LayerMap::arrange` ran before the output had a
/// mode) must not hide a buffer-backed surface: the output origin is the only meaningful fallback
/// for the full-output `Background`/`Bottom` surfaces that wallpapers and backdrops use
/// (komawm/koma#31).
pub(crate) fn layer_entry_location(
    geometry: Option<Rectangle<i32, smithay::utils::Logical>>,
) -> Point<i32, smithay::utils::Logical> {
    geometry.map_or_else(Point::default, |geo| geo.loc)
}

/// Selects the layer surfaces of `tiers` that contribute pixels to this frame, front-to-back.
///
/// A layer is skipped only when it cannot contribute anything: it is dead, or no surface in its
/// tree has committed a buffer yet (the first frame of a layer client is buffer-less by protocol).
/// Buffer-backed layers are always returned, so a `Background` wallpaper can never be dropped from
/// the element list for lack of a cached geometry (komawm/koma#31).
///
/// `tiers` is a slice so the fullscreen policy can suppress the foreground tiers entirely
/// (`KomaState::output_layer_tiers`) while every tier literal still lives in the two constants above.
pub(crate) fn layer_tier_entries(
    map: &LayerMap,
    tiers: &[Layer],
) -> Vec<(LayerSurface, Point<i32, smithay::utils::Logical>)> {
    let mut entries = Vec::new();
    for tier in tiers {
        for layer in map.layers_on(*tier).rev() {
            if !layer.alive() {
                continue;
            }
            let bbox = layer.bbox();
            if bbox.size.w <= 0 || bbox.size.h <= 0 {
                continue;
            }
            let location = layer_entry_location(map.layer_geometry(layer));
            entries.push((layer.clone(), location));
        }
    }
    entries
}

pub(crate) fn append_layer_tier_elements<R>(
    renderer: &mut R,
    output: &Output,
    output_scale: smithay::utils::Scale<f64>,
    tiers: &[Layer],
    elements: &mut Vec<koma_render::elements::KomaRenderElement<R>>,
) where
    R: smithay::backend::renderer::Renderer + smithay::backend::renderer::ImportAll,
    R::TextureId: Clone + 'static,
{
    use smithay::backend::renderer::element::{
        AsRenderElements, surface::WaylandSurfaceRenderElement,
    };

    let map = layer_map_for_output(output);
    for (layer, location) in layer_tier_entries(&map, tiers) {
        let loc_phys = location.to_f64().to_physical_precise_round(output_scale);
        let l_elems: Vec<WaylandSurfaceRenderElement<R>> =
            AsRenderElements::render_elements(&layer, renderer, loc_phys, output_scale, 1.0);
        elements.extend(
            l_elems
                .into_iter()
                .map(koma_render::elements::KomaRenderElement::from),
        );
    }
}

pub(crate) fn append_ime_popup_elements<R>(
    renderer: &mut R,
    state: &KomaState,
    output: &Output,
    output_scale: smithay::utils::Scale<f64>,
    elements: &mut Vec<koma_render::elements::KomaRenderElement<R>>,
) where
    R: smithay::backend::renderer::Renderer + smithay::backend::renderer::ImportAll,
    R::TextureId: Clone + 'static,
{
    use smithay::backend::renderer::element::{
        Kind,
        surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
    };

    for (surface, origin) in state.ime_popup_render_targets(output) {
        let popup_elems: Vec<WaylandSurfaceRenderElement<R>> = render_elements_from_surface_tree(
            renderer,
            &surface,
            origin.to_physical_precise_round(output_scale),
            output_scale,
            1.0,
            Kind::Unspecified,
        );
        elements.extend(
            popup_elems
                .into_iter()
                .map(koma_render::elements::KomaRenderElement::from),
        );
    }
}

pub(crate) fn assemble_locked_frame_elements<R>(
    renderer: &mut R,
    state: &KomaState,
    output: &Output,
    output_scale: smithay::utils::Scale<f64>,
    cursor_elems: Vec<koma_render::elements::KomaRenderElement<R>>,
) -> Vec<koma_render::elements::KomaRenderElement<R>>
where
    R: smithay::backend::renderer::Renderer + smithay::backend::renderer::ImportAll,
    R::TextureId: Clone + 'static,
{
    use smithay::backend::renderer::element::{
        Kind,
        surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
    };

    let (out_w, out_h, _) = crate::state::output_logical_viewport(output);
    let mut elements = cursor_elems;
    if let Some(lock_surface) = state.session_lock.active_lock_surface_for_output(output)
        && crate::state::is_surface_mapped(lock_surface.wl_surface())
    {
        let lock_elems: Vec<WaylandSurfaceRenderElement<R>> = render_elements_from_surface_tree(
            renderer,
            lock_surface.wl_surface(),
            (0, 0),
            output_scale,
            1.0,
            Kind::Unspecified,
        );
        elements.extend(
            lock_elems
                .into_iter()
                .map(koma_render::elements::KomaRenderElement::from),
        );
    }
    let blackout_rect =
        smithay::utils::Rectangle::from_size((f64::from(out_w), f64::from(out_h)).into());
    elements.push(koma_render::elements::KomaRenderElement::from(
        koma_render::elements::KomaSolidColorElement::oneshot(blackout_rect, [0.0, 0.0, 0.0, 1.0]),
    ));
    elements
}

pub(crate) fn dispatch_output_frame_callbacks_filtered(
    state: &KomaState,
    output: &Output,
    throttle: Option<Duration>,
) {
    use smithay::{
        desktop::{
            PopupManager,
            utils::{OutputPresentationFeedback, take_presentation_feedback_surface_tree},
        },
        reexports::{
            wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
            wayland_server::protocol::wl_surface::WlSurface,
        },
        utils::Monotonic,
        wayland::{compositor::SurfaceData, presentation::Refresh},
    };

    let elapsed = state.elapsed_duration();
    let mut output_feedback = OutputPresentationFeedback::new(output);
    let refresh = output
        .current_mode()
        .filter(|m| m.refresh > 0)
        .map(|m| Refresh::fixed(Duration::from_secs_f64(1_000f64 / f64::from(m.refresh))))
        .unwrap_or(Refresh::Unknown);
    let seq = state.next_presentation_seq();
    let scanout = |_: &WlSurface, _: &SurfaceData| Some(output.clone());
    let flags = |_: &WlSurface, _: &SurfaceData| wp_presentation_feedback::Kind::Vsync;

    if state.is_locked_or_abandoned() {
        if let Some(lock_surf) = state.session_lock.active_lock_surface_for_output(output) {
            send_frames_surface_tree(lock_surf.wl_surface(), output, elapsed, throttle, scanout);
            take_presentation_feedback_surface_tree(
                lock_surf.wl_surface(),
                &mut output_feedback,
                scanout,
                flags,
            );
        }
        output_feedback.presented::<_, Monotonic>(
            state.clock.now(),
            refresh,
            seq,
            wp_presentation_feedback::Kind::Vsync,
        );
        return;
    }

    for wl_surf in state.visible_physical_toplevel_surfaces_for_output(&output.name()) {
        send_frames_surface_tree(&wl_surf, output, elapsed, throttle, scanout);
        take_presentation_feedback_surface_tree(&wl_surf, &mut output_feedback, scanout, flags);
        for (popup, _) in PopupManager::popups_for_surface(&wl_surf) {
            let popup_surf = popup.wl_surface();
            send_frames_surface_tree(popup_surf, output, elapsed, throttle, scanout);
            take_presentation_feedback_surface_tree(
                popup_surf,
                &mut output_feedback,
                scanout,
                flags,
            );
        }
    }
    let map = layer_map_for_output(output);
    for layer in map.layers() {
        layer.send_frame(output, elapsed, throttle, scanout);
        layer.take_presentation_feedback(&mut output_feedback, scanout, flags);
    }
    for (ime_surf, _) in state.ime_popup_render_targets(output) {
        send_frames_surface_tree(&ime_surf, output, elapsed, throttle, scanout);
        take_presentation_feedback_surface_tree(&ime_surf, &mut output_feedback, scanout, flags);
    }

    output_feedback.presented::<_, Monotonic>(
        state.clock.now(),
        refresh,
        seq,
        wp_presentation_feedback::Kind::Vsync,
    );
}

/// Executes a complete single-frame render pass onto the Winit EGL swapchain (`Frame 1` & live updates).
pub fn render_winit_frame(state: &mut KomaState) -> anyhow::Result<()> {
    state.redraw_queued = false;
    state.reconcile_session_lock();
    let locked = state.is_locked_or_abandoned();
    if !locked {
        state.tick_frame_animations_and_edge_scroll();
    }

    let Some(ActiveBackend::Winit(mut winit_state)) = state.backend.take() else {
        return Ok(());
    };
    let out_name = winit_state.output.name();
    let out_ws_id = state.active_workspace_for_output(&out_name);
    let Some(ws) = state.workspaces.get(&out_ws_id) else {
        state.backend = Some(ActiveBackend::Winit(winit_state));
        return Ok(());
    };
    let viewport = ws.viewport;
    let lease_state = ws.lease.state().clone();
    let output_scale =
        smithay::utils::Scale::from(winit_state.output.current_scale().fractional_scale());

    let pointer_loc = state.pointer_location - winit_state.output.current_location().to_f64();
    let now_ms = state.elapsed_ms();
    let cursor_elems = state.cursor_manager.render_elements(
        winit_state.backend.renderer(),
        pointer_loc,
        output_scale,
        now_ms,
    );
    if let Some(delay_ms) = state
        .cursor_manager
        .next_frame_delay_ms(output_scale.x, now_ms)
    {
        state.schedule_cursor_animation_redraw(delay_ms);
    }
    winit_state.sync_host_cursor(state.cursor_manager.status());
    let agent_cursor_snap = (!locked)
        .then(|| state.step_agent_cursor_for_output(&out_name, &out_ws_id))
        .flatten();

    // 1. Assemble front-to-back render elements & composite damaged regions into Winit framebuffer
    let render_res = {
        let (elements, clear_color) = if locked {
            (
                assemble_locked_frame_elements(
                    winit_state.backend.renderer(),
                    state,
                    &winit_state.output,
                    output_scale,
                    cursor_elems,
                ),
                Color32F::new(0.0, 0.0, 0.0, 1.0),
            )
        } else {
            let (base_scene_items, special_overlay) = state.collect_output_scene_items(&out_name);
            let blur_cfg = state.lua_sandbox.config_snapshot().appearance.blur;
            let any_window_needs_blur = blur_cfg.enabled
                && base_scene_items
                    .iter()
                    .chain(special_overlay.iter().flat_map(|(_, v)| v.iter()))
                    .any(|it| it.wants_blur && (blur_cfg.ignore_opacity || it.opacity < 0.999));

            let mut elements: Vec<koma_render::elements::KomaRenderElement<GlesRenderer>> =
                Vec::new();
            let mut bg_layer_elements: Vec<koma_render::elements::KomaRenderElement<GlesRenderer>> =
                Vec::new();
            append_ime_popup_elements(
                winit_state.backend.renderer(),
                state,
                &winit_state.output,
                output_scale,
                &mut elements,
            );
            let layer_tiers = state.output_layer_tiers(&out_ws_id);
            append_layer_tier_elements(
                winit_state.backend.renderer(),
                &winit_state.output,
                output_scale,
                layer_tiers.above_toplevels,
                &mut elements,
            );
            append_layer_tier_elements(
                winit_state.backend.renderer(),
                &winit_state.output,
                output_scale,
                layer_tiers.below_toplevels,
                &mut bg_layer_elements,
            );

            let frame_blur_ctx = if any_window_needs_blur {
                let phys_sz = winit_state.backend.window_size();
                state
                    .backdrop_blur_cache
                    .borrow_mut()
                    .update_and_blur(
                        winit_state.backend.renderer(),
                        phys_sz,
                        output_scale,
                        DEFAULT_BACKDROP_CLEAR,
                        &bg_layer_elements,
                        blur_cfg,
                    )
                    .map(|texture| FrameBlurContext {
                        texture,
                        config: blur_cfg,
                    })
            } else {
                None
            };

            if let Some((sp_lease, sp_items)) = &special_overlay {
                let sp_drop = state
                    .active_special_ws_id
                    .as_ref()
                    .and_then(|sp_id| state.drop_preview_for_workspace(sp_id));
                let sp_elems = assemble_workspace_elements(
                    winit_state.backend.renderer(),
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
                append_tab_indicator_elements(
                    winit_state.backend.renderer(),
                    output_scale,
                    &tab_segs,
                    &mut elements,
                );
            }
            let base_drop = state.drop_preview_for_workspace(&out_ws_id);
            let ws_elements = assemble_workspace_elements(
                winit_state.backend.renderer(),
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
            elements.extend(bg_layer_elements);
            if state.config_error_banner.is_some() {
                prepend_config_error_banner(viewport, &mut elements);
            }
            prepend_agent_cursor_elements(
                winit_state.backend.renderer(),
                RenderTargetKind::PhysicalOutput,
                output_scale,
                agent_cursor_snap.as_ref(),
                &mut elements,
            );
            elements.splice(0..0, cursor_elems);
            (elements, DEFAULT_BACKDROP_CLEAR)
        };

        let (renderer, mut framebuffer) = winit_state
            .backend
            .bind()
            .map_err(|e| anyhow::anyhow!("failed binding Winit EGL framebuffer: {e:?}"))?;

        winit_state
            .damage_tracker
            .render_output(renderer, &mut framebuffer, 0, &elements, clear_color)
            .context("OutputDamageTracker::render_output failed")?
    };

    // 2. Submit damaged rectangles to host compositor (`WinitGraphicsBackend::submit`)
    if let Some(damage) = render_res.damage {
        winit_state
            .backend
            .submit(Some(damage))
            .map_err(|e| anyhow::anyhow!("failed submitting Winit damage: {e:?}"))?;
    }

    // 3. Dispatch `wl_surface.frame` callbacks strictly respecting session lock masking
    dispatch_output_frame_callbacks_filtered(state, &winit_state.output, Some(Duration::ZERO));

    let keep_animating = !locked && state.has_active_animations();
    if keep_animating {
        winit_state.request_redraw();
    }
    state.backend = Some(ActiveBackend::Winit(winit_state));
    Ok(())
}

/// Shared Ring 0 Preempt Takeover handler (`Alt+G` nested Winit / `Super+G` bare-metal TTY):
/// bumps `Epoch`, flushes held `VirtualSeat` keys/buttons in < 1 ms, cancels active Phodopus
/// macro coroutines, and broadcasts `WorkspaceEvent::Preempted` / `Resumed`.
pub(crate) fn toggle_ring0_preempt_takeover(state: &mut KomaState, trigger_label: &str) {
    let now = state.elapsed_ms();
    let ws_id = state.active_ws_id.clone();
    let mut maybe_broadcast = None;
    let fx_opt = state
        .workspaces
        .get_mut(&ws_id)
        .map(|ws| ws.lease.human_toggle_takeover(60_000, now));
    if let Some(fx) = fx_opt {
        let flushed_keys = if fx.flush_virtual_seat {
            crate::macro_runtime::release_all_vseat_held_inputs(state, &ws_id)
        } else {
            Vec::new()
        };
        let mut cancelled_lua = false;
        if fx.cancel_lua_coroutines {
            let c1 = state.lua_scheduler.cancel_workspace_tasks(&ws_id).len();
            let c2 = state.lua_sandbox.cancel_workspace_coroutines(
                &ws_id,
                &format!("preempted by human {trigger_label} takeover"),
            );
            state.pointer_animations.retain(|a| a.ws_id != ws_id);
            if let Some(ac) = state.agent_cursors.get_mut(&ws_id) {
                ac.visual.on_preempted(now);
            }
            state.queue_redraw();
            cancelled_lua = (c1 + c2) > 0;
        }
        info!(
            trigger = %trigger_label,
            epoch = %fx.new_epoch,
            human_takeover = fx.entered_human_takeover,
            "Ring 0 Preempt Takeover toggled; border & epoch updated"
        );
        let ev = if fx.entered_human_takeover {
            koma_proto::WorkspaceEvent::Preempted {
                reason: "human_hotkey_takeover".to_owned(),
                flushed_keys,
                cancelled_lua_tx: cancelled_lua,
            }
        } else {
            koma_proto::WorkspaceEvent::Resumed {
                new_epoch: fx.new_epoch,
            }
        };
        maybe_broadcast = Some((fx.new_epoch, ev));
    }
    if let Some((ep, ev)) = maybe_broadcast {
        state.broadcast_workspace_event(koma_proto::EventTopic::WorkspaceLease, &ws_id, ep, ev);
    }
    state.reconcile_keyboard_shortcuts_inhibitors();
    state.queue_redraw();
}

/// Processes nested keyboard, pointer scroll (`Mod+WheelScroll*` / `TouchpadScroll*`), and
/// multi-finger touchpad gestures (`GestureSwipe*` / `GesturePinch*` / `GestureHold*`).
fn process_nested_input_event<B>(state: &mut KomaState, event: InputEvent<B>)
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
                false,
            );
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let Some(ActiveBackend::Winit(winit_state)) = state.backend.as_ref() else {
                return;
            };
            let winit_out = winit_state.output.clone();
            let rect = state.output_logical_rect(&winit_out);
            // No output-transform correction here: the compose mirror and the EGL window
            // surface mirror cancel, so host rows map 1:1 onto scene rows (`presented_row`).
            let local_pos: Point<f64, _> = event.position_transformed(rect.size);
            state.handle_pointer_absolute_motion(local_pos + rect.loc.to_f64(), event.time());
        }
        InputEvent::PointerButton { event } => {
            crate::keybind_dispatcher::process_pointer_button_event(
                state,
                event.button_code(),
                event.state(),
                event.time(),
                false,
            );
        }
        InputEvent::PointerAxis { event } => {
            crate::keybind_dispatcher::process_pointer_axis_event::<B>(state, &event, false);
        }
        InputEvent::GestureSwipeBegin { event } => {
            crate::gesture_dispatcher::process_gesture_swipe_begin::<B>(state, &event);
        }
        InputEvent::GestureSwipeUpdate { event } => {
            crate::gesture_dispatcher::process_gesture_swipe_update::<B>(state, &event, false);
        }
        InputEvent::GestureSwipeEnd { event } => {
            crate::gesture_dispatcher::process_gesture_swipe_end::<B>(state, &event);
        }
        InputEvent::GesturePinchBegin { event } => {
            crate::gesture_dispatcher::process_gesture_pinch_begin::<B>(state, &event);
        }
        InputEvent::GesturePinchUpdate { event } => {
            crate::gesture_dispatcher::process_gesture_pinch_update::<B>(state, &event, false);
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
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Logical, Size};

    use super::*;

    /// Row at which the parent compositor displays a scene row of the nested output.
    ///
    /// Models the two mirrors of the nested presentation path: the damage tracker compose step
    /// (`NESTED_RENDER_TRANSFORM`) and the parent compositor's treatment of an EGL window-surface
    /// buffer, which arrives y-mirrored.
    pub(crate) fn presented_row(scene_y: f64, height: f64, render_transform: Transform) -> f64 {
        let area: Size<f64, Logical> = (height, height).into();
        let composed = render_transform.transform_point_in(Point::from((0.0, scene_y)), &area);
        // EGL window surfaces reach the host mirrored (`komawm/koma#35`): flip the composed row.
        area.h - composed.y
    }

    const HEIGHT: f64 = 480.0;

    /// What the nested desktop composes must be what the host displays: every scene row has to
    /// come back as the same host row. Regression guard for komawm/koma#35, where the nested
    /// output was shown upside down.
    #[test]
    fn nested_presentation_is_wysiwyg() {
        for scene_y in [0.0, 1.0, HEIGHT / 2.0, 47.25, HEIGHT - 1.0, HEIGHT] {
            let host_y = presented_row(scene_y, HEIGHT, NESTED_RENDER_TRANSFORM);
            assert!(
                (host_y - scene_y).abs() < 1e-9,
                "scene row {scene_y} is displayed at host row {host_y}"
            );
        }
    }

    /// Without the compose mirror the host would draw the nested desktop mirrored: this is the
    /// exact bug from the issue, and it must keep failing so the constant above is understood as
    /// a required compensation rather than a cosmetic preference.
    #[test]
    fn unmirrored_compose_is_shown_upside_down() {
        for scene_y in [0.0, 1.0, HEIGHT / 2.0, HEIGHT - 1.0] {
            let host_y = presented_row(scene_y, HEIGHT, Transform::Normal);
            assert!(
                (host_y - (HEIGHT - scene_y)).abs() < 1e-9,
                "expected a mirror of row {scene_y}, got host row {host_y}"
            );
        }
        assert_eq!(presented_row(0.0, HEIGHT, Transform::Normal), HEIGHT);
    }
}
