//! OpenGL ES 2.0 / 3.0 custom shaders (`ClippedSurface`, `RoundedBorder`, `BlurFinishRounded`)
//! and `KomaGlesRenderer` / `KomaGlesFrame` abstractions shared across nested `Winit`
//! (`GlesRenderer`) and bare-metal `DRM/KMS` (`MultiRenderer` / `TtyRenderer`).
//!
//! Modeled directly on `niri/src/render_helpers/{shaders/mod.rs, clipped_surface.rs, renderer.rs}`
//! and `Hyprland/src/render/shaders/glsl/blurFinish.glsl`.

use std::sync::Mutex;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportMem as _,
            gles::{
                GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName,
                UniformType, UniformValue,
            },
            multigpu::{MultiFrame, MultiRenderer, gbm::GbmGlesBackend},
        },
    },
    utils::{DeviceFd, Size},
};
use tracing::warn;

/// Unifies access to the underlying primary `GlesRenderer` across nested `Winit` (`GlesRenderer`)
/// and bare-metal `DRM/KMS` (`MultiRenderer`).
pub trait KomaGlesRenderer {
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer;
}

impl KomaGlesRenderer for GlesRenderer {
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer {
        self
    }
}

impl KomaGlesRenderer
    for MultiRenderer<
        '_,
        '_,
        GbmGlesBackend<GlesRenderer, DeviceFd>,
        GbmGlesBackend<GlesRenderer, DeviceFd>,
    >
{
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer {
        self.as_mut()
    }
}

/// Unifies access to `GlesFrame` across `GlesRenderer` and `MultiRenderer`.
pub trait KomaGlesFrame<'frame, 'buffer>
where
    Self: 'frame,
{
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer>;
}

impl<'frame, 'buffer> KomaGlesFrame<'frame, 'buffer> for GlesFrame<'frame, 'buffer> {
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer> {
        self
    }
}

impl<'frame, 'buffer> KomaGlesFrame<'frame, 'buffer>
    for MultiFrame<
        '_,
        '_,
        'frame,
        'buffer,
        GbmGlesBackend<GlesRenderer, DeviceFd>,
        GbmGlesBackend<GlesRenderer, DeviceFd>,
    >
{
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer> {
        self.as_mut()
    }
}

/// Analytical 1-pixel smoothstep Signed Distance Field corner-rounding helper
/// (`niri/src/render_helpers/shaders/rounding_alpha.frag`).
pub const GLSL_ROUNDING_ALPHA_FN: &str = r#"
float koma_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius, float scale) {
    vec2 center;
    float radius;

    if (coords.x < corner_radius.x && coords.y < corner_radius.x) {
        radius = corner_radius.x;
        center = vec2(radius, radius);
    } else if (size.x - corner_radius.y < coords.x && coords.y < corner_radius.y) {
        radius = corner_radius.y;
        center = vec2(size.x - radius, radius);
    } else if (size.x - corner_radius.z < coords.x && size.y - corner_radius.z < coords.y) {
        radius = corner_radius.z;
        center = vec2(size.x - radius, size.y - radius);
    } else if (coords.x < corner_radius.w && size.y - corner_radius.w < coords.y) {
        radius = corner_radius.w;
        center = vec2(radius, size.y - radius);
    } else {
        return 1.0;
    }

    float dist = distance(coords, center);
    float t = clamp((dist - radius) * scale + 0.5, 0.0, 1.0);
    return 1.0 - t * t * (3.0 - 2.0 * t);
}
"#;

/// Wayland surface texture shader applying SDF rounded-corner clipping + `window_opacity`
/// (`niri/src/render_helpers/shaders/clipped_surface.frag`).
const CLIPPED_SURFACE_FRAG_BODY: &str = r#"
#version 100
//_DEFINES_
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
precision highp float;

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float koma_scale;
uniform float window_opacity;
uniform vec2 geo_size;
uniform vec4 corner_radius;
uniform mat3 input_to_geo;

float koma_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius, float scale);

void main() {
    vec3 coords_geo = input_to_geo * vec3(v_coords, 1.0);
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0);
#endif

    if (coords_geo.x < 0.0 || 1.0 < coords_geo.x || coords_geo.y < 0.0 || 1.0 < coords_geo.y) {
        color = vec4(0.0);
    } else {
        float mask = koma_rounding_alpha(coords_geo.xy * geo_size, geo_size, corner_radius, koma_scale);
        color = color * mask;
    }

    color = color * (alpha * window_opacity);
#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif
    gl_FragColor = color;
}
"#;

/// Analytical SDF rounded hollow border ring shader (`niri/src/render_helpers/shaders/border.frag`).
const ROUNDED_BORDER_FRAG_BODY: &str = r#"
#version 100
//_DEFINES_
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
precision highp float;

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float koma_scale;
uniform vec2 geo_size;
uniform vec4 outer_radius;
uniform float border_width;
uniform vec4 border_color;

float koma_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius, float scale);

void main() {
    vec4 dummy_sample = texture2D(tex, v_coords);
    vec2 coords = v_coords * geo_size;

    // Premultiply RGB by alpha
    vec4 color = vec4(border_color.rgb * border_color.a, border_color.a) * (dummy_sample.a * 0.0 + 1.0);
    color *= koma_rounding_alpha(coords, geo_size, outer_radius, koma_scale);

    if (border_width > 0.0) {
        vec2 inner_coords = coords - vec2(border_width);
        vec2 inner_size = geo_size - vec2(border_width * 2.0);
        if (0.0 <= inner_coords.x && inner_coords.x <= inner_size.x
            && 0.0 <= inner_coords.y && inner_coords.y <= inner_size.y)
        {
            vec4 inner_radius = max(outer_radius - vec4(border_width), 0.0);
            color *= (1.0 - koma_rounding_alpha(inner_coords, inner_size, inner_radius, koma_scale));
        }
    }

    gl_FragColor = color * alpha;
}
"#;

/// Post-Kawase frosted-glass finish & SDF rounded-corner clip shader
/// (`Hyprland/src/render/shaders/glsl/{blurprepare.glsl, blurFinish.glsl}`).
const BLUR_FINISH_ROUNDED_FRAG_BODY: &str = r#"
#version 100
//_DEFINES_
#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
precision highp float;

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float koma_scale;
uniform vec2 geo_size;
uniform vec4 corner_radius;
uniform vec2 local_uv_origin;
uniform vec2 local_uv_scale;
uniform float blur_noise;
uniform float blur_contrast;
uniform float blur_brightness;

float koma_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius, float scale);

float hypr_hash(vec2 p) {
    vec3 p3 = fract(vec3(p.xyx) * 1689.1984);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

void main() {
    vec4 pix = texture2D(tex, v_coords);

    // Contrast around mid-gray (Hyprland blurprepare)
    pix.rgb = clamp((pix.rgb - 0.5) * blur_contrast + 0.5, 0.0, 1.0);

    // Anti-banding hash dithering noise + brightness (Hyprland blurFinish.glsl)
    float n = (hypr_hash(v_coords * geo_size) - 0.5) * blur_noise;
    pix.rgb = clamp((pix.rgb + n * pix.a) * blur_brightness, 0.0, 1.0);

    vec2 local_norm = (v_coords - local_uv_origin) * local_uv_scale;
    float mask = koma_rounding_alpha(local_norm * geo_size, geo_size, corner_radius, koma_scale);

    gl_FragColor = pix * (mask * alpha);
}
"#;

/// Compiled custom `GlesTexProgram` suite + `1x1` solid carrier texture cached on `EGLContext::user_data()`.
#[derive(Debug, Clone)]
pub struct KomaCompiledShaders {
    pub clipped_surface: Option<GlesTexProgram>,
    pub rounded_border: Option<GlesTexProgram>,
    pub blur_finish_rounded: Option<GlesTexProgram>,
    pub white_1x1_tex: Option<GlesTexture>,
}

struct CachedShadersSlot(Mutex<Option<KomaCompiledShaders>>);

impl KomaCompiledShaders {
    /// Lazily compiles and caches all custom GLSL shaders inside `renderer.egl_context().user_data()`.
    pub fn get_or_compile(renderer: &mut GlesRenderer) -> Self {
        renderer
            .egl_context()
            .user_data()
            .insert_if_missing_threadsafe(|| CachedShadersSlot(Mutex::new(None)));

        if let Some(slot) = renderer
            .egl_context()
            .user_data()
            .get::<CachedShadersSlot>()
            && let Some(existing) = slot.0.lock().unwrap().clone()
        {
            return existing;
        }

        let clipped_src = format!("{CLIPPED_SURFACE_FRAG_BODY}\n{GLSL_ROUNDING_ALPHA_FN}");
        let clipped_surface = renderer
            .compile_custom_texture_shader(
                &clipped_src,
                &[
                    UniformName::new("koma_scale", UniformType::_1f),
                    UniformName::new("window_opacity", UniformType::_1f),
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("corner_radius", UniformType::_4f),
                    UniformName::new("input_to_geo", UniformType::Matrix3x3),
                ],
            )
            .map_err(|e| warn!("failed compiling clipped_surface GLSL shader: {e:?}"))
            .ok();

        let border_src = format!("{ROUNDED_BORDER_FRAG_BODY}\n{GLSL_ROUNDING_ALPHA_FN}");
        let rounded_border = renderer
            .compile_custom_texture_shader(
                &border_src,
                &[
                    UniformName::new("koma_scale", UniformType::_1f),
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("outer_radius", UniformType::_4f),
                    UniformName::new("border_width", UniformType::_1f),
                    UniformName::new("border_color", UniformType::_4f),
                ],
            )
            .map_err(|e| warn!("failed compiling rounded_border GLSL shader: {e:?}"))
            .ok();

        let blur_finish_src = format!("{BLUR_FINISH_ROUNDED_FRAG_BODY}\n{GLSL_ROUNDING_ALPHA_FN}");
        let blur_finish_rounded = renderer
            .compile_custom_texture_shader(
                &blur_finish_src,
                &[
                    UniformName::new("koma_scale", UniformType::_1f),
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("corner_radius", UniformType::_4f),
                    UniformName::new("local_uv_origin", UniformType::_2f),
                    UniformName::new("local_uv_scale", UniformType::_2f),
                    UniformName::new("blur_noise", UniformType::_1f),
                    UniformName::new("blur_contrast", UniformType::_1f),
                    UniformName::new("blur_brightness", UniformType::_1f),
                ],
            )
            .map_err(|e| warn!("failed compiling blur_finish_rounded GLSL shader: {e:?}"))
            .ok();

        let white_1x1_tex = renderer
            .import_memory(
                &[255_u8, 255, 255, 255],
                Fourcc::Abgr8888,
                Size::from((1, 1)),
                false,
            )
            .map_err(|e| warn!("failed creating 1x1 white carrier texture: {e:?}"))
            .ok();

        let compiled = Self {
            clipped_surface,
            rounded_border,
            blur_finish_rounded,
            white_1x1_tex,
        };

        if let Some(slot) = renderer
            .egl_context()
            .user_data()
            .get::<CachedShadersSlot>()
        {
            *slot.0.lock().unwrap() = Some(compiled.clone());
        }
        compiled
    }
}

/// Pure-Rust column-major 3x3 homogeneous matrix (`[f32; 9]`) avoiding external math crate bloat.
#[derive(Debug, Clone, Copy)]
pub struct Mat3f(pub [f32; 9]);

impl Mat3f {
    pub const IDENTITY: Self = Self([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);

    #[must_use]
    pub const fn from_translation(tx: f32, ty: f32) -> Self {
        Self([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, tx, ty, 1.0])
    }

    #[must_use]
    pub const fn from_scale(sx: f32, sy: f32) -> Self {
        Self([sx, 0.0, 0.0, 0.0, sy, 0.0, 0.0, 0.0, 1.0])
    }

    #[must_use]
    #[allow(clippy::should_implement_trait)] // Matrix composition helper called explicitly as `.mul(...)`.
    pub fn mul(self, rhs: Self) -> Self {
        let a = self.0;
        let b = rhs.0;
        let mut out = [0.0_f32; 9];
        for col in 0..3 {
            for row in 0..3 {
                out[col * 3 + row] =
                    a[row] * b[col * 3] + a[3 + row] * b[col * 3 + 1] + a[6 + row] * b[col * 3 + 2];
            }
        }
        Self(out)
    }
}

#[must_use]
pub fn mat3_uniform(name: &'static str, mat: Mat3f) -> Uniform<'static> {
    Uniform::new(
        name,
        UniformValue::Matrix3x3 {
            matrices: vec![mat.0],
            transpose: false,
        },
    )
}
