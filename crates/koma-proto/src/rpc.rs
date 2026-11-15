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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bow: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overshoot: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tremor: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorSettleParams {
    pub ws_id: WorkspaceId,
    #[serde(default)]
    pub immediate: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelativeMoveParams {
    pub ws_id: WorkspaceId,
    pub expected_epoch: Epoch,
    pub dx: f64,
    pub dy: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dx_unaccel: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dy_unaccel: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WheelScrollParams {
    pub ws_id: WorkspaceId,
    pub expected_epoch: Epoch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<serde_json::Value>,
    #[serde(default)]
    pub dx: f64,
    #[serde(default)]
    pub dy: f64,
    #[serde(default)]
    pub discrete_x: i32,
    #[serde(default)]
    pub discrete_y: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitTestParams {
    pub ws_id: WorkspaceId,
    pub target: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitTestResult {
    pub reaches_target: bool,
    pub target_win_id: Option<WindowId>,
    pub target_app_id: Option<String>,
    pub blocking_win_id: Option<WindowId>,
    pub blocking_app_id: Option<String>,
    pub blocking_title: Option<String>,
    pub blocking_rect: Option<[i32; 4]>,
    pub geometry_stable: bool,
    #[serde(default)]
    pub sampled_points: Vec<[f64; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_pos: Option<[f64; 2]>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursorLabelParams {
    pub ws_id: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caret_rect: Option<[f64; 4]>,
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

    #[test]
    fn vseat_extended_rpc_params_and_results_roundtrip() {
        let rel = RelativeMoveParams {
            ws_id: WorkspaceId::new("ws-1"),
            expected_epoch: Epoch(3),
            dx: 12.5,
            dy: -4.0,
            dx_unaccel: Some(12.5),
            dy_unaccel: None,
            modifiers: vec!["ctrl".to_owned(), "shift".to_owned()],
        };
        let rel_back: RelativeMoveParams =
            serde_json::from_str(&serde_json::to_string(&rel).unwrap()).unwrap();
        assert_eq!(rel, rel_back);

        let wheel = WheelScrollParams {
            ws_id: WorkspaceId::new("ws-1"),
            expected_epoch: Epoch(4),
            target: Some(serde_json::json!({ "win_id": 7 })),
            dx: 0.0,
            dy: 15.0,
            discrete_x: 0,
            discrete_y: 120,
            modifiers: vec!["ctrl".to_owned()],
        };
        let wheel_back: WheelScrollParams =
            serde_json::from_str(&serde_json::to_string(&wheel).unwrap()).unwrap();
        assert_eq!(wheel, wheel_back);

        let ht_params = HitTestParams {
            ws_id: WorkspaceId::new("ws-1"),
            target: serde_json::json!({ "grid_225": "H8" }),
        };
        let ht_params_back: HitTestParams =
            serde_json::from_str(&serde_json::to_string(&ht_params).unwrap()).unwrap();
        assert_eq!(ht_params, ht_params_back);

        let ht_result = HitTestResult {
            reaches_target: false,
            target_win_id: Some(WindowId {
                slot: 1,
                generation: 1,
            }),
            target_app_id: Some("foot".to_owned()),
            blocking_win_id: Some(WindowId {
                slot: 2,
                generation: 1,
            }),
            blocking_app_id: Some("dialog".to_owned()),
            blocking_title: Some("Confirm".to_owned()),
            blocking_rect: Some([100, 100, 400, 200]),
            geometry_stable: true,
            sampled_points: vec![[300.0, 200.0]],
            cursor_pos: Some([50.0, 60.0]),
        };
        let ht_result_back: HitTestResult =
            serde_json::from_str(&serde_json::to_string(&ht_result).unwrap()).unwrap();
        assert_eq!(ht_result, ht_result_back);

        let label = CursorLabelParams {
            ws_id: WorkspaceId::new("ws-1"),
            label: Some("CLICK SUBMIT".to_owned()),
            caret_rect: Some([10.0, 20.0, 80.0, 28.0]),
        };
        let label_back: CursorLabelParams =
            serde_json::from_str(&serde_json::to_string(&label).unwrap()).unwrap();
        assert_eq!(label, label_back);

        let path_json = serde_json::json!({
            "ws_id": "ws-1",
            "expected_epoch": 9,
            "waypoints": [{"x": 10.0, "y": 10.0}, {"x": 100.0, "y": 80.0}],
            "modifiers": ["shift"],
            "label": "DRAG",
            "bow": 12.0,
            "overshoot": 6.0,
            "tremor": 1.5
        });
        let parsed_path: PointerPathParams = serde_json::from_value(path_json).unwrap();
        assert_eq!(parsed_path.modifiers, vec!["shift"]);
        assert_eq!(parsed_path.label.as_deref(), Some("DRAG"));
        assert_eq!(parsed_path.bow, Some(12.0));
        assert_eq!(parsed_path.overshoot, Some(6.0));
        assert_eq!(parsed_path.tremor, Some(1.5));
    }
}
