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
        append_tab_indicator_elements, assemble_workspace_elements, prepend_config_error_banner,
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
        renderer::{
            ImportDma as _, ImportEgl as _, damage::OutputDamageTracker, gles::GlesRenderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    desktop::{layer_map_for_output, utils::send_frames_surface_tree},
    input::{keyboard::KeyboardSource, pointer::MotionEvent},
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::winit::{
        dpi::LogicalSize, platform::wayland::WindowAttributesWayland, window::WindowAttributes,
    },
    utils::{Point, SERIAL_COUNTER},
    wayland::{
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal},
        shell::wlr_layer::Layer,
    },
};
use tracing::{debug, info, trace, warn};

use crate::state::{ActiveBackend, KomaState};

pub struct WinitBackendState {
    pub output: Output,
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    pub damage_tracker: OutputDamageTracker,
    pub render_node: Option<DrmNode>,
    pub dmabuf_global: Option<DmabufGlobal>,
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

        let damage_tracker = OutputDamageTracker::from_output(&output);

        let logical_w = (f64::from(win_size.w) / scale_factor).round().max(64.0) as u32;
        let logical_h = (f64::from(win_size.h) / scale_factor).round().max(64.0) as u32;
        let initial_viewport = LogicalViewport {
            width: logical_w,
            height: logical_h,
            scale: scale_factor,
        };

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
            },
            initial_viewport,
        ))
    }

    /// Binds EGL/DMA-BUF globals onto `DisplayHandle` and advertises the `wl_output` global
    /// (`niri/src/backend/winit.rs` `Winit::init` & `create_dmabuf_global`).
    pub fn bind_wayland_globals(&mut self, state: &mut KomaState) {
        let renderer = self.backend.renderer();
        if let Err(err) = renderer.bind_wl_display(&state.display_handle) {
            trace!("legacy wl_drm EGL bind skipped (expected on modern Mesa): {err}");
        }

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
    }

    pub fn request_redraw(&self) {
        self.backend.window().request_redraw();
    }
}

/// Executes a complete single-frame render pass onto the Winit EGL swapchain (`Frame 1` & live updates).
pub fn render_winit_frame(state: &mut KomaState) -> anyhow::Result<()> {
    state.redraw_queued = false;
    state.tick_frame_animations_and_edge_scroll();

    let Some(ws) = state.workspaces.get(&state.active_ws_id) else {
        return Ok(());
    };
    let viewport = ws.viewport;
    let lease_state = ws.lease.state().clone();

    let Some(ActiveBackend::Winit(mut winit_state)) = state.backend.take() else {
        return Ok(());
    };
    let output_scale =
        smithay::utils::Scale::from(winit_state.output.current_scale().fractional_scale());

    // Build front-to-back scene items across optional `special:<name>` Scratchpad + `active_ws_id`
    let (base_scene_items, special_overlay) = state.collect_physical_scene_items();
    let blur_cfg = state.lua_sandbox.config_snapshot().appearance.blur;
    let any_window_needs_blur = blur_cfg.enabled
        && base_scene_items
            .iter()
            .chain(special_overlay.iter().flat_map(|(_, v)| v.iter()))
            .any(|it| it.wants_blur && (blur_cfg.ignore_opacity || it.opacity < 0.999));

    // 1. Assemble front-to-back render elements & composite damaged regions into Winit framebuffer
    let render_res = {
        let mut elements: Vec<koma_render::elements::KomaRenderElement<GlesRenderer>> = Vec::new();
        let mut bg_layer_elements: Vec<koma_render::elements::KomaRenderElement<GlesRenderer>> =
            Vec::new();
        {
            let map = layer_map_for_output(&winit_state.output);
            for layer_tier in [Layer::Overlay, Layer::Top] {
                for layer in map.layers_on(layer_tier) {
                    if let Some(geo) = map.layer_geometry(layer) {
                        let loc_phys = geo.loc.to_f64().to_physical_precise_round(output_scale);
                        let l_elems: Vec<smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<GlesRenderer>> =
                            smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                                winit_state.backend.renderer(),
                                layer.wl_surface(),
                                loc_phys,
                                output_scale,
                                1.0,
                                smithay::backend::renderer::element::Kind::Unspecified,
                            );
                        elements.extend(
                            l_elems
                                .into_iter()
                                .map(koma_render::elements::KomaRenderElement::from),
                        );
                    }
                }
            }
            for layer_tier in [Layer::Bottom, Layer::Background] {
                for layer in map.layers_on(layer_tier) {
                    if let Some(geo) = map.layer_geometry(layer) {
                        let loc_phys = geo.loc.to_f64().to_physical_precise_round(output_scale);
                        let l_elems: Vec<smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<GlesRenderer>> =
                            smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                                winit_state.backend.renderer(),
                                layer.wl_surface(),
                                loc_phys,
                                output_scale,
                                1.0,
                                smithay::backend::renderer::element::Kind::Unspecified,
                            );
                        bg_layer_elements.extend(
                            l_elems
                                .into_iter()
                                .map(koma_render::elements::KomaRenderElement::from),
                        );
                    }
                }
            }
        }

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

        if let Some(ws) = state.workspaces.get(&state.active_ws_id) {
            if ws.layout_mode == koma_model::LayoutMode::Scrolling && ws.fullscreen_window.is_none()
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
        }
        let base_drop = state.drop_preview_for_workspace(&state.active_ws_id);
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

        let (renderer, mut framebuffer) = winit_state
            .backend
            .bind()
            .map_err(|e| anyhow::anyhow!("failed binding Winit EGL framebuffer: {e:?}"))?;

        winit_state
            .damage_tracker
            .render_output(
                renderer,
                &mut framebuffer,
                0,
                &elements,
                DEFAULT_BACKDROP_CLEAR,
            )
            .context("OutputDamageTracker::render_output failed")?
    };

    // 2. Submit damaged rectangles to host compositor (`WinitGraphicsBackend::submit`)
    if let Some(damage) = render_res.damage {
        winit_state
            .backend
            .submit(Some(damage))
            .map_err(|e| anyhow::anyhow!("failed submitting Winit damage: {e:?}"))?;
    }

    // 3. Dispatch `wl_surface.frame` callbacks to ALL visible tiled, floating & Scratchpad windows
    let elapsed = state.elapsed_duration();
    let output_ref = winit_state.output.clone();
    for wl_surf in state.visible_physical_toplevel_surfaces() {
        send_frames_surface_tree(
            &wl_surf,
            &output_ref,
            elapsed,
            Some(Duration::ZERO),
            |_, _| Some(output_ref.clone()),
        );
    }

    let keep_animating = state.has_active_animations();
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
    if let Some(ws) = state.workspaces.get_mut(&ws_id) {
        let fx = ws.lease.human_toggle_takeover(60_000, now);
        let mut flushed_keys = Vec::new();
        if fx.flush_virtual_seat {
            if let Some(vseat) = state.vseats.get_mut(&ws.vseat_id) {
                let (keys, _) = vseat.flush_all_held();
                flushed_keys = keys;
            }
        }
        let mut cancelled_lua = false;
        if fx.cancel_lua_coroutines {
            let c1 = state.lua_scheduler.cancel_workspace_tasks(&ws_id).len();
            let c2 = state.lua_sandbox.cancel_workspace_coroutines(
                &ws_id,
                &format!("preempted by human {trigger_label} takeover"),
            );
            state.pointer_animations.retain(|a| a.ws_id != ws_id);
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
            if !matches!(state.backend, Some(ActiveBackend::Winit(_))) {
                return;
            }
            let out_geo_size = state
                .workspaces
                .get(&state.active_ws_id)
                .map(|ws| (ws.viewport.width as i32, ws.viewport.height as i32))
                .unwrap_or((1280, 800));
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
        _ => {}
    }
}

pub(crate) fn hit_test_active_deck_surface(
    state: &KomaState,
    global_pos: Point<f64, smithay::utils::Logical>,
) -> Option<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    Point<f64, smithay::utils::Logical>,
)> {
    state
        .hit_test_visible_surface(global_pos)
        .map(|(_, _, surf, origin)| (surf, origin))
}
