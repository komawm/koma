//! Front-to-back scene graph element assembly for nested `Winit` (`GlesRenderer`),
//! bare-metal `DRM/KMS` (`TtyRenderer` / `MultiRenderer`), and `Headless` `LazyFbo`,
//! integrating SDF rounded-corner surface clipping, anti-aliased rounded borders,
//! 4-quadrant `smart_split` drop-zone previews, and multi-pass `Dual Kawase Blur` backdrops.

use anyhow::Context as _;
use koma_model::{
    AgentCursorFrameSnapshot, BlurConfig, BorderConfig, DimConfig, FocusRingConfig, LogicalRect,
    LogicalViewport, ShadowConfig, WorkspaceLeaseState, parse_hex_color,
};
use koma_proto::OverlayKind;
use smithay::{
    backend::renderer::{
        Color32F, ImportAll, ImportMem, Renderer,
        element::{
            Kind,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
        gles::{GlesRenderer, GlesTexture},
    },
    desktop::{PopupManager, Window},
    utils::{Logical, Physical, Point, Rectangle, Scale, Size},
};

use crate::{
    RenderTargetKind,
    elements::{
        KomaAgentCursorElement, KomaAgentTrailElement, KomaBlurredBackdropElement,
        KomaClippedSurfaceElement, KomaDropShadowElement, KomaRenderElement,
        KomaRoundedBorderElement, KomaSolidColorElement, to_smithay_logical_rect,
    },
    offscreen_fbo::capture_elements_to_memory,
    shaders::{KomaCompiledShaders, KomaGlesRenderer},
};

/// Default dark-slate `#111216` clear color (`LUA_API_SPEC.md` §2.1).
pub const DEFAULT_BACKDROP_CLEAR: Color32F = Color32F::new(0.067, 0.071, 0.086, 1.0);

pub const COLOR_BORDER_FOCUSED: [f32; 4] = [0.345, 0.651, 1.0, 1.0]; // #58a6ff
pub const COLOR_BORDER_INACTIVE: [f32; 4] = [0.188, 0.212, 0.239, 1.0]; // #30363d
pub const COLOR_BORDER_AGENT_LEASED: [f32; 4] = [0.824, 0.659, 1.0, 1.0]; // #d2a8ff
pub const COLOR_BORDER_HUMAN_PREEMPT: [f32; 4] = [0.941, 0.533, 0.243, 1.0]; // #f0883e
pub const COLOR_PROTECTED_BLACKOUT: [f32; 4] = [0.0, 0.0, 0.0, 1.0]; // #000000
pub const COLOR_GRID_HAIRLINE: [f32; 4] = [0.345, 0.651, 1.0, 0.45];
#[allow(clippy::approx_constant)] // #f85149 green channel 0x51/255 ≈ 0.318, not 1/pi.
pub const COLOR_CONFIG_ERROR_BANNER: [f32; 4] = [0.973, 0.318, 0.286, 0.95]; // #f85149
pub const COLOR_SCRATCHPAD_DIM_SCRIM: [f32; 4] = [0.035, 0.043, 0.063, 0.58];
pub const COLOR_DROP_PREVIEW_FILL: [f32; 4] = [0.345, 0.651, 1.0, 0.18];
pub const COLOR_DROP_PREVIEW_BORDER: [f32; 4] = [0.345, 0.651, 1.0, 0.88];
pub const COLOR_TAB_INDICATOR_ACTIVE: [f32; 4] = [0.345, 0.651, 1.0, 0.96]; // #58a6ff
pub const COLOR_TAB_INDICATOR_INACTIVE: [f32; 4] = [0.282, 0.310, 0.345, 0.72]; // #484f58
pub const COLOR_SHADOW_DEFAULT: [f32; 4] = [0.0, 0.0, 0.0, 0.451]; // #00000073

/// Per-window descriptor passed into the scene assembler for a single frame.
pub struct WindowSceneItem<'a> {
    pub window: &'a Window,
    pub rect: LogicalRect,
    pub is_floating: bool,
    pub is_focused: bool,
    /// Animated border transition factor (`0.0` = unfocused, `1.0` = focused).
    pub focus_factor: f32,
    pub is_protected_surface: bool,
    pub opacity: f32,
    pub corner_radius: f32,
    pub clip_to_geometry: bool,
    pub wants_blur: bool,
    pub no_shadow: bool,
    pub border_color_override: Option<String>,
    pub gradient_time_ms: u64,
    pub border: BorderConfig,
    pub focus_ring: FocusRingConfig,
    pub shadow: ShadowConfig,
    pub dim: DimConfig,
}

/// A captured closing window frame, still composited while `appearance.animations.windows_close`
/// settles. The owning `wl_surface` is already gone, so the pixels come from the per-window frame
/// cache (or [`capture_window_snapshot`]) rather than from the live surface tree.
pub struct ClosingWindowSceneItem<'a> {
    pub snapshot: &'a MemoryRenderBuffer,
    /// Pixel size of the captured snapshot (its own `buffer_scale` is `1`, so this is also the
    /// source rectangle in buffer coordinates).
    pub snapshot_size: (i32, i32),
    pub rect: LogicalRect,
    pub opacity: f32,
}

/// Resolved logical geometry and shader parameters for a rounded drop shadow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropShadowGeometry {
    pub area: Rectangle<i32, Logical>,
    pub shadow_box: Rectangle<f64, Logical>,
    pub shadow_radii: [f32; 4],
    pub sigma: f32,
    pub sharp: bool,
    pub color: [f32; 4],
}

/// Resolved outer rectangle and thickness for a Niri-style focus ring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FocusRingGeometry {
    pub ring_rect: Rectangle<f64, Logical>,
    pub outer_radii: [f32; 4],
    pub width: f32,
}

/// Resolves the normalized `[f32; 4]` RGBA drop-shadow color for active or inactive windows.
#[must_use]
pub fn resolve_shadow_rgba(shadow: &ShadowConfig, is_focused: bool) -> [f32; 4] {
    let raw = if is_focused {
        shadow.color.as_str()
    } else {
        shadow.inactive_color.as_deref().unwrap_or(&shadow.color)
    };
    parse_hex_color(raw, COLOR_SHADOW_DEFAULT)
}

/// Computes the padded drop-shadow draw area, scaled/offset shadow caster box, and blur sigma.
#[must_use]
pub fn compute_drop_shadow_geometry(
    window_rect: Rectangle<f64, Logical>,
    corner_radius: f32,
    shadow: &ShadowConfig,
    is_focused: bool,
) -> Option<DropShadowGeometry> {
    if !shadow.enabled || window_rect.size.w <= 0.0 || window_rect.size.h <= 0.0 {
        return None;
    }

    let color = resolve_shadow_rgba(shadow, is_focused);
    if color[3] <= 0.0 {
        return None;
    }

    let scale = f64::from(shadow.scale.clamp(0.05, 4.0));
    let sw = (window_rect.size.w * scale).max(1.0);
    let sh = (window_rect.size.h * scale).max(1.0);
    let sx = window_rect.loc.x + (window_rect.size.w - sw) * 0.5 + f64::from(shadow.offset_x);
    let sy = window_rect.loc.y + (window_rect.size.h - sh) * 0.5 + f64::from(shadow.offset_y);
    let shadow_box = Rectangle::new(Point::from((sx, sy)), Size::from((sw, sh)));

    let max_corner = (sw.min(sh) as f32) * 0.5;
    let r = (corner_radius.max(0.0) * scale as f32).min(max_corner);
    let shadow_radii = [r; 4];

    let sharp = shadow.sharp || shadow.range == 0;
    let (sigma, pad) = if sharp {
        (0.0_f32, 2.0_f64)
    } else {
        let range = shadow.range as f32;
        let power = f32::from(shadow.render_power.clamp(1, 8));
        let sigma = (range / (1.5 + 0.5 * power)).max(0.5);
        let pad = f64::from((3.0 * sigma).ceil().max(range) + 2.0);
        (sigma, pad)
    };

    let min_x = (shadow_box.loc.x - pad).floor() as i32;
    let min_y = (shadow_box.loc.y - pad).floor() as i32;
    let max_x = (shadow_box.loc.x + shadow_box.size.w + pad).ceil() as i32;
    let max_y = (shadow_box.loc.y + shadow_box.size.h + pad).ceil() as i32;
    let area = Rectangle::new(
        Point::from((min_x, min_y)),
        Size::from(((max_x - min_x).max(1), (max_y - min_y).max(1))),
    );

    Some(DropShadowGeometry {
        area,
        shadow_box,
        shadow_radii,
        sigma,
        sharp,
        color,
    })
}

/// Resolves the normalized `[f32; 4]` RGBA focus-ring color for active or inactive windows.
#[must_use]
pub fn resolve_focus_ring_rgba(focus_ring: &FocusRingConfig, is_focused: bool) -> [f32; 4] {
    if is_focused {
        parse_hex_color(&focus_ring.active_color, COLOR_BORDER_FOCUSED)
    } else {
        parse_hex_color(&focus_ring.inactive_color, COLOR_BORDER_INACTIVE)
    }
}

/// Computes outer ring bounds wrapped around `window_rect` when `focus_ring.enabled` is true.
#[must_use]
pub fn compute_focus_ring_geometry(
    window_rect: Rectangle<f64, Logical>,
    corner_radius: f32,
    focus_ring: &FocusRingConfig,
) -> Option<FocusRingGeometry> {
    let width = focus_ring.width.max(0.0);
    if !focus_ring.enabled || width <= 0.0 || window_rect.size.w <= 0.0 || window_rect.size.h <= 0.0
    {
        return None;
    }

    let w_f64 = f64::from(width);
    let ring_rect = Rectangle::new(
        Point::from((window_rect.loc.x - w_f64, window_rect.loc.y - w_f64)),
        Size::from((
            window_rect.size.w + w_f64 * 2.0,
            window_rect.size.h + w_f64 * 2.0,
        )),
    );
    let base_radius = corner_radius.max(0.0);
    let outer_radius = if base_radius > 0.0 {
        base_radius + width
    } else {
        0.0
    };

    Some(FocusRingGeometry {
        ring_rect,
        outer_radii: [outer_radius; 4],
        width,
    })
}

/// Computes the effective opacity/brightness multiplier for a window given `DimConfig`.
#[must_use]
pub fn effective_window_opacity_for(base_opacity: f32, is_focused: bool, dim: &DimConfig) -> f32 {
    let base = base_opacity.clamp(0.0, 1.0);
    let brightness = dim.brightness.clamp(0.1, 2.0);
    let factor = if !is_focused && dim.dim_inactive {
        (1.0 - dim.dim_strength.clamp(0.0, 0.95)) * brightness
    } else {
        brightness
    };
    (base * factor).clamp(0.0, 1.0)
}

/// Computes the effective opacity/brightness multiplier for `item` given `dim`.
#[must_use]
pub fn effective_window_opacity(item: &WindowSceneItem<'_>, dim: &DimConfig) -> f32 {
    effective_window_opacity_for(item.opacity, item.is_focused, dim)
}

/// Computes the logical origin of a toplevel's surface tree inside its frame.
///
/// Client-side-decorated clients keep their drop shadow (and any other decoration) inside the
/// buffer and describe the visible area through `xdg_surface.set_window_geometry`. The surface's
/// own `(0, 0)` therefore lives at `inner_rect_loc - geometry.loc`, while the geometry rect
/// (`inner_rect_loc`, size `inner_rect.size`) remains the visible/clip box.
///
/// This is the single definition of the surface origin: rendering, pointer hit-testing, pointer
/// constraints and agent cursors all derive from it. Server-side-decorated and XWayland windows
/// report a `(0, 0)` geometry origin and are placed unchanged.
#[must_use]
pub fn surface_origin(
    inner_rect_loc: Point<f64, Logical>,
    window_geometry_loc: Point<i32, Logical>,
) -> Point<f64, Logical> {
    inner_rect_loc - window_geometry_loc.to_f64()
}

/// Computes the physical origin of a toplevel's surface tree inside its frame.
///
/// See [`surface_origin`] for the logical definition; this only applies the output scale to it.
#[must_use]
pub fn toplevel_surface_origin(
    inner_rect_loc: Point<f64, Logical>,
    window_geometry_loc: Point<i32, Logical>,
    output_scale: Scale<f64>,
) -> Point<i32, Physical> {
    surface_origin(inner_rect_loc, window_geometry_loc).to_physical_precise_round(output_scale)
}

/// Window-local pixels prepared for offscreen capture: the logical window size plus the render
/// elements that reproduce the toplevel's committed frame at the origin.
pub struct WindowSnapshotElements {
    pub size: Size<i32, Logical>,
    pub elements: Vec<KomaRenderElement<GlesRenderer>>,
}

/// Builds the render elements that reproduce `window`'s committed frame inside a
/// window-local `(0, 0, rect.w, rect.h)` box, clipped to the configured corner radius.
///
/// Shared by the on-commit frame cache and the one-shot close snapshot so both capture identical
/// pixels.
///
/// # Errors
///
/// Fails when the window is not an xdg toplevel or has no committed buffer.
pub fn window_snapshot_elements(
    renderer: &mut GlesRenderer,
    window: &Window,
    rect: LogicalRect,
    output_scale: Scale<f64>,
    corner_radius: f32,
) -> anyhow::Result<WindowSnapshotElements> {
    let toplevel = window
        .toplevel()
        .context("only xdg toplevels can be snapshotted")?;
    let size = Size::<i32, Logical>::from((rect.w.max(1) as i32, rect.h.max(1) as i32));
    let surface_origin =
        toplevel_surface_origin(Point::from((0.0, 0.0)), window.geometry().loc, output_scale);
    let surface_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        render_elements_from_surface_tree(
            renderer,
            toplevel.wl_surface(),
            surface_origin,
            output_scale,
            1.0,
            Kind::Unspecified,
        );
    if surface_elems.is_empty() {
        anyhow::bail!("toplevel has no committed buffer to snapshot");
    }

    let shaders = KomaCompiledShaders::get_or_compile(renderer);
    let inner_rect = Rectangle::<f64, Logical>::new(
        Point::from((0.0, 0.0)),
        Size::from((f64::from(size.w), f64::from(size.h))),
    );
    let radius = corner_radius.max(0.0);
    let mut elements: Vec<KomaRenderElement<GlesRenderer>> = Vec::new();
    if radius > 0.5
        && let Some(clip_prog) = shaders.clipped_surface.as_ref()
    {
        for surf_el in surface_elems {
            elements.push(KomaRenderElement::ClippedSurface(
                KomaClippedSurfaceElement::new(
                    surf_el,
                    clip_prog.clone(),
                    inner_rect,
                    [radius; 4],
                    1.0,
                    output_scale,
                ),
            ));
        }
    } else {
        elements.extend(surface_elems.into_iter().map(KomaRenderElement::from));
    }
    Ok(WindowSnapshotElements { size, elements })
}

/// Captures a toplevel's currently committed buffer into a CPU-side snapshot, clipped to the
/// window rectangle with the configured corner radius.
///
/// The compositor normally reads the per-window frame cache instead; this one-shot path covers
/// clients that still hold a buffer when their toplevel is destroyed.
///
/// # Errors
///
/// Fails when the window is not an xdg toplevel, when the surface has no committed buffer, or when
/// the offscreen read-back fails.
pub fn capture_window_snapshot(
    renderer: &mut GlesRenderer,
    window: &Window,
    rect: LogicalRect,
    output_scale: Scale<f64>,
    corner_radius: f32,
) -> anyhow::Result<(MemoryRenderBuffer, (i32, i32))> {
    let snapshot = window_snapshot_elements(renderer, window, rect, output_scale, corner_radius)?;
    capture_elements_to_memory(renderer, snapshot.size, output_scale, &snapshot.elements)
}

/// Resolves `(start_rgba, end_rgba, angle_rad)` for a gradient border interpolated smoothly by `focus_factor`.
#[must_use]
pub fn resolve_border_gradient(
    border: &BorderConfig,
    lease_state: &WorkspaceLeaseState,
    focus_factor: f32,
    border_color_override: Option<&str>,
    now_ms: u64,
) -> ([f32; 4], [f32; 4], f32) {
    let t = if focus_factor.is_finite() {
        focus_factor.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let inactive_start = parse_hex_color(&border.inactive_color, COLOR_BORDER_INACTIVE);
    let inactive_end = border
        .inactive_color_end
        .as_deref()
        .map_or(inactive_start, |c| parse_hex_color(c, inactive_start));

    let (active_start, active_end) = match lease_state {
        WorkspaceLeaseState::HumanPreempted { .. } => {
            let c = parse_hex_color(&border.human_preempt_color, COLOR_BORDER_HUMAN_PREEMPT);
            (c, c)
        }
        WorkspaceLeaseState::AgentExclusive { .. } => {
            let c = parse_hex_color(&border.agent_leased_color, COLOR_BORDER_AGENT_LEASED);
            (c, c)
        }
        _ => {
            if let Some(custom) = border_color_override {
                let c = parse_hex_color(custom, COLOR_BORDER_FOCUSED);
                (c, c)
            } else {
                let s = parse_hex_color(&border.active_color, COLOR_BORDER_FOCUSED);
                let e = border
                    .active_color_end
                    .as_deref()
                    .map_or(s, |c| parse_hex_color(c, s));
                (s, e)
            }
        }
    };

    let mix = |a: [f32; 4], b: [f32; 4]| -> [f32; 4] {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
            a[3] + (b[3] - a[3]) * t,
        ]
    };
    (
        mix(inactive_start, active_start),
        mix(inactive_end, active_end),
        border.effective_gradient_angle_rad(now_ms),
    )
}

/// Resolves the normalized `[f32; 4]` RGBA border color for a window from `BorderConfig` and `WorkspaceLeaseState`.
#[must_use]
pub fn resolve_border_rgba(
    border: &BorderConfig,
    lease_state: &WorkspaceLeaseState,
    is_focused: bool,
) -> [f32; 4] {
    match lease_state {
        WorkspaceLeaseState::HumanPreempted { .. } if is_focused => {
            parse_hex_color(&border.human_preempt_color, COLOR_BORDER_HUMAN_PREEMPT)
        }
        WorkspaceLeaseState::AgentExclusive { .. } if is_focused => {
            parse_hex_color(&border.agent_leased_color, COLOR_BORDER_AGENT_LEASED)
        }
        _ if is_focused => parse_hex_color(&border.active_color, COLOR_BORDER_FOCUSED),
        _ => parse_hex_color(&border.inactive_color, COLOR_BORDER_INACTIVE),
    }
}

/// Optional pre-blurred `Dual Kawase` backdrop passed into `assemble_workspace_elements` on
/// physical outputs when `blur.enabled` is active.
#[derive(Debug, Clone)]
pub struct FrameBlurContext {
    pub texture: GlesTexture,
    pub config: BlurConfig,
}

/// Builds the **front-to-back** `KomaRenderElement<R>` list consumed by `OutputDamageTracker::render_output`
/// or `DrmCompositor::render_frame`.
///
/// When `drop_preview_rect` is `Some` (during an interactive `Mod + MouseLeft` tile drag), inserts
/// the translucent `#58a6ff` 4-quadrant drop zone card directly **behind** the floating drag window
/// and **in front of** all underlying tiled windows!
#[allow(clippy::too_many_arguments)]
pub fn assemble_workspace_elements<R>(
    renderer: &mut R,
    viewport: LogicalViewport,
    output_scale: Scale<f64>,
    lease_state: &WorkspaceLeaseState,
    target: RenderTargetKind,
    overlay: OverlayKind,
    blur_ctx: Option<&FrameBlurContext>,
    drop_preview_rect: Option<LogicalRect>,
    windows_top_to_bottom: &[WindowSceneItem<'_>],
) -> Vec<KomaRenderElement<R>>
where
    R: Renderer + ImportAll + ImportMem + KomaGlesRenderer,
    R::TextureId: Clone + 'static,
{
    let shaders = KomaCompiledShaders::get_or_compile(renderer.as_gles_renderer());
    let mut elements: Vec<KomaRenderElement<R>> = Vec::new();

    // 1. Optional topmost 15x15 `grid_225` decision grid overlay (`IPC_SPEC.md` §5.4)
    if target == RenderTargetKind::AgentCapture && overlay == OverlayKind::Grid225 {
        append_grid_225_hairlines(viewport, &mut elements);
    }

    let viewport_size =
        Size::<f64, Logical>::from((f64::from(viewport.width), f64::from(viewport.height)));
    let mut drop_preview_emitted =
        drop_preview_rect.is_none() || target != RenderTargetKind::PhysicalOutput;
    let default_preview_radius = windows_top_to_bottom
        .first()
        .map(|w| w.corner_radius)
        .unwrap_or(10.0);

    // 2. Windows ordered front-to-back:
    //    Floating windows -> [DropZonePreview] -> Tiled windows!
    for item in windows_top_to_bottom {
        if !drop_preview_emitted && !item.is_floating {
            if let Some(p_rect) = drop_preview_rect {
                push_drop_preview_elements(
                    p_rect,
                    default_preview_radius,
                    output_scale,
                    &shaders,
                    &mut elements,
                );
            }
            drop_preview_emitted = true;
        }

        let border_thickness = if item.border.enabled {
            f64::from(item.border.width.max(0.0))
        } else {
            0.0
        };
        let outer_rect = to_smithay_logical_rect(item.rect);
        let eff_opacity = effective_window_opacity(item, &item.dim);
        let radius = item.corner_radius.max(0.0);
        let outer_radii = [radius; 4];
        let inner_radius = (radius - border_thickness as f32).max(0.0);
        let inner_radii = [inner_radius; 4];

        // Protected Surface Blackout Invariant (`IPC_SPEC.md` §3.2)
        if item.is_protected_surface && target != RenderTargetKind::PhysicalOutput {
            elements.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
                outer_rect,
                COLOR_PROTECTED_BLACKOUT,
            )));
            continue;
        }

        let inner_rect = Rectangle::<f64, Logical>::new(
            Point::from((
                f64::from(item.rect.x) + border_thickness,
                f64::from(item.rect.y) + border_thickness,
            )),
            Size::from((
                (f64::from(item.rect.w) - border_thickness * 2.0).max(16.0),
                (f64::from(item.rect.h) - border_thickness * 2.0).max(16.0),
            )),
        );

        let (border_rgba, border_rgba_end, border_angle_rad) = resolve_border_gradient(
            &item.border,
            lease_state,
            item.focus_factor,
            item.border_color_override.as_deref(),
            item.gradient_time_ms,
        );

        // 2a. Transient xdg_popup menus attached to this toplevel (front-most, unclipped)
        if let Some(toplevel) = item.window.toplevel() {
            let wl_surface = toplevel.wl_surface();
            for (popup, popup_offset) in PopupManager::popups_for_surface(wl_surface) {
                let popup_loc_logical =
                    inner_rect.loc + (popup_offset - popup.geometry().loc).to_f64();
                let popup_loc_phys: Point<i32, Physical> =
                    popup_loc_logical.to_physical_precise_round(output_scale);
                let popup_elems: Vec<WaylandSurfaceRenderElement<R>> =
                    render_elements_from_surface_tree(
                        renderer,
                        popup.wl_surface(),
                        popup_loc_phys,
                        output_scale,
                        eff_opacity,
                        Kind::Unspecified,
                    );
                elements.extend(popup_elems.into_iter().map(KomaRenderElement::from));
            }
        }

        // 2b. Anti-aliased SDF rounded border ring + optional outer Niri-style focus ring
        if let Some(ring_geom) = compute_focus_ring_geometry(outer_rect, radius, &item.focus_ring) {
            let ring_rgba = resolve_focus_ring_rgba(&item.focus_ring, item.is_focused);
            if ring_rgba[3] > 0.0
                && let (Some(border_prog), Some(white_tex)) = (
                    shaders.rounded_border.as_ref(),
                    shaders.white_1x1_tex.as_ref(),
                )
            {
                elements.push(KomaRenderElement::RoundedBorder(
                    KomaRoundedBorderElement::new(
                        ring_geom.ring_rect,
                        ring_geom.outer_radii,
                        ring_geom.width,
                        ring_rgba,
                        eff_opacity,
                        output_scale,
                        white_tex.clone(),
                        border_prog.clone(),
                    ),
                ));
            }
        }

        if item.border.enabled
            && border_thickness > 0.0
            && let (Some(border_prog), Some(white_tex)) = (
                shaders.rounded_border.as_ref(),
                shaders.white_1x1_tex.as_ref(),
            )
        {
            elements.push(KomaRenderElement::RoundedBorder(
                KomaRoundedBorderElement::new(
                    outer_rect,
                    outer_radii,
                    border_thickness as f32,
                    border_rgba,
                    eff_opacity,
                    output_scale,
                    white_tex.clone(),
                    border_prog.clone(),
                )
                .with_gradient(border_rgba_end, border_angle_rad),
            ));
        }

        // 2c. Main toplevel surface tree (clipped to `inner_rect` + `inner_radii` when shader available)
        if let Some(toplevel) = item.window.toplevel() {
            let wl_surface = toplevel.wl_surface();
            // CSD buffers carry their own decoration/shadow inset; place the visible window
            // geometry on the frame instead of the buffer origin (komawm/koma#41).
            let surface_loc_phys =
                toplevel_surface_origin(inner_rect.loc, item.window.geometry().loc, output_scale);
            let surface_elems: Vec<WaylandSurfaceRenderElement<R>> =
                render_elements_from_surface_tree(
                    renderer,
                    wl_surface,
                    surface_loc_phys,
                    output_scale,
                    1.0,
                    Kind::Unspecified,
                );

            let needs_shader_clip =
                item.clip_to_geometry && (inner_radius > 0.5 || eff_opacity < 0.999);
            if let (true, Some(clip_prog)) = (needs_shader_clip, shaders.clipped_surface.as_ref()) {
                for surf_el in surface_elems {
                    elements.push(KomaRenderElement::ClippedSurface(
                        KomaClippedSurfaceElement::new(
                            surf_el,
                            clip_prog.clone(),
                            inner_rect,
                            inner_radii,
                            eff_opacity,
                            output_scale,
                        ),
                    ));
                }
            } else {
                elements.extend(surface_elems.into_iter().map(KomaRenderElement::from));
            }
        }

        // 2d. Multi-Pass `Dual Kawase Blur` frosted-glass backdrop slice behind translucent windows
        if target == RenderTargetKind::PhysicalOutput
            && item.wants_blur
            && let (Some(b_ctx), Some(blur_prog)) = (blur_ctx, shaders.blur_finish_rounded.as_ref())
        {
            let should_blur = b_ctx.config.ignore_opacity || eff_opacity < 0.999;
            if should_blur {
                elements.push(KomaRenderElement::BlurredBackdrop(
                    KomaBlurredBackdropElement::new(
                        inner_rect,
                        viewport_size,
                        inner_radii,
                        eff_opacity,
                        output_scale,
                        b_ctx.config.noise,
                        b_ctx.config.contrast,
                        b_ctx.config.brightness,
                        b_ctx.texture.clone(),
                        blur_prog.clone(),
                    ),
                ));
            }
        }

        if item.border.enabled && border_thickness > 0.0 && shaders.rounded_border.is_none() {
            elements.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
                outer_rect,
                border_rgba,
            )));
        }

        if shaders.rounded_border.is_none()
            && let Some(ring_geom) =
                compute_focus_ring_geometry(outer_rect, radius, &item.focus_ring)
        {
            let ring_rgba = resolve_focus_ring_rgba(&item.focus_ring, item.is_focused);
            if ring_rgba[3] > 0.0 {
                elements.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
                    ring_geom.ring_rect,
                    ring_rgba,
                )));
            }
        }

        // 2e. Rounded drop shadow behind window, border, focus ring, and blur backdrop
        if !item.no_shadow
            && let (Some(shadow_geom), Some(shadow_prog)) = (
                compute_drop_shadow_geometry(outer_rect, radius, &item.shadow, item.is_focused),
                shaders.rounded_shadow.as_ref(),
            )
        {
            elements.push(KomaRenderElement::DropShadow(KomaDropShadowElement::new(
                shadow_geom.area,
                shadow_geom.shadow_box,
                shadow_geom.shadow_radii,
                shadow_geom.sigma,
                shadow_geom.sharp,
                shadow_geom.color,
                outer_rect,
                outer_radii,
                eff_opacity,
                output_scale,
                shadow_prog.clone(),
            )));
        }
    }

    // If the dragged window is the only window on the workspace (0 tiled windows below it),
    // emit the full-workarea drop preview right behind the floating drag card!
    if !drop_preview_emitted && let Some(p_rect) = drop_preview_rect {
        push_drop_preview_elements(
            p_rect,
            default_preview_radius,
            output_scale,
            &shaders,
            &mut elements,
        );
    }

    elements
}

/// Appends captured closing-window frames on top of the already assembled workspace elements.
///
/// The rect is the `windows_close` spring's sampled output, so the frame shrinks towards its
/// centre and fades out. The window is no longer part of the layout, hit-testing, focus or IPC
/// inventory at this point; only these pixels linger.
pub fn append_closing_window_elements<R>(
    renderer: &mut R,
    items: &[ClosingWindowSceneItem<'_>],
    elements: &mut Vec<KomaRenderElement<R>>,
) where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Send + 'static,
{
    for item in items {
        if item.rect.w == 0 || item.rect.h == 0 || item.opacity <= 0.001 {
            continue;
        }
        let src = Rectangle::<f64, Logical>::from_size(Size::from((
            f64::from(item.snapshot_size.0.max(1)),
            f64::from(item.snapshot_size.1.max(1)),
        )));
        let logical_size = Size::<i32, Logical>::from((item.rect.w as i32, item.rect.h as i32));
        let location =
            Point::<f64, Physical>::from((f64::from(item.rect.x), f64::from(item.rect.y)));
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            item.snapshot,
            Some(item.opacity.clamp(0.0, 1.0)),
            Some(src),
            Some(logical_size),
            Kind::Unspecified,
        ) {
            Ok(elem) => elements.push(KomaRenderElement::MemoryBuffer(elem)),
            Err(err) => {
                tracing::warn!(?err, "failed to import closing window snapshot");
            }
        }
    }
}

fn push_drop_preview_elements<R>(
    preview_rect: LogicalRect,
    corner_radius: f32,
    output_scale: Scale<f64>,
    shaders: &KomaCompiledShaders,
    elements: &mut Vec<KomaRenderElement<R>>,
) where
    R: Renderer + ImportAll,
{
    let outer = to_smithay_logical_rect(preview_rect);
    let radii = [corner_radius.max(0.0); 4];
    if let (Some(border_prog), Some(white_tex)) = (
        shaders.rounded_border.as_ref(),
        shaders.white_1x1_tex.as_ref(),
    ) {
        elements.push(KomaRenderElement::RoundedBorder(
            KomaRoundedBorderElement::new(
                outer,
                radii,
                2.0,
                COLOR_DROP_PREVIEW_BORDER,
                1.0,
                output_scale,
                white_tex.clone(),
                border_prog.clone(),
            ),
        ));
    }
    let inset = Rectangle::<f64, Logical>::new(
        Point::from((
            f64::from(preview_rect.x) + 2.0,
            f64::from(preview_rect.y) + 2.0,
        )),
        Size::from((
            (f64::from(preview_rect.w) - 4.0).max(16.0),
            (f64::from(preview_rect.h) - 4.0).max(16.0),
        )),
    );
    elements.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
        inset,
        COLOR_DROP_PREVIEW_FILL,
    )));
}

/// Appends a translucent dark backdrop scrim behind an open `special:<name>` Scratchpad overlay.
pub fn append_scratchpad_dim_scrim<R>(
    viewport: LogicalViewport,
    elements: &mut Vec<KomaRenderElement<R>>,
) where
    R: Renderer + ImportAll,
{
    let scrim = Rectangle::new(
        Point::from((0.0, 0.0)),
        Size::from((f64::from(viewport.width), f64::from(viewport.height))),
    );
    elements.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
        scrim,
        COLOR_SCRATCHPAD_DIM_SCRIM,
    )));
}

/// Prepends a 4-logical-pixel coral-red (`#f85149`) top alert bar on physical outputs whenever
/// `~/.config/koma/init.lua` failed its most recent hot-reload.
pub fn prepend_config_error_banner<R>(
    viewport: LogicalViewport,
    elements: &mut Vec<KomaRenderElement<R>>,
) where
    R: Renderer + ImportAll,
{
    let bar = Rectangle::new(
        Point::from((0.0, 0.0)),
        Size::from((f64::from(viewport.width), 4.0)),
    );
    elements.insert(
        0,
        KomaRenderElement::from(KomaSolidColorElement::oneshot(
            bar,
            COLOR_CONFIG_ERROR_BANNER,
        )),
    );
}

fn append_grid_225_hairlines<R>(viewport: LogicalViewport, elements: &mut Vec<KomaRenderElement<R>>)
where
    R: Renderer + ImportAll,
{
    let w = f64::from(viewport.width);
    let h = f64::from(viewport.height);
    for idx in 1..15 {
        let x = (w / 15.0) * f64::from(idx);
        let v_line = Rectangle::new(Point::from((x, 0.0)), Size::from((1.0, h)));
        elements.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
            v_line,
            COLOR_GRID_HAIRLINE,
        )));

        let y = (h / 15.0) * f64::from(idx);
        let h_line = Rectangle::new(Point::from((0.0, y)), Size::from((w, 1.0)));
        elements.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
            h_line,
            COLOR_GRID_HAIRLINE,
        )));
    }
}

/// Emits anti-aliased rounded pill segments for every visible `ColumnDisplay::Tabbed` column
/// (`niri/src/layout/tab_indicator.rs`).
pub fn append_tab_indicator_elements<R>(
    renderer: &mut R,
    output_scale: Scale<f64>,
    segments: &[koma_model::TabIndicatorSegment],
    elements: &mut Vec<KomaRenderElement<R>>,
) where
    R: Renderer + ImportAll + ImportMem + KomaGlesRenderer,
{
    if segments.is_empty() {
        return;
    }
    let shaders = KomaCompiledShaders::get_or_compile(renderer.as_gles_renderer());

    for seg in segments {
        let rect = to_smithay_logical_rect(seg.rect);
        let rgba = if seg.is_active {
            COLOR_TAB_INDICATOR_ACTIVE
        } else {
            COLOR_TAB_INDICATOR_INACTIVE
        };
        let radius = seg.corner_radius.max(0.0);

        if radius > 0.25
            && let (Some(border_prog), Some(white_tex)) = (
                shaders.rounded_border.as_ref(),
                shaders.white_1x1_tex.as_ref(),
            )
        {
            // `border_width = 0.0` fills the entire rounded SDF pill cleanly
            elements.push(KomaRenderElement::RoundedBorder(
                KomaRoundedBorderElement::new(
                    rect,
                    [radius; 4],
                    0.0,
                    rgba,
                    1.0,
                    output_scale,
                    white_tex.clone(),
                    border_prog.clone(),
                ),
            ));
            continue;
        }
        elements.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
            rect, rgba,
        )));
    }
}

/// Pure view-isolation gate for agent cursor elements: human `PhysicalOutput` and
/// `ScreencastPreview` render the agent cursor overlay, whereas `RenderTargetKind::AgentCapture`
/// strictly omits all agent cursor, aura fog, ripple, and motion trail elements.
#[inline]
#[must_use]
pub const fn should_render_agent_cursor(target: RenderTargetKind) -> bool {
    !matches!(target, RenderTargetKind::AgentCapture)
}

/// Prepends agent cursor, label pill, ripple, fog, motion-trail, and optional caret-box elements
/// (cursor front-most) onto `elements`, obeying `RenderTargetKind::AgentCapture` strict view isolation.
pub fn prepend_agent_cursor_elements<R>(
    renderer: &mut R,
    target: RenderTargetKind,
    output_scale: Scale<f64>,
    snapshot: Option<&AgentCursorFrameSnapshot>,
    elements: &mut Vec<KomaRenderElement<R>>,
) where
    R: Renderer + ImportAll + ImportMem + KomaGlesRenderer + crate::elements::KomaRenderTarget,
    R::TextureId: Send + Clone + 'static,
{
    if !should_render_agent_cursor(target) {
        return;
    }
    let Some(snap) = snapshot else {
        return;
    };
    if snap.opacity <= 0.001 {
        return;
    }

    let shaders = KomaCompiledShaders::get_or_compile(renderer.as_gles_renderer());
    let mut overlay: Vec<KomaRenderElement<R>> = Vec::with_capacity(8);
    if let Some(cursor_prog) = shaders.agent_cursor {
        overlay.push(KomaRenderElement::from(
            KomaAgentCursorElement::from_snapshot(snap, output_scale, cursor_prog),
        ));
    }
    if let Some(pill) = &snap.label_pill
        && let Some(pill_elem) = crate::elements::build_label_pill_element(
            renderer,
            pill,
            snap.accent_rgba,
            snap.opacity,
            output_scale,
        )
    {
        overlay.push(pill_elem);
    }
    if let Some(trail_prog) = shaders.agent_trail
        && let Some(trail_elem) = KomaAgentTrailElement::from_points(
            &snap.trail_points,
            snap.accent_rgba,
            snap.opacity,
            output_scale,
            trail_prog,
        )
    {
        overlay.push(KomaRenderElement::from(trail_elem));
    }
    if let Some(caret) = snap.caret_rect
        && caret.w > 0
        && caret.h > 0
    {
        let caret_f64 = to_smithay_logical_rect(caret);
        let [r, g, b, _] = snap.accent_rgba;
        let alpha = snap.opacity.clamp(0.0, 1.0);
        if let (Some(border_prog), Some(white_tex)) = (
            shaders.rounded_border.as_ref(),
            shaders.white_1x1_tex.as_ref(),
        ) {
            overlay.push(KomaRenderElement::RoundedBorder(
                KomaRoundedBorderElement::new(
                    caret_f64,
                    [4.0; 4],
                    1.5,
                    [r, g, b, 0.85 * alpha],
                    alpha,
                    output_scale,
                    white_tex.clone(),
                    border_prog.clone(),
                ),
            ));
        } else {
            let stroke = 1.5_f64;
            let outline_rgba = [r, g, b, 0.85 * alpha];
            let x = caret_f64.loc.x;
            let y = caret_f64.loc.y;
            let w = caret_f64.size.w;
            let h = caret_f64.size.h;
            overlay.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
                Rectangle::new(Point::from((x, y)), Size::from((w, stroke))),
                outline_rgba,
            )));
            overlay.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
                Rectangle::new(
                    Point::from((x, (y + h - stroke).max(y))),
                    Size::from((w, stroke)),
                ),
                outline_rgba,
            )));
            overlay.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
                Rectangle::new(Point::from((x, y)), Size::from((stroke, h))),
                outline_rgba,
            )));
            overlay.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
                Rectangle::new(
                    Point::from(((x + w - stroke).max(x), y)),
                    Size::from((stroke, h)),
                ),
                outline_rgba,
            )));
        }
        overlay.push(KomaRenderElement::from(KomaSolidColorElement::oneshot(
            caret_f64,
            [r, g, b, 0.14 * alpha],
        )));
    }
    if !overlay.is_empty() {
        elements.splice(0..0, overlay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_window_opacity_applies_inactive_dim_and_brightness() {
        let dim = DimConfig {
            dim_inactive: true,
            dim_strength: 0.25,
            brightness: 0.8,
            contrast: 1.0,
        };
        // Focused window only scales by brightness (1.0 * 0.8 = 0.8)
        let focused = effective_window_opacity_for(1.0, true, &dim);
        assert!((focused - 0.8).abs() < 1e-5);

        // Inactive window scales by (1 - 0.25) * 0.8 = 0.6
        let inactive = effective_window_opacity_for(1.0, false, &dim);
        assert!((inactive - 0.6).abs() < 1e-5);
    }

    #[test]
    fn csd_toplevel_surface_origin_subtracts_the_window_geometry_inset() {
        // A GTK/Chrome-style CSD buffer puts a shadow band left/top of the window geometry.
        let inner_loc = Point::<f64, Logical>::from((100.0, 50.0));
        let geometry_loc = Point::<i32, Logical>::from((12, 8));

        let origin = toplevel_surface_origin(inner_loc, geometry_loc, Scale::from(1.0));

        assert_eq!(origin, Point::<i32, Physical>::from((88, 42)));
        // The client's visible area (surface origin + geometry inset) lands on the frame.
        assert_eq!(
            origin + Point::<i32, Physical>::from((12, 8)),
            Point::<i32, Physical>::from((100, 50))
        );
        // The logical helper used by hit-testing and constraints agrees with rendering.
        assert_eq!(
            surface_origin(inner_loc, geometry_loc),
            Point::<f64, Logical>::from((88.0, 42.0))
        );
    }

    #[test]
    fn toplevel_surface_origin_keeps_ssd_windows_in_place_and_scales() {
        let inner_loc = Point::<f64, Logical>::from((100.0, 50.0));
        let no_inset = Point::<i32, Logical>::from((0, 0));

        assert_eq!(
            toplevel_surface_origin(inner_loc, no_inset, Scale::from(1.0)),
            Point::<i32, Physical>::from((100, 50)),
            "SSD/XWayland windows must not move"
        );
        assert_eq!(
            toplevel_surface_origin(inner_loc, no_inset, Scale::from(2.0)),
            Point::<i32, Physical>::from((200, 100)),
            "the inset has to be applied before the output scale"
        );
    }

    #[test]
    fn resolve_border_rgba_parses_custom_configured_colors() {
        let border = BorderConfig {
            enabled: true,
            width: 3.0,
            active_color: "#ff0000".to_owned(),
            inactive_color: "#00ff00".to_owned(),
            agent_leased_color: "#0000ff".to_owned(),
            human_preempt_color: "#ffff00".to_owned(),
            active_color_end: Some("#0000ff".to_owned()),
            inactive_color_end: None,
            gradient_angle: 90.0,
            gradient_rotate_rpm: 10.0,
        };

        let active = resolve_border_rgba(&border, &WorkspaceLeaseState::Unheld, true);
        assert_eq!(active, [1.0, 0.0, 0.0, 1.0]);

        let inactive = resolve_border_rgba(&border, &WorkspaceLeaseState::Unheld, false);
        assert_eq!(inactive, [0.0, 1.0, 0.0, 1.0]);

        let (mid_s, mid_e, angle0) =
            resolve_border_gradient(&border, &WorkspaceLeaseState::Unheld, 0.5, None, 0);
        assert!((mid_s[0] - 0.5).abs() < 1e-5);
        assert!((mid_s[1] - 0.5).abs() < 1e-5);
        assert!((mid_e[2] - 0.5).abs() < 1e-5);
        assert!((angle0 - 90.0_f32.to_radians()).abs() < 1e-5);

        let (_, _, angle_spun) =
            resolve_border_gradient(&border, &WorkspaceLeaseState::Unheld, 1.0, None, 1500);
        assert!((angle_spun - 180.0_f32.to_radians()).abs() < 1e-4);
    }

    #[test]
    fn compute_drop_shadow_geometry_respects_offsets_scale_range_sharp_and_colors() {
        let win = Rectangle::new(Point::from((100.0, 200.0)), Size::from((800.0, 600.0)));
        let cfg = ShadowConfig {
            enabled: true,
            range: 20,
            render_power: 3,
            sharp: false,
            color: "#112233cc".to_owned(),
            inactive_color: Some("#44556677".to_owned()),
            offset_x: 6.0,
            offset_y: -4.0,
            scale: 0.9,
        };

        let active = compute_drop_shadow_geometry(win, 10.0, &cfg, true).expect("active shadow");
        assert!((active.shadow_box.size.w - 720.0).abs() < 1e-4);
        assert!((active.shadow_box.size.h - 540.0).abs() < 1e-4);
        // Centered delta is +40/+30 plus offset (6, -4)
        assert!((active.shadow_box.loc.x - 146.0).abs() < 1e-4);
        assert!((active.shadow_box.loc.y - 226.0).abs() < 1e-4);
        assert!(!active.sharp);
        assert!(active.sigma > 0.0);
        // Padded area must strictly enclose shadow_box
        assert!(f64::from(active.area.loc.x) < active.shadow_box.loc.x);
        assert!(f64::from(active.area.loc.y) < active.shadow_box.loc.y);
        assert!(
            f64::from(active.area.loc.x + active.area.size.w)
                > active.shadow_box.loc.x + active.shadow_box.size.w
        );
        assert!(
            f64::from(active.area.loc.y + active.area.size.h)
                > active.shadow_box.loc.y + active.shadow_box.size.h
        );
        assert!((active.shadow_radii[0] - 9.0).abs() < 1e-5);
        assert_eq!(
            active.color,
            [
                f32::from(0x11_u8) / 255.0,
                f32::from(0x22_u8) / 255.0,
                f32::from(0x33_u8) / 255.0,
                f32::from(0xcc_u8) / 255.0,
            ]
        );

        let inactive =
            compute_drop_shadow_geometry(win, 10.0, &cfg, false).expect("inactive shadow");
        assert_eq!(
            inactive.color,
            [
                f32::from(0x44_u8) / 255.0,
                f32::from(0x55_u8) / 255.0,
                f32::from(0x66_u8) / 255.0,
                f32::from(0x77_u8) / 255.0,
            ]
        );

        let fallback_inactive = ShadowConfig {
            inactive_color: None,
            ..cfg.clone()
        };
        assert_eq!(resolve_shadow_rgba(&fallback_inactive, false), active.color);

        let sharp_cfg = ShadowConfig {
            sharp: true,
            ..cfg.clone()
        };
        let sharp = compute_drop_shadow_geometry(win, 10.0, &sharp_cfg, true).expect("sharp");
        assert!(sharp.sharp);
        assert_eq!(sharp.sigma, 0.0);

        let disabled_cfg = ShadowConfig {
            enabled: false,
            ..cfg
        };
        assert!(compute_drop_shadow_geometry(win, 10.0, &disabled_cfg, true).is_none());
    }

    #[test]
    fn compute_focus_ring_geometry_and_colors_behave_cleanly() {
        let win = Rectangle::new(Point::from((50.0, 80.0)), Size::from((400.0, 300.0)));
        let disabled = FocusRingConfig::default();
        assert!(compute_focus_ring_geometry(win, 8.0, &disabled).is_none());

        let enabled = FocusRingConfig {
            enabled: true,
            width: 3.0,
            active_color: "#ff0000".to_owned(),
            inactive_color: "#00ff0080".to_owned(),
        };
        let ring = compute_focus_ring_geometry(win, 8.0, &enabled).expect("enabled ring");
        assert_eq!(ring.ring_rect.loc, Point::from((47.0, 77.0)));
        assert_eq!(ring.ring_rect.size, Size::from((406.0, 306.0)));
        assert_eq!(ring.outer_radii, [11.0; 4]);
        assert_eq!(ring.width, 3.0);

        // Square windows retain square outer focus ring corners
        let square = compute_focus_ring_geometry(win, 0.0, &enabled).expect("square ring");
        assert_eq!(square.outer_radii, [0.0; 4]);

        assert_eq!(
            resolve_focus_ring_rgba(&enabled, true),
            [1.0, 0.0, 0.0, 1.0]
        );
        assert_eq!(
            resolve_focus_ring_rgba(&enabled, false),
            [0.0, 1.0, 0.0, f32::from(0x80_u8) / 255.0]
        );
    }

    #[test]
    fn agent_cursor_view_isolation_excludes_agent_capture_targets() {
        assert!(!should_render_agent_cursor(RenderTargetKind::AgentCapture));
        assert!(should_render_agent_cursor(RenderTargetKind::PhysicalOutput));
        assert!(should_render_agent_cursor(
            RenderTargetKind::ScreencastPreview
        ));
    }
}
