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
    #[serde(rename = "grid_225", alias = "grid225")]
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
    #[serde(alias = "hooke", alias = "hooke_spring")]
    Spring,
    Bezier,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KinematicsParams {
    pub profile: KinematicProfileKind,
    #[serde(default)]
    pub arc_curvature: f64,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curve: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mass: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub damping: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub damping_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stiffness: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epsilon: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bezier: Option<[f64; 4]>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSplineKind {
    #[default]
    Auto,
    CatmullRom,
    Bezier,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointerPathParams {
    pub ws_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_epoch: Option<Epoch>,
    pub waypoints: Vec<serde_json::Value>,
    #[serde(default)]
    pub spline: PathSplineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    #[serde(default, alias = "hold_before_move_ms")]
    pub hold_start_ms: u64,
    #[serde(default, alias = "hold_before_release_ms")]
    pub hold_end_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinematics: Option<KinematicsParams>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorSettleParams {
    pub ws_id: WorkspaceId,
    #[serde(default)]
    pub immediate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSetModeParams {
    pub ws_id: WorkspaceId,
    pub mode: WorkspaceModeSpec,
    pub output: Option<OutputId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_kind_grid225_uses_spec_wire_name_with_legacy_alias() {
        assert_eq!(
            serde_json::to_string(&OverlayKind::Grid225).unwrap(),
            "\"grid_225\""
        );
        assert_eq!(
            serde_json::from_str::<OverlayKind>("\"grid_225\"").unwrap(),
            OverlayKind::Grid225
        );
        assert_eq!(
            serde_json::from_str::<OverlayKind>("\"grid225\"").unwrap(),
            OverlayKind::Grid225
        );
        assert_eq!(
            serde_json::to_string(&ImageFormat::RawRgba8).unwrap(),
            "\"raw_rgba8\""
        );
        assert_eq!(
            serde_json::to_string(&TransportKind::ShmFd).unwrap(),
            "\"shm_fd\""
        );
    }

    #[test]
    fn kinematic_profile_supports_hooke_aliases_and_bezier() {
        assert_eq!(
            serde_json::to_string(&KinematicProfileKind::Spring).unwrap(),
            "\"spring\""
        );
        for wire in ["\"spring\"", "\"hooke\"", "\"hooke_spring\""] {
            assert_eq!(
                serde_json::from_str::<KinematicProfileKind>(wire).unwrap(),
                KinematicProfileKind::Spring
            );
        }
        assert_eq!(
            serde_json::to_string(&KinematicProfileKind::Bezier).unwrap(),
            "\"bezier\""
        );
        assert_eq!(
            serde_json::from_str::<KinematicProfileKind>("\"bezier\"").unwrap(),
            KinematicProfileKind::Bezier
        );
    }

    #[test]
    fn pointer_path_and_cursor_settle_params_roundtrip() {
        let path_json = serde_json::json!({
            "ws_id": "ws-1",
            "expected_epoch": 7,
            "waypoints": [{"grid_225": "B2"}, {"x": 500.0, "y": 400.0}],
            "spline": "catmull_rom",
            "button": "left",
            "hold_before_move_ms": 40,
            "hold_end_ms": 25,
            "kinematics": { "profile": "spring", "duration_ms": 180 }
        });
        let parsed: PointerPathParams = serde_json::from_value(path_json).unwrap();
        assert_eq!(parsed.spline, PathSplineKind::CatmullRom);
        assert_eq!(parsed.button.as_deref(), Some("left"));
        assert_eq!(parsed.hold_start_ms, 40);
        assert_eq!(parsed.hold_end_ms, 25);
        assert_eq!(
            parsed.kinematics.as_ref().map(|k| k.profile),
            Some(KinematicProfileKind::Spring)
        );
        let serialized = serde_json::to_string(&parsed).unwrap();
        let back: PointerPathParams = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, back);

        let settle = CursorSettleParams {
            ws_id: WorkspaceId::new("ws-1"),
            immediate: true,
        };
        let settle_s = serde_json::to_string(&settle).unwrap();
        let settle_back: CursorSettleParams = serde_json::from_str(&settle_s).unwrap();
        assert_eq!(settle, settle_back);
    }
}
