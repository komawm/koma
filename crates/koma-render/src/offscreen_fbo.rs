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
            gles::{GlesRenderer, GlesTexture},
        },
    },
    utils::{Buffer as BufferCoords, Physical, Rectangle, Scale, Size, Transform},
};

use crate::{
    DmabufPlaneDescriptor, elements::KomaRenderElement, frame_composer::DEFAULT_BACKDROP_CLEAR,
};

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
