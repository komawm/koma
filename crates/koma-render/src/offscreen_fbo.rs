//! Lazy offscreen FBO allocation, `ExportMem` pixel readout, and `DMA-BUF` plane duplication
//! (adapted from `niri/src/render_helpers/{offscreen,mod}.rs`).

use std::{io, os::fd::OwnedFd};

use anyhow::Context as _;
use smithay::{
    backend::{
        allocator::{Buffer as _, Fourcc, dmabuf::Dmabuf},
        renderer::{
            Bind as _, ExportMem as _, Offscreen as _,
            damage::OutputDamageTracker,
            element::memory::MemoryRenderBuffer,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    utils::{Buffer as BufferCoords, Logical, Physical, Rectangle, Scale, Size, Transform},
};

use crate::{
    DmabufPlaneDescriptor, elements::KomaRenderElement, frame_composer::DEFAULT_BACKDROP_CLEAR,
};

/// Clear color for window snapshots: fully transparent so the captured window keeps its own
/// rounded-corner alpha instead of baking a workspace backdrop into the frame.
const SNAPSHOT_CLEAR: smithay::backend::renderer::Color32F =
    smithay::backend::renderer::Color32F::new(0.0, 0.0, 0.0, 0.0);

/// Lazily backed OpenGL ES texture & damage tracker for a `Headless` workspace.
/// Dropped cleanly when `AdaptiveFrameClock` enters `FrameClockTier::FrozenZeroHz`.
pub struct LazyOffscreenSurface {
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    texture: Option<GlesTexture>,
    damage_tracker: OutputDamageTracker,
}

impl LazyOffscreenSurface {
    #[must_use]
    pub fn new(width: i32, height: i32, scale: f64) -> Self {
        let size = Size::from((width.max(1), height.max(1)));
        let damage_tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
        Self {
            size,
            scale: Scale::from(scale),
            texture: None,
            damage_tracker,
        }
    }

    /// Renders `elements` on demand (`Render-on-Read`), allocating the backing `GlesTexture`
    /// only on the first capture or after a `FrozenZeroHz` reclamation.
    #[must_use]
    pub const fn size(&self) -> Size<i32, Physical> {
        self.size
    }

    pub fn render_on_read(
        &mut self,
        renderer: &mut GlesRenderer,
        elements: &[KomaRenderElement<GlesRenderer>],
    ) -> anyhow::Result<&GlesTexture> {
        if self.texture.is_none() {
            let buf_size: Size<i32, BufferCoords> = Size::from((self.size.w, self.size.h));
            let tex: GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, buf_size)
                .context("failed to allocate offscreen LazyFbo GlesTexture")?;
            self.texture = Some(tex);
        }

        let tex = self.texture.as_mut().expect("allocated above");
        {
            let mut fb = renderer
                .bind(tex)
                .context("failed to bind offscreen LazyFbo GlesTexture")?;
            self.damage_tracker
                .render_output(renderer, &mut fb, 0, elements, DEFAULT_BACKDROP_CLEAR)
                .context("failed to composite workspace into LazyFbo")?;
        }
        Ok(self.texture.as_ref().expect("allocated above"))
    }

    /// Composites `elements` into the offscreen FBO and downloads raw RGBA8 pixels via Smithay's
    /// `ExportMem` (`copy_framebuffer` + `map_texture`, matching `niri/src/render_helpers/mod.rs`),
    /// ready to seal into a read-only `memfd` for `SCM_RIGHTS` zero-copy IPC transfer.
    pub fn capture_rgba8_pixels(
        &mut self,
        renderer: &mut GlesRenderer,
        elements: &[KomaRenderElement<GlesRenderer>],
    ) -> anyhow::Result<(u32, u32, Vec<u8>)> {
        let buf_size: Size<i32, BufferCoords> = Size::from((self.size.w, self.size.h));
        if self.texture.is_none() {
            let tex: GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, buf_size)
                .context("failed to allocate capture LazyFbo GlesTexture")?;
            self.texture = Some(tex);
        }

        let tex = self.texture.as_mut().expect("allocated above");
        let mapping = {
            let mut fb = renderer
                .bind(tex)
                .context("failed to bind capture LazyFbo GlesTexture")?;
            self.damage_tracker
                .render_output(renderer, &mut fb, 0, elements, DEFAULT_BACKDROP_CLEAR)
                .context("failed rendering capture frame")?;
            let copy_rect = Rectangle::from_size(buf_size);
            renderer
                .copy_framebuffer(&fb, copy_rect, Fourcc::Abgr8888)
                .context("GlesRenderer::copy_framebuffer failed")?
        };

        let slice = renderer
            .map_texture(&mapping)
            .context("GlesRenderer::map_texture failed")?;
        Ok((self.size.w as u32, self.size.h as u32, slice.to_vec()))
    }

    /// Releases GPU VRAM immediately when `FrameClockTier::FrozenZeroHz` is entered.
    pub fn evict_vram(&mut self) {
        self.texture = None;
    }

    #[must_use]
    pub const fn scale(&self) -> Scale<f64> {
        self.scale
    }
}

/// Renders `elements` into a one-shot offscreen `GlesTexture` with a **transparent** clear and
/// copies the result into a CPU-side [`MemoryRenderBuffer`], returning the buffer together with
/// its pixel dimensions.
///
/// # Errors
///
/// Propagates `GlesRenderer` allocation, bind, render and read-back failures.
pub fn capture_elements_to_memory(
    renderer: &mut GlesRenderer,
    size: Size<i32, Logical>,
    output_scale: Scale<f64>,
    elements: &[KomaRenderElement<GlesRenderer>],
) -> anyhow::Result<(MemoryRenderBuffer, (i32, i32))> {
    let mut cache = WindowFrameCache::default();
    cache.refresh(renderer, size, output_scale, elements)?;
    cache
        .read_back(renderer)?
        .context("window snapshot produced no pixels")
}

/// Latest committed pixels of one toplevel, kept in GPU memory so a close animation can fade them
/// out after the client released its buffers.
///
/// Refreshed in the `wl_surface` commit handler (the last moment the client's buffer is readable)
/// and read back exactly once when the window actually closes.
#[derive(Debug)]
pub struct WindowFrameCache {
    texture: Option<GlesTexture>,
    buf_size: Size<i32, BufferCoords>,
    output_scale: Scale<f64>,
}

impl Default for WindowFrameCache {
    fn default() -> Self {
        Self {
            texture: None,
            buf_size: Size::from((0, 0)),
            output_scale: Scale::from(1.0),
        }
    }
}

impl WindowFrameCache {
    /// Returns `true` when no frame has been captured yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.texture.is_none()
    }

    /// Re-renders `elements` into the cached texture, reallocating it when the window resized.
    ///
    /// # Errors
    ///
    /// Propagates `GlesRenderer` allocation, bind and render failures.
    pub fn refresh(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Logical>,
        output_scale: Scale<f64>,
        elements: &[KomaRenderElement<GlesRenderer>],
    ) -> anyhow::Result<()> {
        let phys_w = (f64::from(size.w.max(1)) * output_scale.x).round().max(1.0) as i32;
        let phys_h = (f64::from(size.h.max(1)) * output_scale.y).round().max(1.0) as i32;
        let buf_size: Size<i32, BufferCoords> = Size::from((phys_w, phys_h));
        let needs_alloc = self.texture.is_none()
            || self.buf_size != buf_size
            || (self.output_scale.x - output_scale.x).abs() > f64::EPSILON;
        if needs_alloc {
            self.texture = Some(
                renderer
                    .create_buffer(Fourcc::Abgr8888, buf_size)
                    .context("failed to allocate window frame cache GlesTexture")?,
            );
            self.buf_size = buf_size;
            self.output_scale = output_scale;
        }
        let phys_size: Size<i32, Physical> = Size::from((phys_w, phys_h));
        let mut damage_tracker =
            OutputDamageTracker::new(phys_size, output_scale, Transform::Normal);
        let tex = self.texture.as_mut().expect("allocated above");
        let mut fb = renderer
            .bind(tex)
            .context("failed to bind window frame cache GlesTexture")?;
        damage_tracker
            .render_output(renderer, &mut fb, 0, elements, SNAPSHOT_CLEAR)
            .context("failed to composite window frame cache")?;
        Ok(())
    }

    /// Copies the cached texture into CPU pixels as a [`MemoryRenderBuffer`].
    ///
    /// Returns `Ok(None)` when nothing was captured, so callers fall back to removing the window
    /// without an animation.
    ///
    /// # Errors
    ///
    /// Propagates `GlesRenderer` bind, read-back and mapping failures.
    pub fn read_back(
        &mut self,
        renderer: &mut GlesRenderer,
    ) -> anyhow::Result<Option<(MemoryRenderBuffer, (i32, i32))>> {
        let Some(tex) = self.texture.as_mut() else {
            return Ok(None);
        };
        let buf_size = self.buf_size;
        let mapping = {
            let fb = renderer
                .bind(tex)
                .context("failed to bind window frame cache for read-back")?;
            renderer
                .copy_framebuffer(&fb, Rectangle::from_size(buf_size), Fourcc::Abgr8888)
                .context("GlesRenderer::copy_framebuffer failed for window frame cache")?
        };
        let slice = renderer
            .map_texture(&mapping)
            .context("GlesRenderer::map_texture failed for window frame cache")?;
        let buffer = MemoryRenderBuffer::from_slice(
            slice,
            Fourcc::Abgr8888,
            buf_size,
            1,
            Transform::Normal,
            None,
        );
        Ok(Some((buffer, (buf_size.w, buf_size.h))))
    }

    /// Releases the cached VRAM (the window closed or its animation settled).
    pub fn clear(&mut self) {
        self.texture = None;
        self.buf_size = Size::from((0, 0));
    }
}

/// Duplicates multi-planar `Dmabuf` file descriptors (`FD_CLOEXEC`) and extracts plane metadata
/// for `capture.export_dmabuf` `SCM_RIGHTS` export (`IPC_SPEC.md` §5.4).
pub fn duplicate_dmabuf_planes(
    dmabuf: &Dmabuf,
) -> io::Result<(Vec<OwnedFd>, Vec<DmabufPlaneDescriptor>)> {
    let format = dmabuf.format();
    let size = dmabuf.size();
    let fourcc = format.code as u32;
    let modifier = u64::from(format.modifier);

    let mut fds = Vec::new();
    let mut planes = Vec::new();
    for ((handle, stride), offset) in dmabuf.handles().zip(dmabuf.strides()).zip(dmabuf.offsets()) {
        let dup_fd = handle.try_clone_to_owned()?;
        fds.push(dup_fd);
        planes.push(DmabufPlaneDescriptor {
            fourcc,
            modifier,
            width: size.w as u32,
            height: size.h as u32,
            stride,
            offset,
        });
    }
    Ok((fds, planes))
}
