//! Render elements for `Koma`:
//! 1. `KomaClippedSurfaceElement<R>` — SDF rounded-corner clipped & opacity-modulated Wayland surface,
//! 2. `KomaRoundedBorderElement` — analytical SDF anti-aliased outer/inner rounded border ring,
//! 3. `KomaBlurredBackdropElement` — frosted-glass `Dual Kawase Blur` slice clipped to rounded window,
//! 4. `KomaSolidColorElement` — fractional-scale solid rects (`#000000` Protected blackout, `grid_225`).

use koma_model::{AgentCursorFrameSnapshot, LabelPillSnapshot, LogicalPoint};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Color32F, Frame, ImportAll, ImportMem, Renderer, buffer_y_inverted,
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
    MemoryBuffer = MemoryRenderBufferRenderElement<R>,
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
    if spec.width == 0 || spec.height == 0 {
        return None;
    }
    let int_scale = output_scale.x.ceil().clamp(1.0, 4.0) as i32;
    let (pixels, px_w, px_h) = rasterize_text_panel_argb8888(spec, int_scale);
    let buffer = MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (px_w as i32, px_h as i32),
        int_scale,
        Transform::Normal,
        None,
    );
    let loc_phys: Point<i32, Physical> =
        Point::from((location.x, location.y)).to_physical_precise_round(output_scale);
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        loc_phys.to_f64(),
        &buffer,
        None,
        None,
        None,
        Kind::Unspecified,
    )
    .ok()
    .map(KomaRenderElement::MemoryBuffer)
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
}
