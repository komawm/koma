//! Render elements for `Koma`:
//! 1. `KomaClippedSurfaceElement<R>` — SDF rounded-corner clipped & opacity-modulated Wayland surface,
//! 2. `KomaRoundedBorderElement` — analytical SDF anti-aliased outer/inner rounded border ring,
//! 3. `KomaBlurredBackdropElement` — frosted-glass `Dual Kawase Blur` slice clipped to rounded window,
//! 4. `KomaSolidColorElement` — fractional-scale solid rects (`#000000` Protected blackout, `grid_225`).

use std::collections::{HashMap, hash_map::Entry};

use koma_model::{AgentCursorFrameSnapshot, LabelPillSnapshot, LogicalPoint};
use koma_proto::WindowId;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Color32F, Frame, ImportAll, ImportMem, Renderer, Texture as _, buffer_y_inverted,
            element::{
                Element, Id, Kind, RenderElement, UnderlyingStorage,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                render_elements,
                surface::WaylandSurfaceRenderElement,
            },
            gles::{
                GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture,
                Uniform, element::PixelShaderElement,
            },
            multigpu::{Error as MultiError, MultiRenderer, gbm::GbmGlesBackend},
            utils::{CommitCounter, DamageSet, OpaqueRegions},
        },
    },
    utils::{
        Buffer, DeviceFd, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
        user_data::UserDataMap,
    },
};

use crate::shaders::{KomaGlesFrame as _, Mat3f, mat3_uniform};

type TtyMultiRenderer<'render> = MultiRenderer<
    'render,
    'render,
    GbmGlesBackend<GlesRenderer, DeviceFd>,
    GbmGlesBackend<GlesRenderer, DeviceFd>,
>;

type TtyMultiError =
    MultiError<GbmGlesBackend<GlesRenderer, DeviceFd>, GbmGlesBackend<GlesRenderer, DeviceFd>>;

/// Bridges `GlesRenderer` (nested Winit / offscreen FBO) and `TtyMultiRenderer` (DRM/KMS)
/// so `KomaRenderElement<R>` can implement `RenderElement<R>` uniformly under `render_elements!`.
pub trait KomaRenderTarget: Renderer + ImportAll + ImportMem {
    fn gles_frame_mut<'a, 'frame, 'buffer>(
        frame: &'a mut Self::Frame<'frame, 'buffer>,
    ) -> &'a mut GlesFrame<'frame, 'buffer>
    where
        'buffer: 'frame,
        Self: 'frame;

    fn map_gles_error(err: GlesError) -> Self::Error;
}

impl KomaRenderTarget for GlesRenderer {
    fn gles_frame_mut<'a, 'frame, 'buffer>(
        frame: &'a mut Self::Frame<'frame, 'buffer>,
    ) -> &'a mut GlesFrame<'frame, 'buffer>
    where
        'buffer: 'frame,
        Self: 'frame,
    {
        frame.as_gles_frame()
    }

    fn map_gles_error(err: GlesError) -> Self::Error {
        err
    }
}

impl<'render> KomaRenderTarget for TtyMultiRenderer<'render> {
    fn gles_frame_mut<'a, 'frame, 'buffer>(
        frame: &'a mut Self::Frame<'frame, 'buffer>,
    ) -> &'a mut GlesFrame<'frame, 'buffer>
    where
        'buffer: 'frame,
        Self: 'frame,
    {
        frame.as_gles_frame()
    }

    fn map_gles_error(err: GlesError) -> Self::Error {
        TtyMultiError::Render(err)
    }
}

// ==============================================================================
// 1. `KomaClippedSurfaceElement<R>` (SDF Rounded-Corner Clipping + Opacity)
// ==============================================================================

#[derive(Debug)]
pub struct KomaClippedSurfaceElement<R: Renderer> {
    inner: WaylandSurfaceRenderElement<R>,
    program: GlesTexProgram,
    clip_geo: Rectangle<f64, Logical>,
    corner_radius: [f32; 4],
    opacity: f32,
    scale: f32,
}

impl<R: Renderer + ImportAll> KomaClippedSurfaceElement<R> {
    #[must_use]
    pub fn new(
        inner: WaylandSurfaceRenderElement<R>,
        program: GlesTexProgram,
        clip_geo: Rectangle<f64, Logical>,
        corner_radius: [f32; 4],
        opacity: f32,
        scale: Scale<f64>,
    ) -> Self {
        Self {
            inner,
            program,
            clip_geo,
            corner_radius,
            opacity: opacity.clamp(0.0, 1.0),
            scale: scale.x as f32,
        }
    }

    fn compute_uniforms(&self) -> Vec<Uniform<'static>> {
        let scale = Scale::from(f64::from(self.scale));
        let elem_geo = self.inner.geometry(scale);
        let clip_phys: Rectangle<i32, Physical> = self.clip_geo.to_physical_precise_round(scale);

        let ew = (elem_geo.size.w.max(1)) as f32;
        let eh = (elem_geo.size.h.max(1)) as f32;
        let gw = (clip_phys.size.w.max(1)) as f32;
        let gh = (clip_phys.size.h.max(1)) as f32;

        let buf_sz = self.inner.buffer_size();
        let bw = (buf_sz.w.max(1)) as f32;
        let bh = (buf_sz.h.max(1)) as f32;

        let view = self.inner.view();
        let sw = (view.src.size.w.max(1.0)) as f32;
        let sh = (view.src.size.h.max(1.0)) as f32;
        let sx = view.src.loc.x as f32;
        let sy = view.src.loc.y as f32;

        let y_invert = if buffer_y_inverted(self.inner.buffer()).unwrap_or(false) {
            Mat3f::from_translation(0.0, 1.0).mul(Mat3f::from_scale(1.0, -1.0))
        } else {
            Mat3f::IDENTITY
        };

        let input_to_geo = Mat3f::from_scale(ew / gw, eh / gh)
            .mul(Mat3f::from_translation(
                (elem_geo.loc.x - clip_phys.loc.x) as f32 / ew,
                (elem_geo.loc.y - clip_phys.loc.y) as f32 / eh,
            ))
            .mul(Mat3f::from_scale(bw / sw, bh / sh))
            .mul(Mat3f::from_translation(-sx / bw, -sy / bh))
            .mul(y_invert);

        vec![
            Uniform::new("koma_scale", self.scale),
            Uniform::new("window_opacity", self.opacity),
            Uniform::new(
                "geo_size",
                (self.clip_geo.size.w as f32, self.clip_geo.size.h as f32),
            ),
            Uniform::new("corner_radius", self.corner_radius),
            mat3_uniform("input_to_geo", input_to_geo),
        ]
    }
}

impl<R: Renderer + ImportAll> Element for KomaClippedSurfaceElement<R> {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        if self.inner.transform() != Transform::Normal {
            return OpaqueRegions::default();
        }
        clipped_surface_opaque_regions(
            self.inner.opaque_regions(scale),
            self.inner.geometry(scale).loc,
            self.clip_geo,
            self.corner_radius,
            self.opacity,
            scale,
        )
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha() * self.opacity
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl<R> RenderElement<R> for KomaClippedSurfaceElement<R>
where
    R: KomaRenderTarget,
    R::TextureId: smithay::backend::renderer::Texture + 'static,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        R::gles_frame_mut(frame)
            .override_default_tex_program(self.program.clone(), self.compute_uniforms());
        let res =
            RenderElement::<R>::draw(&self.inner, frame, src, dst, damage, opaque_regions, cache);
        R::gles_frame_mut(frame).clear_tex_program_override();
        res
    }

    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// Computes physical opaque regions relative to `elem_loc` for a surface clipped to `clip_geo`
/// with `corner_radius` and `opacity`.
///
/// Preserves the inner surface's opaque regions clipped to `clip_geo` when `opacity >= 0.999`,
/// subtracting only the 4 conservative rounded-corner bounding rectangles so underlying windows and
/// wallpaper remain occluded behind opaque rounded surfaces.
#[must_use]
pub fn clipped_surface_opaque_regions(
    inner_regions: OpaqueRegions<i32, Physical>,
    elem_loc: Point<i32, Physical>,
    clip_geo: Rectangle<f64, Logical>,
    corner_radius: [f32; 4],
    opacity: f32,
    scale: Scale<f64>,
) -> OpaqueRegions<i32, Physical> {
    if opacity < 1.0 || inner_regions.is_empty() {
        return OpaqueRegions::default();
    }

    let mut clip_rel = clip_geo.to_physical_precise_round(scale);
    clip_rel.loc -= elem_loc;

    let mut regions: Vec<Rectangle<i32, Physical>> = inner_regions
        .into_iter()
        .filter_map(|rect| rect.intersection(clip_rel))
        .collect();
    if regions.is_empty() || corner_radius.iter().all(|&r| r <= 0.5) {
        return OpaqueRegions::from_slice(&regions);
    }

    let sx = scale
        .x
        .max(f64::from(clip_rel.size.w) / clip_geo.size.w.max(1.0));
    let sy = scale
        .y
        .max(f64::from(clip_rel.size.h) / clip_geo.size.h.max(1.0));
    let corner_size = |r: f32| -> Size<i32, Physical> {
        if r <= 0.0 {
            Size::default()
        } else {
            let w = ((f64::from(r) * sx).ceil() as i32).min(clip_rel.size.w.max(0));
            let h = ((f64::from(r) * sy).ceil() as i32).min(clip_rel.size.h.max(0));
            Size::from((w, h))
        }
    };

    let tl = corner_size(corner_radius[0]);
    let tr = corner_size(corner_radius[1]);
    let br = corner_size(corner_radius[2]);
    let bl = corner_size(corner_radius[3]);
    let corners = [
        Rectangle::new(clip_rel.loc, tl),
        Rectangle::new(
            Point::from((clip_rel.loc.x + clip_rel.size.w - tr.w, clip_rel.loc.y)),
            tr,
        ),
        Rectangle::new(
            Point::from((
                clip_rel.loc.x + clip_rel.size.w - br.w,
                clip_rel.loc.y + clip_rel.size.h - br.h,
            )),
            br,
        ),
        Rectangle::new(
            Point::from((clip_rel.loc.x, clip_rel.loc.y + clip_rel.size.h - bl.h)),
            bl,
        ),
    ];
    regions = Rectangle::subtract_rects_many_in_place(
        regions,
        corners.into_iter().filter(|rect| !rect.is_empty()),
    );
    OpaqueRegions::from_slice(&regions)
}

// ==============================================================================
// 2. `KomaRoundedBorderElement` (Analytical SDF Anti-Aliased Rounded Ring)
// ==============================================================================

/// Computes the `input_to_geo` matrix mapping `[0, 1]^2` across `strip_geo` into `[0, 1]^2`
/// across `full_geo` in physical pixel space so perimeter strips produce seamless SDF edges and
/// linear gradients.
#[must_use]
pub fn rounded_border_input_to_geo(
    strip_geo: Rectangle<f64, Logical>,
    full_geo: Rectangle<f64, Logical>,
    scale: Scale<f64>,
) -> Mat3f {
    let strip_phys: Rectangle<i32, Physical> = strip_geo.to_physical_precise_round(scale);
    let full_phys: Rectangle<i32, Physical> = full_geo.to_physical_precise_round(scale);
    let sw = (strip_phys.size.w.max(1)) as f32;
    let sh = (strip_phys.size.h.max(1)) as f32;
    let fw = (full_phys.size.w.max(1)) as f32;
    let fh = (full_phys.size.h.max(1)) as f32;
    Mat3f::from_scale(sw / fw, sh / fh).mul(Mat3f::from_translation(
        (strip_phys.loc.x - full_phys.loc.x) as f32 / sw,
        (strip_phys.loc.y - full_phys.loc.y) as f32 / sh,
    ))
}

/// Splits a hollow rounded border ring into up to 4 non-overlapping perimeter edge strips around the
/// interior hole where output alpha is provably identically zero.
///
/// Returns `(strip_idx, strip_logical_rect)` pairs where `0..=3` denote top, bottom, left, right
/// bands, or `[(4, full_geo)]` as a seamless single-quad fallback for filled pills
/// (`border_width <= 0.0`) or tiny geometries.
#[must_use]
pub fn split_rounded_border_strips(
    full_geo: Rectangle<f64, Logical>,
    outer_radius: [f32; 4],
    border_width: f32,
    scale: Scale<f64>,
) -> Vec<(u8, Rectangle<f64, Logical>)> {
    if border_width <= 0.0 || full_geo.size.w <= 0.0 || full_geo.size.h <= 0.0 {
        return vec![(4, full_geo)];
    }
    let max_inner_r = outer_radius
        .iter()
        .map(|&r| (r - border_width).max(0.0))
        .fold(0.0_f32, f32::max);
    let inset = f64::from(border_width) + f64::from(max_inner_r) + 2.0;
    if full_geo.size.w <= inset * 2.0 || full_geo.size.h <= inset * 2.0 {
        return vec![(4, full_geo)];
    }

    let full_phys = full_geo.to_physical_precise_round(scale);
    let full_x0 = full_phys.loc.x;
    let full_y0 = full_phys.loc.y;
    let full_x1 = full_x0 + full_phys.size.w;
    let full_y1 = full_y0 + full_phys.size.h;

    let sx = scale.x.max(0.01);
    let sy = scale.y.max(0.01);
    let hole_x0 = ((full_geo.loc.x + inset) * sx)
        .ceil()
        .clamp(f64::from(full_x0), f64::from(full_x1)) as i32;
    let hole_y0 = ((full_geo.loc.y + inset) * sy)
        .ceil()
        .clamp(f64::from(full_y0), f64::from(full_y1)) as i32;
    let hole_x1 = ((full_geo.loc.x + full_geo.size.w - inset) * sx)
        .floor()
        .clamp(f64::from(full_x0), f64::from(full_x1)) as i32;
    let hole_y1 = ((full_geo.loc.y + full_geo.size.h - inset) * sy)
        .floor()
        .clamp(f64::from(full_y0), f64::from(full_y1)) as i32;

    if hole_x1 <= hole_x0 || hole_y1 <= hole_y0 {
        return vec![(4, full_geo)];
    }

    let to_logical = |x: i32, y: i32, w: i32, h: i32| -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((f64::from(x) / sx, f64::from(y) / sy)),
            Size::from((f64::from(w) / sx, f64::from(h) / sy)),
        )
    };

    let mut strips = Vec::with_capacity(4);
    if hole_y0 > full_y0 {
        strips.push((
            0,
            to_logical(full_x0, full_y0, full_phys.size.w, hole_y0 - full_y0),
        ));
    }
    if full_y1 > hole_y1 {
        strips.push((
            1,
            to_logical(full_x0, hole_y1, full_phys.size.w, full_y1 - hole_y1),
        ));
    }
    let mid_h = hole_y1 - hole_y0;
    if hole_x0 > full_x0 && mid_h > 0 {
        strips.push((2, to_logical(full_x0, hole_y0, hole_x0 - full_x0, mid_h)));
    }
    if full_x1 > hole_x1 && mid_h > 0 {
        strips.push((3, to_logical(hole_x1, hole_y0, full_x1 - hole_x1, mid_h)));
    }
    if strips.is_empty() {
        vec![(4, full_geo)]
    } else {
        strips
    }
}

#[derive(Debug, Clone)]
pub struct KomaRoundedBorderElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<f64, Logical>,
    full_geometry: Rectangle<f64, Logical>,
    outer_radius: [f32; 4],
    border_width: f32,
    border_color: [f32; 4],
    border_color_end: [f32; 4],
    gradient_angle_rad: f32,
    opacity: f32,
    scale: f32,
    carrier_tex: GlesTexture,
    program: GlesTexProgram,
}

impl KomaRoundedBorderElement {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        geometry: Rectangle<f64, Logical>,
        outer_radius: [f32; 4],
        border_width: f32,
        border_color: [f32; 4],
        opacity: f32,
        scale: Scale<f64>,
        carrier_tex: GlesTexture,
        program: GlesTexProgram,
    ) -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            geometry,
            full_geometry: geometry,
            outer_radius,
            border_width,
            border_color,
            border_color_end: border_color,
            gradient_angle_rad: 0.0,
            opacity: opacity.clamp(0.0, 1.0),
            scale: scale.x as f32,
            carrier_tex,
            program,
        }
    }

    #[must_use]
    pub fn with_gradient(mut self, border_color_end: [f32; 4], gradient_angle_rad: f32) -> Self {
        self.border_color_end = border_color_end;
        self.gradient_angle_rad = gradient_angle_rad;
        self
    }

    #[must_use]
    pub fn with_full_geometry(mut self, full_geometry: Rectangle<f64, Logical>) -> Self {
        self.full_geometry = full_geometry;
        self
    }

    #[must_use]
    pub fn with_id_and_commit(mut self, id: Id, commit: CommitCounter) -> Self {
        self.id = id;
        self.commit = commit;
        self
    }

    fn uniforms(&self) -> [Uniform<'static>; 8] {
        let scale = Scale::from(f64::from(self.scale.max(0.01)));
        let input_to_geo = rounded_border_input_to_geo(self.geometry, self.full_geometry, scale);
        [
            Uniform::new("koma_scale", self.scale),
            Uniform::new(
                "geo_size",
                (
                    self.full_geometry.size.w as f32,
                    self.full_geometry.size.h as f32,
                ),
            ),
            Uniform::new("outer_radius", self.outer_radius),
            Uniform::new("border_width", self.border_width),
            Uniform::new("border_color", self.border_color),
            Uniform::new("border_color_end", self.border_color_end),
            Uniform::new("gradient_angle_rad", self.gradient_angle_rad),
            mat3_uniform("input_to_geo", input_to_geo),
        ]
    }
}

impl Element for KomaRoundedBorderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((1.0, 1.0)))
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry.to_physical_precise_round(scale)
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.opacity
    }
}

impl<R: KomaRenderTarget> RenderElement<R> for KomaRoundedBorderElement {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        let uniforms = self.uniforms();
        R::gles_frame_mut(frame)
            .render_texture_from_to(
                &self.carrier_tex,
                src,
                dst,
                damage,
                &[],
                Transform::Normal,
                self.opacity,
                Some(&self.program),
                &uniforms,
            )
            .map_err(R::map_gles_error)
    }

    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

// ==============================================================================
// 3. `KomaBlurredBackdropElement` (Dual Kawase Frosted-Glass Slice)
// ==============================================================================

/// Computes `(local_uv_origin, local_uv_scale)` mapping backdrop texture `v_coords` back to
/// normalized `[0, 1]^2` window-local coordinates for rounded-corner masking.
#[must_use]
pub fn blurred_backdrop_local_uv(
    geometry: Rectangle<f64, Logical>,
    viewport_size: Size<f64, Logical>,
    tex_size: Size<i32, Buffer>,
    scale: Scale<f64>,
) -> ((f32, f32), (f32, f32)) {
    let phys: Rectangle<i32, Physical> = geometry.to_physical_precise_round(scale);
    if tex_size.w > 0 && tex_size.h > 0 && phys.size.w > 0 && phys.size.h > 0 {
        let tw = tex_size.w as f32;
        let th = tex_size.h as f32;
        (
            (phys.loc.x as f32 / tw, phys.loc.y as f32 / th),
            (tw / phys.size.w as f32, th / phys.size.h as f32),
        )
    } else {
        let vw = viewport_size.w.max(1.0) as f32;
        let vh = viewport_size.h.max(1.0) as f32;
        let gx = geometry.loc.x as f32;
        let gy = geometry.loc.y as f32;
        let gw = geometry.size.w.max(1.0) as f32;
        let gh = geometry.size.h.max(1.0) as f32;
        ((gx / vw, gy / vh), (vw / gw, vh / gh))
    }
}

#[derive(Debug, Clone)]
pub struct KomaBlurredBackdropElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<f64, Logical>,
    viewport_size: Size<f64, Logical>,
    corner_radius: [f32; 4],
    opacity: f32,
    scale: f32,
    noise: f32,
    contrast: f32,
    brightness: f32,
    pre_toned: bool,
    blurred_tex: GlesTexture,
    program: GlesTexProgram,
}

impl KomaBlurredBackdropElement {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        geometry: Rectangle<f64, Logical>,
        viewport_size: Size<f64, Logical>,
        corner_radius: [f32; 4],
        opacity: f32,
        scale: Scale<f64>,
        noise: f32,
        contrast: f32,
        brightness: f32,
        blurred_tex: GlesTexture,
        program: GlesTexProgram,
    ) -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            geometry,
            viewport_size,
            corner_radius,
            opacity: opacity.clamp(0.0, 1.0),
            scale: scale.x as f32,
            noise,
            contrast,
            brightness,
            pre_toned: false,
            blurred_tex,
            program,
        }
    }

    #[must_use]
    pub fn with_id_and_commit(mut self, id: Id, commit: CommitCounter) -> Self {
        self.id = id;
        self.commit = commit;
        self
    }

    #[must_use]
    pub fn with_pre_toned(mut self, pre_toned: bool) -> Self {
        self.pre_toned = pre_toned;
        self
    }

    fn uniforms(&self) -> [Uniform<'static>; 9] {
        let scale = Scale::from(f64::from(self.scale.max(0.01)));
        let (uv_origin, uv_scale) = blurred_backdrop_local_uv(
            self.geometry,
            self.viewport_size,
            self.blurred_tex.size(),
            scale,
        );
        let gw = self.geometry.size.w.max(1.0) as f32;
        let gh = self.geometry.size.h.max(1.0) as f32;

        [
            Uniform::new("koma_scale", self.scale),
            Uniform::new("geo_size", (gw, gh)),
            Uniform::new("corner_radius", self.corner_radius),
            Uniform::new("local_uv_origin", uv_origin),
            Uniform::new("local_uv_scale", uv_scale),
            Uniform::new("blur_noise", self.noise),
            Uniform::new("blur_contrast", self.contrast),
            Uniform::new("blur_brightness", self.brightness),
            Uniform::new(
                "blur_pre_toned",
                if self.pre_toned { 1.0_f32 } else { 0.0_f32 },
            ),
        ]
    }
}

impl Element for KomaBlurredBackdropElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        let scale = Scale::from(f64::from(self.scale));
        let phys: Rectangle<i32, Physical> = self.geometry.to_physical_precise_round(scale);
        Rectangle::new(
            Point::from((f64::from(phys.loc.x), f64::from(phys.loc.y))),
            Size::from((f64::from(phys.size.w), f64::from(phys.size.h))),
        )
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry.to_physical_precise_round(scale)
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.opacity
    }
}

impl<R: KomaRenderTarget> RenderElement<R> for KomaBlurredBackdropElement {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        let uniforms = self.uniforms();
        R::gles_frame_mut(frame)
            .render_texture_from_to(
                &self.blurred_tex,
                src,
                dst,
                damage,
                &[],
                Transform::Normal,
                self.opacity,
                Some(&self.program),
                &uniforms,
            )
            .map_err(R::map_gles_error)
    }

    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

// ==============================================================================
// 4. `KomaSolidColorElement` & Unified `KomaRenderElement<R>`
// ==============================================================================

/// Converts a straight-alpha `RGBA` color into the premultiplied form the GLES frame expects.
///
/// Every Koma color constant and pixel-shader uniform is authored in straight (non-premultiplied)
/// alpha; the pixel shaders multiply `rgb * a` themselves (see `shaders.rs`). Smithay's
/// `Frame::draw_solid` does *not* premultiply, and its blend function is
/// `ONE, ONE_MINUS_SRC_ALPHA` (premultiplied), so a straight-alpha solid color has to be
/// premultiplied here or it blends in at `1 / alpha` times its intended intensity.
#[must_use]
pub fn premultiplied_rgba(rgba: [f32; 4]) -> [f32; 4] {
    let a = rgba[3].clamp(0.0, 1.0);
    [rgba[0] * a, rgba[1] * a, rgba[2] * a, a]
}

#[derive(Debug, Clone)]
pub struct KomaSolidColorElement {
    id: Id,
    geometry: Rectangle<f64, Logical>,
    commit: CommitCounter,
    /// Straight-alpha color as authored by callers; premultiplied only at draw time.
    color: Color32F,
    kind: Kind,
}

impl KomaSolidColorElement {
    #[must_use]
    pub fn new(
        id: Id,
        geometry: Rectangle<f64, Logical>,
        commit: CommitCounter,
        color: impl Into<Color32F>,
        kind: Kind,
    ) -> Self {
        Self {
            id,
            geometry,
            commit,
            color: color.into(),
            kind,
        }
    }

    #[must_use]
    pub fn oneshot(rect: Rectangle<f64, Logical>, rgba: [f32; 4]) -> Self {
        Self::new(
            Id::new(),
            rect,
            CommitCounter::default(),
            Color32F::new(rgba[0], rgba[1], rgba[2], rgba[3]),
            Kind::Unspecified,
        )
    }
}

impl Element for KomaSolidColorElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((1.0, 1.0)))
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry.to_physical_precise_round(scale)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        if self.color.is_opaque() {
            let rect = Rectangle::from_size(self.geometry.size).to_physical_precise_down(scale);
            OpaqueRegions::from_slice(&[rect])
        } else {
            OpaqueRegions::default()
        }
    }

    fn alpha(&self) -> f32 {
        self.color.a()
    }

    fn kind(&self) -> Kind {
        self.kind
    }
}

impl<R: Renderer> RenderElement<R> for KomaSolidColorElement {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        let [r, g, b, a] = premultiplied_rgba([
            self.color.r(),
            self.color.g(),
            self.color.b(),
            self.color.a(),
        ]);
        frame.draw_solid(dst, damage, Color32F::new(r, g, b, a))
    }

    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

// ==============================================================================
// 5. `KomaDropShadowElement` (Analytical Gaussian/Erf Rounded Drop Shadow)
// ==============================================================================

#[inline]
fn phys_pos_i32(v: i32, scale: f64) -> i32 {
    (f64::from(v) * scale).round() as i32
}

#[inline]
fn phys_span_end_i32(start: i32, len: i32, scale: f64) -> i32 {
    phys_pos_i32(start, scale) + phys_pos_i32(len, scale)
}

/// Splits a drop-shadow bounding `area` around the interior cutout of `window_geo` (inset by
/// `max_window_radius + 2.0` logical pixels) where the cutout term `(1.0 - inside)` is provably
/// identically zero.
///
/// Returns `(strip_idx, strip_area)` pairs (`0..=3` for top, bottom, left, right perimeter bands,
/// or `[(4, area)]` when no cutout hole overlaps `area`). If `area` lies entirely inside the
/// zero-alpha interior cutout, returns an empty vector so zero pixels are shaded.
#[must_use]
pub fn split_drop_shadow_strips(
    area: Rectangle<i32, Logical>,
    window_geo: Rectangle<f64, Logical>,
    window_radii: [f32; 4],
    scale: Scale<f64>,
) -> Vec<(u8, Rectangle<i32, Logical>)> {
    if area.size.w <= 0 || area.size.h <= 0 {
        return Vec::new();
    }
    if window_geo.size.w <= 0.0 || window_geo.size.h <= 0.0 {
        return vec![(4, area)];
    }

    let max_win_r = f64::from(
        window_radii
            .iter()
            .copied()
            .fold(0.0_f32, f32::max)
            .max(0.0),
    );
    let inset = max_win_r + 2.0;
    if window_geo.size.w <= inset * 2.0 || window_geo.size.h <= inset * 2.0 {
        return vec![(4, area)];
    }

    let raw_x0 = (window_geo.loc.x + inset).ceil() as i32;
    let raw_y0 = (window_geo.loc.y + inset).ceil() as i32;
    let raw_x1 = (window_geo.loc.x + window_geo.size.w - inset).floor() as i32;
    let raw_y1 = (window_geo.loc.y + window_geo.size.h - inset).floor() as i32;

    let area_x0 = area.loc.x;
    let area_y0 = area.loc.y;
    let area_x1 = area_x0 + area.size.w;
    let area_y1 = area_y0 + area.size.h;

    let mut x0 = raw_x0.max(area_x0);
    let mut y0 = raw_y0.max(area_y0);
    let mut x1 = raw_x1.min(area_x1);
    let mut y1 = raw_y1.min(area_y1);

    if x1 <= x0 || y1 <= y0 {
        return vec![(4, area)];
    }

    // Align split boundaries inward (shrinking the hole by 0..=4 logical px if needed) so
    // `Rectangle::to_physical_precise_round(scale)` never opens a 1px physical gap at fractional
    // scales like 1.25x or 1.6x.
    let sx = scale.x.max(0.01);
    let sy = scale.y.max(0.01);

    if y0 > area_y0 {
        let chosen = (0..=4)
            .filter(|&k| y0 + k < y1)
            .find(|&k| {
                phys_span_end_i32(area_y0, (y0 + k) - area_y0, sy) == phys_pos_i32(y0 + k, sy)
            })
            .or_else(|| {
                (0..=4).filter(|&k| y0 + k < y1).find(|&k| {
                    phys_span_end_i32(area_y0, (y0 + k) - area_y0, sy) >= phys_pos_i32(y0 + k, sy)
                })
            });
        if let Some(k) = chosen {
            y0 += k;
        }
    }

    if y1 < area_y1 {
        let area_phys_end_y = phys_span_end_i32(area_y0, area.size.h, sy);
        let chosen = (0..=4)
            .filter(|&k| y1 - k > y0)
            .find(|&k| {
                let cand = y1 - k;
                phys_span_end_i32(y0, cand - y0, sy) == phys_pos_i32(cand, sy)
                    && phys_span_end_i32(cand, area_y1 - cand, sy) == area_phys_end_y
            })
            .or_else(|| {
                (0..=4).filter(|&k| y1 - k > y0).find(|&k| {
                    let cand = y1 - k;
                    phys_span_end_i32(y0, cand - y0, sy) >= phys_pos_i32(cand, sy)
                        && phys_span_end_i32(cand, area_y1 - cand, sy) >= area_phys_end_y
                })
            });
        if let Some(k) = chosen {
            y1 -= k;
        }
    }

    if x0 > area_x0 {
        let chosen = (0..=4)
            .filter(|&k| x0 + k < x1)
            .find(|&k| {
                phys_span_end_i32(area_x0, (x0 + k) - area_x0, sx) == phys_pos_i32(x0 + k, sx)
            })
            .or_else(|| {
                (0..=4).filter(|&k| x0 + k < x1).find(|&k| {
                    phys_span_end_i32(area_x0, (x0 + k) - area_x0, sx) >= phys_pos_i32(x0 + k, sx)
                })
            });
        if let Some(k) = chosen {
            x0 += k;
        }
    }

    if x1 < area_x1 {
        let area_phys_end_x = phys_span_end_i32(area_x0, area.size.w, sx);
        let chosen = (0..=4)
            .filter(|&k| x1 - k > x0)
            .find(|&k| phys_span_end_i32(x1 - k, area_x1 - (x1 - k), sx) == area_phys_end_x)
            .or_else(|| {
                (0..=4)
                    .filter(|&k| x1 - k > x0)
                    .find(|&k| phys_span_end_i32(x1 - k, area_x1 - (x1 - k), sx) >= area_phys_end_x)
            });
        if let Some(k) = chosen {
            x1 -= k;
        }
    }

    if x1 <= x0 || y1 <= y0 {
        return vec![(4, area)];
    }

    let aligned_y = (y0 == area_y0
        || phys_span_end_i32(area_y0, y0 - area_y0, sy) == phys_pos_i32(y0, sy))
        && (y1 == area_y1
            || (phys_span_end_i32(y0, y1 - y0, sy) == phys_pos_i32(y1, sy)
                && phys_span_end_i32(y1, area_y1 - y1, sy)
                    == phys_span_end_i32(area_y0, area.size.h, sy)));
    let aligned_x = (x0 == area_x0
        || phys_span_end_i32(area_x0, x0 - area_x0, sx) == phys_pos_i32(x0, sx))
        && (x1 == area_x1
            || phys_span_end_i32(x1, area_x1 - x1, sx)
                == phys_span_end_i32(area_x0, area.size.w, sx));
    if !aligned_x || !aligned_y {
        return vec![(4, area)];
    }

    let mut strips = Vec::with_capacity(4);
    if y0 > area_y0 {
        strips.push((
            0,
            Rectangle::new(
                Point::from((area_x0, area_y0)),
                Size::from((area.size.w, y0 - area_y0)),
            ),
        ));
    }
    if area_y1 > y1 {
        strips.push((
            1,
            Rectangle::new(
                Point::from((area_x0, y1)),
                Size::from((area.size.w, area_y1 - y1)),
            ),
        ));
    }
    let mid_h = y1 - y0;
    if x0 > area_x0 && mid_h > 0 {
        strips.push((
            2,
            Rectangle::new(
                Point::from((area_x0, y0)),
                Size::from((x0 - area_x0, mid_h)),
            ),
        ));
    }
    if area_x1 > x1 && mid_h > 0 {
        strips.push((
            3,
            Rectangle::new(Point::from((x1, y0)), Size::from((area_x1 - x1, mid_h))),
        ));
    }
    strips
}

/// Analytical GLES2 rounded-rectangle drop shadow element wrapping Smithay's [`PixelShaderElement`]
/// for both `GlesRenderer` and `TtyMultiRenderer` via [`KomaRenderTarget`].
#[derive(Debug, Clone)]
pub struct KomaDropShadowElement {
    inner: PixelShaderElement,
}

impl KomaDropShadowElement {
    #[allow(clippy::too_many_arguments)]
    fn build_uniforms(
        area: Rectangle<i32, Logical>,
        shadow_geo: Rectangle<f64, Logical>,
        shadow_radii: [f32; 4],
        sigma: f32,
        sharp: bool,
        color: [f32; 4],
        window_geo: Rectangle<f64, Logical>,
        window_radii: [f32; 4],
        opacity: f32,
        scale: Scale<f64>,
    ) -> Vec<Uniform<'static>> {
        let area_w = area.size.w.max(1) as f32;
        let area_h = area.size.h.max(1) as f32;
        let input_to_geo = Mat3f::from_translation(
            area.loc.x as f32 - shadow_geo.loc.x as f32,
            area.loc.y as f32 - shadow_geo.loc.y as f32,
        )
        .mul(Mat3f::from_scale(area_w, area_h));
        let window_input_to_geo = Mat3f::from_translation(
            area.loc.x as f32 - window_geo.loc.x as f32,
            area.loc.y as f32 - window_geo.loc.y as f32,
        )
        .mul(Mat3f::from_scale(area_w, area_h));
        let eff_color = [
            color[0],
            color[1],
            color[2],
            color[3] * opacity.clamp(0.0, 1.0),
        ];

        vec![
            Uniform::new("koma_scale", scale.x.max(1.0) as f32),
            Uniform::new("shadow_color", eff_color),
            Uniform::new("shadow_sigma", sigma.max(0.0)),
            Uniform::new("shadow_sharp", if sharp { 1.0_f32 } else { 0.0_f32 }),
            Uniform::new(
                "geo_size",
                (
                    shadow_geo.size.w.max(1.0) as f32,
                    shadow_geo.size.h.max(1.0) as f32,
                ),
            ),
            Uniform::new("corner_radius", shadow_radii),
            mat3_uniform("input_to_geo", input_to_geo),
            Uniform::new(
                "window_geo_size",
                (
                    window_geo.size.w.max(0.0) as f32,
                    window_geo.size.h.max(0.0) as f32,
                ),
            ),
            Uniform::new("window_corner_radius", window_radii),
            mat3_uniform("window_input_to_geo", window_input_to_geo),
        ]
    }

    /// Builds a drop shadow element over `area`, rendering `shadow_geo` with optional
    /// interior cutout matching `window_geo` so translucent windows are never darkened inside.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        area: Rectangle<i32, Logical>,
        shadow_geo: Rectangle<f64, Logical>,
        shadow_radii: [f32; 4],
        sigma: f32,
        sharp: bool,
        color: [f32; 4],
        window_geo: Rectangle<f64, Logical>,
        window_radii: [f32; 4],
        opacity: f32,
        scale: Scale<f64>,
        program: GlesPixelProgram,
    ) -> Self {
        let uniforms = Self::build_uniforms(
            area,
            shadow_geo,
            shadow_radii,
            sigma,
            sharp,
            color,
            window_geo,
            window_radii,
            opacity,
            scale,
        );

        Self {
            // Keep `PixelShaderElement` alpha at `1.0` and fold `opacity` into `shadow_color.a`
            // so cached instances update in-place across opacity transitions without regenerating `Id`.
            inner: PixelShaderElement::new(program, area, None, 1.0, uniforms, Kind::Unspecified),
        }
    }

    pub(crate) fn update(&mut self, params: &DropShadowParams) {
        self.inner.resize(params.area, None);
        let uniforms = Self::build_uniforms(
            params.area,
            params.shadow_geo,
            params.shadow_radii,
            params.sigma,
            params.sharp,
            params.color,
            params.window_geo,
            params.window_radii,
            params.opacity,
            Scale::from(f64::from(params.scale)),
        );
        self.inner.update_uniforms(uniforms);
    }
}

macro_rules! impl_pixel_shader_wrapper {
    ($ty:ident) => {
        impl Element for $ty {
            fn id(&self) -> &Id {
                self.inner.id()
            }

            fn current_commit(&self) -> CommitCounter {
                self.inner.current_commit()
            }

            fn src(&self) -> Rectangle<f64, Buffer> {
                self.inner.src()
            }

            fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
                self.inner.geometry(scale)
            }

            fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
                self.inner.opaque_regions(scale)
            }

            fn alpha(&self) -> f32 {
                self.inner.alpha()
            }

            fn kind(&self) -> Kind {
                self.inner.kind()
            }
        }

        impl<R: KomaRenderTarget> RenderElement<R> for $ty {
            fn draw(
                &self,
                frame: &mut R::Frame<'_, '_>,
                src: Rectangle<f64, Buffer>,
                dst: Rectangle<i32, Physical>,
                damage: &[Rectangle<i32, Physical>],
                opaque_regions: &[Rectangle<i32, Physical>],
                cache: Option<&UserDataMap>,
            ) -> Result<(), R::Error> {
                RenderElement::<GlesRenderer>::draw(
                    &self.inner,
                    R::gles_frame_mut(frame),
                    src,
                    dst,
                    damage,
                    opaque_regions,
                    cache,
                )
                .map_err(R::map_gles_error)
            }

            fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
                None
            }
        }
    };
}

impl_pixel_shader_wrapper!(KomaDropShadowElement);

// ==============================================================================
// 6. `KomaAgentCursorElement` & `KomaAgentTrailElement` (AI Virtual Cursor)
// ==============================================================================

/// Analytical SDF dart cursor, ambient aura fog, and expanding click-ripple shader element.
#[derive(Debug, Clone)]
pub struct KomaAgentCursorElement {
    inner: PixelShaderElement,
}

impl KomaAgentCursorElement {
    /// Builds an agent cursor render element covering the dart arrow, fog, and active ripples.
    #[must_use]
    pub fn from_snapshot(
        snapshot: &AgentCursorFrameSnapshot,
        scale: Scale<f64>,
        program: GlesPixelProgram,
    ) -> Self {
        let tip_x = snapshot.tip_pos.x;
        let tip_y = snapshot.tip_pos.y;
        let body_cx = tip_x + snapshot.body_offset.x;
        let body_cy = tip_y + snapshot.body_offset.y;
        let body_pad = f64::from(snapshot.size * snapshot.body_scale.max(0.5) * 1.45) + 6.0;

        let mut min_x = body_cx - body_pad;
        let mut min_y = body_cy - body_pad;
        let mut max_x = body_cx + body_pad;
        let mut max_y = body_cy + body_pad;

        if snapshot.fog_opacity > 0.001 {
            let fog_cx = tip_x + snapshot.fog_offset.x;
            let fog_cy = tip_y + snapshot.fog_offset.y;
            let fog_pad = f64::from(snapshot.size * snapshot.fog_scale * 1.25) + 4.0;
            min_x = min_x.min(fog_cx - fog_pad);
            min_y = min_y.min(fog_cy - fog_pad);
            max_x = max_x.max(fog_cx + fog_pad);
            max_y = max_y.max(fog_cy + fog_pad);
        }

        for &(center, radius, alpha) in &snapshot.ripples {
            if alpha > 0.001 && radius > 0.5 {
                let r_pad = f64::from(radius) + 6.0;
                min_x = min_x.min(center.x - r_pad);
                min_y = min_y.min(center.y - r_pad);
                max_x = max_x.max(center.x + r_pad);
                max_y = max_y.max(center.y + r_pad);
            }
        }

        let area_x = min_x.floor() as i32;
        let area_y = min_y.floor() as i32;
        let area_w = (max_x.ceil() as i32 - area_x).max(2);
        let area_h = (max_y.ceil() as i32 - area_y).max(2);
        let area = Rectangle::new(Point::from((area_x, area_y)), Size::from((area_w, area_h)));

        let pack_ripple = |idx: usize| -> [f32; 4] {
            snapshot
                .ripples
                .get(idx)
                .map_or([0.0; 4], |&(center, radius, alpha)| {
                    [
                        (center.x - f64::from(area_x)) as f32,
                        (center.y - f64::from(area_y)) as f32,
                        radius,
                        alpha,
                    ]
                })
        };

        let uniforms = vec![
            Uniform::new("elem_size", (area_w as f32, area_h as f32)),
            Uniform::new("koma_scale", scale.x.max(1.0) as f32),
            Uniform::new(
                "tip_local",
                (
                    (tip_x - f64::from(area_x)) as f32,
                    (tip_y - f64::from(area_y)) as f32,
                ),
            ),
            Uniform::new(
                "body_offset",
                (snapshot.body_offset.x as f32, snapshot.body_offset.y as f32),
            ),
            Uniform::new("cursor_rot", snapshot.rotation_rad),
            Uniform::new("cursor_size", snapshot.size),
            Uniform::new("body_scale", snapshot.body_scale),
            Uniform::new("accent_color", snapshot.accent_rgba),
            Uniform::new(
                "fog_offset",
                (snapshot.fog_offset.x as f32, snapshot.fog_offset.y as f32),
            ),
            Uniform::new("fog_scale", snapshot.fog_scale),
            Uniform::new("fog_opacity", snapshot.fog_opacity),
            Uniform::new("ripple0", pack_ripple(0)),
            Uniform::new("ripple1", pack_ripple(1)),
        ];

        Self {
            inner: PixelShaderElement::new(
                program,
                area,
                None,
                snapshot.opacity.clamp(0.0, 1.0),
                uniforms,
                Kind::Unspecified,
            ),
        }
    }
}

impl_pixel_shader_wrapper!(KomaAgentCursorElement);

/// Tapered capsule motion-trail shader element supporting up to 8 control points.
#[derive(Debug, Clone)]
pub struct KomaAgentTrailElement {
    inner: PixelShaderElement,
}

impl KomaAgentTrailElement {
    /// Builds a tapered trail ribbon element when at least 2 active points exist.
    #[must_use]
    pub fn from_points(
        points: &[(LogicalPoint, f32, f32)],
        accent_rgba: [f32; 4],
        opacity: f32,
        scale: Scale<f64>,
        program: GlesPixelProgram,
    ) -> Option<Self> {
        let count = points.len().min(8);
        if count < 2 || opacity <= 0.001 {
            return None;
        }
        let slice = &points[..count];

        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        let mut max_hw = 1.0_f64;
        for &(p, _, hw) in slice {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
            max_hw = max_hw.max(f64::from(hw));
        }
        let pad = max_hw + 4.0;
        let area_x = (min_x - pad).floor() as i32;
        let area_y = (min_y - pad).floor() as i32;
        let area_w = ((max_x + pad).ceil() as i32 - area_x).max(2);
        let area_h = ((max_y + pad).ceil() as i32 - area_y).max(2);
        let area = Rectangle::new(Point::from((area_x, area_y)), Size::from((area_w, area_h)));

        let pack_pt = |idx: usize| -> [f32; 4] {
            slice.get(idx).map_or([0.0; 4], |&(p, pt_alpha, hw)| {
                [
                    (p.x - f64::from(area_x)) as f32,
                    (p.y - f64::from(area_y)) as f32,
                    pt_alpha,
                    hw,
                ]
            })
        };

        let uniforms = vec![
            Uniform::new("elem_size", (area_w as f32, area_h as f32)),
            Uniform::new("koma_scale", scale.x.max(1.0) as f32),
            Uniform::new("accent_color", accent_rgba),
            Uniform::new("point_count", count as f32),
            Uniform::new("trail_p0", pack_pt(0)),
            Uniform::new("trail_p1", pack_pt(1)),
            Uniform::new("trail_p2", pack_pt(2)),
            Uniform::new("trail_p3", pack_pt(3)),
            Uniform::new("trail_p4", pack_pt(4)),
            Uniform::new("trail_p5", pack_pt(5)),
            Uniform::new("trail_p6", pack_pt(6)),
            Uniform::new("trail_p7", pack_pt(7)),
        ];

        Some(Self {
            inner: PixelShaderElement::new(
                program,
                area,
                None,
                opacity.clamp(0.0, 1.0),
                uniforms,
                Kind::Unspecified,
            ),
        })
    }
}

impl_pixel_shader_wrapper!(KomaAgentTrailElement);

render_elements! {
    pub KomaRenderElement<R> where R: KomaRenderTarget, R::TextureId: 'static;
    Surface = WaylandSurfaceRenderElement<R>,
    ClippedSurface = KomaClippedSurfaceElement<R>,
    RoundedBorder = KomaRoundedBorderElement,
    BlurredBackdrop = KomaBlurredBackdropElement,
    DropShadow = KomaDropShadowElement,
    AgentCursor = KomaAgentCursorElement,
    AgentTrail = KomaAgentTrailElement,
    MemoryBuffer = MemoryRenderBufferRenderElement<R>,
    Solid = KomaSolidColorElement,
}

// ==============================================================================
// 7. `FrameDecorationCache` (Persistent Element `Id` & `CommitCounter` Tracking)
// ==============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BorderRole {
    FocusRing,
    Border,
    DropPreview,
    AgentCaret,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowSolidKind {
    ProtectedBlackout,
    BorderFallback,
    FocusRingFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverlaySolidKind {
    ScratchpadScrim,
    ConfigErrorBanner,
    DropPreviewFill(u8),
    AgentCaretFill,
    AgentCaretEdge(u8),
    Grid225Line { is_vertical: bool, idx: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecorationSlot {
    FocusRingStrip(u8),
    BorderStrip(u8),
    DropShadowStrip(u8),
    BlurredBackdrop,
    Solid(WindowSolidKind),
    TabIndicatorBorder,
    TabIndicatorSolid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecorationKey {
    Window {
        win_id: WindowId,
        slot: DecorationSlot,
    },
    OverlayBorderStrip {
        role: BorderRole,
        scope: u8,
        strip: u8,
    },
    OverlaySolid(OverlaySolidKind),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolidColorParams {
    pub rect: Rectangle<f64, Logical>,
    pub rgba: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoundedBorderParams {
    pub geometry: Rectangle<f64, Logical>,
    pub full_geometry: Rectangle<f64, Logical>,
    pub outer_radius: [f32; 4],
    pub border_width: f32,
    pub border_color: [f32; 4],
    pub border_color_end: [f32; 4],
    pub gradient_angle_rad: f32,
    pub opacity: f32,
    pub scale: f32,
    pub carrier_tex_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlurredBackdropParams {
    pub geometry: Rectangle<f64, Logical>,
    pub viewport_size: Size<f64, Logical>,
    pub corner_radius: [f32; 4],
    pub opacity: f32,
    pub scale: f32,
    pub noise: f32,
    pub contrast: f32,
    pub brightness: f32,
    pub pre_toned: bool,
    pub tex_id: u32,
    pub tex_size: Size<i32, Buffer>,
    pub blur_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropShadowParams {
    pub area: Rectangle<i32, Logical>,
    pub shadow_geo: Rectangle<f64, Logical>,
    pub shadow_radii: [f32; 4],
    pub sigma: f32,
    pub sharp: bool,
    pub color: [f32; 4],
    pub window_geo: Rectangle<f64, Logical>,
    pub window_radii: [f32; 4],
    pub opacity: f32,
    pub scale: f32,
}

#[derive(Debug, Clone)]
struct CachedSlot<P, E> {
    params: P,
    id: Id,
    commit: CommitCounter,
    element: E,
    seen_gen: u64,
}

fn resolve_cached_slot<P: PartialEq, E: Clone>(
    map: &mut HashMap<DecorationKey, CachedSlot<P, E>>,
    key: DecorationKey,
    generation: u64,
    params: P,
    build: impl FnOnce(Id, CommitCounter, &P) -> E,
) -> E {
    match map.entry(key) {
        Entry::Occupied(mut occ) => {
            let entry = occ.get_mut();
            entry.seen_gen = generation;
            if entry.params != params {
                entry.commit.increment();
                entry.element = build(entry.id.clone(), entry.commit, &params);
                entry.params = params;
            }
            entry.element.clone()
        }
        Entry::Vacant(vac) => {
            let id = Id::new();
            let commit = CommitCounter::default();
            let element = build(id.clone(), commit, &params);
            vac.insert(CachedSlot {
                params,
                id,
                commit,
                element: element.clone(),
                seen_gen: generation,
            });
            element
        }
    }
}

/// Per-output persistent cache preserving Smithay [`Id`] and monotonic [`CommitCounter`] across
/// frames for borders, focus rings, drop shadows, blurred backdrops, and overlay solid elements.
///
/// Entries not touched between [`begin_frame`](Self::begin_frame) and [`end_frame`](Self::end_frame)
/// are evicted automatically when windows close or unmap.
#[derive(Debug, Default)]
pub struct FrameDecorationCache {
    generation: u64,
    drop_preview_seq: u8,
    solid_slots: HashMap<DecorationKey, CachedSlot<SolidColorParams, KomaSolidColorElement>>,
    border_slots: HashMap<DecorationKey, CachedSlot<RoundedBorderParams, KomaRoundedBorderElement>>,
    shadow_slots: HashMap<DecorationKey, CachedSlot<DropShadowParams, KomaDropShadowElement>>,
    blur_slots:
        HashMap<DecorationKey, CachedSlot<BlurredBackdropParams, KomaBlurredBackdropElement>>,
}

impl FrameDecorationCache {
    /// Starts a new presentation frame and resets per-frame overlay sub-scopes.
    pub fn begin_frame(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.drop_preview_seq = 0;
    }

    /// Sweeps entries that were not emitted during the current frame.
    pub fn end_frame(&mut self) {
        let generation = self.generation;
        self.solid_slots.retain(|_, v| v.seen_gen == generation);
        self.border_slots.retain(|_, v| v.seen_gen == generation);
        self.shadow_slots.retain(|_, v| v.seen_gen == generation);
        self.blur_slots.retain(|_, v| v.seen_gen == generation);
    }

    /// Returns a unique per-frame occurrence index for drop-zone preview overlays.
    pub fn next_drop_preview_scope(&mut self) -> u8 {
        let scope = self.drop_preview_seq;
        self.drop_preview_seq = self.drop_preview_seq.saturating_add(1);
        scope
    }

    /// Returns the number of cached entries across all element kinds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.solid_slots.len()
            + self.border_slots.len()
            + self.shadow_slots.len()
            + self.blur_slots.len()
    }

    /// Returns `true` when no decoration entries are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn solid(
        &mut self,
        key: DecorationKey,
        rect: Rectangle<f64, Logical>,
        rgba: [f32; 4],
    ) -> KomaSolidColorElement {
        let params = SolidColorParams { rect, rgba };
        resolve_cached_slot(
            &mut self.solid_slots,
            key,
            self.generation,
            params,
            |id, commit, p| {
                KomaSolidColorElement::new(
                    id,
                    p.rect,
                    commit,
                    Color32F::new(p.rgba[0], p.rgba[1], p.rgba[2], p.rgba[3]),
                    Kind::Unspecified,
                )
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn rounded_border(
        &mut self,
        key: DecorationKey,
        geometry: Rectangle<f64, Logical>,
        full_geometry: Rectangle<f64, Logical>,
        outer_radius: [f32; 4],
        border_width: f32,
        border_color: [f32; 4],
        border_color_end: [f32; 4],
        gradient_angle_rad: f32,
        opacity: f32,
        scale: Scale<f64>,
        carrier_tex: &GlesTexture,
        program: &GlesTexProgram,
    ) -> KomaRoundedBorderElement {
        let params = RoundedBorderParams {
            geometry,
            full_geometry,
            outer_radius,
            border_width,
            border_color,
            border_color_end,
            gradient_angle_rad,
            opacity: opacity.clamp(0.0, 1.0),
            scale: scale.x as f32,
            carrier_tex_id: carrier_tex.tex_id(),
        };
        resolve_cached_slot(
            &mut self.border_slots,
            key,
            self.generation,
            params,
            |id, commit, p| {
                KomaRoundedBorderElement::new(
                    p.geometry,
                    p.outer_radius,
                    p.border_width,
                    p.border_color,
                    p.opacity,
                    Scale::from(f64::from(p.scale)),
                    carrier_tex.clone(),
                    program.clone(),
                )
                .with_gradient(p.border_color_end, p.gradient_angle_rad)
                .with_full_geometry(p.full_geometry)
                .with_id_and_commit(id, commit)
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn blurred_backdrop(
        &mut self,
        key: DecorationKey,
        geometry: Rectangle<f64, Logical>,
        viewport_size: Size<f64, Logical>,
        corner_radius: [f32; 4],
        opacity: f32,
        scale: Scale<f64>,
        noise: f32,
        contrast: f32,
        brightness: f32,
        pre_toned: bool,
        blur_generation: u64,
        blurred_tex: &GlesTexture,
        program: &GlesTexProgram,
    ) -> KomaBlurredBackdropElement {
        let params = BlurredBackdropParams {
            geometry,
            viewport_size,
            corner_radius,
            opacity: opacity.clamp(0.0, 1.0),
            scale: scale.x as f32,
            noise,
            contrast,
            brightness,
            pre_toned,
            tex_id: blurred_tex.tex_id(),
            tex_size: blurred_tex.size(),
            blur_generation,
        };
        resolve_cached_slot(
            &mut self.blur_slots,
            key,
            self.generation,
            params,
            |id, commit, p| {
                KomaBlurredBackdropElement::new(
                    p.geometry,
                    p.viewport_size,
                    p.corner_radius,
                    p.opacity,
                    Scale::from(f64::from(p.scale)),
                    p.noise,
                    p.contrast,
                    p.brightness,
                    blurred_tex.clone(),
                    program.clone(),
                )
                .with_pre_toned(p.pre_toned)
                .with_id_and_commit(id, commit)
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn drop_shadow(
        &mut self,
        key: DecorationKey,
        area: Rectangle<i32, Logical>,
        shadow_geo: Rectangle<f64, Logical>,
        shadow_radii: [f32; 4],
        sigma: f32,
        sharp: bool,
        color: [f32; 4],
        window_geo: Rectangle<f64, Logical>,
        window_radii: [f32; 4],
        opacity: f32,
        scale: Scale<f64>,
        program: &GlesPixelProgram,
    ) -> KomaDropShadowElement {
        let params = DropShadowParams {
            area,
            shadow_geo,
            shadow_radii,
            sigma,
            sharp,
            color,
            window_geo,
            window_radii,
            opacity: opacity.clamp(0.0, 1.0),
            scale: scale.x.max(1.0) as f32,
        };
        match self.shadow_slots.entry(key) {
            Entry::Occupied(mut occ) => {
                let entry = occ.get_mut();
                entry.seen_gen = self.generation;
                if entry.params != params {
                    entry.element.update(&params);
                    entry.commit = entry.element.current_commit();
                    entry.params = params;
                }
                entry.element.clone()
            }
            Entry::Vacant(vac) => {
                let element = KomaDropShadowElement::new(
                    params.area,
                    params.shadow_geo,
                    params.shadow_radii,
                    params.sigma,
                    params.sharp,
                    params.color,
                    params.window_geo,
                    params.window_radii,
                    params.opacity,
                    Scale::from(f64::from(params.scale)),
                    program.clone(),
                );
                let id = element.id().clone();
                let commit = element.current_commit();
                vac.insert(CachedSlot {
                    params,
                    id,
                    commit,
                    element: element.clone(),
                    seen_gen: self.generation,
                });
                element
            }
        }
    }
}

/// Converts a `koma-model` `LogicalRect` into a Smithay `Rectangle<f64, Logical>`.
#[must_use]
pub fn to_smithay_logical_rect(rect: koma_model::LogicalRect) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((f64::from(rect.x), f64::from(rect.y))),
        Size::from((f64::from(rect.w), f64::from(rect.h))),
    )
}

#[inline]
const fn premul_bgra(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    let af = a as u16;
    [
        ((b as u16 * af + 127) / 255) as u8,
        ((g as u16 * af + 127) / 255) as u8,
        ((r as u16 * af + 127) / 255) as u8,
        a,
    ]
}

#[inline]
fn f32_to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Compact 5x7 monochrome bitmap font covering printable ASCII `0x20..=0x7E` (7 rows x 5 bits).
const FONT_5X7_ASCII: [[u8; 7]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // ' '
    [0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x04], // '!'
    [0x0A, 0x0A, 0x0A, 0x00, 0x00, 0x00, 0x00], // '"'
    [0x0A, 0x0A, 0x1F, 0x0A, 0x1F, 0x0A, 0x0A], // '#'
    [0x04, 0x0F, 0x14, 0x0E, 0x05, 0x1E, 0x04], // '$'
    [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03], // '%'
    [0x0C, 0x12, 0x14, 0x08, 0x15, 0x12, 0x0D], // '&'
    [0x06, 0x04, 0x08, 0x00, 0x00, 0x00, 0x00], // '\''
    [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02], // '('
    [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08], // ')'
    [0x00, 0x04, 0x15, 0x0E, 0x15, 0x04, 0x00], // '*'
    [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00], // '+'
    [0x00, 0x00, 0x00, 0x00, 0x06, 0x04, 0x08], // ','
    [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00], // '-'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x06], // '.'
    [0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10], // '/'
    [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E], // '0'
    [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E], // '1'
    [0x0E, 0x11, 0x01, 0x06, 0x08, 0x10, 0x1F], // '2'
    [0x1F, 0x02, 0x04, 0x06, 0x01, 0x11, 0x0E], // '3'
    [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02], // '4'
    [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E], // '5'
    [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E], // '6'
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08], // '7'
    [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E], // '8'
    [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C], // '9'
    [0x00, 0x06, 0x06, 0x00, 0x06, 0x06, 0x00], // ':'
    [0x00, 0x06, 0x06, 0x00, 0x06, 0x04, 0x08], // ';'
    [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02], // '<'
    [0x00, 0x00, 0x1F, 0x00, 0x1F, 0x00, 0x00], // '='
    [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08], // '>'
    [0x0E, 0x11, 0x01, 0x02, 0x04, 0x00, 0x04], // '?'
    [0x0E, 0x11, 0x17, 0x15, 0x17, 0x10, 0x0E], // '@'
    [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11], // 'A'
    [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E], // 'B'
    [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E], // 'C'
    [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C], // 'D'
    [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F], // 'E'
    [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10], // 'F'
    [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F], // 'G'
    [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11], // 'H'
    [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E], // 'I'
    [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C], // 'J'
    [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11], // 'K'
    [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F], // 'L'
    [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11], // 'M'
    [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11], // 'N'
    [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E], // 'O'
    [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10], // 'P'
    [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D], // 'Q'
    [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11], // 'R'
    [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E], // 'S'
    [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04], // 'T'
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E], // 'U'
    [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04], // 'V'
    [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11], // 'W'
    [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11], // 'X'
    [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04], // 'Y'
    [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F], // 'Z'
    [0x0E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x0E], // '['
    [0x10, 0x10, 0x08, 0x04, 0x02, 0x01, 0x01], // '\'
    [0x0E, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0E], // ']'
    [0x04, 0x0A, 0x11, 0x00, 0x00, 0x00, 0x00], // '^'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F], // '_'
    [0x08, 0x04, 0x02, 0x00, 0x00, 0x00, 0x00], // '`'
    [0x00, 0x00, 0x0E, 0x01, 0x0F, 0x11, 0x0F], // 'a'
    [0x10, 0x10, 0x16, 0x19, 0x11, 0x11, 0x1E], // 'b'
    [0x00, 0x00, 0x0E, 0x10, 0x10, 0x11, 0x0E], // 'c'
    [0x01, 0x01, 0x0D, 0x13, 0x11, 0x11, 0x0F], // 'd'
    [0x00, 0x00, 0x0E, 0x11, 0x1F, 0x10, 0x0E], // 'e'
    [0x06, 0x08, 0x08, 0x1E, 0x08, 0x08, 0x08], // 'f'
    [0x00, 0x0F, 0x11, 0x11, 0x0F, 0x01, 0x0E], // 'g'
    [0x10, 0x10, 0x16, 0x19, 0x11, 0x11, 0x11], // 'h'
    [0x04, 0x00, 0x0C, 0x04, 0x04, 0x04, 0x0E], // 'i'
    [0x02, 0x00, 0x06, 0x02, 0x02, 0x12, 0x0C], // 'j'
    [0x10, 0x10, 0x12, 0x14, 0x18, 0x14, 0x12], // 'k'
    [0x0C, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E], // 'l'
    [0x00, 0x00, 0x1A, 0x15, 0x15, 0x11, 0x11], // 'm'
    [0x00, 0x00, 0x16, 0x19, 0x11, 0x11, 0x11], // 'n'
    [0x00, 0x00, 0x0E, 0x11, 0x11, 0x11, 0x0E], // 'o'
    [0x00, 0x00, 0x1E, 0x11, 0x1E, 0x10, 0x10], // 'p'
    [0x00, 0x00, 0x0D, 0x13, 0x0F, 0x01, 0x01], // 'q'
    [0x00, 0x00, 0x16, 0x19, 0x10, 0x10, 0x10], // 'r'
    [0x00, 0x00, 0x0E, 0x10, 0x0E, 0x01, 0x1E], // 's'
    [0x08, 0x08, 0x1E, 0x08, 0x08, 0x09, 0x06], // 't'
    [0x00, 0x00, 0x11, 0x11, 0x11, 0x13, 0x0D], // 'u'
    [0x00, 0x00, 0x11, 0x11, 0x11, 0x0A, 0x04], // 'v'
    [0x00, 0x00, 0x11, 0x11, 0x15, 0x15, 0x0A], // 'w'
    [0x00, 0x00, 0x11, 0x0A, 0x04, 0x0A, 0x11], // 'x'
    [0x00, 0x00, 0x11, 0x11, 0x0F, 0x01, 0x0E], // 'y'
    [0x00, 0x00, 0x1F, 0x02, 0x04, 0x08, 0x1F], // 'z'
    [0x02, 0x04, 0x04, 0x08, 0x04, 0x04, 0x02], // '{'
    [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04], // '|'
    [0x08, 0x04, 0x04, 0x02, 0x04, 0x04, 0x08], // '}'
    [0x00, 0x00, 0x08, 0x15, 0x02, 0x00, 0x00], // '~'
];

#[inline]
const fn glyph_5x7(ch: char) -> [u8; 7] {
    let code = ch as u32;
    if code >= 0x20 && code <= 0x7E {
        FONT_5X7_ASCII[(code - 0x20) as usize]
    } else {
        [0x1F, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1F]
    }
}

/// Rasterizes a `LabelPillSnapshot` into a premultiplied `Fourcc::Argb8888` (`[B, G, R, A]`) pixel
/// buffer at integer `scale` (`1..=4`), drawing a rounded dark backdrop, subtle accent border,
/// left accent bar, and built-in crisp 5x7 ASCII glyphs.
///
/// Complexity: $O(W \cdot H)$ in pixel area (`W <= 600 * scale`, `H = 26 * scale`).
#[must_use]
pub fn rasterize_label_pill_argb8888(
    pill: &LabelPillSnapshot,
    accent_rgba: [f32; 4],
    scale: i32,
) -> (Vec<u8>, u32, u32) {
    let s = scale.clamp(1, 4) as u32;
    let w = pill.rect.w.max(1) * s;
    let h = pill.rect.h.max(1) * s;
    let mut pixels = vec![0_u8; (w * h * 4) as usize];

    let ar = f32_to_u8(accent_rgba[0]);
    let ag = f32_to_u8(accent_rgba[1]);
    let ab = f32_to_u8(accent_rgba[2]);
    let half_w = w as f32 * 0.5;
    let half_h = h as f32 * 0.5;
    let radius = (6.0 * s as f32).min(half_w.min(half_h));
    let inner_rx = (half_w - radius).max(0.0);
    let inner_ry = (half_h - radius).max(0.0);

    let bar_x0 = 5 * s;
    let bar_x1 = 8 * s;
    let bar_y0 = 5 * s;
    let bar_y1 = h.saturating_sub(5 * s);

    for py in 0..h {
        let qy = (py as f32 + 0.5 - half_h).abs() - inner_ry;
        for px in 0..w {
            let qx = (px as f32 + 0.5 - half_w).abs() - inner_rx;
            let dist = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
            let cov = (0.5 - dist).clamp(0.0, 1.0);
            if cov <= 0.001 {
                continue;
            }
            let in_border = dist > -(1.2 * s as f32);
            let in_bar = px >= bar_x0 && px < bar_x1 && py >= bar_y0 && py < bar_y1;
            let rgba = if in_bar {
                premul_bgra(ar, ag, ab, (255.0 * cov).round() as u8)
            } else if in_border {
                let r = (13.0 * 0.55 + f32::from(ar) * 0.45).round() as u8;
                let g = (17.0 * 0.55 + f32::from(ag) * 0.45).round() as u8;
                let b = (23.0 * 0.55 + f32::from(ab) * 0.45).round() as u8;
                premul_bgra(r, g, b, (238.0 * cov).round() as u8)
            } else {
                premul_bgra(13, 17, 23, (230.0 * cov).round() as u8)
            };
            let idx = ((py * w + px) * 4) as usize;
            pixels[idx..idx + 4].copy_from_slice(&rgba);
        }
    }

    let glyph_scale = 2 * s;
    let advance = 12 * s;
    let text_x0 = 14 * s;
    let text_y0 = h.saturating_sub(7 * glyph_scale) / 2;
    let text_bgra = premul_bgra(240, 246, 252, 255);
    let max_text_x = w.saturating_sub(4 * s);

    for (char_idx, ch) in pill.text.chars().enumerate() {
        let base_x = text_x0 + (char_idx as u32) * advance;
        if base_x + 5 * glyph_scale > max_text_x {
            break;
        }
        let rows = glyph_5x7(ch);
        for (ry, bits) in rows.into_iter().enumerate() {
            for rx in 0..5_u32 {
                if ((bits >> (4 - rx)) & 1) == 0 {
                    continue;
                }
                for dy in 0..glyph_scale {
                    let py = text_y0 + (ry as u32) * glyph_scale + dy;
                    if py >= h {
                        continue;
                    }
                    for dx in 0..glyph_scale {
                        let px = base_x + rx * glyph_scale + dx;
                        if px >= w {
                            continue;
                        }
                        let idx = ((py * w + px) * 4) as usize;
                        pixels[idx..idx + 4].copy_from_slice(&text_bgra);
                    }
                }
            }
        }
    }

    (pixels, w, h)
}

/// Builds a Smithay `MemoryBuffer` render element displaying a rasterized [`LabelPillSnapshot`].
#[must_use]
pub fn build_label_pill_element<R>(
    renderer: &mut R,
    pill: &LabelPillSnapshot,
    accent_rgba: [f32; 4],
    opacity: f32,
    output_scale: Scale<f64>,
) -> Option<KomaRenderElement<R>>
where
    R: KomaRenderTarget,
    R::TextureId: Send + Clone + 'static,
{
    if opacity <= 0.001 || pill.rect.w == 0 || pill.rect.h == 0 {
        return None;
    }
    let int_scale = output_scale.x.ceil().clamp(1.0, 4.0) as i32;
    let (pixels, px_w, px_h) = rasterize_label_pill_argb8888(pill, accent_rgba, int_scale);
    let buffer = MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (px_w as i32, px_h as i32),
        int_scale,
        Transform::Normal,
        None,
    );
    let loc_phys: Point<i32, Physical> =
        Point::from((f64::from(pill.rect.x), f64::from(pill.rect.y)))
            .to_physical_precise_round(output_scale);
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        loc_phys.to_f64(),
        &buffer,
        Some(opacity.clamp(0.0, 1.0)),
        None,
        None,
        Kind::Unspecified,
    )
    .ok()
    .map(KomaRenderElement::MemoryBuffer)
}

/// One text run inside a [`TextPanelSpec`], positioned in logical pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPanelLine {
    pub x: u32,
    pub y: u32,
    pub text: String,
    /// Straight-alpha RGB plus alpha (`#f0f6fc` -> `[240, 246, 252, 255]`).
    pub color: [u8; 4],
}

/// A rounded panel of bitmap text, used by the compositor keybind overview.
#[derive(Debug, Clone, PartialEq)]
pub struct TextPanelSpec {
    pub width: u32,
    pub height: u32,
    pub corner_radius: u32,
    /// Straight-alpha panel fill.
    pub background: [f32; 4],
    /// Straight-alpha one-pixel border.
    pub border: [f32; 4],
    pub lines: Vec<TextPanelLine>,
}

impl TextPanelSpec {
    #[must_use]
    pub fn new(width: u32, height: u32, corner_radius: u32) -> Self {
        Self {
            width,
            height,
            corner_radius,
            background: [0.067, 0.086, 0.114, 0.97],
            border: [0.188, 0.212, 0.239, 1.0],
            lines: Vec::new(),
        }
    }
}

/// Rasterizes a rounded [`TextPanelSpec`] into premultiplied `Fourcc::Argb8888` (`[B, G, R, A]`).
///
/// Text is drawn with the same built-in 5x7 glyphs as the agent label pill, at a 2x logical
/// scale (`12 px` advance, `14 px` cap height). Positions in `spec.lines` are logical; the
/// returned buffer is `width * scale` by `height * scale`.
///
/// Complexity: $O(W \cdot H + \text{glyph pixels})$ in panel area.
#[must_use]
pub fn rasterize_text_panel_argb8888(
    spec: &TextPanelSpec,
    pixel_scale: i32,
) -> (Vec<u8>, u32, u32) {
    let s = pixel_scale.clamp(1, 4) as u32;
    let w = spec.width.max(1) * s;
    let h = spec.height.max(1) * s;
    let mut pixels = vec![0_u8; (w * h * 4) as usize];

    let bg = [
        f32_to_u8(spec.background[0]),
        f32_to_u8(spec.background[1]),
        f32_to_u8(spec.background[2]),
        f32_to_u8(spec.background[3]),
    ];
    let border = [
        f32_to_u8(spec.border[0]),
        f32_to_u8(spec.border[1]),
        f32_to_u8(spec.border[2]),
        f32_to_u8(spec.border[3]),
    ];
    let half_w = w as f32 * 0.5;
    let half_h = h as f32 * 0.5;
    let radius = (spec.corner_radius as f32 * s as f32).min(half_w.min(half_h));
    let inner_rx = (half_w - radius).max(0.0);
    let inner_ry = (half_h - radius).max(0.0);

    for py in 0..h {
        let qy = (py as f32 + 0.5 - half_h).abs() - inner_ry;
        for px in 0..w {
            let qx = (px as f32 + 0.5 - half_w).abs() - inner_rx;
            let dist = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
            let cov = (0.5 - dist).clamp(0.0, 1.0);
            if cov <= 0.001 {
                continue;
            }
            let in_border = dist > -(1.5 * s as f32);
            let rgba = if in_border {
                premul_bgra(
                    border[0],
                    border[1],
                    border[2],
                    (f32::from(border[3]) * cov).round() as u8,
                )
            } else {
                premul_bgra(bg[0], bg[1], bg[2], (f32::from(bg[3]) * cov).round() as u8)
            };
            let idx = ((py * w + px) * 4) as usize;
            pixels[idx..idx + 4].copy_from_slice(&rgba);
        }
    }

    let glyph_scale = 2 * s;
    // 5 glyph columns + one column of spacing at the 2x logical glyph scale.
    let advance = 6 * glyph_scale;
    for line in &spec.lines {
        let base_y = line.y * s;
        let base_x = line.x * s;
        let color = premul_bgra(line.color[0], line.color[1], line.color[2], line.color[3]);
        for (char_idx, ch) in line.text.chars().enumerate() {
            let glyph_x = base_x + char_idx as u32 * advance;
            if glyph_x + 5 * glyph_scale > w {
                break;
            }
            for (ry, bits) in glyph_5x7(ch).into_iter().enumerate() {
                for rx in 0..5_u32 {
                    if ((bits >> (4 - rx)) & 1) == 0 {
                        continue;
                    }
                    for dy in 0..glyph_scale {
                        let py = base_y + ry as u32 * glyph_scale + dy;
                        if py >= h {
                            continue;
                        }
                        for dx in 0..glyph_scale {
                            let px = glyph_x + rx * glyph_scale + dx;
                            if px >= w {
                                continue;
                            }
                            let idx = ((py * w + px) * 4) as usize;
                            pixels[idx..idx + 4].copy_from_slice(&color);
                        }
                    }
                }
            }
        }
    }

    (pixels, w, h)
}

/// Rasterizes a [`TextPanelSpec`] into a reusable Smithay [`MemoryRenderBuffer`] at `output_scale`.
#[must_use]
pub fn rasterize_text_panel_buffer(
    spec: &TextPanelSpec,
    output_scale: Scale<f64>,
) -> Option<MemoryRenderBuffer> {
    if spec.width == 0 || spec.height == 0 {
        return None;
    }
    let int_scale = output_scale.x.ceil().clamp(1.0, 4.0) as i32;
    let (pixels, px_w, px_h) = rasterize_text_panel_argb8888(spec, int_scale);
    Some(MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (px_w as i32, px_h as i32),
        int_scale,
        Transform::Normal,
        None,
    ))
}

/// Builds a Smithay `MemoryBuffer` element from a pre-rasterized [`MemoryRenderBuffer`] at `location`.
#[must_use]
pub fn render_text_panel_buffer<R>(
    renderer: &mut R,
    buffer: &MemoryRenderBuffer,
    location: LogicalPoint,
    output_scale: Scale<f64>,
) -> Option<KomaRenderElement<R>>
where
    R: KomaRenderTarget,
    R::TextureId: Send + Clone + 'static,
{
    let loc_phys: Point<i32, Physical> =
        Point::from((location.x, location.y)).to_physical_precise_round(output_scale);
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        loc_phys.to_f64(),
        buffer,
        None,
        None,
        None,
        Kind::Unspecified,
    )
    .ok()
    .map(KomaRenderElement::MemoryBuffer)
}

/// Builds a Smithay `MemoryBuffer` element for a [`TextPanelSpec`] at `location`.
#[must_use]
pub fn build_text_panel_element<R>(
    renderer: &mut R,
    spec: &TextPanelSpec,
    location: LogicalPoint,
    output_scale: Scale<f64>,
) -> Option<KomaRenderElement<R>>
where
    R: KomaRenderTarget,
    R::TextureId: Send + Clone + 'static,
{
    let buffer = rasterize_text_panel_buffer(spec, output_scale)?;
    render_text_panel_buffer(renderer, &buffer, location, output_scale)
}

#[cfg(test)]
mod tests {
    use koma_model::{LogicalRect, place_label_pill};

    use super::*;

    #[test]
    fn text_panel_rasterizer_draws_rounded_background_border_and_glyphs() {
        let mut spec = TextPanelSpec::new(240, 80, 10);
        spec.lines.push(TextPanelLine {
            x: 12,
            y: 12,
            text: "AB".to_owned(),
            color: [240, 246, 252, 255],
        });
        let (pixels, w, h) = rasterize_text_panel_argb8888(&spec, 1);
        assert_eq!((w, h), (240, 80));
        assert_eq!(pixels.len(), (w * h * 4) as usize);

        // Rounded corner stays transparent.
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);

        // Panel centre is opaque background (#11161d -> BGRA with alpha 247).
        let centre = (((h / 2) * w + w / 2) * 4) as usize;
        assert!(pixels[centre + 3] > 240);
        assert!(
            pixels[centre] > pixels[centre + 2],
            "background stays blue-ish (BGRA)"
        );

        // Glyph pixels use the requested near-white colour.
        let white = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[0] == 252 && px[1] == 246 && px[2] == 240 && px[3] == 255)
            .count();
        assert!(white > 20, "expected readable glyphs, found {white} pixels");

        // Scaled rasterization keeps logical geometry.
        let (scaled, sw, sh) = rasterize_text_panel_argb8888(&spec, 2);
        assert_eq!((sw, sh), (480, 160));
        assert_eq!(scaled.len(), (sw * sh * 4) as usize);
    }

    #[test]
    fn solid_color_premultiplies_straight_alpha_for_premultiplied_blending() {
        // The drop-zone preview fill is authored as straight `#58a6ff` at 18 % alpha. Under
        // `BlendFunc(ONE, ONE_MINUS_SRC_ALPHA)` a straight-alpha value would land at ~5x its
        // intended intensity (a bright blue wash over the whole drop rect), so the element has to
        // premultiply before handing the color to `draw_solid`.
        let straight = [0.345, 0.651, 1.0, 0.18];
        let premul = premultiplied_rgba(straight);
        assert!((premul[0] - 0.0621).abs() < 1e-4, "r {}", premul[0]);
        assert!((premul[1] - 0.11718).abs() < 1e-4, "g {}", premul[1]);
        assert!((premul[2] - 0.18).abs() < 1e-4, "b {}", premul[2]);
        assert!((premul[3] - 0.18).abs() < 1e-6, "a {}", premul[3]);
        for (i, channel) in premul[..3].iter().enumerate() {
            assert!(
                *channel <= premul[3] + 1e-6,
                "premultiplied channel {i} exceeds alpha"
            );
        }

        // Opaque colors must pass through unchanged, otherwise every blackout rect would tint.
        let opaque = [0.0, 0.0, 0.0, 1.0];
        assert_eq!(premultiplied_rgba(opaque), opaque);
        let accent = [0.345, 0.651, 1.0, 1.0];
        assert_eq!(premultiplied_rgba(accent), accent);
    }

    #[test]
    fn solid_color_element_keeps_authored_color_and_alpha() {
        // Callers keep using straight-alpha constants; only the draw path premultiplies.
        let rect = Rectangle::from_size(Size::from((16.0, 16.0)));
        let element = KomaSolidColorElement::oneshot(rect, [0.345, 0.651, 1.0, 0.18]);
        assert!((element.alpha() - 0.18).abs() < 1e-6);
        assert_eq!(element.color.r(), 0.345);
        assert_eq!(element.color.g(), 0.651);
        assert_eq!(element.color.b(), 1.0);
        assert!(!element.color.is_opaque());
    }

    #[test]
    fn label_pill_rasterizer_draws_backdrop_accent_bar_and_visible_glyphs() {
        let pill = place_label_pill(
            LogicalPoint { x: 100.0, y: 100.0 },
            "CLICK BLU",
            LogicalRect::new(0, 0, 1920, 1080),
        )
        .expect("label pill");
        let (pixels, w, h) = rasterize_label_pill_argb8888(&pill, [0.49, 0.85, 0.53, 1.0], 1);
        assert_eq!(w, pill.rect.w);
        assert_eq!(h, pill.rect.h);
        assert_eq!(pixels.len(), (w * h * 4) as usize);

        // Corner pixel (0, 0) outside rounded pill corner should be transparent.
        assert_eq!(&pixels[0..4], &[0, 0, 0, 0]);

        // Left accent stripe at (6, h/2) should reflect accent tint and full alpha.
        let bar_idx = (((h / 2) * w + 6) * 4) as usize;
        assert_eq!(pixels[bar_idx + 3], 255);
        assert!(pixels[bar_idx + 1] > pixels[bar_idx]); // higher G than B

        // Text region contains bright white foreground glyph pixels (#f0f6fc).
        let white_pixels = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[0] == 252 && px[1] == 246 && px[2] == 240 && px[3] == 255)
            .count();
        assert!(
            white_pixels > 40,
            "expected readable bitmap glyphs, found {white_pixels} white pixels"
        );
    }

    #[derive(Debug, Clone)]
    struct TestCachedElement {
        id: Id,
        commit: CommitCounter,
        geometry: Rectangle<f64, Logical>,
        alpha: f32,
        opaque: bool,
        corner_radius: [f32; 4],
    }

    impl Element for TestCachedElement {
        fn id(&self) -> &Id {
            &self.id
        }

        fn current_commit(&self) -> CommitCounter {
            self.commit
        }

        fn src(&self) -> Rectangle<f64, Buffer> {
            Rectangle::from_size(Size::from((1.0, 1.0)))
        }

        fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
            self.geometry.to_physical_precise_round(scale)
        }

        fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
            if !self.opaque {
                return OpaqueRegions::default();
            }
            let phys = self.geometry.to_physical_precise_round(scale);
            let local = Rectangle::from_size(phys.size);
            clipped_surface_opaque_regions(
                OpaqueRegions::from_slice(&[local]),
                phys.loc,
                self.geometry,
                self.corner_radius,
                self.alpha,
                scale,
            )
        }

        fn alpha(&self) -> f32 {
            self.alpha
        }
    }

    #[test]
    fn clipped_surface_opaque_regions_handles_zero_and_csd_offsets_and_opacity() {
        let scale = Scale::from(1.0);
        let clip_geo = Rectangle::new(Point::from((100.0, 50.0)), Size::from((800.0, 600.0)));

        // 1. Opacity < 0.999 clears opaque claim completely.
        let inner = OpaqueRegions::from_slice(&[Rectangle::from_size(Size::from((800, 600)))]);
        let translucent = clipped_surface_opaque_regions(
            inner,
            Point::from((100, 50)),
            clip_geo,
            [10.0; 4],
            0.95,
            scale,
        );
        assert!(translucent.is_empty());

        // 2. Corner radius <= 0.5 preserves inner opaque regions clipped to clip_geo.
        let inner = OpaqueRegions::from_slice(&[Rectangle::from_size(Size::from((800, 600)))]);
        let square = clipped_surface_opaque_regions(
            inner,
            Point::from((100, 50)),
            clip_geo,
            [0.4; 4],
            1.0,
            scale,
        );
        assert_eq!(
            &*square,
            &[Rectangle::new(Point::from((0, 0)), Size::from((800, 600)))]
        );

        // 3. Zero-offset rounded window preserves interior minus only the 4 10x10 corner boxes.
        let inner = OpaqueRegions::from_slice(&[Rectangle::from_size(Size::from((800, 600)))]);
        let rounded = clipped_surface_opaque_regions(
            inner,
            Point::from((100, 50)),
            clip_geo,
            [10.0; 4],
            1.0,
            scale,
        );
        let total_area: i32 = rounded.iter().map(|r| r.size.w * r.size.h).sum();
        assert_eq!(total_area, 800 * 600 - 4 * 10 * 10);
        let corners = [
            Rectangle::new(Point::from((0, 0)), Size::from((10, 10))),
            Rectangle::new(Point::from((790, 0)), Size::from((10, 10))),
            Rectangle::new(Point::from((790, 590)), Size::from((10, 10))),
            Rectangle::new(Point::from((0, 590)), Size::from((10, 10))),
        ];
        for r in rounded.iter() {
            for c in &corners {
                assert!(
                    !r.overlaps(*c),
                    "opaque region {r:?} overlapped corner {c:?}"
                );
            }
        }

        // 4. CSD-offset window (surface origin shifted left/top by (12, 8) for client shadow).
        // Even if client claims whole 824x616 buffer opaque, clip_geo clamps to (12, 8, 800, 600)
        // in element-relative coordinates and subtracts the 4 corners at (12, 8)-offset positions.
        let csd_elem_loc = Point::from((88, 42));
        let csd_inner = OpaqueRegions::from_slice(&[Rectangle::from_size(Size::from((824, 616)))]);
        let csd_rounded = clipped_surface_opaque_regions(
            csd_inner,
            csd_elem_loc,
            clip_geo,
            [10.0; 4],
            1.0,
            scale,
        );
        let csd_area: i32 = csd_rounded.iter().map(|r| r.size.w * r.size.h).sum();
        assert_eq!(csd_area, 800 * 600 - 4 * 10 * 10);
        let clip_rel = Rectangle::new(Point::from((12, 8)), Size::from((800, 600)));
        let csd_corners = [
            Rectangle::new(Point::from((12, 8)), Size::from((10, 10))),
            Rectangle::new(Point::from((802, 8)), Size::from((10, 10))),
            Rectangle::new(Point::from((802, 598)), Size::from((10, 10))),
            Rectangle::new(Point::from((12, 598)), Size::from((10, 10))),
        ];
        for r in csd_rounded.iter() {
            assert!(
                clip_rel.contains_rect(*r),
                "CSD region {r:?} escaped clip_rel {clip_rel:?}"
            );
            for c in &csd_corners {
                assert!(!r.overlaps(*c), "CSD region {r:?} overlapped corner {c:?}");
            }
        }

        // 5. Fractional scale (1.6x) and asymmetric corners subtract ceil(r * scale) boxes.
        let frac_scale = Scale::from(1.6);
        let frac_inner =
            OpaqueRegions::from_slice(&[Rectangle::from_size(Size::from((1280, 960)))]);
        let frac_rounded = clipped_surface_opaque_regions(
            frac_inner,
            Point::from((160, 80)),
            clip_geo,
            [10.0, 0.0, 5.0, 0.0],
            1.0,
            frac_scale,
        );
        let frac_area: i32 = frac_rounded.iter().map(|r| r.size.w * r.size.h).sum();
        assert_eq!(frac_area, 1280 * 960 - 16 * 16 - 8 * 8);
    }

    #[test]
    fn clipped_surface_opaque_regions_cull_underlying_damage_in_damage_tracker() {
        use smithay::backend::renderer::damage::OutputDamageTracker;

        let mut tracker = OutputDamageTracker::new((1920, 1080), 1.0, Transform::Normal);
        let fg = TestCachedElement {
            id: Id::new(),
            commit: CommitCounter::from(0),
            geometry: Rectangle::new(Point::from((100.0, 100.0)), Size::from((800.0, 600.0))),
            alpha: 1.0,
            opaque: true,
            corner_radius: [10.0; 4],
        };
        let bg_id = Id::new();
        let bg0 = TestCachedElement {
            id: bg_id.clone(),
            commit: CommitCounter::from(0),
            geometry: Rectangle::new(Point::from((200.0, 200.0)), Size::from((400.0, 300.0))),
            alpha: 1.0,
            opaque: true,
            corner_radius: [0.0; 4],
        };
        let _ = tracker
            .damage_output(1, &[fg.clone(), bg0])
            .expect("frame 1");

        // Underlying window commits inside the occluded interior of `fg`: zero output damage.
        let bg1 = TestCachedElement {
            id: bg_id,
            commit: CommitCounter::from(1),
            geometry: Rectangle::new(Point::from((200.0, 200.0)), Size::from((400.0, 300.0))),
            alpha: 1.0,
            opaque: true,
            corner_radius: [0.0; 4],
        };
        let (damage, _) = tracker.damage_output(1, &[fg, bg1]).expect("frame 2");
        assert!(
            damage.is_none(),
            "occluded background commit should produce no output damage, got {damage:?}"
        );
    }

    #[test]
    fn decoration_cache_produces_zero_damage_on_identical_frames_and_targets_changes() {
        use smithay::backend::renderer::damage::OutputDamageTracker;

        let win1 = WindowId::new(1, 0);
        let win2 = WindowId::new(2, 0);
        let mut tracker = OutputDamageTracker::new((1920, 1080), 1.0, Transform::Normal);
        let mut cache = FrameDecorationCache::default();
        let mut border_slots: HashMap<
            DecorationKey,
            CachedSlot<RoundedBorderParams, TestCachedElement>,
        > = HashMap::new();
        let mut shadow_slots: HashMap<
            DecorationKey,
            CachedSlot<DropShadowParams, TestCachedElement>,
        > = HashMap::new();
        let mut blur_slots: HashMap<
            DecorationKey,
            CachedSlot<BlurredBackdropParams, TestCachedElement>,
        > = HashMap::new();

        let make_border_params = |color: [f32; 4]| RoundedBorderParams {
            geometry: Rectangle::new(Point::from((100.0, 100.0)), Size::from((400.0, 14.0))),
            full_geometry: Rectangle::new(Point::from((100.0, 100.0)), Size::from((400.0, 300.0))),
            outer_radius: [10.0; 4],
            border_width: 2.0,
            border_color: color,
            border_color_end: color,
            gradient_angle_rad: 0.0,
            opacity: 1.0,
            scale: 1.0,
            carrier_tex_id: 1,
        };
        let make_shadow_params = |alpha: f32| DropShadowParams {
            area: Rectangle::new(Point::from((80, 80)), Size::from((440, 34))),
            shadow_geo: Rectangle::new(Point::from((100.0, 100.0)), Size::from((400.0, 300.0))),
            shadow_radii: [10.0; 4],
            sigma: 6.0,
            sharp: false,
            color: [0.0, 0.0, 0.0, alpha],
            window_geo: Rectangle::new(Point::from((100.0, 100.0)), Size::from((400.0, 300.0))),
            window_radii: [10.0; 4],
            opacity: 1.0,
            scale: 1.0,
        };
        let make_blur_params = |blur_gen: u64| BlurredBackdropParams {
            geometry: Rectangle::new(Point::from((600.0, 100.0)), Size::from((300.0, 200.0))),
            viewport_size: Size::from((1920.0, 1080.0)),
            corner_radius: [10.0; 4],
            opacity: 0.85,
            scale: 1.0,
            noise: 0.0117,
            contrast: 0.8916,
            brightness: 0.8172,
            pre_toned: true,
            tex_id: 7,
            tex_size: Size::from((1920, 1080)),
            blur_generation: blur_gen,
        };

        let mut build_frame = |border_color: [f32; 4],
                               shadow_alpha: f32,
                               blur_gen: u64,
                               scrim_alpha: f32,
                               cursor_x: f64,
                               include_win2: bool| {
            cache.begin_frame();
            let frame_gen = cache.generation;
            let border_el = resolve_cached_slot(
                &mut border_slots,
                DecorationKey::Window {
                    win_id: win1,
                    slot: DecorationSlot::BorderStrip(0),
                },
                frame_gen,
                make_border_params(border_color),
                |id, commit, p| TestCachedElement {
                    id,
                    commit,
                    geometry: p.geometry,
                    alpha: p.opacity,
                    opaque: false,
                    corner_radius: [0.0; 4],
                },
            );
            let shadow_el = resolve_cached_slot(
                &mut shadow_slots,
                DecorationKey::Window {
                    win_id: win1,
                    slot: DecorationSlot::DropShadowStrip(0),
                },
                frame_gen,
                make_shadow_params(shadow_alpha),
                |id, commit, p| TestCachedElement {
                    id,
                    commit,
                    geometry: p.area.to_f64(),
                    alpha: 1.0,
                    opaque: false,
                    corner_radius: [0.0; 4],
                },
            );
            let blur_el = include_win2.then(|| {
                resolve_cached_slot(
                    &mut blur_slots,
                    DecorationKey::Window {
                        win_id: win2,
                        slot: DecorationSlot::BlurredBackdrop,
                    },
                    frame_gen,
                    make_blur_params(blur_gen),
                    |id, commit, p| TestCachedElement {
                        id,
                        commit,
                        geometry: p.geometry,
                        alpha: p.opacity,
                        opaque: false,
                        corner_radius: [0.0; 4],
                    },
                )
            });
            let scrim_el = cache.solid(
                DecorationKey::OverlaySolid(OverlaySolidKind::ScratchpadScrim),
                Rectangle::new(Point::from((10.0, 10.0)), Size::from((80.0, 40.0))),
                [0.035, 0.043, 0.063, scrim_alpha],
            );
            let cursor_el = cache.solid(
                DecorationKey::OverlaySolid(OverlaySolidKind::AgentCaretFill),
                Rectangle::new(Point::from((cursor_x, 500.0)), Size::from((20.0, 20.0))),
                [1.0, 1.0, 1.0, 1.0],
            );
            cache.end_frame();
            border_slots.retain(|_, v| v.seen_gen == frame_gen);
            shadow_slots.retain(|_, v| v.seen_gen == frame_gen);
            blur_slots.retain(|_, v| v.seen_gen == frame_gen);

            let mut elems = vec![
                TestCachedElement {
                    id: cursor_el.id().clone(),
                    commit: cursor_el.current_commit(),
                    geometry: Rectangle::new(
                        Point::from((cursor_x, 500.0)),
                        Size::from((20.0, 20.0)),
                    ),
                    alpha: cursor_el.alpha(),
                    opaque: false,
                    corner_radius: [0.0; 4],
                },
                border_el,
                shadow_el,
                TestCachedElement {
                    id: scrim_el.id().clone(),
                    commit: scrim_el.current_commit(),
                    geometry: Rectangle::new(Point::from((10.0, 10.0)), Size::from((80.0, 40.0))),
                    alpha: scrim_el.alpha(),
                    opaque: false,
                    corner_radius: [0.0; 4],
                },
            ];
            if let Some(blur_el) = blur_el {
                elems.push(blur_el);
            }
            (elems, scrim_el)
        };

        // Frame 1: initial full damage.
        let (f1, scrim1) = build_frame([0.3, 0.6, 1.0, 1.0], 0.45, 1, 0.58, 1200.0, true);
        let (d1, _) = tracker.damage_output(1, &f1).expect("frame 1");
        assert!(d1.is_some());

        // Frame 2: identical inputs produce ZERO damage!
        let (f2, scrim2) = build_frame([0.3, 0.6, 1.0, 1.0], 0.45, 1, 0.58, 1200.0, true);
        assert_eq!(scrim1.id(), scrim2.id());
        assert_eq!(scrim1.current_commit(), scrim2.current_commit());
        let (d2, _) = tracker.damage_output(1, &f2).expect("frame 2");
        assert!(
            d2.is_none(),
            "identical cached elements should produce zero damage, got {d2:?}"
        );

        // Frame 3: only cursor moves -> damages ONLY cursor old + new rects, never decorations!
        let (f3, _) = build_frame([0.3, 0.6, 1.0, 1.0], 0.45, 1, 0.58, 1250.0, true);
        let (d3, _) = tracker.damage_output(1, &f3).expect("frame 3");
        let d3 = d3.expect("cursor move must produce damage");
        for rect in d3 {
            assert!(
                rect.loc.y >= 500 && rect.loc.y + rect.size.h <= 520,
                "unrelated decoration rect was damaged: {rect:?}"
            );
        }

        // Frame 4: only win1's border color changes -> same Id, bumped CommitCounter, only border damaged!
        let (f4, _) = build_frame([1.0, 0.2, 0.2, 1.0], 0.45, 1, 0.58, 1250.0, true);
        assert_eq!(f4[1].id(), f3[1].id(), "border Id must remain persistent");
        assert!(
            f4[1].current_commit() > f3[1].current_commit(),
            "border CommitCounter must advance"
        );
        let (d4, _) = tracker.damage_output(1, &f4).expect("frame 4");
        assert_eq!(
            d4.as_ref().map(|v| v.as_slice()),
            Some(
                &[Rectangle::new(
                    Point::from((100, 100)),
                    Size::from((400, 14))
                )][..]
            )
        );

        // Frame 5: only blur generation bumps -> only blur rect damaged!
        let (f5, _) = build_frame([1.0, 0.2, 0.2, 1.0], 0.45, 2, 0.58, 1250.0, true);
        assert_eq!(f5[4].id(), f4[4].id(), "blur Id must remain persistent");
        let (d5, _) = tracker.damage_output(1, &f5).expect("frame 5");
        assert_eq!(
            d5.as_ref().map(|v| v.as_slice()),
            Some(
                &[Rectangle::new(
                    Point::from((600, 100)),
                    Size::from((300, 200))
                )][..]
            )
        );

        // Frame 6: only shadow alpha changes -> shadow Id preserved, CommitCounter advances, only shadow damaged!
        let (f6, _) = build_frame([1.0, 0.2, 0.2, 1.0], 0.25, 2, 0.58, 1250.0, true);
        assert_eq!(f6[2].id(), f5[2].id(), "shadow Id must remain persistent");
        assert!(f6[2].current_commit() > f5[2].current_commit());
        let (d6, _) = tracker.damage_output(1, &f6).expect("frame 6");
        assert_eq!(
            d6.as_ref().map(|v| v.as_slice()),
            Some(&[Rectangle::new(Point::from((80, 80)), Size::from((440, 34)))][..])
        );

        // Frame 7: win2 closes -> end_frame evicts win2's blur entry automatically!
        let (f7, _) = build_frame([1.0, 0.2, 0.2, 1.0], 0.25, 2, 0.58, 1250.0, false);
        assert!(!blur_slots.contains_key(&DecorationKey::Window {
            win_id: win2,
            slot: DecorationSlot::BlurredBackdrop,
        }));
        let _ = tracker.damage_output(1, &f7).expect("frame 7");
        assert_eq!(cache.len(), 2); // scrim + cursor

        // Direct KomaSolidColorElement drive through OutputDamageTracker using FrameDecorationCache.
        let mut solid_tracker = OutputDamageTracker::new((1920, 1080), 1.0, Transform::Normal);
        let mut solid_cache = FrameDecorationCache::default();
        let mut emit_solids = |banner_visible: bool, cursor_y: f64| -> Vec<KomaSolidColorElement> {
            solid_cache.begin_frame();
            let mut v = vec![solid_cache.solid(
                DecorationKey::OverlaySolid(OverlaySolidKind::AgentCaretFill),
                Rectangle::new(Point::from((100.0, cursor_y)), Size::from((16.0, 16.0))),
                [0.2, 0.8, 0.4, 1.0],
            )];
            if banner_visible {
                v.push(solid_cache.solid(
                    DecorationKey::OverlaySolid(OverlaySolidKind::ConfigErrorBanner),
                    Rectangle::new(Point::from((0.0, 0.0)), Size::from((1920.0, 4.0))),
                    [0.95, 0.30, 0.25, 0.95],
                ));
            }
            solid_cache.end_frame();
            v
        };
        let s1 = emit_solids(true, 300.0);
        assert!(solid_tracker.damage_output(1, &s1).expect("s1").0.is_some());
        let s2 = emit_solids(true, 300.0);
        assert!(
            solid_tracker.damage_output(1, &s2).expect("s2").0.is_none(),
            "identical cached KomaSolidColorElements must emit zero damage"
        );
        let s3 = emit_solids(true, 320.0);
        let (sd3, _) = solid_tracker.damage_output(1, &s3).expect("s3");
        let sd3 = sd3.expect("moved cursor solid should damage");
        assert!(
            sd3.iter()
                .all(|r| r.loc.y >= 300 && r.loc.y + r.size.h <= 336)
        );
        let s4 = emit_solids(false, 320.0);
        assert_eq!(solid_cache.len(), 1);
        assert!(solid_tracker.damage_output(1, &s4).expect("s4").0.is_some());
    }

    #[test]
    fn split_rounded_border_strips_culls_interior_without_overlap_or_gaps() {
        let full = Rectangle::new(Point::from((100.0, 50.0)), Size::from((800.0, 600.0)));
        for scale_val in [1.0, 1.25, 1.5, 1.6, 2.0] {
            let scale = Scale::from(scale_val);
            let strips = split_rounded_border_strips(full, [10.0; 4], 2.0, scale);
            assert_eq!(strips.len(), 4, "expected 4 strips at scale {scale_val}");

            let phys_strips: Vec<Rectangle<i32, Physical>> = strips
                .iter()
                .map(|(_, r)| r.to_physical_precise_round(scale))
                .collect();

            // Zero pairwise overlap among the 4 strips.
            for i in 0..phys_strips.len() {
                for j in (i + 1)..phys_strips.len() {
                    assert!(
                        !phys_strips[i].overlaps(phys_strips[j]),
                        "border strips {i} and {j} overlapped at scale {scale_val}: {:?} vs {:?}",
                        phys_strips[i],
                        phys_strips[j]
                    );
                }
            }

            // Interior center must not touch any strip; remaining area equals full minus strips.
            let full_phys = full.to_physical_precise_round(scale);
            let remainder =
                Rectangle::subtract_rects_many([full_phys], phys_strips.iter().copied());
            assert_eq!(
                remainder.len(),
                1,
                "expected 1 rectangular hole at scale {scale_val}, got {remainder:?}"
            );
            let hole = remainder[0];
            assert!(hole.size.w > (750.0 * scale_val) as i32);
            assert!(hole.size.h > (550.0 * scale_val) as i32);

            // Global input_to_geo preserves exact normalized coordinates at strip boundaries.
            let mat = rounded_border_input_to_geo(strips[3].1, full, scale);
            let right_phys = phys_strips[3];
            let expected_u0 = (right_phys.loc.x - full_phys.loc.x) as f32 / full_phys.size.w as f32;
            let expected_v0 = (right_phys.loc.y - full_phys.loc.y) as f32 / full_phys.size.h as f32;
            // mat * vec3(0, 0, 1) = (mat[6], mat[7])
            assert!((mat.0[6] - expected_u0).abs() < 1e-5);
            assert!((mat.0[7] - expected_v0).abs() < 1e-5);
        }

        // Filled pill (border_width == 0.0) and tiny window fall back cleanly to 1 unsplit quad.
        let pill = split_rounded_border_strips(full, [4.0; 4], 0.0, Scale::from(1.0));
        assert_eq!(pill, vec![(4, full)]);

        let tiny = Rectangle::new(Point::from((10.0, 10.0)), Size::from((20.0, 20.0)));
        let tiny_strips = split_rounded_border_strips(tiny, [10.0; 4], 2.0, Scale::from(1.0));
        assert_eq!(tiny_strips, vec![(4, tiny)]);
    }

    #[test]
    fn split_drop_shadow_strips_culls_cutout_interior_without_overlap_or_gaps() {
        let win = Rectangle::new(Point::from((100.0, 50.0)), Size::from((800.0, 600.0)));
        let area = Rectangle::new(Point::from((70, 20)), Size::from((860, 660)));

        for scale_val in [1.0, 1.25, 1.5, 1.6, 2.0] {
            let scale = Scale::from(scale_val);
            let strips = split_drop_shadow_strips(area, win, [10.0; 4], scale);
            assert_eq!(
                strips.len(),
                4,
                "expected 4 shadow strips at scale {scale_val}"
            );

            let phys_strips: Vec<Rectangle<i32, Physical>> = strips
                .iter()
                .map(|(_, r)| r.to_physical_precise_round(scale))
                .collect();

            // Zero pairwise overlap among the 4 shadow strips.
            for i in 0..phys_strips.len() {
                for j in (i + 1)..phys_strips.len() {
                    assert!(
                        !phys_strips[i].overlaps(phys_strips[j]),
                        "shadow strips {i} and {j} overlapped at scale {scale_val}: {:?} vs {:?}",
                        phys_strips[i],
                        phys_strips[j]
                    );
                }
            }

            // Strips + interior hole leave zero uncovered border/outer-shadow pixels.
            let area_phys = area.to_physical_precise_round(scale);
            let remainder =
                Rectangle::subtract_rects_many([area_phys], phys_strips.iter().copied());
            assert_eq!(
                remainder.len(),
                1,
                "expected 1 rectangular cutout at scale {scale_val}, got {remainder:?}"
            );
            let hole = remainder[0];
            assert!(hole.size.w > (750.0 * scale_val) as i32);
            assert!(hole.size.h > (550.0 * scale_val) as i32);

            // Hole sits strictly inside `win` inset by `max_window_radius + 2.0`.
            let safe_inset_px = ((10.0 + 2.0) * scale_val).floor() as i32;
            let win_phys: Rectangle<i32, Physical> = win.to_physical_precise_round(scale);
            assert!(hole.loc.x >= win_phys.loc.x + safe_inset_px);
            assert!(hole.loc.y >= win_phys.loc.y + safe_inset_px);
            assert!(hole.loc.x + hole.size.w <= win_phys.loc.x + win_phys.size.w - safe_inset_px);
            assert!(hole.loc.y + hole.size.h <= win_phys.loc.y + win_phys.size.h - safe_inset_px);
        }

        // Shadow area completely inside the interior cutout culls all pixels (0 strips).
        let hidden_area = Rectangle::new(Point::from((200, 150)), Size::from((400, 300)));
        assert!(split_drop_shadow_strips(hidden_area, win, [10.0; 4], Scale::from(1.0)).is_empty());

        // Tiny window falls back cleanly to 1 unsplit quad.
        let tiny_win = Rectangle::new(Point::from((10.0, 10.0)), Size::from((20.0, 20.0)));
        let tiny_area = Rectangle::new(Point::from((0, 0)), Size::from((40, 40)));
        assert_eq!(
            split_drop_shadow_strips(tiny_area, tiny_win, [10.0; 4], Scale::from(1.0)),
            vec![(4, tiny_area)]
        );
    }

    #[test]
    fn blurred_backdrop_local_uv_maps_geometry_accurately() {
        let geo = Rectangle::new(Point::from((100.0, 50.0)), Size::from((500.0, 400.0)));
        let vp = Size::from((1280.0, 800.0));
        let tex_sz = Size::from((2048, 1280));
        let (orig, scale) = blurred_backdrop_local_uv(geo, vp, tex_sz, Scale::from(1.6));
        assert!((orig.0 - 160.0 / 2048.0).abs() < 1e-6);
        assert!((orig.1 - 80.0 / 1280.0).abs() < 1e-6);
        assert!((scale.0 - 2048.0 / 800.0).abs() < 1e-6);
        assert!((scale.1 - 1280.0 / 640.0).abs() < 1e-6);
    }
}
