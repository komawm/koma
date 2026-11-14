//! Render elements for `Koma`:
//! 1. `KomaClippedSurfaceElement<R>` — SDF rounded-corner clipped & opacity-modulated Wayland surface,
//! 2. `KomaRoundedBorderElement` — analytical SDF anti-aliased outer/inner rounded border ring,
//! 3. `KomaBlurredBackdropElement` — frosted-glass `Dual Kawase Blur` slice clipped to rounded window,
//! 4. `KomaSolidColorElement` — fractional-scale solid rects (`#000000` Protected blackout, `grid_225`).

use smithay::{
    backend::renderer::{
        Color32F, Frame, ImportAll, Renderer, buffer_y_inverted,
        element::{
            Element, Id, Kind, RenderElement, UnderlyingStorage, render_elements,
            surface::WaylandSurfaceRenderElement,
        },
        gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform},
        multigpu::{MultiError, MultiFrame, MultiRenderer, gbm::GbmGlesBackend},
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

type TtyMultiFrame<'a, 'render, 'frame, 'buffer> = MultiFrame<
    'a,
    'render,
    'frame,
    'buffer,
    GbmGlesBackend<GlesRenderer, DeviceFd>,
    GbmGlesBackend<GlesRenderer, DeviceFd>,
>;

type TtyMultiError<'render> =
    MultiError<GbmGlesBackend<GlesRenderer, DeviceFd>, GbmGlesBackend<GlesRenderer, DeviceFd>>;

// ==============================================================================
// 1. `KomaClippedSurfaceElement<R>` (SDF Rounded-Corner Clipping + Opacity)
// ==============================================================================

#[derive(Debug)]
pub struct KomaClippedSurfaceElement<R: Renderer + ImportAll> {
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
        let clip_phys = self.clip_geo.to_physical_precise_round(scale);

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

impl RenderElement<GlesRenderer> for KomaClippedSurfaceElement<GlesRenderer> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.override_default_tex_program(self.program.clone(), self.compute_uniforms());
        let res = RenderElement::<GlesRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        );
        frame.clear_tex_program_override();
        res
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

impl<'render> RenderElement<TtyMultiRenderer<'render>>
    for KomaClippedSurfaceElement<TtyMultiRenderer<'render>>
{
    fn draw(
        &self,
        frame: &mut TtyMultiFrame<'_, 'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyMultiError<'render>> {
        frame
            .as_gles_frame()
            .override_default_tex_program(self.program.clone(), self.compute_uniforms());
        let res = RenderElement::draw(&self.inner, frame, src, dst, damage, opaque_regions, cache);
        frame.as_gles_frame().clear_tex_program_override();
        res
    }

    fn underlying_storage(
        &self,
        _renderer: &mut TtyMultiRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
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
            opacity: opacity.clamp(0.0, 1.0),
            scale: scale.x as f32,
            carrier_tex,
            program,
        }
    }

    fn uniforms(&self) -> [Uniform<'static>; 5] {
        [
            Uniform::new("koma_scale", self.scale),
            Uniform::new(
                "geo_size",
                (self.geometry.size.w as f32, self.geometry.size.h as f32),
            ),
            Uniform::new("outer_radius", self.outer_radius),
            Uniform::new("border_width", self.border_width),
            Uniform::new("border_color", self.border_color),
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

impl RenderElement<GlesRenderer> for KomaRoundedBorderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let uniforms = self.uniforms();
        frame.render_texture_from_to(
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
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

impl<'render> RenderElement<TtyMultiRenderer<'render>> for KomaRoundedBorderElement {
    fn draw(
        &self,
        frame: &mut TtyMultiFrame<'_, 'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyMultiError<'render>> {
        RenderElement::<GlesRenderer>::draw(
            self,
            frame.as_gles_frame(),
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )
        .map_err(TtyMultiError::Render)
    }

    fn underlying_storage(
        &self,
        _renderer: &mut TtyMultiRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
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
        let phys = self.geometry.to_physical_precise_round(scale);
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

impl RenderElement<GlesRenderer> for KomaBlurredBackdropElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let uniforms = self.uniforms();
        frame.render_texture_from_to(
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
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

impl<'render> RenderElement<TtyMultiRenderer<'render>> for KomaBlurredBackdropElement {
    fn draw(
        &self,
        frame: &mut TtyMultiFrame<'_, 'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyMultiError<'render>> {
        RenderElement::<GlesRenderer>::draw(
            self,
            frame.as_gles_frame(),
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )
        .map_err(TtyMultiError::Render)
    }

    fn underlying_storage(
        &self,
        _renderer: &mut TtyMultiRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
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

render_elements! {
    pub KomaRenderElement<R> where R: smithay::backend::renderer::ImportAll;
    Surface = WaylandSurfaceRenderElement<R>,
    ClippedSurface = KomaClippedSurfaceElement<R>,
    RoundedBorder = KomaRoundedBorderElement,
    BlurredBackdrop = KomaBlurredBackdropElement,
    Solid = KomaSolidColorElement,
}

/// Converts a `koma-model` `LogicalRect` into a Smithay `Rectangle<f64, Logical>`.
#[must_use]
pub fn to_smithay_logical_rect(rect: koma_model::LogicalRect) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((f64::from(rect.x), f64::from(rect.y))),
        Size::from((f64::from(rect.width), f64::from(rect.height))),
    )
}
