//! Multi-pass `4-Tap Dual Kawase Blur` framebuffer pyramid & dirty-tracked `xray` backdrop cache.
//!
//! Combines:
//! - `Hyprland/src/render/shaders/glsl/{blur1.glsl, blur2.glsl}` (5-tap Kawase downsample with
//!   perceived-luminance HSL `vibrancy` boost + 8-tap tent Kawase upsample), and
//! - `niri/src/render_helpers/{blur.rs, effect_buffer.rs}` (`Offscreen::<GlesTexture>::create_buffer`
//!   + `OutputDamageTracker` dirty-only re-blurring so static wallpapers consume 0% GPU across
//!   240 Hz window spring animations!).

use std::{
    cmp::max,
    iter::{once, zip},
    rc::Rc,
};

use anyhow::{Context as _, ensure};
use koma_model::BlurConfig;
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind as _, Color32F, ContextId, Offscreen as _, Renderer as _, Texture as _,
            damage::OutputDamageTracker,
            gles::{GlesError, GlesRenderer, GlesTexture, ffi, link_program},
        },
    },
    gpu_span_location,
    utils::{Buffer, Physical, Scale, Size, Transform},
};
use tracing::{debug, trace};

use crate::elements::KomaRenderElement;

const KAWASE_BLUR_VERT: &str = r#"#version 100
attribute vec2 vert;
varying vec2 v_coords;

void main() {
    v_coords = vert;
    vec2 position = vert * 2.0 - 1.0;
    gl_Position = vec4(position, 1.0, 1.0);
}
"#;

/// 5-tap Dual Kawase downsample pass + Hyprland HSL perceived-brightness `vibrancy` boost
/// (`Hyprland/src/render/shaders/glsl/blur1.glsl` + `niri/src/render_helpers/shaders/blur_down.frag`).
const KAWASE_DOWN_FRAG: &str = r#"#version 100
precision highp float;

varying vec2 v_coords;
uniform sampler2D tex;
uniform vec2 half_pixel;
uniform float offset;
uniform float vibrancy;

vec3 rgb2hsl(vec3 col) {
    float minc = min(col.r, min(col.g, col.b));
    float maxc = max(col.r, max(col.g, col.b));
    float delta = maxc - minc;
    float lum = (minc + maxc) * 0.5;
    float sat = 0.0;
    float hue = 0.0;
    if (lum > 0.0 && lum < 1.0) {
        float mul = (lum < 0.5) ? lum : (1.0 - lum);
        sat = delta / (mul * 2.0);
    }
    if (delta > 0.0) {
        if (maxc == col.r) {
            hue = (col.g - col.b) / delta + (col.g < col.b ? 6.0 : 0.0);
        } else if (maxc == col.g) {
            hue = (col.b - col.r) / delta + 2.0;
        } else {
            hue = (col.r - col.g) / delta + 4.0;
        }
        hue /= 6.0;
    }
    return vec3(hue, sat, lum);
}

float hue2rgb(float p, float q, float t) {
    if (t < 0.0) t += 1.0;
    if (t > 1.0) t -= 1.0;
    if (t < 1.0 / 6.0) return p + (q - p) * 6.0 * t;
    if (t < 1.0 / 2.0) return q;
    if (t < 2.0 / 3.0) return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
    return p;
}

vec3 hsl2rgb(vec3 hsl) {
    if (hsl.y == 0.0) {
        return vec3(hsl.z);
    }
    float q = hsl.z < 0.5 ? hsl.z * (1.0 + hsl.y) : hsl.z + hsl.y - hsl.z * hsl.y;
    float p = 2.0 * hsl.z - q;
    return vec3(
        hue2rgb(p, q, hsl.x + 1.0 / 3.0),
        hue2rgb(p, q, hsl.x),
        hue2rgb(p, q, hsl.x - 1.0 / 3.0)
    );
}

void main() {
    vec2 o = half_pixel * offset;

    vec4 sum = clamp(texture2D(tex, v_coords), 0.0, 1.0) * 4.0;
    sum += clamp(texture2D(tex, v_coords + vec2(-o.x, -o.y)), 0.0, 1.0);
    sum += clamp(texture2D(tex, v_coords + vec2( o.x, -o.y)), 0.0, 1.0);
    sum += clamp(texture2D(tex, v_coords + vec2(-o.x,  o.y)), 0.0, 1.0);
    sum += clamp(texture2D(tex, v_coords + vec2( o.x,  o.y)), 0.0, 1.0);

    vec4 color = sum / 8.0;
    if (vibrancy > 0.001 && color.a > 0.001) {
        vec3 unpremul = clamp(color.rgb / color.a, 0.0, 1.0);
        vec3 hsl = rgb2hsl(unpremul);
        hsl.y = clamp(hsl.y + vibrancy * (1.0 - abs(2.0 * hsl.z - 1.0)), 0.0, 1.0);
        color.rgb = hsl2rgb(hsl) * color.a;
    }
    gl_FragColor = color;
}
"#;

/// 8-tap Dual Kawase tent upsample pass (`Hyprland/src/render/shaders/glsl/blur2.glsl`).
const KAWASE_UP_FRAG: &str = r#"#version 100
precision highp float;

varying vec2 v_coords;
uniform sampler2D tex;
uniform vec2 half_pixel;
uniform float offset;

void main() {
    vec2 o = half_pixel * offset;
    vec4 sum = vec4(0.0);

    sum += texture2D(tex, v_coords + vec2(-o.x * 2.0, 0.0));
    sum += texture2D(tex, v_coords + vec2( o.x * 2.0, 0.0));
    sum += texture2D(tex, v_coords + vec2(0.0, -o.y * 2.0));
    sum += texture2D(tex, v_coords + vec2(0.0,  o.y * 2.0));

    sum += texture2D(tex, v_coords + vec2(-o.x,  o.y)) * 2.0;
    sum += texture2D(tex, v_coords + vec2( o.x,  o.y)) * 2.0;
    sum += texture2D(tex, v_coords + vec2(-o.x, -o.y)) * 2.0;
    sum += texture2D(tex, v_coords + vec2( o.x, -o.y)) * 2.0;

    gl_FragColor = sum / 12.0;
}
"#;

#[derive(Debug)]
struct KawasePassLocations {
    program: ffi::types::GLuint,
    uniform_tex: ffi::types::GLint,
    uniform_half_pixel: ffi::types::GLint,
    uniform_offset: ffi::types::GLint,
    uniform_vibrancy: ffi::types::GLint,
    attrib_vert: ffi::types::GLint,
}

#[derive(Debug)]
struct DualKawaseProgramInner {
    down: KawasePassLocations,
    up: KawasePassLocations,
}

#[derive(Debug, Clone)]
pub struct DualKawaseProgram(Rc<DualKawaseProgramInner>);

unsafe fn compile_kawase_pass(
    gl: &ffi::Gles2,
    frag_src: &str,
) -> Result<KawasePassLocations, GlesError> {
    let program = unsafe { link_program(gl, KAWASE_BLUR_VERT, frag_src)? };
    let vert = c"vert";
    let tex = c"tex";
    let half_pixel = c"half_pixel";
    let offset = c"offset";
    let vibrancy = c"vibrancy";

    Ok(KawasePassLocations {
        program,
        uniform_tex: unsafe { gl.GetUniformLocation(program, tex.as_ptr()) },
        uniform_half_pixel: unsafe { gl.GetUniformLocation(program, half_pixel.as_ptr()) },
        uniform_offset: unsafe { gl.GetUniformLocation(program, offset.as_ptr()) },
        uniform_vibrancy: unsafe { gl.GetUniformLocation(program, vibrancy.as_ptr()) },
        attrib_vert: unsafe { gl.GetAttribLocation(program, vert.as_ptr()) },
    })
}

impl DualKawaseProgram {
    pub fn compile(renderer: &mut GlesRenderer) -> anyhow::Result<Self> {
        renderer
            .with_context(|gl| unsafe {
                let down =
                    compile_kawase_pass(gl, KAWASE_DOWN_FRAG).context("Kawase down shader")?;
                let up = compile_kawase_pass(gl, KAWASE_UP_FRAG).context("Kawase up shader")?;
                Ok(Self(Rc::new(DualKawaseProgramInner { down, up })))
            })
            .context("failed binding GLES context for DualKawaseProgram")?
    }
}

/// Multi-pass FBO mip-chain executing `passes` downsample draws + `passes` upsample draws.
#[derive(Debug)]
pub struct DualKawaseBlurEngine {
    program: DualKawaseProgram,
    context_id: ContextId<GlesTexture>,
    mip_textures: Vec<GlesTexture>,
}

impl DualKawaseBlurEngine {
    pub fn new(renderer: &mut GlesRenderer) -> anyhow::Result<Self> {
        let program = DualKawaseProgram::compile(renderer)?;
        Ok(Self {
            program,
            context_id: renderer.context_id(),
            mip_textures: Vec::new(),
        })
    }

    pub fn ensure_mip_chain(
        &mut self,
        renderer: &mut GlesRenderer,
        source_size: Size<i32, Buffer>,
        passes: u8,
    ) -> anyhow::Result<()> {
        let depth = passes.clamp(1, 6) as usize;
        let recreate = self
            .mip_textures
            .first()
            .is_none_or(|t| t.size() != source_size);
        if recreate {
            self.mip_textures.clear();
        }

        let mut w = source_size.w.max(1);
        let mut h = source_size.h.max(1);
        for level in 0..=depth {
            let level_sz = Size::from((w, h));
            w = max(1, w / 2);
            h = max(1, h / 2);
            if self.mip_textures.len() > level {
                continue;
            }
            let tex: GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, level_sz)
                .context("failed allocating Dual Kawase mip GlesTexture")?;
            self.mip_textures.push(tex);
        }
        self.mip_textures.truncate(depth + 1);
        Ok(())
    }

    pub fn execute_blur(
        &mut self,
        renderer: &mut GlesRenderer,
        source: &GlesTexture,
        cfg: BlurConfig,
    ) -> anyhow::Result<GlesTexture> {
        ensure!(
            renderer.context_id() == self.context_id,
            "DualKawaseBlurEngine context mismatch"
        );
        let passes = cfg.passes.clamp(1, 6);
        self.ensure_mip_chain(renderer, source.size(), passes)?;

        let offset = (cfg.size.clamp(1, 24) as f32) * 0.5;
        let per_pass_vibrancy = cfg.vibrancy.clamp(0.0, 1.0) / f32::from(passes);

        renderer
            .with_profiled_context(gpu_span_location!("DualKawaseBlur::execute"), |gl| unsafe {
                while gl.GetError() != ffi::NO_ERROR {}

                gl.Disable(ffi::BLEND);
                gl.Disable(ffi::SCISSOR_TEST);
                gl.ActiveTexture(ffi::TEXTURE0);

                let mut fbo: ffi::types::GLuint = 0;
                gl.GenFramebuffers(1, &mut fbo);
                gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);

                let quad_verts: [f32; 12] =
                    [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0];

                // 1. Downsample cascade (level 0 -> 1 -> ... -> passes)
                let down = &self.program.0.down;
                gl.UseProgram(down.program);
                gl.Uniform1i(down.uniform_tex, 0);
                gl.Uniform1f(down.uniform_offset, offset);
                if down.uniform_vibrancy >= 0 {
                    gl.Uniform1f(down.uniform_vibrancy, per_pass_vibrancy);
                }
                gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
                gl.EnableVertexAttribArray(down.attrib_vert as u32);
                gl.VertexAttribPointer(
                    down.attrib_vert as u32,
                    2,
                    ffi::FLOAT,
                    ffi::FALSE,
                    0,
                    quad_verts.as_ptr().cast(),
                );

                for (src_tex, dst_tex) in zip(
                    once(source).chain(&self.mip_textures[1..]),
                    &self.mip_textures[1..],
                ) {
                    let dst_sz = dst_tex.size();
                    gl.Viewport(0, 0, dst_sz.w, dst_sz.h);
                    gl.Uniform2f(
                        down.uniform_half_pixel,
                        0.5 / (dst_sz.w.max(1) as f32),
                        0.5 / (dst_sz.h.max(1) as f32),
                    );
                    gl.FramebufferTexture2D(
                        ffi::FRAMEBUFFER,
                        ffi::COLOR_ATTACHMENT0,
                        ffi::TEXTURE_2D,
                        dst_tex.tex_id(),
                        0,
                    );
                    gl.BindTexture(ffi::TEXTURE_2D, src_tex.tex_id());
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_WRAP_S,
                        ffi::CLAMP_TO_EDGE as i32,
                    );
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_WRAP_T,
                        ffi::CLAMP_TO_EDGE as i32,
                    );
                    gl.DrawArrays(ffi::TRIANGLES, 0, 6);
                }
                gl.DisableVertexAttribArray(down.attrib_vert as u32);

                // 2. Upsample cascade (passes -> ... -> 1 -> 0)
                let up = &self.program.0.up;
                gl.UseProgram(up.program);
                gl.Uniform1i(up.uniform_tex, 0);
                gl.Uniform1f(up.uniform_offset, offset);
                gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
                gl.EnableVertexAttribArray(up.attrib_vert as u32);
                gl.VertexAttribPointer(
                    up.attrib_vert as u32,
                    2,
                    ffi::FLOAT,
                    ffi::FALSE,
                    0,
                    quad_verts.as_ptr().cast(),
                );

                for (src_tex, dst_tex) in zip(
                    self.mip_textures.iter().rev(),
                    self.mip_textures.iter().rev().skip(1),
                ) {
                    let dst_sz = dst_tex.size();
                    let src_sz = src_tex.size();
                    gl.Viewport(0, 0, dst_sz.w, dst_sz.h);
                    gl.Uniform2f(
                        up.uniform_half_pixel,
                        0.5 / (src_sz.w.max(1) as f32),
                        0.5 / (src_sz.h.max(1) as f32),
                    );
                    gl.FramebufferTexture2D(
                        ffi::FRAMEBUFFER,
                        ffi::COLOR_ATTACHMENT0,
                        ffi::TEXTURE_2D,
                        dst_tex.tex_id(),
                        0,
                    );
                    gl.BindTexture(ffi::TEXTURE_2D, src_tex.tex_id());
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_WRAP_S,
                        ffi::CLAMP_TO_EDGE as i32,
                    );
                    gl.TexParameteri(
                        ffi::TEXTURE_2D,
                        ffi::TEXTURE_WRAP_T,
                        ffi::CLAMP_TO_EDGE as i32,
                    );
                    gl.DrawArrays(ffi::TRIANGLES, 0, 6);
                }
                gl.DisableVertexAttribArray(up.attrib_vert as u32);

                gl.BindTexture(ffi::TEXTURE_2D, 0);
                gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
                gl.DeleteFramebuffers(1, &fbo);
            })
            .context("GLES context error during Dual Kawase blur")?;

        Ok(self.mip_textures[0].clone())
    }
}

/// Output-level cached `xray` backdrop blur buffer (`niri/src/render_helpers/effect_buffer.rs`):
/// renders wallpaper / `Bottom`+`Background` layer surfaces into an offscreen `GlesTexture` and
/// re-executes the `DualKawaseBlurEngine` **only** when background damage or `BlurConfig` changes!
#[derive(Debug, Default)]
pub struct CachedBackdropBlur {
    source_tex: Option<GlesTexture>,
    damage_tracker: Option<OutputDamageTracker>,
    engine: Option<DualKawaseBlurEngine>,
    cached_blurred: Option<GlesTexture>,
    last_size: Size<i32, Buffer>,
    last_scale: f64,
    last_cfg: Option<BlurConfig>,
}

impl CachedBackdropBlur {
    /// Updates the offscreen backdrop from `backdrop_elements` (e.g. `swww` wallpaper / bottom layers)
    /// and returns a ready-to-sample blurred `GlesTexture` when `cfg.enabled` is true.
    pub fn update_and_blur(
        &mut self,
        renderer: &mut GlesRenderer,
        phys_size: Size<i32, Physical>,
        scale: Scale<f64>,
        clear_color: Color32F,
        backdrop_elements: &[KomaRenderElement<GlesRenderer>],
        cfg: BlurConfig,
    ) -> Option<GlesTexture> {
        if !cfg.enabled || phys_size.w <= 0 || phys_size.h <= 0 {
            return None;
        }

        let buf_size = Size::<i32, Buffer>::from((phys_size.w.max(16), phys_size.h.max(16)));
        let need_reinit = self.source_tex.is_none()
            || self.last_size != buf_size
            || (self.last_scale - scale.x).abs() > 1e-4;

        if need_reinit {
            debug!(
                ?buf_size,
                scale = scale.x,
                "allocating CachedBackdropBlur offscreen FBO"
            );
            let tex = renderer.create_buffer(Fourcc::Abgr8888, buf_size).ok()?;
            let tracker = OutputDamageTracker::new(phys_size, scale, Transform::Normal);
            self.source_tex = Some(tex);
            self.damage_tracker = Some(tracker);
            self.cached_blurred = None;
            self.last_size = buf_size;
            self.last_scale = scale.x;
        }

        if self.last_cfg != Some(cfg) {
            self.cached_blurred = None;
            self.last_cfg = Some(cfg);
        }

        let src_tex = self.source_tex.as_mut()?;
        let tracker = self.damage_tracker.as_mut()?;

        let damaged = {
            let mut target = renderer.bind(src_tex).ok()?;
            let res = tracker
                .render_output(renderer, &mut target, 1, backdrop_elements, clear_color)
                .ok()?;
            res.damage.is_some_and(|d| !d.is_empty())
        };

        if damaged || self.cached_blurred.is_none() {
            trace!("backdrop damaged or invalidated; running Dual Kawase blur pyramid");
            if self.engine.is_none() {
                self.engine = DualKawaseBlurEngine::new(renderer).ok();
            }
            let engine = self.engine.as_mut()?;
            let src_ref = self.source_tex.as_ref()?;
            self.cached_blurred = engine.execute_blur(renderer, src_ref, cfg).ok();
        }

        self.cached_blurred.clone()
    }
}
