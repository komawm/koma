#![forbid(unsafe_code)]

//! Pure-Rust compositor domain model (`Tier 0`).
//!
//! Combines Hyprland's `CSpace` dual-layer architecture (`DwindleTree` dynamic binary splits +
//! `FloatingStack` Z-order + `special:<name>` Scratchpad overlays), Niri's `ScrollingStrip`
//! horizontal tape (`preset_column_widths`, velocity-preserving `camera_spring`, and `30 px` drag
//! edge auto-scroll), and Koma's symmetrical `Headed`/`Headless` `Deck/Stash` Agent workspaces with
//! `Lease + Epoch` preemptive concurrency control and 4-tier adaptive frame clocks.

pub mod animation;
pub mod clock;
pub mod cursor_path;
pub mod dwindle;
pub mod geometry;
pub mod keybind_overlay;
pub mod kinematics;
pub mod layout;
pub mod lease;
pub mod output_layout;
pub mod rules;
pub mod scrolling;
pub mod workspace;

pub use crate::{
    animation::{
        AnimationChannelConfig, AnimationConfig, AnimationSolverKind, BezierTrack, BlurConfig,
        BorderConfig, CubicBezier, CurveSpec, DimConfig, FocusRingConfig, HookeSpringParams,
        HookeSpringTrack, MotionTrack, SampledWindowVisual, SampledWorkspaceTransform,
        ShadowConfig, SlideEdge, Spring, SpringParams, SpringTrack, WindowAnimationTable,
        WindowOpenStyle, WorkspaceTransitionTable, parse_hex_color, resolve_named_curve,
    },
    clock::{AdaptiveFrameClock, FrameClockConfig, FrameClockTier, FrameTickDecision},
    cursor_path::{
        AgentCursorConfig, AgentCursorFrameSnapshot, AgentCursorVisualDynamics, ArcLengthSpline,
        CURSOR_CLICK_PULSE_MS, CURSOR_CLICK_RIPPLE_MS, CURSOR_MAX_RIPPLES, CURSOR_SETTLE_FADE_MS,
        CURSOR_TRAIL_LIFETIME_MS, CURSOR_TRAIL_MAX_SAMPLES, CURSOR_TRAIL_RENDER_POINTS,
        CentripetalCatmullRom, CubicBezierSegment2D, HandPathModulation, HeadingArcChooser,
        LABEL_PILL_HEIGHT, LABEL_PILL_MAX_CHARS, LABEL_PILL_MAX_WORDS, LabelPillSnapshot,
        PathSplineKind, PointerPathTrack, VelocityVerletSpring1D, place_label_pill, wrap_pi,
    },
    dwindle::{
        CardinalDir, DropZonePreview, DwindleConfig, DwindleInsertOutcome, DwindleTree,
        ResizeCorner,
    },
    geometry::{
        Anchor9, AnchorPoint, LogicalPoint, LogicalRect, LogicalViewport, SafeAreaInsets, SizeHints,
    },
    keybind_overlay::{
        KeybindOverlayLayout, KeybindOverlayRow, KeybindOverlayText, KeybindOverlayTextRole,
        compact_chord, layout_keybind_overlay, layout_keybind_overlay_page,
    },
    kinematics::{
        BezierPointerSolver, MAX_POINTER_SPRING_MS, MinimumJerkSolver, PointerSpring2D,
        PointerTrajectorySpec, PointerTrajectoryTrack, SpringSolver, TrajectorySolver,
    },
    layout::{DeckStashStack, FloatingEntry, FloatingStack, LayoutMode},
    lease::{PreemptEffects, WorkspaceLease, WorkspaceLeaseState},
    output_layout::{
        OutputLayout, OutputLayoutEntry, OutputLayoutSpec, OutputTransform, SavedOutputConfig,
        closest_representable_scale, compute_auto_scale,
    },
    rules::{
        BlockOutPolicy, DynamicWindowRules, InitialWindowRules, OpenMode,
        ProtectedSurfaceClassifier, WindowMatchContext, WindowMatchSpec, WindowRuleEngine,
        WindowRuleEntry, is_waydroid_app_id, is_waydroid_multiwindow_app_id,
    },
    scrolling::{
        CenterFocusedColumn, ColumnDisplay, EdgeAutoScrollConfig, EdgeScrollStep,
        GESTURE_DECELERATION_TOUCHPAD, PresetColumnWidth, SWIPE_HISTORY_WINDOW_MS, ScrollingColumn,
        ScrollingConfig, ScrollingDropTarget, ScrollingDropZone, ScrollingStrip, ScrollingTile,
        SwipeTracker, TabIndicatorConfig, TabIndicatorPosition, TabIndicatorSegment, TileHeight,
        VIEW_GESTURE_WORKING_AREA_MOVEMENT,
    },
    workspace::{
        DEFAULT_SCRATCHPAD_NAME, PresentationTarget, SPECIAL_WORKSPACE_PREFIX, WindowPresentation,
        WorkspaceState,
    },
};
