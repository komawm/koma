//! Compositor rendering, first-frame scene assembly & `Render-on-Read` offscreen FBO pipeline (`Tier 2`).

use std::collections::BTreeMap;

use koma_model::LogicalRect;
use koma_proto::{OverlayKind, WorkspaceId};

#[cfg(feature = "smithay-renderer")]
pub mod blur_pipeline;
#[cfg(feature = "smithay-renderer")]
pub mod elements;
#[cfg(feature = "smithay-renderer")]
pub mod frame_composer;
#[cfg(feature = "smithay-renderer")]
pub mod offscreen_fbo;
#[cfg(feature = "smithay-renderer")]
pub mod shaders;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderTargetKind {
    /// Hardware KMS or nested Winit window scanout (Protected Surfaces render normally for human).
    PhysicalOutput,
    /// Agent `capture.frame` or `ext-image-copy-capture-v1` (Protected Surfaces blacked out to `#000000`).
    AgentCapture,
    /// PipeWire screencast or `koma-overview` thumbnail (`384x216`).
    ScreencastPreview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmabufPlaneDescriptor {
    pub fourcc: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
}

#[derive(Debug, Clone)]
pub struct CaptureRenderRequest {
    pub ws_id: WorkspaceId,
    pub target_rect: LogicalRect,
    pub protected_blackout_rects: Vec<LogicalRect>,
    pub overlay: OverlayKind,
}

/// Hardware renderer abstraction implemented by `Smithay` `GlesRenderer` / Vulkan in bare-metal builds.
pub trait CompositorRenderer {
    fn render_on_read(
        &mut self,
        req: &CaptureRenderRequest,
    ) -> Result<DmabufPlaneDescriptor, String>;
    fn release_offscreen_fbo(&mut self, fbo_slot: u32);
}

/// Manages pooled offscreen FBO slots for `Headless` workspaces and drops VRAM on `FrozenZeroHz`.
#[derive(Debug, Default)]
pub struct LazyFboPool {
    allocated: BTreeMap<WorkspaceId, u32>,
    next_slot: u32,
}

impl LazyFboPool {
    pub fn acquire_or_borrow(&mut self, ws_id: &WorkspaceId) -> u32 {
        if let Some(&slot) = self.allocated.get(ws_id) {
            return slot;
        }
        self.next_slot += 1;
        let slot = self.next_slot;
        self.allocated.insert(ws_id.clone(), slot);
        slot
    }

    pub fn release_on_freeze(
        &mut self,
        ws_id: &WorkspaceId,
        renderer: &mut dyn CompositorRenderer,
    ) {
        if let Some(slot) = self.allocated.remove(ws_id) {
            renderer.release_offscreen_fbo(slot);
        }
    }
}
