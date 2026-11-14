//! Pure-Rust compositor domain model (`Tier 0`).
//!
//! Combines Hyprland's `CSpace` dual-layer architecture (`DwindleTree` dynamic binary splits +
//! `FloatingStack` Z-order + `special:<name>` Scratchpad overlays), Niri's `ScrollingStrip`
//! horizontal tape (`preset_column_widths`, velocity-preserving `camera_spring`, and `30 px` drag
//! edge auto-scroll), and Koma's symmetrical `Headed`/`Headless` `Deck/Stash` Agent workspaces with
//! `Lease + Epoch` preemptive concurrency control and 4-tier adaptive frame clocks.

pub mod animation;
pub mod clock;
pub mod dwindle;
pub mod geometry;
pub mod kinematics;
pub mod layout;
pub mod lease;
pub mod rules;
pub mod scrolling;
pub mod workspace;

pub use crate::{
    animation::{
        AnimationConfig, BlurConfig, SampledWindowVisual, Spring, SpringParams, SpringTrack,
        WindowAnimationTable,
    },
    clock::{AdaptiveFrameClock, FrameClockConfig, FrameClockTier, FrameTickDecision},
    dwindle::{
        CardinalDir, DropZonePreview, DwindleConfig, DwindleInsertOutcome, DwindleTree,
        ResizeCorner,
    },
    geometry::{Anchor9, AnchorPoint, LogicalPoint, LogicalRect, LogicalViewport},
    kinematics::{MinimumJerkSolver, SpringSolver, TrajectorySolver},
    layout::{DeckStashStack, FloatingEntry, FloatingStack, LayoutMode},
    lease::{PreemptEffects, WorkspaceLease, WorkspaceLeaseState},
    rules::{
        BlockOutPolicy, DynamicWindowRules, InitialWindowRules, OpenMode,
        ProtectedSurfaceClassifier, WindowMatchContext, WindowMatchSpec, WindowRuleEngine,
        WindowRuleEntry,
    },
    scrolling::{
        CenterFocusedColumn, ColumnDisplay, EdgeAutoScrollConfig, EdgeScrollStep,
        GESTURE_DECELERATION_TOUCHPAD, PresetColumnWidth, SWIPE_HISTORY_WINDOW_MS, ScrollingColumn,
        ScrollingConfig, ScrollingDropTarget, ScrollingDropZone, ScrollingStrip, ScrollingTile,
        SwipeTracker, TabIndicatorConfig, TabIndicatorPosition, TabIndicatorSegment, TileHeight,
        VIEW_GESTURE_WORKING_AREA_MOVEMENT,
    },
    workspace::{
        DEFAULT_SCRATCHPAD_NAME, PresentationTarget, SPECIAL_WORKSPACE_PREFIX, WorkspaceState,
    },
};
