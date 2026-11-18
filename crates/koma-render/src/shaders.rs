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
                GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
                UniformName, UniformType, UniformValue,
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
uniform vec4 border_color_end;
uniform float gradient_angle_rad;
uniform mat3 input_to_geo;

float koma_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius, float scale);

void main() {
    vec4 dummy_sample = texture2D(tex, v_coords);
    vec2 geo_uv = (input_to_geo * vec3(v_coords, 1.0)).xy;
    vec2 coords = geo_uv * geo_size;

    vec2 grad_dir = vec2(cos(gradient_angle_rad), sin(gradient_angle_rad));
    float grad_t = clamp(dot(geo_uv - vec2(0.5), grad_dir) + 0.5, 0.0, 1.0);
    vec4 grad_rgba = mix(border_color, border_color_end, grad_t);

    // Premultiply RGB by alpha
    vec4 color = vec4(grad_rgba.rgb * grad_rgba.a, grad_rgba.a) * (dummy_sample.a * 0.0 + 1.0);
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
uniform float blur_pre_toned;

float koma_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius, float scale);

float hypr_hash(vec2 p) {
    vec3 p3 = fract(vec3(p.xyx) * 1689.1984);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

void main() {
    vec4 pix = texture2D(tex, v_coords);

    if (blur_pre_toned < 0.5) {
        // Contrast around mid-gray (Hyprland blurprepare)
        pix.rgb = clamp((pix.rgb - 0.5) * blur_contrast + 0.5, 0.0, 1.0);

        // Anti-banding hash dithering noise + brightness (Hyprland blurFinish.glsl)
        float n = (hypr_hash(v_coords * geo_size) - 0.5) * blur_noise;
        pix.rgb = clamp((pix.rgb + n * pix.a) * blur_brightness, 0.0, 1.0);
    }

    vec2 local_norm = (v_coords - local_uv_origin) * local_uv_scale;
    float mask = koma_rounding_alpha(local_norm * geo_size, geo_size, corner_radius, koma_scale);

    gl_FragColor = pix * (mask * alpha);
}
"#;

/// Analytical erf/Gaussian rounded-rectangle drop shadow fragment body for Smithay `GlesPixelProgram`.
///
/// Compiled via `GlesRenderer::compile_custom_pixel_shader`, which prepends `#version 100`
/// and optional `#define DEBUG_FLAGS` and supplies `v_coords`, `size`, `alpha`, and `tint`.
const ROUNDED_SHADOW_FRAG_BODY: &str = r#"
precision highp float;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;

uniform float koma_scale;
uniform vec4 shadow_color;
uniform float shadow_sigma;
uniform float shadow_sharp;
uniform vec2 geo_size;
uniform vec4 corner_radius;
uniform mat3 input_to_geo;

uniform vec2 window_geo_size;
uniform vec4 window_corner_radius;
uniform mat3 window_input_to_geo;

float koma_rounding_alpha(vec2 coords, vec2 size, vec4 corner_radius, float scale);

float koma_gaussian(float x, float sigma) {
    const float pi = 3.141592653589793;
    return exp(-(x * x) / (2.0 * sigma * sigma)) / (sqrt(2.0 * pi) * sigma);
}

// Abramowitz & Stegun 7.1.25 rational approximation of erf(x) in GLES2.
vec2 koma_erf(vec2 x) {
    vec2 s = sign(x);
    vec2 a = abs(x);
    vec2 t = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
    t *= t;
    return s - s / (t * t);
}

float koma_rounded_box_shadow_x(float x, float y, float sigma, float corner, vec2 half_size) {
    float delta = min(half_size.y - corner - abs(y), 0.0);
    float curved = half_size.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    vec2 integral = 0.5 + 0.5 * koma_erf((x + vec2(-curved, curved)) * (sqrt(0.5) / sigma));
    return integral.y - integral.x;
}

float koma_rounded_box_shadow(vec2 box_size, vec2 point, float sigma, float corner) {
    vec2 half_size = box_size * 0.5;
    float clamped_corner = clamp(corner, 0.0, min(half_size.x, half_size.y));
    vec2 p = point - half_size;

    float low = p.y - half_size.y;
    float high = p.y + half_size.y;
    float start = clamp(-3.0 * sigma, low, high);
    float end = clamp(3.0 * sigma, low, high);

    float step_y = (end - start) / 4.0;
    if (step_y <= 0.0) {
        return 0.0;
    }

    float y = start + step_y * 0.5;
    float acc = 0.0;
    for (int i = 0; i < 4; i++) {
        acc += koma_rounded_box_shadow_x(p.x, p.y - y, sigma, clamped_corner, half_size)
            * koma_gaussian(y, sigma)
            * step_y;
        y += step_y;
    }
    return clamp(acc, 0.0, 1.0);
}

void main() {
    vec2 coords_geo = (input_to_geo * vec3(v_coords, 1.0)).xy;
    float shadow_mask;

    if (shadow_sharp > 0.5 || shadow_sigma < 0.1) {
        if (0.0 <= coords_geo.x && coords_geo.x <= geo_size.x
            && 0.0 <= coords_geo.y && coords_geo.y <= geo_size.y)
        {
            shadow_mask = koma_rounding_alpha(coords_geo, geo_size, corner_radius, koma_scale);
        } else {
            shadow_mask = 0.0;
        }
    } else {
        shadow_mask = koma_rounded_box_shadow(geo_size, coords_geo, shadow_sigma, corner_radius.x);
    }

    if (window_geo_size.x > 0.0 && window_geo_size.y > 0.0) {
        vec2 win_coords = (window_input_to_geo * vec3(v_coords, 1.0)).xy;
        if (0.0 <= win_coords.x && win_coords.x <= window_geo_size.x
            && 0.0 <= win_coords.y && win_coords.y <= window_geo_size.y)
        {
            float inside = koma_rounding_alpha(
                win_coords,
                window_geo_size,
                window_corner_radius,
                koma_scale
            );
            shadow_mask *= (1.0 - inside);
        }
    }

    vec4 color = vec4(shadow_color.rgb * shadow_color.a, shadow_color.a);
    color *= shadow_mask * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0) {
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
    }
#endif

    gl_FragColor = color;
}
"#;

/// Returns the combined GLSL fragment source for `KomaRoundedBorderElement`.
#[must_use]
pub fn rounded_border_fragment_source() -> String {
    format!("{ROUNDED_BORDER_FRAG_BODY}\n{GLSL_ROUNDING_ALPHA_FN}")
}

/// Returns the combined GLSL fragment source for `KomaBlurredBackdropElement`.
#[must_use]
pub fn blur_finish_rounded_fragment_source() -> String {
    format!("{BLUR_FINISH_ROUNDED_FRAG_BODY}\n{GLSL_ROUNDING_ALPHA_FN}")
}

/// Returns the combined GLSL fragment source passed to `GlesRenderer::compile_custom_pixel_shader`.
#[must_use]
pub fn rounded_shadow_fragment_source() -> String {
    format!("{ROUNDED_SHADOW_FRAG_BODY}\n{GLSL_ROUNDING_ALPHA_FN}")
}

/// SDF dart cursor, ambient aura fog, and expanding click ripple fragment shader body
/// for Smithay `GlesPixelProgram` (compiled via `compile_custom_pixel_shader`).
const AGENT_CURSOR_FRAG_BODY: &str = r#"
precision highp float;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;

uniform vec2 elem_size;
uniform float koma_scale;
uniform vec2 tip_local;
uniform vec2 body_offset;
uniform float cursor_rot;
uniform float cursor_size;
uniform float body_scale;
uniform vec4 accent_color;
uniform vec2 fog_offset;
uniform float fog_scale;
uniform float fog_opacity;
uniform vec4 ripple0;
uniform vec4 ripple1;

float koma_sd_triangle(vec2 p, vec2 p0, vec2 p1, vec2 p2) {
    vec2 e0 = p1 - p0;
    vec2 e1 = p2 - p1;
    vec2 e2 = p0 - p2;
    vec2 v0 = p - p0;
    vec2 v1 = p - p1;
    vec2 v2 = p - p2;
    vec2 pq0 = v0 - e0 * clamp(dot(v0, e0) / max(dot(e0, e0), 1e-6), 0.0, 1.0);
    vec2 pq1 = v1 - e1 * clamp(dot(v1, e1) / max(dot(e1, e1), 1e-6), 0.0, 1.0);
    vec2 pq2 = v2 - e2 * clamp(dot(v2, e2) / max(dot(e2, e2), 1e-6), 0.0, 1.0);
    float s = sign(e0.x * e2.y - e0.y * e2.x);
    vec2 d = min(
        min(
            vec2(dot(pq0, pq0), s * (v0.x * e0.y - v0.y * e0.x)),
            vec2(dot(pq1, pq1), s * (v1.x * e1.y - v1.y * e1.x))
        ),
        vec2(dot(pq2, pq2), s * (v2.x * e2.y - v2.y * e2.x))
    );
    return -sqrt(d.x) * sign(d.y);
}

vec4 koma_over(vec4 dst_premul, vec4 src_premul) {
    return src_premul + dst_premul * (1.0 - src_premul.a);
}

vec4 koma_eval_ripple(vec2 p, vec4 r, vec3 rgb, float aa) {
    if (r.w <= 0.001 || r.z <= 0.5) {
        return vec4(0.0);
    }
    float dist_c = length(p - r.xy);
    float ring = 1.0 - smoothstep(1.3 - aa, 1.6 + aa, abs(dist_c - r.z));
    float inner_wash = smoothstep(r.z, 0.0, dist_c) * 0.12;
    float a = clamp((ring + inner_wash) * r.w, 0.0, 1.0);
    return vec4(rgb * a, a);
}

float koma_cursor_arrow_sdf(vec2 d_px, float rot_rad, float scale_px) {
    float c = cos(rot_rad);
    float s = sin(rot_rad);
    vec2 q = vec2(c * d_px.x + s * d_px.y, -s * d_px.x + c * d_px.y) / max(scale_px, 1.0);
    vec2 A = vec2(0.0, 0.0);
    vec2 B = vec2(-0.88, -0.36);
    vec2 C = vec2(-0.62, 0.0);
    vec2 D = vec2(-0.88, 0.36);
    float d1 = koma_sd_triangle(q, A, B, C);
    float d2 = koma_sd_triangle(q, A, C, D);
    return min(d1, d2) * scale_px;
}

void main() {
    vec2 p = v_coords * elem_size;
    float aa = 1.0 / max(koma_scale, 1.0);
    vec4 col = vec4(0.0);

    // 1. Ambient aura fog centered near velocity-lagged tip
    if (fog_opacity > 0.001) {
        vec2 fog_center = tip_local + fog_offset;
        float fog_r = max(cursor_size * 1.15 * fog_scale, 4.0);
        float d_fog = length(p - fog_center) / fog_r;
        float fog_mask = exp(-3.2 * d_fog * d_fog) * smoothstep(1.0, 0.12, d_fog);
        float fog_a = clamp(fog_mask * fog_opacity, 0.0, 1.0);
        col = koma_over(col, vec4(accent_color.rgb * fog_a, fog_a));
    }

    // 2. Expanding click ripples
    col = koma_over(col, koma_eval_ripple(p, ripple0, accent_color.rgb, aa));
    col = koma_over(col, koma_eval_ripple(p, ripple1, accent_color.rgb, aa));

    // 3. Soft drop shadow + crisp dark border halo + luminous accent dart core
    vec2 body_origin = tip_local + body_offset;
    float scale_px = cursor_size * max(body_scale, 0.25);

    float sh_sdf = koma_cursor_arrow_sdf(p - (body_origin + vec2(1.2, 1.8)), cursor_rot, scale_px);
    float sh_a = (1.0 - smoothstep(-1.0, 3.4, sh_sdf)) * 0.38;
    col = koma_over(col, vec4(0.0, 0.0, 0.0, sh_a));

    float sdf = koma_cursor_arrow_sdf(p - body_origin, cursor_rot, scale_px);
    float outline_a = 1.0 - smoothstep(-aa, aa, sdf - 1.35);
    col = koma_over(col, vec4(vec3(0.07, 0.07, 0.11) * outline_a, outline_a));

    float core_a = 1.0 - smoothstep(-aa, aa, sdf);
    float tip_glow = smoothstep(scale_px * 0.85, 0.0, length(p - body_origin));
    vec3 core_rgb = mix(accent_color.rgb, vec3(0.97, 0.96, 1.0), 0.38 * tip_glow);
    col = koma_over(col, vec4(core_rgb * core_a, core_a));

    col *= alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0) {
        col = vec4(0.0, 0.2, 0.0, 0.2) + col * 0.8;
    }
#endif

    gl_FragColor = col;
}
"#;

/// Tapered ribbon motion-trail fragment shader body for Smithay `GlesPixelProgram`
/// using 8 individual `vec4` uniforms (`trail_p0`..`trail_p7`) to obey GLSL ES 1.00.
const AGENT_TRAIL_FRAG_BODY: &str = r#"
precision highp float;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float alpha;
uniform vec2 size;
varying vec2 v_coords;

uniform vec2 elem_size;
uniform float koma_scale;
uniform vec4 accent_color;
uniform float point_count;
uniform vec4 trail_p0;
uniform vec4 trail_p1;
uniform vec4 trail_p2;
uniform vec4 trail_p3;
uniform vec4 trail_p4;
uniform vec4 trail_p5;
uniform vec4 trail_p6;
uniform vec4 trail_p7;

float koma_eval_trail_seg(vec2 p, vec4 a, vec4 b, float aa) {
    vec2 pa = p - a.xy;
    vec2 ba = b.xy - a.xy;
    float denom = max(dot(ba, ba), 1e-4);
    float h = clamp(dot(pa, ba) / denom, 0.0, 1.0);
    float r = max(mix(a.w, b.w, h), 0.5);
    float d = length(pa - ba * h) - r;
    float cov = 1.0 - smoothstep(-aa, aa, d);
    return cov * mix(a.z, b.z, h);
}

void main() {
    vec2 p = v_coords * elem_size;
    float aa = 1.0 / max(koma_scale, 1.0);
    float best = 0.0;

    if (point_count > 1.5) {
        best = max(best, koma_eval_trail_seg(p, trail_p0, trail_p1, aa));
    }
    if (point_count > 2.5) {
        best = max(best, koma_eval_trail_seg(p, trail_p1, trail_p2, aa));
    }
    if (point_count > 3.5) {
        best = max(best, koma_eval_trail_seg(p, trail_p2, trail_p3, aa));
    }
    if (point_count > 4.5) {
        best = max(best, koma_eval_trail_seg(p, trail_p3, trail_p4, aa));
    }
    if (point_count > 5.5) {
        best = max(best, koma_eval_trail_seg(p, trail_p4, trail_p5, aa));
    }
    if (point_count > 6.5) {
        best = max(best, koma_eval_trail_seg(p, trail_p5, trail_p6, aa));
    }
    if (point_count > 7.5) {
        best = max(best, koma_eval_trail_seg(p, trail_p6, trail_p7, aa));
    }

    float a = clamp(best * accent_color.a * alpha, 0.0, 1.0);
    vec4 col = vec4(accent_color.rgb * a, a);

#if defined(DEBUG_FLAGS)
    if (tint == 1.0) {
        col = vec4(0.0, 0.2, 0.0, 0.2) + col * 0.8;
    }
#endif

    gl_FragColor = col;
}
"#;

/// Returns the GLSL fragment source for `KomaAgentCursorElement`.
#[must_use]
pub fn agent_cursor_fragment_source() -> String {
    AGENT_CURSOR_FRAG_BODY.to_owned()
}

/// Returns the GLSL fragment source for `KomaAgentTrailElement`.
#[must_use]
pub fn agent_trail_fragment_source() -> String {
    AGENT_TRAIL_FRAG_BODY.to_owned()
}

/// Compiled custom `GlesTexProgram` / `GlesPixelProgram` suite + `1x1` solid carrier texture
/// cached on `EGLContext::user_data()`.
#[derive(Debug, Clone)]
pub struct KomaCompiledShaders {
    pub clipped_surface: Option<GlesTexProgram>,
    pub rounded_border: Option<GlesTexProgram>,
    pub blur_finish_rounded: Option<GlesTexProgram>,
    pub rounded_shadow: Option<GlesPixelProgram>,
    pub agent_cursor: Option<GlesPixelProgram>,
    pub agent_trail: Option<GlesPixelProgram>,
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

        let border_src = rounded_border_fragment_source();
        let rounded_border = renderer
            .compile_custom_texture_shader(
                &border_src,
                &[
                    UniformName::new("koma_scale", UniformType::_1f),
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("outer_radius", UniformType::_4f),
                    UniformName::new("border_width", UniformType::_1f),
                    UniformName::new("border_color", UniformType::_4f),
                    UniformName::new("border_color_end", UniformType::_4f),
                    UniformName::new("gradient_angle_rad", UniformType::_1f),
                    UniformName::new("input_to_geo", UniformType::Matrix3x3),
                ],
            )
            .map_err(|e| warn!("failed compiling rounded_border GLSL shader: {e:?}"))
            .ok();

        let blur_finish_src = blur_finish_rounded_fragment_source();
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
                    UniformName::new("blur_pre_toned", UniformType::_1f),
                ],
            )
            .map_err(|e| warn!("failed compiling blur_finish_rounded GLSL shader: {e:?}"))
            .ok();

        let shadow_src = rounded_shadow_fragment_source();
        let rounded_shadow = renderer
            .compile_custom_pixel_shader(
                &shadow_src,
                &[
                    UniformName::new("koma_scale", UniformType::_1f),
                    UniformName::new("shadow_color", UniformType::_4f),
                    UniformName::new("shadow_sigma", UniformType::_1f),
                    UniformName::new("shadow_sharp", UniformType::_1f),
                    UniformName::new("geo_size", UniformType::_2f),
                    UniformName::new("corner_radius", UniformType::_4f),
                    UniformName::new("input_to_geo", UniformType::Matrix3x3),
                    UniformName::new("window_geo_size", UniformType::_2f),
                    UniformName::new("window_corner_radius", UniformType::_4f),
                    UniformName::new("window_input_to_geo", UniformType::Matrix3x3),
                ],
            )
            .map_err(|e| warn!("failed compiling rounded_shadow GLSL pixel shader: {e:?}"))
            .ok();

        let agent_cursor_src = agent_cursor_fragment_source();
        let agent_cursor = renderer
            .compile_custom_pixel_shader(
                &agent_cursor_src,
                &[
                    UniformName::new("elem_size", UniformType::_2f),
                    UniformName::new("koma_scale", UniformType::_1f),
                    UniformName::new("tip_local", UniformType::_2f),
                    UniformName::new("body_offset", UniformType::_2f),
                    UniformName::new("cursor_rot", UniformType::_1f),
                    UniformName::new("cursor_size", UniformType::_1f),
                    UniformName::new("body_scale", UniformType::_1f),
                    UniformName::new("accent_color", UniformType::_4f),
                    UniformName::new("fog_offset", UniformType::_2f),
                    UniformName::new("fog_scale", UniformType::_1f),
                    UniformName::new("fog_opacity", UniformType::_1f),
                    UniformName::new("ripple0", UniformType::_4f),
                    UniformName::new("ripple1", UniformType::_4f),
                ],
            )
            .map_err(|e| warn!("failed compiling agent_cursor GLSL pixel shader: {e:?}"))
            .ok();

        let agent_trail_src = agent_trail_fragment_source();
        let agent_trail = renderer
            .compile_custom_pixel_shader(
                &agent_trail_src,
                &[
                    UniformName::new("elem_size", UniformType::_2f),
                    UniformName::new("koma_scale", UniformType::_1f),
                    UniformName::new("accent_color", UniformType::_4f),
                    UniformName::new("point_count", UniformType::_1f),
                    UniformName::new("trail_p0", UniformType::_4f),
                    UniformName::new("trail_p1", UniformType::_4f),
                    UniformName::new("trail_p2", UniformType::_4f),
                    UniformName::new("trail_p3", UniformType::_4f),
                    UniformName::new("trail_p4", UniformType::_4f),
                    UniformName::new("trail_p5", UniformType::_4f),
                    UniformName::new("trail_p6", UniformType::_4f),
                    UniformName::new("trail_p7", UniformType::_4f),
                ],
            )
            .map_err(|e| warn!("failed compiling agent_trail GLSL pixel shader: {e:?}"))
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
            rounded_shadow,
            agent_cursor,
            agent_trail,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_shadow_shader_omits_version_header_and_defines_required_uniforms() {
        let src = rounded_shadow_fragment_source();
        assert!(
            !src.contains("#version"),
            "Smithay compile_custom_pixel_shader prepends #version 100 itself"
        );
        for required in [
            "varying vec2 v_coords;",
            "uniform float alpha;",
            "uniform float koma_scale;",
            "uniform vec4 shadow_color;",
            "uniform float shadow_sigma;",
            "uniform float shadow_sharp;",
            "uniform vec2 geo_size;",
            "uniform vec4 corner_radius;",
            "uniform mat3 input_to_geo;",
            "uniform vec2 window_geo_size;",
            "uniform vec4 window_corner_radius;",
            "uniform mat3 window_input_to_geo;",
            "koma_erf",
            "koma_rounded_box_shadow",
            "float koma_rounding_alpha(",
        ] {
            assert!(
                src.contains(required),
                "shadow shader missing expected symbol `{required}`"
            );
        }
    }

    #[test]
    fn agent_cursor_and_trail_shaders_obey_gles_es1_constraints() {
        let cursor_src = agent_cursor_fragment_source();
        assert!(!cursor_src.contains("#version"));
        for req in [
            "varying vec2 v_coords;",
            "uniform float alpha;",
            "uniform vec2 elem_size;",
            "uniform vec2 tip_local;",
            "uniform vec4 ripple0;",
            "uniform vec4 ripple1;",
        ] {
            assert!(
                cursor_src.contains(req),
                "missing cursor shader decl `{req}`"
            );
        }

        let trail_src = agent_trail_fragment_source();
        assert!(!trail_src.contains("#version"));
        assert!(
            !trail_src.contains('['),
            "GLES 1.00 trail shader must avoid array indexing completely"
        );
        for req in [
            "varying vec2 v_coords;",
            "uniform float alpha;",
            "uniform float point_count;",
            "uniform vec4 trail_p0;",
            "uniform vec4 trail_p7;",
        ] {
            assert!(trail_src.contains(req), "missing trail shader decl `{req}`");
        }
    }

    #[test]
    fn rounded_border_and_blur_finish_shaders_expose_strip_and_pretone_uniforms() {
        let border_src = rounded_border_fragment_source();
        for req in [
            "uniform mat3 input_to_geo;",
            "vec2 geo_uv = (input_to_geo * vec3(v_coords, 1.0)).xy;",
            "float koma_rounding_alpha(",
        ] {
            assert!(
                border_src.contains(req),
                "border shader missing expected symbol `{req}`"
            );
        }

        let blur_src = blur_finish_rounded_fragment_source();
        for req in [
            "uniform float blur_pre_toned;",
            "if (blur_pre_toned < 0.5)",
            "float koma_rounding_alpha(",
        ] {
            assert!(
                blur_src.contains(req),
                "blur finish shader missing expected symbol `{req}`"
            );
        }
    }
}
