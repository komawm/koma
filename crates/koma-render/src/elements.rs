//! Render elements for `Koma`:
//! 1. `KomaClippedSurfaceElement<R>` — SDF rounded-corner clipped & opacity-modulated Wayland surface,
//! 2. `KomaRoundedBorderElement` — analytical SDF anti-aliased outer/inner rounded border ring,
//! 3. `KomaBlurredBackdropElement` — frosted-glass `Dual Kawase Blur` slice clipped to rounded window,
//! 4. `KomaSolidColorElement` — fractional-scale solid rects (`#000000` Protected blackout, `grid_225`).

use koma_model::{AgentCursorFrameSnapshot, LogicalPoint};
use smithay::{
    backend::renderer::{
        Color32F, Frame, ImportAll, ImportMem, Renderer, buffer_y_inverted,
        element::{
            Element, Id, Kind, RenderElement, UnderlyingStorage,
            memory::MemoryRenderBufferRenderElement, render_elements,
            surface::WaylandSurfaceRenderElement,
        },
        gles::{
            GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture,
            Uniform, element::PixelShaderElement,
        },
        multigpu::{Error as MultiError, MultiRenderer, gbm::GbmGlesBackend},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
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
        // Rounded corners & translucent windows expose underlying pixels, so clear opaque claim
        // when `opacity < 1.0` or `corner_radius > 0` to keep `OutputDamageTracker` accurate!
        if self.opacity < 0.999 || self.corner_radius.iter().any(|&r| r > 0.5) {
            OpaqueRegions::default()
        } else {
            self.inner.opaque_regions(scale)
        }
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

// ==============================================================================
// 2. `KomaRoundedBorderElement` (Analytical SDF Anti-Aliased Rounded Ring)
// ==============================================================================

#[derive(Debug, Clone)]
pub struct KomaRoundedBorderElement {
    id: Id,
    geometry: Rectangle<f64, Logical>,
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
            geometry,
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

    fn uniforms(&self) -> [Uniform<'static>; 7] {
        [
            Uniform::new("koma_scale", self.scale),
            Uniform::new(
                "geo_size",
                (self.geometry.size.w as f32, self.geometry.size.h as f32),
            ),
            Uniform::new("outer_radius", self.outer_radius),
            Uniform::new("border_width", self.border_width),
            Uniform::new("border_color", self.border_color),
            Uniform::new("border_color_end", self.border_color_end),
            Uniform::new("gradient_angle_rad", self.gradient_angle_rad),
        ]
    }
}

impl Element for KomaRoundedBorderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        CommitCounter::default()
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

#[derive(Debug, Clone)]
pub struct KomaBlurredBackdropElement {
    id: Id,
    geometry: Rectangle<f64, Logical>,
    viewport_size: Size<f64, Logical>,
    corner_radius: [f32; 4],
    opacity: f32,
    scale: f32,
    noise: f32,
    contrast: f32,
    brightness: f32,
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
            geometry,
            viewport_size,
            corner_radius,
            opacity: opacity.clamp(0.0, 1.0),
            scale: scale.x as f32,
            noise,
            contrast,
            brightness,
            blurred_tex,
            program,
        }
    }

    fn uniforms(&self) -> [Uniform<'static>; 8] {
        let vw = self.viewport_size.w.max(1.0) as f32;
        let vh = self.viewport_size.h.max(1.0) as f32;
        let gx = self.geometry.loc.x as f32;
        let gy = self.geometry.loc.y as f32;
        let gw = self.geometry.size.w.max(1.0) as f32;
        let gh = self.geometry.size.h.max(1.0) as f32;

        [
            Uniform::new("koma_scale", self.scale),
            Uniform::new("geo_size", (gw, gh)),
            Uniform::new("corner_radius", self.corner_radius),
            Uniform::new("local_uv_origin", (gx / vw, gy / vh)),
            Uniform::new("local_uv_scale", (vw / gw, vh / gh)),
            Uniform::new("blur_noise", self.noise),
            Uniform::new("blur_contrast", self.contrast),
            Uniform::new("blur_brightness", self.brightness),
        ]
    }
}

impl Element for KomaBlurredBackdropElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        CommitCounter::default()
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

#[derive(Debug, Clone)]
pub struct KomaSolidColorElement {
    id: Id,
    geometry: Rectangle<f64, Logical>,
    commit: CommitCounter,
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
        frame.draw_solid(dst, damage, self.color)
    }

    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

// ==============================================================================
// 5. `KomaDropShadowElement` (Analytical Gaussian/Erf Rounded Drop Shadow)
// ==============================================================================

/// Analytical GLES2 rounded-rectangle drop shadow element wrapping Smithay's [`PixelShaderElement`]
/// for both `GlesRenderer` and `TtyMultiRenderer` via [`KomaRenderTarget`].
#[derive(Debug, Clone)]
pub struct KomaDropShadowElement {
    inner: PixelShaderElement,
}

impl KomaDropShadowElement {
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

        let uniforms = vec![
            Uniform::new("koma_scale", scale.x.max(1.0) as f32),
            Uniform::new("shadow_color", color),
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
        ];

        Self {
            inner: PixelShaderElement::new(
                program,
                area,
                None,
                opacity.clamp(0.0, 1.0),
                uniforms,
                Kind::Unspecified,
            ),
        }
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
    CursorBuffer = MemoryRenderBufferRenderElement<R>,
    Solid = KomaSolidColorElement,
}

/// Converts a `koma-model` `LogicalRect` into a Smithay `Rectangle<f64, Logical>`.
#[must_use]
pub fn to_smithay_logical_rect(rect: koma_model::LogicalRect) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((f64::from(rect.x), f64::from(rect.y))),
        Size::from((f64::from(rect.w), f64::from(rect.h))),
    )
}
