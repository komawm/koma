use serde::{Deserialize, Serialize};

use crate::{
    frame::AncillaryFdRef,
    ids::{Epoch, MacroId, OutputId, WindowId, WorkspaceId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceModeSpec {
    Headed,
    Headless,
    Peek,
    Takeover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayKind {
    None,
    Som,
    Grid225,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Webp,
    Png,
    RawRgba8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    ShmFd,
    InlineBase64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureFrameParams {
    pub ws_id: WorkspaceId,
    pub win_id: Option<WindowId>,
    pub overlay_kind: OverlayKind,
    pub max_edge: u32,
    pub format: ImageFormat,
    pub transport: TransportKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureFrameResult {
    pub epoch: Epoch,
    pub width: u32,
    pub height: u32,
    pub byte_len: usize,
    pub fd_index: Option<AncillaryFdRef>,
    pub has_protected_blackout: bool,
    pub grid_cells: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerDeviceKind {
    Pointer,
    Relative,
    Tablet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KinematicProfileKind {
    Instant,
    MinimumJerk,
    Spring,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct KinematicsParams {
    pub profile: KinematicProfileKind,
    pub arc_curvature: f64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextInjectStrategy {
    Auto,
    TextInputV3,
    Clipboard,
    XkbKeymap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LuaExecTxParams {
    pub ws_id: WorkspaceId,
    pub expected_epoch: Epoch,
    pub macro_id: MacroId,
    #[serde(default)]
    pub args: serde_json::Value,
    pub fuel_budget: u64,
    pub memory_limit_bytes: usize,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSetModeParams {
    pub ws_id: WorkspaceId,
    pub mode: WorkspaceModeSpec,
    pub output: Option<OutputId>,
}
