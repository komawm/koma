//! Front-to-back scene graph element assembly for nested `Winit` (`GlesRenderer`),
//! bare-metal `DRM/KMS` (`TtyRenderer` / `MultiRenderer`), and `Headless` `LazyFbo`,
//! integrating SDF rounded-corner surface clipping, anti-aliased rounded borders,
//! 4-quadrant `smart_split` drop-zone previews, and multi-pass `Dual Kawase Blur` backdrops.

use koma_model::{
    BlurConfig, BorderConfig, DimConfig, LogicalRect, LogicalViewport, ShadowConfig,
    WorkspaceLeaseState, parse_hex_color,
};
use koma_proto::OverlayKind;
use smithay::{
    backend::renderer::{
        Color32F, ImportAll, ImportMem, Renderer,
        element::{
            Kind,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
        gles::GlesTexture,
    },
    desktop::{PopupManager, Window},
    utils::{Logical, Physical, Point, Rectangle, Scale, Size},
};

use crate::{
    RenderTargetKind,
    elements::{
        KomaBlurredBackdropElement, KomaClippedSurfaceElement, KomaRenderElement,
        KomaRoundedBorderElement, KomaSolidColorElement, to_smithay_logical_rect,
    },
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

/// Per-window descriptor passed into the scene assembler for a single frame.
pub struct WindowSceneItem<'a> {
    pub window: &'a Window,
    pub rect: LogicalRect,
    pub is_floating: bool,
    pub is_focused: bool,
    pub is_protected_surface: bool,
    pub opacity: f32,
    pub corner_radius: f32,
    pub wants_blur: bool,
    pub border: BorderConfig,
    pub shadow: ShadowConfig,
    pub dim: DimConfig,
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

        let border_rgba = resolve_border_rgba(&item.border, lease_state, item.is_focused);

        // 2a. Transient xdg_popup menus attached to this toplevel (front-most, unclipped)
        if let Some(toplevel) = item.window.toplevel() {
            let wl_surface = toplevel.wl_surface();
            for (popup, popup_offset) in PopupManager::popups_for_surface(wl_surface) {
                let popup_loc_logical = inner_rect.loc + popup_offset.to_f64();
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

        // 2b. Anti-aliased SDF rounded border ring (or solid fallback when radius == 0)
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
                ),
            ));
        }

        // 2c. Main toplevel surface tree (clipped to `inner_rect` + `inner_radii` when shader available)
        if let Some(toplevel) = item.window.toplevel() {
            let wl_surface = toplevel.wl_surface();
            let surface_loc_phys: Point<i32, Physical> =
                inner_rect.loc.to_physical_precise_round(output_scale);
            let surface_elems: Vec<WaylandSurfaceRenderElement<R>> =
                render_elements_from_surface_tree(
                    renderer,
                    wl_surface,
                    surface_loc_phys,
                    output_scale,
                    1.0,
                    Kind::Unspecified,
                );

            let needs_shader_clip = inner_radius > 0.5 || eff_opacity < 0.999;
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
    fn resolve_border_rgba_parses_custom_configured_colors() {
        let border = BorderConfig {
            enabled: true,
            width: 3.0,
            active_color: "#ff0000".to_owned(),
            inactive_color: "#00ff00".to_owned(),
            agent_leased_color: "#0000ff".to_owned(),
            human_preempt_color: "#ffff00".to_owned(),
        };

        let active = resolve_border_rgba(&border, &WorkspaceLeaseState::Unheld, true);
        assert_eq!(active, [1.0, 0.0, 0.0, 1.0]);

        let inactive = resolve_border_rgba(&border, &WorkspaceLeaseState::Unheld, false);
        assert_eq!(inactive, [0.0, 1.0, 0.0, 1.0]);
    }
}
