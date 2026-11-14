//! Niri-faithful horizontal `Scrolling` strip layout engine (`layout = "scrolling"`):
//! - Multi-tile vertical column stacking (`ScrollingColumn` owning `1..=N` `ScrollingTile`s with
//!   `TileHeight::{Auto, Preset}`, `preset_window_heights`, `reset_window_height`, vertical focus
//!   `focus_up/down`, vertical swap `move_tile_up/down`, and interactive vertical divider resize),
//! - Full Niri `consume` & `expel` suite (`consume_or_expel_window_left`,
//!   `consume_or_expel_window_right`, `consume_window_into_column`, `expel_window_from_column`),
//! - `preset_column_widths` (`Mod + R` / `Mod + Ctrl + R`) bidirectional cycling with exact pixel
//!   fallback when manually resized (`niri/src/layout/scrolling.rs` `Column::toggle_width`),
//! - `CenterFocusedColumn` (`Never`, `Always`, `OnOverflow`) + `always_center_single_column`
//!   (`ScrollingSpace::compute_new_view_offset_for_column`),
//! - Velocity-preserving 2nd-order `camera_spring` (`HorizontalViewMovement`) + 3-finger touchpad
//!   1:1 continuous `ViewOffsetGesture` & `0.997` velocity projection (`SwipeTracker`),
//! - Interactive window-drag edge auto-scroll (`30 px` / `100 ms` / `1500 px/s`) + 2D drop zones
//!   (insert as new column OR stack into upper/lower slot of an existing column!).

use std::collections::VecDeque;

use koma_proto::WindowId;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    animation::{SpringParams, SpringTrack},
    dwindle::ResizeCorner,
    geometry::{LogicalPoint, LogicalRect, LogicalViewport},
};

/// Unaccelerated touchpad units corresponding to one full working-area width of horizontal view
/// movement (`niri/src/layout/scrolling.rs` lines 30-31: `VIEW_GESTURE_WORKING_AREA_MOVEMENT = 1200.`).
pub const VIEW_GESTURE_WORKING_AREA_MOVEMENT: f64 = 1200.0;

/// Per-millisecond exponential velocity retention factor for touchpad kinetic projection
/// (`libadwaita` `AdwSwipeTracker` / `niri/src/input/swipe_tracker.rs`: `DECELERATION_TOUCHPAD = 0.997`).
pub const GESTURE_DECELERATION_TOUCHPAD: f64 = 0.997;

/// Rolling timestamp window in milliseconds retained by `SwipeTracker` (`MAX_HISTORY_TIME = 150 ms`).
pub const SWIPE_HISTORY_WINDOW_MS: u64 = 150;

/// Rubber-band resistance multiplier applied when dragging past `[min_camera_x, max_camera_x]`.
pub const GESTURE_OVERSCROLL_RUBBER_BAND: f64 = 0.25;

#[derive(Debug, Clone, Copy)]
struct SwipeSample {
    delta: f64,
    timestamp_ms: u64,
}

/// 1D touchpad swipe displacement & rolling `150 ms` velocity tracker modeled directly on
/// `niri/src/input/swipe_tracker.rs` and `libadwaita` `AdwSwipeTracker`.
#[derive(Debug, Clone, Default)]
pub struct SwipeTracker {
    pos: f64,
    history: VecDeque<SwipeSample>,
}

impl SwipeTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pos: 0.0,
            history: VecDeque::with_capacity(32),
        }
    }

    pub fn push(&mut self, delta: f64, timestamp_ms: u64) {
        if !delta.is_finite() {
            return;
        }
        self.pos += delta;
        let monotonic_ts = self
            .history
            .back()
            .map_or(timestamp_ms, |last| timestamp_ms.max(last.timestamp_ms));
        self.history.push_back(SwipeSample {
            delta,
            timestamp_ms: monotonic_ts,
        });
        self.trim_stale(monotonic_ts);
    }

    pub fn reset_pos_keep_history(&mut self) {
        self.pos = 0.0;
    }

    #[must_use]
    pub const fn pos(&self) -> f64 {
        self.pos
    }

    fn trim_stale(&mut self, reference_ms: u64) {
        let cutoff = reference_ms.saturating_sub(SWIPE_HISTORY_WINDOW_MS);
        while self
            .history
            .front()
            .is_some_and(|sample| sample.timestamp_ms < cutoff)
        {
            let _ = self.history.pop_front();
        }
    }

    pub fn velocity(&mut self, release_ms: u64) -> f64 {
        let effective_now = self
            .history
            .back()
            .map_or(release_ms, |last| release_ms.max(last.timestamp_ms));
        self.trim_stale(effective_now);

        if self.history.len() < 2 {
            return 0.0;
        }
        let first_ts = self.history.front().map_or(0, |s| s.timestamp_ms);
        let last_ts = self.history.back().map_or(0, |s| s.timestamp_ms);
        let total_dt_ms = last_ts.saturating_sub(first_ts);
        if total_dt_ms == 0 {
            return 0.0;
        }

        let sum_delta: f64 = self.history.iter().skip(1).map(|s| s.delta).sum();
        let dt_secs = (total_dt_ms as f64) / 1000.0;
        let v = sum_delta / dt_secs;
        if v.is_finite() { v } else { 0.0 }
    }

    #[must_use]
    pub fn project_distance(velocity_per_sec: f64) -> f64 {
        if !velocity_per_sec.is_finite() || velocity_per_sec.abs() <= 1e-6 {
            return 0.0;
        }
        let denom = -1000.0 * GESTURE_DECELERATION_TOUCHPAD.ln();
        velocity_per_sec / denom
    }

    pub fn projected_end_pos(&mut self, release_ms: u64) -> f64 {
        let v = self.velocity(release_ms);
        self.pos + Self::project_distance(v)
    }
}

/// Proportional (`0.1..=1.0`) or fixed-pixel (`>= 64`) size specification used by both
/// `preset_column_widths` and `preset_window_heights` (`niri_config::PresetSize`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PresetColumnWidth {
    Proportion(f64),
    Fixed(u32),
}

impl Default for PresetColumnWidth {
    fn default() -> Self {
        Self::Proportion(0.5)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RawPresetColumnWidth {
    TaggedProportion { proportion: f64 },
    TaggedFixed { fixed: u32 },
    BareNumber(f64),
}

impl Serialize for PresetColumnWidth {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match *self {
            Self::Proportion(p) => {
                RawPresetColumnWidth::TaggedProportion { proportion: p }.serialize(serializer)
            }
            Self::Fixed(px) => {
                RawPresetColumnWidth::TaggedFixed { fixed: px }.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for PresetColumnWidth {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawPresetColumnWidth::deserialize(deserializer)?;
        match raw {
            RawPresetColumnWidth::TaggedProportion { proportion } => {
                if !proportion.is_finite() || proportion <= 0.0 {
                    return Err(serde::de::Error::custom(
                        "preset `proportion` must be a finite number > 0.0",
                    ));
                }
                Ok(Self::Proportion(proportion.clamp(0.1, 1.0)))
            }
            RawPresetColumnWidth::TaggedFixed { fixed } => Ok(Self::Fixed(fixed.max(64))),
            RawPresetColumnWidth::BareNumber(v) => {
                if !v.is_finite() || v <= 0.0 {
                    return Err(serde::de::Error::custom(
                        "preset size must be a positive number",
                    ));
                }
                if v <= 1.0 {
                    Ok(Self::Proportion(v.clamp(0.1, 1.0)))
                } else {
                    Ok(Self::Fixed((v.round() as u32).max(64)))
                }
            }
        }
    }
}

impl PresetColumnWidth {
    #[must_use]
    pub fn resolve_px(self, usable_span: f64) -> f64 {
        let max_s = usable_span.max(64.0);
        match self {
            Self::Proportion(p) => (max_s * p.clamp(0.1, 1.0)).round().clamp(48.0, max_s),
            Self::Fixed(px) => f64::from(px).clamp(48.0, max_s),
        }
    }

    #[must_use]
    pub fn approx_eq(self, other: Self) -> bool {
        match (self, other) {
            (Self::Proportion(a), Self::Proportion(b)) => (a - b).abs() <= 1e-4,
            (Self::Fixed(a), Self::Fixed(b)) => a == b,
            _ => false,
        }
    }
}

/// Vertical height allocation mode of an individual `ScrollingTile` inside a `ScrollingColumn`
/// (`niri/src/layout/scrolling.rs` `WindowHeight`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TileHeight {
    /// Shares remaining column height evenly (or weighted after interactive divider drag) with
    /// sibling `Auto` tiles (`reset-window-height` restores `weight = 1.0`).
    Auto { weight: f64 },
    /// Explicit proportional or fixed height set via `switch_preset_window_height` or resize.
    Preset(PresetColumnWidth),
}

impl Default for TileHeight {
    fn default() -> Self {
        Self::Auto { weight: 1.0 }
    }
}

/// Single window tile stacked vertically inside a `ScrollingColumn`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollingTile {
    pub win_id: WindowId,
    pub height: TileHeight,
    pub preset_height_idx: Option<usize>,
}

impl ScrollingTile {
    #[must_use]
    pub const fn new_auto(win_id: WindowId) -> Self {
        Self {
            win_id,
            height: TileHeight::Auto { weight: 1.0 },
            preset_height_idx: None,
        }
    }
}

/// Policy governing how the horizontal viewport camera aligns when focusing a column
/// (`niri_config::CenterFocusedColumn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CenterFocusedColumn {
    #[default]
    Never,
    Always,
    #[serde(alias = "on-overflow")]
    OnOverflow,
}

/// Column presentation mode (`niri/src/layout/scrolling.rs` `ColumnDisplay`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnDisplay {
    /// Classic vertical tile stack sharing column height.
    #[default]
    Normal,
    /// Stacked tabbed deck showing only `active_tile_idx` at full column height + side `tab-indicator`.
    Tabbed,
}

/// Edge of the column where `tab-indicator` pills are anchored (`niri_config::TabIndicatorPosition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabIndicatorPosition {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

/// Declarative `koma.scrolling({ tab_indicator = { ... } })` configuration
/// (`niri-config/src/appearance.rs` `TabIndicator`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabIndicatorConfig {
    pub enabled: bool,
    pub hide_when_single_tab: bool,
    pub place_within_column: bool,
    pub position: TabIndicatorPosition,
    /// Indicator bar thickness in logical pixels (`4.0` default).
    pub width: f64,
    /// Gap between the column edge and the indicator bar in logical pixels (`4.0` default).
    pub gap: f64,
    /// Gap between adjacent tab pill segments in logical pixels (`4.0` default).
    pub gaps_between_tabs: f64,
    /// Total indicator bar span as a proportion (`0.1..=1.0`) of the column edge (`0.5` default).
    pub length_proportion: f64,
    /// Rounded pill corner radius in logical pixels (`2.0` default).
    pub corner_radius: f32,
}

impl Default for TabIndicatorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hide_when_single_tab: true,
            place_within_column: false,
            position: TabIndicatorPosition::Left,
            width: 4.0,
            gap: 4.0,
            gaps_between_tabs: 4.0,
            length_proportion: 0.5,
            corner_radius: 2.0,
        }
    }
}

/// Single rendered pill segment of a column's `tab-indicator`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabIndicatorSegment {
    pub win_id: WindowId,
    pub rect: LogicalRect,
    pub is_active: bool,
    pub corner_radius: f32,
}

/// Edge view auto-scroll parameters active while dragging a window near the left/right screen edge
/// (`niri-config/src/gestures.rs` `DndEdgeViewScroll`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EdgeAutoScrollConfig {
    pub enabled: bool,
    pub trigger_width: f64,
    pub delay_ms: u64,
    pub max_speed: f64,
}

impl Default for EdgeAutoScrollConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_width: 30.0,
            delay_ms: 100,
            max_speed: 1500.0,
        }
    }
}

/// Declarative `koma.scrolling({ ... })` layout configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrollingConfig {
    /// Ordered width presets cycled via `Mod + R` / `Mod + Ctrl + R` (`[1/3, 1/2, 2/3]` default).
    pub preset_column_widths: Vec<PresetColumnWidth>,
    /// Ordered tile height presets cycled via `Mod + Ctrl + Shift + R` (`[1/3, 1/2, 2/3]` default).
    pub preset_window_heights: Vec<PresetColumnWidth>,
    /// Initial width assigned to newly opened columns (`Proportion(0.5)` default).
    pub default_column_width: PresetColumnWidth,
    /// Default display mode (`normal` | `tabbed`) for newly created columns.
    pub default_column_display: ColumnDisplay,
    /// Side/top/bottom pill `tab-indicator` styling for `ColumnDisplay::Tabbed` columns.
    pub tab_indicator: TabIndicatorConfig,
    /// Camera alignment mode when focusing columns (`Never` default).
    pub center_focused_column: CenterFocusedColumn,
    /// Center a lone column on screen even when `center_focused_column = "never"`.
    pub always_center_single_column: bool,
    /// Camera spring damping ratio (`1.0` critically damped Niri `HorizontalViewMovement` default).
    pub camera_damping_ratio: f64,
    /// Camera spring stiffness (`800.0` default).
    pub camera_stiffness: f64,
    /// Camera spring rest threshold (`0.0001` default).
    pub camera_epsilon: f64,
    /// Unaccelerated touchpad units per full working-area width during 3-finger continuous swipe.
    pub swipe_working_area_movement: f64,
    /// Horizontal & intra-column vertical gap in logical pixels (`8` default).
    pub inner_gap: u32,
    /// Outer viewport margin in logical pixels (`10` default).
    pub outer_gap: u32,
    /// Interactive window-drag edge auto-scroll settings.
    pub edge_auto_scroll: EdgeAutoScrollConfig,
}

impl Default for ScrollingConfig {
    fn default() -> Self {
        Self {
            preset_column_widths: vec![
                PresetColumnWidth::Proportion(1.0 / 3.0),
                PresetColumnWidth::Proportion(0.5),
                PresetColumnWidth::Proportion(2.0 / 3.0),
            ],
            preset_window_heights: vec![
                PresetColumnWidth::Proportion(1.0 / 3.0),
                PresetColumnWidth::Proportion(0.5),
                PresetColumnWidth::Proportion(2.0 / 3.0),
            ],
            default_column_width: PresetColumnWidth::Proportion(0.5),
            default_column_display: ColumnDisplay::Normal,
            tab_indicator: TabIndicatorConfig::default(),
            center_focused_column: CenterFocusedColumn::Never,
            always_center_single_column: false,
            camera_damping_ratio: 1.0,
            camera_stiffness: 800.0,
            camera_epsilon: 0.0001,
            swipe_working_area_movement: VIEW_GESTURE_WORKING_AREA_MOVEMENT,
            inner_gap: 8,
            outer_gap: 10,
            edge_auto_scroll: EdgeAutoScrollConfig::default(),
        }
    }
}

impl ScrollingConfig {
    #[must_use]
    pub fn camera_spring_params(&self) -> SpringParams {
        SpringParams::new(
            self.camera_damping_ratio,
            self.camera_stiffness,
            self.camera_epsilon,
        )
    }

    #[must_use]
    pub fn sanitized(mut self) -> Self {
        if self.preset_column_widths.is_empty() {
            self.preset_column_widths = Self::default().preset_column_widths;
        }
        if self.preset_window_heights.is_empty() {
            self.preset_window_heights = Self::default().preset_window_heights;
        }
        self.camera_damping_ratio = self.camera_damping_ratio.clamp(0.2, 2.0);
        self.camera_stiffness = self.camera_stiffness.clamp(20.0, 5000.0);
        self.camera_epsilon = self.camera_epsilon.clamp(1e-6, 0.05);
        self.swipe_working_area_movement = self.swipe_working_area_movement.clamp(200.0, 5000.0);
        self.edge_auto_scroll.trigger_width = self.edge_auto_scroll.trigger_width.clamp(4.0, 240.0);
        self.edge_auto_scroll.max_speed = self.edge_auto_scroll.max_speed.clamp(50.0, 10_000.0);
        self.tab_indicator.width = self.tab_indicator.width.clamp(2.0, 24.0);
        self.tab_indicator.gap = self.tab_indicator.gap.clamp(0.0, 32.0);
        self.tab_indicator.gaps_between_tabs =
            self.tab_indicator.gaps_between_tabs.clamp(0.0, 24.0);
        self.tab_indicator.length_proportion = self.tab_indicator.length_proportion.clamp(0.1, 1.0);
        self.tab_indicator.corner_radius = self.tab_indicator.corner_radius.clamp(0.0, 12.0);
        self
    }
}

/// Multi-tile vertical stack column inside `ScrollingStrip` (`niri/src/layout/scrolling.rs` `Column`).
///
/// Always contains at least 1 `ScrollingTile` (`tiles.len() >= 1`), maintaining its own remembered
/// `active_tile_idx` (`is-active-in-column` in Niri) across horizontal focus switches.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollingColumn {
    pub tiles: Vec<ScrollingTile>,
    pub active_tile_idx: usize,
    pub display: ColumnDisplay,
    pub width: PresetColumnWidth,
    pub preset_width_idx: Option<usize>,
    pub is_maximized_column: bool,
}

impl ScrollingColumn {
    #[must_use]
    pub fn new_single(
        win_id: WindowId,
        width: PresetColumnWidth,
        presets: &[PresetColumnWidth],
    ) -> Self {
        Self::new_with_tile(ScrollingTile::new_auto(win_id), width, presets)
    }

    #[must_use]
    pub fn new_with_tile(
        tile: ScrollingTile,
        width: PresetColumnWidth,
        presets: &[PresetColumnWidth],
    ) -> Self {
        let preset_width_idx = presets.iter().position(|p| p.approx_eq(width));
        Self {
            tiles: vec![tile],
            active_tile_idx: 0,
            display: ColumnDisplay::Normal,
            width,
            preset_width_idx,
            is_maximized_column: false,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    #[must_use]
    pub fn contains(&self, win_id: WindowId) -> bool {
        self.tiles.iter().any(|t| t.win_id == win_id)
    }

    #[must_use]
    pub fn active_win_id(&self) -> Option<WindowId> {
        self.tiles
            .get(self.active_tile_idx.min(self.tiles.len().saturating_sub(1)))
            .map(|t| t.win_id)
    }

    #[must_use]
    pub const fn effective_width(&self) -> PresetColumnWidth {
        if self.is_maximized_column {
            PresetColumnWidth::Proportion(1.0)
        } else {
            self.width
        }
    }

    fn clamp_active_tile_idx(&mut self) {
        if self.tiles.is_empty() {
            self.active_tile_idx = 0;
        } else if self.active_tile_idx >= self.tiles.len() {
            self.active_tile_idx = self.tiles.len() - 1;
        }
    }

    /// Computes exact `(WindowId, y_px, height_px)` vertical tile allocations inside `[outer_gap, outer_gap + usable_h]`.
    #[must_use]
    pub fn compute_vertical_tile_spans(
        &self,
        outer_gap: u32,
        inner_gap: u32,
        usable_h: u32,
    ) -> Vec<(WindowId, i32, u32)> {
        let n = self.tiles.len();
        if n == 0 {
            return Vec::new();
        }
        let og = outer_gap as i32;
        if self.display == ColumnDisplay::Tabbed {
            let active = self.active_tile_idx.min(n - 1);
            return vec![(self.tiles[active].win_id, og, usable_h.max(48))];
        }
        if n == 1 {
            return vec![(self.tiles[0].win_id, og, usable_h.max(48))];
        }

        let gap_total = inner_gap.saturating_mul((n - 1) as u32);
        let avail_h = f64::from(usable_h.saturating_sub(gap_total).max((n as u32) * 48));
        let min_h = (48.0_f64).min(avail_h / (n as f64));

        let mut raw_heights = vec![0.0_f64; n];
        let mut auto_weight_sum = 0.0_f64;
        let mut auto_count = 0usize;
        let mut preset_sum = 0.0_f64;

        for (idx, tile) in self.tiles.iter().enumerate() {
            match tile.height {
                TileHeight::Auto { weight } => {
                    auto_count += 1;
                    auto_weight_sum += weight.clamp(0.1, 20.0);
                }
                TileHeight::Preset(p) => {
                    let h = p.resolve_px(avail_h).max(min_h);
                    raw_heights[idx] = h;
                    preset_sum += h;
                }
            }
        }

        let min_auto_budget = (auto_count as f64) * min_h;
        if auto_count > 0 && preset_sum + min_auto_budget <= avail_h {
            let remaining = avail_h - preset_sum;
            let denom = auto_weight_sum.max(0.1);
            for (idx, tile) in self.tiles.iter().enumerate() {
                if let TileHeight::Auto { weight } = tile.height {
                    raw_heights[idx] = (remaining * (weight.clamp(0.1, 20.0) / denom)).max(min_h);
                }
            }
        } else {
            // All tiles are explicit `Preset` OR explicit heights overflowed `avail_h`:
            // assign `Auto` tiles a proportional fallback share and normalize all heights to `avail_h`!
            let fallback_auto = (avail_h / (n as f64)).max(min_h);
            for (idx, tile) in self.tiles.iter().enumerate() {
                if let TileHeight::Auto { weight } = tile.height {
                    raw_heights[idx] = fallback_auto * weight.clamp(0.1, 20.0);
                }
            }
            let total_raw: f64 = raw_heights.iter().sum::<f64>().max(1.0);
            for h in &mut raw_heights {
                *h = (*h / total_raw * avail_h).max(min_h);
            }
            // Re-normalize any clamp overshoot so sum == avail_h
            let post_sum: f64 = raw_heights.iter().sum::<f64>().max(1.0);
            for h in &mut raw_heights {
                *h = *h / post_sum * avail_h;
            }
        }

        // Pixel-exact cumulative rounding so gaps remain exact and bottom edge aligns flush to `og + usable_h`!
        let mut out = Vec::with_capacity(n);
        let mut cum_h = 0.0_f64;
        let mut cur_y = og;
        let bottom_limit = og + usable_h as i32;

        for (idx, tile) in self.tiles.iter().enumerate() {
            if idx + 1 == n {
                let last_h = (bottom_limit - cur_y).max(32) as u32;
                out.push((tile.win_id, cur_y, last_h));
            } else {
                cum_h += raw_heights[idx];
                let target_end_y = og + cum_h.round() as i32 + (idx as i32) * (inner_gap as i32);
                let h = (target_end_y - cur_y).max(32) as u32;
                out.push((tile.win_id, cur_y, h));
                cur_y += (h as i32) + (inner_gap as i32);
            }
        }

        out
    }
}

/// Target placement mode when dropping a `MoveWindowGrab` window onto a `ScrollingStrip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollingDropTarget {
    /// Insert as a brand-new standalone column at `insert_idx`.
    NewColumn { insert_idx: usize },
    /// Consume/stack directly into existing `col_idx` at vertical slot `tile_insert_idx`.
    StackIntoColumn {
        col_idx: usize,
        tile_insert_idx: usize,
    },
}

/// Candidate column or intra-column stack insertion target & preview card computed during `MoveWindowGrab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollingDropZone {
    pub target: ScrollingDropTarget,
    pub preview_rect: LogicalRect,
}

/// Result of advancing interactive drag edge auto-scroll at `now_ms`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EdgeScrollStep {
    Inactive,
    AtBoundary,
    Debouncing,
    Scrolled(f64),
}

impl EdgeScrollStep {
    #[must_use]
    pub const fn keeps_frame_loop_alive(self) -> bool {
        matches!(self, Self::Debouncing | Self::Scrolled(_))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct EdgeScrollRuntime {
    hover_start_ms: Option<u64>,
    last_tick_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct ActiveViewOffsetGesture {
    start_camera_x: f64,
    start_active_idx: usize,
}

/// Niri-faithful horizontal scrolling column strip (`layout = "scrolling"`) with multi-tile
/// vertical column stacking and `consume_or_expel_window` support.
#[derive(Debug, Clone)]
pub struct ScrollingStrip {
    pub config: ScrollingConfig,
    pub columns: Vec<ScrollingColumn>,
    pub active_idx: usize,
    pub camera_x: f64,
    pub camera_spring: SpringTrack,
    pub instant_bypass: bool,
    edge_scroll: EdgeScrollRuntime,
    active_gesture: Option<ActiveViewOffsetGesture>,
}

impl Default for ScrollingStrip {
    fn default() -> Self {
        Self::new(ScrollingConfig::default(), false)
    }
}

impl ScrollingStrip {
    #[must_use]
    pub fn new(config: ScrollingConfig, instant_bypass: bool) -> Self {
        let cfg = config.sanitized();
        let params = cfg.camera_spring_params();
        Self {
            config: cfg,
            columns: Vec::new(),
            active_idx: 0,
            camera_x: 0.0,
            camera_spring: SpringTrack::settled_at(0.0, params),
            instant_bypass,
            edge_scroll: EdgeScrollRuntime::default(),
            active_gesture: None,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Number of horizontal columns in the strip.
    #[must_use]
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Total number of tiled windows across all columns & vertical stacks.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.columns.iter().map(ScrollingColumn::len).sum()
    }

    /// Ordered list of all `WindowId`s across columns (left-to-right, top-to-bottom).
    #[must_use]
    pub fn all_window_ids(&self) -> Vec<WindowId> {
        self.columns
            .iter()
            .flat_map(|c| c.tiles.iter().map(|t| t.win_id))
            .collect()
    }

    #[must_use]
    pub fn contains(&self, win_id: WindowId) -> bool {
        self.columns.iter().any(|c| c.contains(win_id))
    }

    /// Locates `(col_idx, tile_idx)` owning `win_id`.
    #[must_use]
    pub fn find_window(&self, win_id: WindowId) -> Option<(usize, usize)> {
        for (c_idx, col) in self.columns.iter().enumerate() {
            if let Some(t_idx) = col.tiles.iter().position(|t| t.win_id == win_id) {
                return Some((c_idx, t_idx));
            }
        }
        None
    }

    /// Returns the column `PresetColumnWidth` if `win_id` is currently the sole window in its column
    /// (so dragging and re-dropping a standalone column preserves its custom width!).
    #[must_use]
    pub fn standalone_column_width_of(&self, win_id: WindowId) -> Option<PresetColumnWidth> {
        let (c_idx, _) = self.find_window(win_id)?;
        let col = &self.columns[c_idx];
        (col.tiles.len() == 1).then_some(col.width)
    }

    #[must_use]
    pub fn active_window(&self) -> Option<WindowId> {
        self.columns
            .get(self.active_idx)
            .and_then(ScrollingColumn::active_win_id)
    }

    #[must_use]
    pub const fn gap_px(&self) -> u32 {
        self.config.inner_gap
    }

    #[must_use]
    pub const fn outer_gap_px(&self) -> u32 {
        self.config.outer_gap
    }

    #[must_use]
    pub const fn is_gesture_active(&self) -> bool {
        self.active_gesture.is_some()
    }

    pub fn set_gaps(&mut self, inner_gap: u32, outer_gap: u32) {
        self.config.inner_gap = inner_gap;
        self.config.outer_gap = outer_gap;
    }

    /// Hot-reloads `ScrollingConfig` losslessly (`niri/src/layout/scrolling.rs` `update_config`).
    pub fn update_config(&mut self, next: ScrollingConfig, viewport: LogicalViewport, now_ms: u64) {
        let sanitized = next.sanitized();
        let width_presets_changed = self.config.preset_column_widths.len()
            != sanitized.preset_column_widths.len()
            || self
                .config
                .preset_column_widths
                .iter()
                .zip(&sanitized.preset_column_widths)
                .any(|(a, b)| !a.approx_eq(*b));

        let height_presets_changed = self.config.preset_window_heights.len()
            != sanitized.preset_window_heights.len()
            || self
                .config
                .preset_window_heights
                .iter()
                .zip(&sanitized.preset_window_heights)
                .any(|(a, b)| !a.approx_eq(*b));

        for col in &mut self.columns {
            if width_presets_changed {
                col.preset_width_idx = sanitized
                    .preset_column_widths
                    .iter()
                    .position(|p| p.approx_eq(col.width));
            }
            if height_presets_changed {
                for tile in &mut col.tiles {
                    tile.preset_height_idx = match tile.height {
                        TileHeight::Preset(h) => sanitized
                            .preset_window_heights
                            .iter()
                            .position(|p| p.approx_eq(h)),
                        TileHeight::Auto { .. } => None,
                    };
                }
            }
        }

        self.config = sanitized;
        if self.active_gesture.is_none() {
            self.recompute_camera_for_active(None, viewport, now_ms);
        }
    }

    #[must_use]
    fn usable_width(&self, viewport: LogicalViewport) -> f64 {
        let wa = viewport.work_area_rect();
        f64::from(wa.w.saturating_sub(self.config.outer_gap * 2).max(64))
    }

    #[must_use]
    fn usable_height(&self, viewport: LogicalViewport) -> u32 {
        let wa = viewport.work_area_rect();
        wa.h.saturating_sub(self.config.outer_gap * 2).max(64)
    }

    #[must_use]
    pub fn touchpad_to_strip_scale(&self, viewport: LogicalViewport) -> f64 {
        self.usable_width(viewport) / self.config.swipe_working_area_movement.max(200.0)
    }

    #[must_use]
    fn column_spans(&self, usable_w: f64) -> Vec<(f64, f64)> {
        let gap = f64::from(self.config.inner_gap);
        let mut out = Vec::with_capacity(self.columns.len());
        let mut cursor_x = 0.0_f64;
        for col in &self.columns {
            let w = col.effective_width().resolve_px(usable_w);
            out.push((cursor_x, w));
            cursor_x += w + gap;
        }
        out
    }

    #[must_use]
    pub fn camera_scroll_bounds(&self, viewport: LogicalViewport) -> (f64, f64) {
        if self.columns.is_empty() {
            return (0.0, 0.0);
        }
        let usable_w = self.usable_width(viewport);
        let spans = self.column_spans(usable_w);
        let (first_x, first_w) = spans[0];
        let (last_x, last_w) = spans[spans.len() - 1];
        let is_centering = self.config.center_focused_column == CenterFocusedColumn::Always
            || (self.config.always_center_single_column && self.columns.len() == 1);

        if is_centering {
            let min_c = if first_w >= usable_w {
                first_x
            } else {
                first_x - (usable_w - first_w) * 0.5
            };
            let max_c = if last_w >= usable_w {
                last_x
            } else {
                last_x - (usable_w - last_w) * 0.5
            };
            (min_c.min(max_c), max_c.max(min_c))
        } else if self.columns.len() == 1 && first_w < usable_w {
            let right_align = first_w - usable_w;
            (right_align.min(0.0), 0.0)
        } else {
            let total_strip_w = last_x + last_w;
            let max_scroll = (total_strip_w - usable_w).max(0.0);
            (0.0, max_scroll)
        }
    }

    #[must_use]
    fn compute_camera_target_for_column(
        &self,
        prev_idx: Option<usize>,
        target_idx: usize,
        viewport: LogicalViewport,
    ) -> f64 {
        if self.columns.is_empty() {
            return 0.0;
        }
        let idx = target_idx.min(self.columns.len() - 1);
        let usable_w = self.usable_width(viewport);
        let spans = self.column_spans(usable_w);
        let (col_x, col_w) = spans[idx];

        let center_col = |cx: f64, cw: f64| -> f64 {
            if cw >= usable_w {
                cx
            } else {
                cx - (usable_w - cw) * 0.5
            }
        };

        let fit_col = |cx: f64, cw: f64, cur_cam: f64| -> f64 {
            let (min_cam, max_cam) = self.camera_scroll_bounds(viewport);
            if cw >= usable_w || cx < cur_cam {
                cx.clamp(min_cam, max_cam)
            } else if cx + cw > cur_cam + usable_w {
                (cx + cw - usable_w).clamp(min_cam, max_cam)
            } else {
                cur_cam.clamp(min_cam, max_cam)
            }
        };

        if self.config.center_focused_column == CenterFocusedColumn::Always
            || (self.config.always_center_single_column && self.columns.len() == 1)
        {
            return center_col(col_x, col_w);
        }

        if self.config.center_focused_column == CenterFocusedColumn::OnOverflow
            && let Some(p_idx) = prev_idx.filter(|&p| p != idx && p < self.columns.len())
        {
            let neighbor_idx = if p_idx > idx {
                (idx + 1).min(self.columns.len() - 1)
            } else {
                idx.saturating_sub(1)
            };
            let (nx, nw) = spans[neighbor_idx];
            let left_edge = col_x.min(nx);
            let right_edge = (col_x + col_w).max(nx + nw);
            let combined_with_padding =
                (right_edge - left_edge) + f64::from(self.config.inner_gap) * 2.0;
            if combined_with_padding > usable_w {
                return center_col(col_x, col_w);
            }
        }

        fit_col(col_x, col_w, self.camera_x)
    }

    fn set_camera_target(&mut self, target: f64, now_ms: u64) {
        self.active_gesture = None;
        self.camera_x = target;
        if self.instant_bypass {
            self.camera_spring.snap_to(target);
        } else {
            let params = self.config.camera_spring_params();
            self.camera_spring.retarget(target, now_ms, params);
        }
    }

    fn recompute_camera_for_active(
        &mut self,
        prev_idx: Option<usize>,
        viewport: LogicalViewport,
        now_ms: u64,
    ) {
        if self.columns.is_empty() {
            self.active_idx = 0;
            self.set_camera_target(0.0, now_ms);
            return;
        }
        if self.active_idx >= self.columns.len() {
            self.active_idx = self.columns.len() - 1;
        }
        self.columns[self.active_idx].clamp_active_tile_idx();
        let next_cam = self.compute_camera_target_for_column(prev_idx, self.active_idx, viewport);
        self.set_camera_target(next_cam, now_ms);
    }

    // =========================================================================
    // Niri Multi-Tile Column Consume & Expel Operations
    // (`niri/src/layout/scrolling.rs` `consume_or_expel_window_{left,right}`,
    //  `consume_window_into_column`, `expel_window_from_column`)
    // =========================================================================

    /// Pulls the topmost tile from the column immediately to the right (`active_idx + 1`) and
    /// appends it to the bottom of the currently focused column (`Mod + Comma`, Niri
    /// `consume-window-into-column`), keeping focus on the current window!
    pub fn consume_window_into_column(&mut self, viewport: LogicalViewport, now_ms: u64) -> bool {
        if self.columns.len() < 2 || self.active_idx + 1 >= self.columns.len() {
            return false;
        }
        let right_idx = self.active_idx + 1;
        let consumed_tile = self.columns[right_idx].tiles.remove(0);
        if self.columns[right_idx].tiles.is_empty() {
            self.columns.remove(right_idx);
        } else {
            self.columns[right_idx].active_tile_idx =
                self.columns[right_idx].active_tile_idx.saturating_sub(1);
            self.columns[right_idx].clamp_active_tile_idx();
        }

        self.columns[self.active_idx].tiles.push(ScrollingTile {
            win_id: consumed_tile.win_id,
            height: TileHeight::Auto { weight: 1.0 },
            preset_height_idx: None,
        });
        self.recompute_camera_for_active(None, viewport, now_ms);
        true
    }

    /// Expels the currently focused tile from a multi-window column (`tiles.len() >= 2`) out into
    /// a new standalone column immediately to the right (`Mod + Period`, Niri
    /// `expel-window-from-column`), keeping focus on the expelled window!
    pub fn expel_window_from_column(&mut self, viewport: LogicalViewport, now_ms: u64) -> bool {
        if self.columns.is_empty() {
            return false;
        }
        let cur_col_idx = self.active_idx.min(self.columns.len() - 1);
        if self.columns[cur_col_idx].tiles.len() < 2 {
            return false;
        }

        let tile_idx = self.columns[cur_col_idx]
            .active_tile_idx
            .min(self.columns[cur_col_idx].tiles.len() - 1);
        let mut expelled = self.columns[cur_col_idx].tiles.remove(tile_idx);
        expelled.height = TileHeight::Auto { weight: 1.0 };
        expelled.preset_height_idx = None;
        self.columns[cur_col_idx].clamp_active_tile_idx();

        let new_col = ScrollingColumn::new_with_tile(
            expelled,
            self.config.default_column_width,
            &self.config.preset_column_widths,
        );
        let insert_at = cur_col_idx + 1;
        self.columns.insert(insert_at, new_col);
        self.active_idx = insert_at;
        self.recompute_camera_for_active(Some(cur_col_idx), viewport, now_ms);
        true
    }

    /// Unified Niri `consume-or-expel-window-left` (`Mod + BracketLeft`):
    /// - If the focused window is in a **multi-tile column (`>= 2` tiles)** -> **expels** it out
    ///   into a new standalone column immediately to the **left**.
    /// - If the focused window is **alone in its column (`1` tile)** -> **consumes** it into the
    ///   bottom of the adjacent column on the **left** (`active_idx - 1`).
    pub fn consume_or_expel_window_left(&mut self, viewport: LogicalViewport, now_ms: u64) -> bool {
        if self.columns.is_empty() {
            return false;
        }
        let cur_idx = self.active_idx.min(self.columns.len() - 1);

        if self.columns[cur_idx].tiles.len() >= 2 {
            // Expel focused tile to a new single-tile column on the LEFT (`insert_at = cur_idx`)
            let t_idx = self.columns[cur_idx]
                .active_tile_idx
                .min(self.columns[cur_idx].tiles.len() - 1);
            let mut expelled = self.columns[cur_idx].tiles.remove(t_idx);
            expelled.height = TileHeight::Auto { weight: 1.0 };
            expelled.preset_height_idx = None;
            self.columns[cur_idx].clamp_active_tile_idx();

            let new_col = ScrollingColumn::new_with_tile(
                expelled,
                self.config.default_column_width,
                &self.config.preset_column_widths,
            );
            self.columns.insert(cur_idx, new_col);
            self.active_idx = cur_idx;
            // Source column shifted right to `cur_idx + 1`
            self.recompute_camera_for_active(Some(cur_idx + 1), viewport, now_ms);
            true
        } else {
            // Alone in column -> consume into bottom of left neighbor (`cur_idx - 1`)
            if cur_idx == 0 {
                return false;
            }
            let mut old_col = self.columns.remove(cur_idx);
            let mut moved_tile = old_col.tiles.remove(0);
            moved_tile.height = TileHeight::Auto { weight: 1.0 };
            moved_tile.preset_height_idx = None;

            let target_idx = cur_idx - 1;
            let dst_col = &mut self.columns[target_idx];
            dst_col.tiles.push(moved_tile);
            dst_col.active_tile_idx = dst_col.tiles.len() - 1;
            self.active_idx = target_idx;
            self.recompute_camera_for_active(None, viewport, now_ms);
            true
        }
    }

    /// Unified Niri `consume-or-expel-window-right` (`Mod + BracketRight`):
    /// - If the focused window is in a **multi-tile column (`>= 2` tiles)** -> **expels** it out
    ///   into a new standalone column immediately to the **right**.
    /// - If the focused window is **alone in its column (`1` tile)** -> **consumes** it into the
    ///   bottom of the adjacent column on the **right** (`active_idx + 1`).
    pub fn consume_or_expel_window_right(
        &mut self,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> bool {
        if self.columns.is_empty() {
            return false;
        }
        let cur_idx = self.active_idx.min(self.columns.len() - 1);

        if self.columns[cur_idx].tiles.len() >= 2 {
            self.expel_window_from_column(viewport, now_ms)
        } else {
            if cur_idx + 1 >= self.columns.len() {
                return false;
            }
            let mut old_col = self.columns.remove(cur_idx);
            let mut moved_tile = old_col.tiles.remove(0);
            moved_tile.height = TileHeight::Auto { weight: 1.0 };
            moved_tile.preset_height_idx = None;

            // After removing `cur_idx`, the former right neighbor now sits at `cur_idx`!
            let dst_col = &mut self.columns[cur_idx];
            dst_col.tiles.push(moved_tile);
            dst_col.active_tile_idx = dst_col.tiles.len() - 1;
            self.active_idx = cur_idx;
            self.recompute_camera_for_active(None, viewport, now_ms);
            true
        }
    }

    // =========================================================================
    // Intra-Column Vertical Navigation, Tile Reordering & Window Height Presets
    // =========================================================================

    /// Focuses the tile immediately above the active tile in the focused column (`Mod + K`).
    pub fn focus_up(&mut self) -> Option<WindowId> {
        let col = self.columns.get_mut(self.active_idx)?;
        col.clamp_active_tile_idx();
        if col.active_tile_idx > 0 {
            col.active_tile_idx -= 1;
        }
        col.active_win_id()
    }

    /// Focuses the tile immediately below the active tile in the focused column (`Mod + J`).
    pub fn focus_down(&mut self) -> Option<WindowId> {
        let col = self.columns.get_mut(self.active_idx)?;
        col.clamp_active_tile_idx();
        if col.active_tile_idx + 1 < col.tiles.len() {
            col.active_tile_idx += 1;
        }
        col.active_win_id()
    }

    /// Moves the active tile up within its vertical column stack (`Mod + Shift + K`, Niri `move-window-up`).
    pub fn move_tile_up(&mut self) -> bool {
        let Some(col) = self.columns.get_mut(self.active_idx) else {
            return false;
        };
        col.clamp_active_tile_idx();
        if col.tiles.len() < 2 || col.active_tile_idx == 0 {
            return false;
        }
        col.tiles.swap(col.active_tile_idx, col.active_tile_idx - 1);
        col.active_tile_idx -= 1;
        true
    }

    /// Moves the active tile down within its vertical column stack (`Mod + Shift + J`, Niri `move-window-down`).
    pub fn move_tile_down(&mut self) -> bool {
        let Some(col) = self.columns.get_mut(self.active_idx) else {
            return false;
        };
        col.clamp_active_tile_idx();
        if col.tiles.len() < 2 || col.active_tile_idx + 1 >= col.tiles.len() {
            return false;
        }
        col.tiles.swap(col.active_tile_idx, col.active_tile_idx + 1);
        col.active_tile_idx += 1;
        true
    }

    /// Cycles the focused window's vertical height preset through `preset_window_heights`
    /// (`Mod + Ctrl + Shift + R`, Niri `switch-preset-window-height`).
    pub fn switch_preset_window_height(
        &mut self,
        forwards: bool,
        viewport: LogicalViewport,
    ) -> Option<PresetColumnWidth> {
        if self.columns.is_empty() || self.config.preset_window_heights.is_empty() {
            return None;
        }
        let usable_h = self.usable_height(viewport);
        let presets = &self.config.preset_window_heights;
        let len = presets.len();
        let col = &mut self.columns[self.active_idx];
        col.clamp_active_tile_idx();
        let n = col.tiles.len();
        let gap_total = self
            .config
            .inner_gap
            .saturating_mul(n.saturating_sub(1) as u32);
        let avail_h = f64::from(usable_h.saturating_sub(gap_total).max(64));

        let spans =
            col.compute_vertical_tile_spans(self.config.outer_gap, self.config.inner_gap, usable_h);
        let cur_px = f64::from(spans[col.active_tile_idx].2);
        let tile = &mut col.tiles[col.active_tile_idx];

        let next_idx = match tile.preset_height_idx.filter(|&i| i < len) {
            Some(idx) => (idx + if forwards { 1 } else { len - 1 }) % len,
            None => {
                if forwards {
                    presets
                        .iter()
                        .position(|p| cur_px + 1.0 < p.resolve_px(avail_h))
                        .unwrap_or(0)
                } else {
                    presets
                        .iter()
                        .rposition(|p| p.resolve_px(avail_h) + 1.0 < cur_px)
                        .unwrap_or(len - 1)
                }
            }
        };

        let chosen = presets[next_idx];
        tile.height = TileHeight::Preset(chosen);
        tile.preset_height_idx = Some(next_idx);
        Some(chosen)
    }

    /// Resets the focused window's height inside its column back to automatic equal sharing
    /// (`TileHeight::Auto { weight: 1.0 }`, Niri `reset-window-height`).
    pub fn reset_window_height(&mut self) -> bool {
        let Some(col) = self.columns.get_mut(self.active_idx) else {
            return false;
        };
        col.clamp_active_tile_idx();
        let tile = &mut col.tiles[col.active_tile_idx];
        tile.height = TileHeight::Auto { weight: 1.0 };
        tile.preset_height_idx = None;
        true
    }

    /// Toggles the focused column between `ColumnDisplay::Normal` (vertical split) and
    /// `ColumnDisplay::Tabbed` (`Mod + W`, Niri `toggle-column-tabbed-display`).
    pub fn toggle_column_tabbed_display(&mut self) -> Option<ColumnDisplay> {
        let col = self.columns.get_mut(self.active_idx)?;
        col.clamp_active_tile_idx();
        col.display = match col.display {
            ColumnDisplay::Normal => ColumnDisplay::Tabbed,
            ColumnDisplay::Tabbed => ColumnDisplay::Normal,
        };
        Some(col.display)
    }

    #[must_use]
    fn column_shows_within_indicator(&self, col: &ScrollingColumn) -> bool {
        let ind = &self.config.tab_indicator;
        col.display == ColumnDisplay::Tabbed
            && ind.enabled
            && ind.place_within_column
            && (!ind.hide_when_single_tab || col.tiles.len() >= 2)
    }

    #[must_use]
    fn apply_within_column_tab_inset(
        &self,
        col: &ScrollingColumn,
        mut rect: LogicalRect,
    ) -> LogicalRect {
        if !self.column_shows_within_indicator(col) {
            return rect;
        }
        let ind = &self.config.tab_indicator;
        let reserve = (ind.width + ind.gap * 2.0).round().max(4.0) as u32;
        match ind.position {
            TabIndicatorPosition::Left => {
                rect.x += reserve as i32;
                rect.w = rect.w.saturating_sub(reserve).max(48);
            }
            TabIndicatorPosition::Right => {
                rect.w = rect.w.saturating_sub(reserve).max(48);
            }
            TabIndicatorPosition::Top => {
                rect.y += reserve as i32;
                rect.h = rect.h.saturating_sub(reserve).max(48);
            }
            TabIndicatorPosition::Bottom => {
                rect.h = rect.h.saturating_sub(reserve).max(48);
            }
        }
        rect
    }

    /// Computes screen-space `TabIndicatorSegment` pill rectangles for all visible `Tabbed` columns
    /// at camera offset `cam_x` (`niri/src/layout/tab_indicator.rs`).
    #[must_use]
    pub fn tab_indicator_segments_at(
        &self,
        viewport: LogicalViewport,
        cam_x: f64,
    ) -> Vec<TabIndicatorSegment> {
        let ind = &self.config.tab_indicator;
        if !ind.enabled || self.columns.is_empty() {
            return Vec::new();
        }

        let wa = viewport.work_area_rect();
        let og = f64::from(self.config.outer_gap);
        let left_og = f64::from(wa.x) + og;
        let top_og = f64::from(wa.y) + og;
        let usable_w = self.usable_width(viewport);
        let usable_h = f64::from(self.usable_height(viewport));
        let spans = self.column_spans(usable_w);
        let mut segments = Vec::new();

        for (col, (strip_x, col_w)) in self.columns.iter().zip(spans) {
            if col.display != ColumnDisplay::Tabbed {
                continue;
            }
            let n = col.tiles.len();
            if n == 0 || (ind.hide_when_single_tab && n < 2) {
                continue;
            }

            let screen_x = left_og + (strip_x - cam_x);
            let margin = ind.width + ind.gap + 4.0;
            if screen_x + col_w + margin <= f64::from(wa.x)
                || screen_x - margin >= f64::from(wa.x + wa.w as i32)
            {
                continue;
            }

            let active_idx = col.active_tile_idx.min(n - 1);
            let bar_thickness = ind.width.clamp(2.0, 24.0).round() as u32;
            let seg_gap = ind.gaps_between_tabs.max(0.0);

            match ind.position {
                TabIndicatorPosition::Left | TabIndicatorPosition::Right => {
                    let total_bar_h = (usable_h * ind.length_proportion.clamp(0.1, 1.0)).max(24.0);
                    let start_y = top_og + (usable_h - total_bar_h) * 0.5;
                    let total_gaps = seg_gap * ((n - 1) as f64);
                    let seg_h = ((total_bar_h - total_gaps) / (n as f64)).max(6.0);

                    let bar_x = match (ind.position, ind.place_within_column) {
                        (TabIndicatorPosition::Left, false) => screen_x - ind.gap - ind.width,
                        (TabIndicatorPosition::Left, true) => screen_x + ind.gap,
                        (TabIndicatorPosition::Right, false) => screen_x + col_w + ind.gap,
                        (TabIndicatorPosition::Right, true) => {
                            screen_x + col_w - ind.gap - ind.width
                        }
                        _ => unreachable!(),
                    };

                    for (idx, tile) in col.tiles.iter().enumerate() {
                        let sy = start_y + (idx as f64) * (seg_h + seg_gap);
                        segments.push(TabIndicatorSegment {
                            win_id: tile.win_id,
                            rect: LogicalRect::new(
                                bar_x.round() as i32,
                                sy.round() as i32,
                                bar_thickness,
                                seg_h.round().max(4.0) as u32,
                            ),
                            is_active: idx == active_idx,
                            corner_radius: ind.corner_radius,
                        });
                    }
                }
                TabIndicatorPosition::Top | TabIndicatorPosition::Bottom => {
                    let total_bar_w = (col_w * ind.length_proportion.clamp(0.1, 1.0)).max(24.0);
                    let start_x = screen_x + (col_w - total_bar_w) * 0.5;
                    let total_gaps = seg_gap * ((n - 1) as f64);
                    let seg_w = ((total_bar_w - total_gaps) / (n as f64)).max(6.0);

                    let bar_y = match (ind.position, ind.place_within_column) {
                        (TabIndicatorPosition::Top, false) => top_og - ind.gap - ind.width,
                        (TabIndicatorPosition::Top, true) => top_og + ind.gap,
                        (TabIndicatorPosition::Bottom, false) => top_og + usable_h + ind.gap,
                        (TabIndicatorPosition::Bottom, true) => {
                            top_og + usable_h - ind.gap - ind.width
                        }
                        _ => unreachable!(),
                    };

                    for (idx, tile) in col.tiles.iter().enumerate() {
                        let sx = start_x + (idx as f64) * (seg_w + seg_gap);
                        segments.push(TabIndicatorSegment {
                            win_id: tile.win_id,
                            rect: LogicalRect::new(
                                sx.round() as i32,
                                bar_y.round() as i32,
                                seg_w.round().max(4.0) as u32,
                                bar_thickness,
                            ),
                            is_active: idx == active_idx,
                            corner_radius: ind.corner_radius,
                        });
                    }
                }
            }
        }

        segments
    }

    /// Hit-tests a pointer click against visible `tab-indicator` pill segments (with a 3px ergonomic pad).
    #[must_use]
    pub fn hit_test_tab_indicator(
        &self,
        pt: LogicalPoint,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> Option<WindowId> {
        let cam_x = self.peek_camera_x(now_ms);
        for seg in self.tab_indicator_segments_at(viewport, cam_x) {
            let pad = 3.0;
            let left = f64::from(seg.rect.x) - pad;
            let right = f64::from(seg.rect.x + seg.rect.w as i32) + pad;
            let top = f64::from(seg.rect.y) - pad;
            let bottom = f64::from(seg.rect.y + seg.rect.h as i32) + pad;
            if pt.x >= left && pt.x <= right && pt.y >= top && pt.y <= bottom {
                return Some(seg.win_id);
            }
        }
        None
    }

    // =========================================================================
    // 3-Finger Touchpad Continuous Pixel Tracking & Velocity Projection
    // =========================================================================

    pub fn begin_view_offset_gesture(&mut self, now_ms: u64) -> f64 {
        let cur_visual_cam = self.sample_camera_x(now_ms);
        self.camera_x = cur_visual_cam;
        self.camera_spring.snap_to(cur_visual_cam);
        self.active_gesture = Some(ActiveViewOffsetGesture {
            start_camera_x: cur_visual_cam,
            start_active_idx: self.active_idx.min(self.columns.len().saturating_sub(1)),
        });
        cur_visual_cam
    }

    pub fn update_view_offset_gesture(
        &mut self,
        cumulative_touchpad_dx: f64,
        viewport: LogicalViewport,
    ) -> f64 {
        let Some(gesture) = self.active_gesture else {
            return self.camera_x;
        };
        if self.columns.is_empty() {
            self.camera_x = 0.0;
            self.camera_spring.snap_to(0.0);
            return 0.0;
        }

        let usable_w = self.usable_width(viewport);
        let scale = self.touchpad_to_strip_scale(viewport);
        let unclamped = gesture.start_camera_x + cumulative_touchpad_dx * scale;
        let (min_cam, max_cam) = self.camera_scroll_bounds(viewport);
        let max_overscroll = usable_w * 0.15;

        let live_cam = if unclamped < min_cam {
            let over = (min_cam - unclamped) * GESTURE_OVERSCROLL_RUBBER_BAND;
            min_cam - over.min(max_overscroll)
        } else if unclamped > max_cam {
            let over = (unclamped - max_cam) * GESTURE_OVERSCROLL_RUBBER_BAND;
            max_cam + over.min(max_overscroll)
        } else {
            unclamped
        };

        self.camera_x = live_cam;
        self.camera_spring.snap_to(live_cam);
        live_cam
    }

    pub fn end_view_offset_gesture(
        &mut self,
        touchpad_velocity_per_s: f64,
        cancelled: bool,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> Option<WindowId> {
        let gesture = self.active_gesture.take();
        if self.columns.is_empty() {
            self.active_idx = 0;
            self.camera_x = 0.0;
            self.camera_spring.snap_to(0.0);
            return None;
        }

        let cur_cam = self.camera_x;
        let start_idx = gesture
            .map_or(self.active_idx, |g| g.start_active_idx)
            .min(self.columns.len() - 1);

        if cancelled {
            self.active_idx = start_idx;
            let revert_cam = self.compute_camera_target_for_column(None, start_idx, viewport);
            self.camera_x = revert_cam;
            if self.instant_bypass {
                self.camera_spring.snap_to(revert_cam);
            } else {
                self.camera_spring.launch_from_velocity(
                    cur_cam,
                    revert_cam,
                    0.0,
                    now_ms,
                    self.config.camera_spring_params(),
                );
            }
            return self.active_window();
        }

        let scale = self.touchpad_to_strip_scale(viewport);
        let vel_px_s = if touchpad_velocity_per_s.is_finite() {
            touchpad_velocity_per_s * scale
        } else {
            0.0
        };
        let projected_cam = cur_cam + SwipeTracker::project_distance(vel_px_s);

        let (best_idx, snap_cam) =
            self.resolve_gesture_end_snap(start_idx, cur_cam, projected_cam, viewport);
        self.active_idx = best_idx;
        self.camera_x = snap_cam;

        if self.instant_bypass {
            self.camera_spring.snap_to(snap_cam);
        } else {
            self.camera_spring.launch_from_velocity(
                cur_cam,
                snap_cam,
                vel_px_s,
                now_ms,
                self.config.camera_spring_params(),
            );
        }

        self.active_window()
    }

    #[must_use]
    fn resolve_gesture_end_snap(
        &self,
        start_idx: usize,
        cur_cam: f64,
        projected_cam: f64,
        viewport: LogicalViewport,
    ) -> (usize, f64) {
        let usable_w = self.usable_width(viewport);
        let spans = self.column_spans(usable_w);
        let (min_cam, max_cam) = self.camera_scroll_bounds(viewport);
        let proj_clamped = projected_cam.clamp(min_cam, max_cam);

        let center_col = |cx: f64, cw: f64| -> f64 {
            if cw >= usable_w {
                cx
            } else {
                cx - (usable_w - cw) * 0.5
            }
        };

        if self.config.center_focused_column == CenterFocusedColumn::Always
            || (self.config.always_center_single_column && self.columns.len() == 1)
        {
            let mut best_i = start_idx;
            let mut best_snap = center_col(spans[start_idx].0, spans[start_idx].1);
            let mut best_dist = (proj_clamped - best_snap).abs();
            for (i, &(cx, cw)) in spans.iter().enumerate() {
                let s = center_col(cx, cw).clamp(min_cam, max_cam);
                let d = (proj_clamped - s).abs();
                if d < best_dist {
                    best_dist = d;
                    best_i = i;
                    best_snap = s;
                }
            }
            return (best_i, best_snap);
        }

        if self.columns.len() == 1 {
            let (_, cw) = spans[0];
            if cw < usable_w {
                let left_align = 0.0;
                let right_align = cw - usable_w;
                let snap = if (proj_clamped - right_align).abs() < (proj_clamped - left_align).abs()
                {
                    right_align
                } else {
                    left_align
                };
                return (0, snap);
            }
            return (0, 0.0);
        }

        let start_cam = spans
            .get(start_idx)
            .map_or(cur_cam, |&(sx, _)| sx.clamp(min_cam, max_cam));
        let swipe_delta = projected_cam - start_cam;
        let proj_view_center = proj_clamped + usable_w * 0.5;

        let mut best_choice = (start_idx, proj_clamped, f64::INFINITY, f64::INFINITY);
        for (i, &(cx, cw)) in spans.iter().enumerate() {
            let candidates: [f64; 2] = if cw >= usable_w {
                let s = cx.clamp(min_cam, max_cam);
                [s, s]
            } else {
                [
                    cx.clamp(min_cam, max_cam),
                    (cx + cw - usable_w).clamp(min_cam, max_cam),
                ]
            };

            let col_center = cx + cw * 0.5;
            let dir_bias = if (swipe_delta > 24.0 && i > start_idx)
                || (swipe_delta < -24.0 && i < start_idx)
            {
                -8.0
            } else {
                0.0
            };
            let center_tiebreak = (col_center - proj_view_center).abs() + dir_bias;

            for snap in candidates {
                let final_snap = if self.config.center_focused_column
                    == CenterFocusedColumn::OnOverflow
                    && i != start_idx
                {
                    let neighbor_idx = if start_idx > i {
                        (i + 1).min(spans.len() - 1)
                    } else {
                        i.saturating_sub(1)
                    };
                    let (nx, nw) = spans[neighbor_idx];
                    let combined = ((cx + cw).max(nx + nw) - cx.min(nx))
                        + f64::from(self.config.inner_gap) * 2.0;
                    if combined > usable_w {
                        center_col(cx, cw).clamp(min_cam, max_cam)
                    } else {
                        snap
                    }
                } else {
                    snap
                };

                let dist = (proj_clamped - final_snap).abs();
                if dist + 0.5 < best_choice.2
                    || ((dist - best_choice.2).abs() <= 0.5 && center_tiebreak < best_choice.3)
                {
                    best_choice = (i, final_snap, dist, center_tiebreak);
                }
            }
        }

        (best_choice.0, best_choice.1)
    }

    // =========================================================================
    // Column Lifecycle, 2D Drop-Zone Prediction & Interactive Resize
    // =========================================================================

    pub fn insert_column(
        &mut self,
        win_id: WindowId,
        explicit_width: Option<PresetColumnWidth>,
        viewport: LogicalViewport,
        now_ms: u64,
    ) {
        let _ = self.remove(win_id, viewport, now_ms);
        let prev_idx = (!self.columns.is_empty()).then_some(self.active_idx);
        let width = explicit_width.unwrap_or(self.config.default_column_width);
        let mut col = ScrollingColumn::new_single(win_id, width, &self.config.preset_column_widths);
        col.display = self.config.default_column_display;
        let insert_at = if self.columns.is_empty() {
            0
        } else {
            (self.active_idx + 1).min(self.columns.len())
        };
        self.columns.insert(insert_at, col);
        self.active_idx = insert_at;
        self.recompute_camera_for_active(prev_idx, viewport, now_ms);
    }

    /// Predicts a 2D drop zone during `MoveWindowGrab`:
    /// - Hovering inside the central `[25%..75%]` horizontal body of an existing column offers
    ///   `StackIntoColumn` (upper/lower vertical insertion slot with half-tile preview card!).
    /// - Hovering near column edges (`< 25%` or `> 75%`) or in inter-column space offers
    ///   `NewColumn` between columns!
    #[must_use]
    pub fn predict_drop_zone_2d(
        &self,
        cursor: LogicalPoint,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> ScrollingDropZone {
        let wa = viewport.work_area_rect();
        let og = self.config.outer_gap as i32;
        let origin_x = wa.x + og;
        let origin_y = wa.y + og;
        let usable_w = self.usable_width(viewport);
        let usable_h = self.usable_height(viewport);
        let preview_w = (self
            .config
            .default_column_width
            .resolve_px(usable_w)
            .round() as u32)
            .clamp(96, usable_w.round() as u32);

        if self.columns.is_empty() {
            let px = if self.config.center_focused_column == CenterFocusedColumn::Always
                || self.config.always_center_single_column
            {
                origin_x + ((usable_w.round() as i32 - preview_w as i32) / 2).max(0)
            } else {
                origin_x
            };
            return ScrollingDropZone {
                target: ScrollingDropTarget::NewColumn { insert_idx: 0 },
                preview_rect: LogicalRect::new(px, origin_y, preview_w, usable_h),
            };
        }

        let cam_now = self.peek_camera_x(now_ms);
        let spans = self.column_spans(usable_w);
        let gap = f64::from(self.config.inner_gap);

        // 1. Check if cursor is inside the central `[0.25..0.75]` X band of a visible column -> stack vertically!
        for (c_idx, &(strip_x, col_w)) in spans.iter().enumerate() {
            let screen_x = f64::from(origin_x) + (strip_x - cam_now);
            let inner_left = screen_x + col_w * 0.25;
            let inner_right = screen_x + col_w * 0.75;
            if cursor.x >= inner_left && cursor.x <= inner_right {
                let col = &self.columns[c_idx];
                let v_spans = col.compute_vertical_tile_spans(
                    self.config.outer_gap,
                    self.config.inner_gap,
                    usable_h,
                );
                for (t_idx, &(_, ty_rel, th)) in v_spans.iter().enumerate() {
                    let ty = wa.y + ty_rel;
                    let t_bottom = ty + th as i32;
                    if cursor.y <= f64::from(t_bottom) || t_idx + 1 == v_spans.len() {
                        let mid_y = f64::from(ty) + f64::from(th) * 0.5;
                        let half_h = (th / 2).max(48);
                        let (tile_insert_idx, py) = if cursor.y < mid_y {
                            (t_idx, ty)
                        } else {
                            (t_idx + 1, ty + (th as i32 - half_h as i32).max(0))
                        };
                        let max_cx = wa.x + (wa.w as i32 - 64).max(0);
                        let clamped_x = (screen_x.round() as i32).clamp(wa.x, max_cx.max(wa.x));
                        let avail_w = (wa.x + wa.w as i32 - clamped_x).max(64) as u32;
                        let clamped_w = (col_w.round() as u32).min(avail_w);
                        return ScrollingDropZone {
                            target: ScrollingDropTarget::StackIntoColumn {
                                col_idx: c_idx,
                                tile_insert_idx,
                            },
                            preview_rect: LogicalRect::new(clamped_x, py, clamped_w, half_h),
                        };
                    }
                }
            }
        }

        // 2. Otherwise insert as a standalone column at the nearest column boundary!
        let mut insert_idx = self.columns.len();
        let mut slot_screen_x = f64::from(origin_x)
            + spans.last().map_or(0.0, |&(lx, lw)| lx + lw + gap * 0.5)
            - cam_now;

        for (idx, &(strip_x, col_w)) in spans.iter().enumerate() {
            let screen_mid = f64::from(origin_x) + (strip_x - cam_now) + col_w * 0.5;
            if cursor.x < screen_mid {
                insert_idx = idx;
                slot_screen_x = f64::from(origin_x) + (strip_x - cam_now);
                break;
            }
        }

        let min_px = origin_x;
        let max_px = origin_x + (usable_w.round() as i32 - preview_w as i32).max(0);
        let clamped_px =
            (slot_screen_x.round() as i32 - (preview_w as i32 / 4)).clamp(min_px, max_px);

        ScrollingDropZone {
            target: ScrollingDropTarget::NewColumn { insert_idx },
            preview_rect: LogicalRect::new(clamped_px, origin_y, preview_w, usable_h),
        }
    }

    /// Backwards-compatible 1D wrapper delegating to `predict_drop_zone_2d` near top margin.
    #[must_use]
    pub fn predict_column_drop_zone(
        &self,
        cursor_x: f64,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> ScrollingDropZone {
        let mut dz = self.predict_drop_zone_2d(
            LogicalPoint {
                x: cursor_x,
                y: f64::from(self.config.outer_gap),
            },
            viewport,
            now_ms,
        );
        if let ScrollingDropTarget::StackIntoColumn { col_idx, .. } = dz.target {
            dz.target = ScrollingDropTarget::NewColumn {
                insert_idx: col_idx,
            };
        }
        dz
    }

    /// Inserts `win_id` into `ScrollingStrip` according to `drop_zone.target` (either as a new
    /// column or stacked vertically inside an existing column!).
    pub fn insert_column_at_drop_zone(
        &mut self,
        win_id: WindowId,
        drop_zone: ScrollingDropZone,
        explicit_width: Option<PresetColumnWidth>,
        viewport: LogicalViewport,
        now_ms: u64,
    ) {
        self.reset_edge_auto_scroll();
        let _ = self.remove(win_id, viewport, now_ms);
        let prev_idx = (!self.columns.is_empty()).then_some(self.active_idx);

        match drop_zone.target {
            ScrollingDropTarget::StackIntoColumn {
                col_idx,
                tile_insert_idx,
            } if col_idx < self.columns.len() => {
                let col = &mut self.columns[col_idx];
                let slot = tile_insert_idx.min(col.tiles.len());
                col.tiles.insert(slot, ScrollingTile::new_auto(win_id));
                col.active_tile_idx = slot;
                self.active_idx = col_idx;
                self.recompute_camera_for_active(prev_idx, viewport, now_ms);
            }
            ScrollingDropTarget::NewColumn { insert_idx }
            | ScrollingDropTarget::StackIntoColumn {
                col_idx: insert_idx,
                ..
            } => {
                let width = explicit_width.unwrap_or(self.config.default_column_width);
                let mut col =
                    ScrollingColumn::new_single(win_id, width, &self.config.preset_column_widths);
                col.display = self.config.default_column_display;
                let idx = insert_idx.min(self.columns.len());
                self.columns.insert(idx, col);
                self.active_idx = idx;
                self.recompute_camera_for_active(prev_idx, viewport, now_ms);
            }
        }
    }

    pub fn switch_preset_column_width(
        &mut self,
        forwards: bool,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> Option<PresetColumnWidth> {
        if self.columns.is_empty() || self.config.preset_column_widths.is_empty() {
            return None;
        }
        let usable_w = self.usable_width(viewport);
        let presets = &self.config.preset_column_widths;
        let len = presets.len();
        let col = &mut self.columns[self.active_idx];

        let effective_preset_idx = if col.is_maximized_column {
            col.is_maximized_column = false;
            None
        } else {
            col.preset_width_idx.filter(|&i| i < len)
        };

        let next_idx = match effective_preset_idx {
            Some(idx) => (idx + if forwards { 1 } else { len - 1 }) % len,
            None => {
                let cur_px = col.width.resolve_px(usable_w);
                if forwards {
                    presets
                        .iter()
                        .position(|p| cur_px + 1.0 < p.resolve_px(usable_w))
                        .unwrap_or(0)
                } else {
                    presets
                        .iter()
                        .rposition(|p| p.resolve_px(usable_w) + 1.0 < cur_px)
                        .unwrap_or(len - 1)
                }
            }
        };

        let chosen = presets[next_idx];
        col.width = chosen;
        col.preset_width_idx = Some(next_idx);
        self.recompute_camera_for_active(None, viewport, now_ms);
        Some(chosen)
    }

    pub fn toggle_maximize_column(
        &mut self,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> Option<bool> {
        let col = self.columns.get_mut(self.active_idx)?;
        col.is_maximized_column = !col.is_maximized_column;
        let state = col.is_maximized_column;
        self.recompute_camera_for_active(None, viewport, now_ms);
        Some(state)
    }

    pub fn center_active_column(&mut self, viewport: LogicalViewport, now_ms: u64) -> bool {
        if self.columns.is_empty() {
            return false;
        }
        let usable_w = self.usable_width(viewport);
        let spans = self.column_spans(usable_w);
        let (col_x, col_w) = spans[self.active_idx.min(spans.len() - 1)];
        let target = if col_w >= usable_w {
            col_x
        } else {
            col_x - (usable_w - col_w) * 0.5
        };
        self.set_camera_target(target, now_ms);
        true
    }

    pub fn move_column_left(&mut self, viewport: LogicalViewport, now_ms: u64) -> bool {
        if self.columns.len() < 2 || self.active_idx == 0 {
            return false;
        }
        let prev_idx = self.active_idx;
        self.columns.swap(self.active_idx, self.active_idx - 1);
        self.active_idx -= 1;
        self.recompute_camera_for_active(Some(prev_idx), viewport, now_ms);
        true
    }

    pub fn move_column_right(&mut self, viewport: LogicalViewport, now_ms: u64) -> bool {
        if self.columns.len() < 2 || self.active_idx + 1 >= self.columns.len() {
            return false;
        }
        let prev_idx = self.active_idx;
        self.columns.swap(self.active_idx, self.active_idx + 1);
        self.active_idx += 1;
        self.recompute_camera_for_active(Some(prev_idx), viewport, now_ms);
        true
    }

    /// Interactive pointer resize (`Mod + MouseRight`) supporting BOTH horizontal column width
    /// (`dx_px`) and intra-column vertical tile divider resizing (`dy_px`) when `tiles.len() >= 2`!
    pub fn adjust_tile_or_column_size(
        &mut self,
        win_id: WindowId,
        dx_px: f64,
        dy_px: f64,
        corner: ResizeCorner,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> bool {
        let usable_w = self.usable_width(viewport);
        let usable_h = self.usable_height(viewport);
        let Some((c_idx, t_idx)) = self.find_window(win_id) else {
            return false;
        };

        if dx_px.abs() > f64::EPSILON {
            let col = &mut self.columns[c_idx];
            let cur_px = col.effective_width().resolve_px(usable_w);
            let signed_dx = if corner.has_left() && !corner.has_right() {
                -dx_px
            } else {
                dx_px
            };
            let next_px = (cur_px + signed_dx).clamp(96.0, usable_w);
            col.is_maximized_column = false;
            col.preset_width_idx = None;
            col.width = match col.width {
                PresetColumnWidth::Proportion(_) => {
                    PresetColumnWidth::Proportion((next_px / usable_w).clamp(0.1, 1.0))
                }
                PresetColumnWidth::Fixed(_) => PresetColumnWidth::Fixed(next_px.round() as u32),
            };
        }

        if dy_px.abs() > f64::EPSILON && self.columns[c_idx].tiles.len() >= 2 {
            let col = &mut self.columns[c_idx];
            let n = col.tiles.len();
            let v_spans = col.compute_vertical_tile_spans(
                self.config.outer_gap,
                self.config.inner_gap,
                usable_h,
            );
            let gap_total = self.config.inner_gap.saturating_mul((n - 1) as u32);
            let avail_h = f64::from(usable_h.saturating_sub(gap_total).max((n as u32) * 48));

            // Identify adjacent neighbor across the dragged horizontal divider
            let neighbor_opt = if corner.has_top() && !corner.has_bottom() {
                if t_idx > 0 {
                    Some((t_idx - 1, -dy_px))
                } else {
                    Some((t_idx + 1, dy_px))
                }
            } else if t_idx + 1 < n {
                Some((t_idx + 1, dy_px))
            } else {
                Some((t_idx - 1, -dy_px))
            };

            if let Some((nb_idx, signed_dy)) = neighbor_opt {
                let cur_h = f64::from(v_spans[t_idx].2);
                let nb_h = f64::from(v_spans[nb_idx].2);
                let pair_total = cur_h + nb_h;
                let min_h = 48.0_f64.min(pair_total * 0.45);
                let new_cur_h = (cur_h + signed_dy).clamp(min_h, (pair_total - min_h).max(min_h));
                let new_nb_h = (pair_total - new_cur_h).max(min_h);

                col.tiles[t_idx].height = TileHeight::Preset(PresetColumnWidth::Proportion(
                    (new_cur_h / avail_h).clamp(0.1, 0.9),
                ));
                col.tiles[t_idx].preset_height_idx = None;
                col.tiles[nb_idx].height = TileHeight::Preset(PresetColumnWidth::Proportion(
                    (new_nb_h / avail_h).clamp(0.1, 0.9),
                ));
                col.tiles[nb_idx].preset_height_idx = None;
            }
        }

        let saved_bypass = self.instant_bypass;
        self.instant_bypass = true;
        self.recompute_camera_for_active(None, viewport, now_ms);
        self.instant_bypass = saved_bypass;
        true
    }

    pub fn adjust_column_width(
        &mut self,
        win_id: WindowId,
        dx_px: f64,
        corner: ResizeCorner,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> bool {
        self.adjust_tile_or_column_size(win_id, dx_px, 0.0, corner, viewport, now_ms)
    }

    pub fn remove(
        &mut self,
        win_id: WindowId,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> Option<WindowId> {
        let (c_idx, t_idx) = self.find_window(win_id)?;
        self.columns[c_idx].tiles.remove(t_idx);

        if self.columns[c_idx].tiles.is_empty() {
            self.columns.remove(c_idx);
            if self.columns.is_empty() {
                self.active_idx = 0;
                self.set_camera_target(0.0, now_ms);
                return None;
            }
            if c_idx < self.active_idx || self.active_idx >= self.columns.len() {
                self.active_idx = self
                    .active_idx
                    .saturating_sub(1)
                    .min(self.columns.len() - 1);
            }
        } else {
            if t_idx < self.columns[c_idx].active_tile_idx {
                self.columns[c_idx].active_tile_idx =
                    self.columns[c_idx].active_tile_idx.saturating_sub(1);
            }
            self.columns[c_idx].clamp_active_tile_idx();
        }

        self.recompute_camera_for_active(None, viewport, now_ms);
        self.active_window()
    }

    pub fn focus_prev(&mut self, viewport: LogicalViewport, now_ms: u64) -> Option<WindowId> {
        if self.columns.is_empty() {
            return None;
        }
        let prev = self.active_idx;
        if self.active_idx > 0 {
            self.active_idx -= 1;
        }
        self.recompute_camera_for_active(Some(prev), viewport, now_ms);
        self.active_window()
    }

    pub fn focus_next(&mut self, viewport: LogicalViewport, now_ms: u64) -> Option<WindowId> {
        if self.columns.is_empty() {
            return None;
        }
        let prev = self.active_idx;
        if self.active_idx + 1 < self.columns.len() {
            self.active_idx += 1;
        }
        self.recompute_camera_for_active(Some(prev), viewport, now_ms);
        self.active_window()
    }

    pub fn activate(&mut self, win_id: WindowId, viewport: LogicalViewport, now_ms: u64) -> bool {
        let Some((c_idx, t_idx)) = self.find_window(win_id) else {
            return false;
        };
        let prev = self.active_idx;
        self.active_idx = c_idx;
        self.columns[c_idx].active_tile_idx = t_idx;
        self.recompute_camera_for_active(Some(prev), viewport, now_ms);
        true
    }

    pub fn ensure_active_visible(&mut self, viewport: LogicalViewport, now_ms: u64) {
        if self.active_gesture.is_none() {
            self.recompute_camera_for_active(None, viewport, now_ms);
        }
    }

    pub fn reset_edge_auto_scroll(&mut self) {
        self.edge_scroll = EdgeScrollRuntime::default();
    }

    #[must_use]
    pub const fn is_edge_scroll_armed(&self) -> bool {
        self.edge_scroll.hover_start_ms.is_some()
    }

    pub fn tick_edge_auto_scroll(
        &mut self,
        cursor_x: f64,
        viewport: LogicalViewport,
        now_ms: u64,
    ) -> EdgeScrollStep {
        let cfg = &self.config.edge_auto_scroll;
        if !cfg.enabled || self.columns.is_empty() {
            self.reset_edge_auto_scroll();
            return EdgeScrollStep::Inactive;
        }

        let work_x = f64::from(self.config.outer_gap);
        let work_w = self.usable_width(viewport);
        let tw = cfg.trigger_width.clamp(1.0, work_w * 0.5);
        let local_x = (cursor_x - work_x).clamp(0.0, work_w);

        let raw_penetration = if local_x < tw {
            -(tw - local_x)
        } else if work_w - local_x < tw {
            tw - (work_w - local_x)
        } else {
            0.0
        };

        if raw_penetration.abs() <= f64::EPSILON {
            self.reset_edge_auto_scroll();
            return EdgeScrollStep::Inactive;
        }

        let norm = (raw_penetration / tw).clamp(-1.0, 1.0);
        let (min_cam, max_cam) = self.camera_scroll_bounds(viewport);
        if (norm < 0.0 && self.camera_x <= min_cam + 0.5)
            || (norm > 0.0 && self.camera_x >= max_cam - 0.5)
        {
            self.reset_edge_auto_scroll();
            return EdgeScrollStep::AtBoundary;
        }

        let hover_start = *self.edge_scroll.hover_start_ms.get_or_insert(now_ms);
        if now_ms.saturating_sub(hover_start) < cfg.delay_ms {
            self.edge_scroll.last_tick_ms = Some(now_ms);
            return EdgeScrollStep::Debouncing;
        }

        let prev_ms = self
            .edge_scroll
            .last_tick_ms
            .replace(now_ms)
            .unwrap_or(now_ms);
        let elapsed_ms = now_ms.saturating_sub(prev_ms);
        let dt_s = if elapsed_ms == 0 {
            1.0 / 60.0
        } else {
            (elapsed_ms.min(100) as f64) / 1000.0
        };

        let delta_px = norm * cfg.max_speed * dt_s;
        let next_cam = (self.camera_x + delta_px).clamp(min_cam, max_cam);
        self.camera_x = next_cam;
        self.camera_spring.snap_to(next_cam);
        EdgeScrollStep::Scrolled(next_cam)
    }

    #[must_use]
    pub fn peek_camera_x(&self, now_ms: u64) -> f64 {
        if self.instant_bypass || self.camera_spring.is_settled() {
            return self.camera_x;
        }
        let mut copy = self.camera_spring;
        copy.sample(now_ms)
    }

    pub fn sample_camera_x(&mut self, now_ms: u64) -> f64 {
        if self.instant_bypass {
            self.camera_spring.snap_to(self.camera_x);
            return self.camera_x;
        }
        self.camera_spring.sample(now_ms)
    }

    #[must_use]
    pub fn is_camera_animating(&self, now_ms: u64) -> bool {
        if self.instant_bypass || self.camera_spring.is_settled() {
            return false;
        }
        let mut copy = self.camera_spring;
        let _ = copy.sample(now_ms);
        !copy.is_settled()
    }

    /// Returns **strip-space** `(WindowId, LogicalRect)` for ALL tiles across ALL columns
    /// (`x` before subtracting `camera_x`).
    #[must_use]
    pub fn strip_geometries(&self, viewport: LogicalViewport) -> Vec<(WindowId, LogicalRect)> {
        if self.columns.is_empty() {
            return Vec::new();
        }
        let wa = viewport.work_area_rect();
        let og = self.config.outer_gap as i32;
        let usable_w = self.usable_width(viewport);
        let usable_h = self.usable_height(viewport);
        let spans = self.column_spans(usable_w);
        let mut out = Vec::with_capacity(self.window_count());

        for (col, (strip_x, col_w)) in self.columns.iter().zip(spans) {
            let cx = wa.x + og + strip_x.round() as i32;
            let cw = col_w.round() as u32;
            for (win_id, ty, th) in col.compute_vertical_tile_spans(
                self.config.outer_gap,
                self.config.inner_gap,
                usable_h,
            ) {
                let r = self
                    .apply_within_column_tab_inset(col, LogicalRect::new(cx, wa.y + ty, cw, th));
                out.push((win_id, r));
            }
        }
        out
    }

    /// Computes visible `(WindowId, LogicalRect)` tiles intersecting `viewport` at arbitrary `cam_x`.
    #[must_use]
    pub fn visible_geometries_at(
        &self,
        viewport: LogicalViewport,
        cam_x: f64,
    ) -> Vec<(WindowId, LogicalRect)> {
        let mut out = Vec::new();
        if self.columns.is_empty() {
            return out;
        }
        let wa = viewport.work_area_rect();
        let og = self.config.outer_gap as i32;
        let usable_w = self.usable_width(viewport);
        let usable_h = self.usable_height(viewport);
        let spans = self.column_spans(usable_w);

        for (col, (strip_x, col_w)) in self.columns.iter().zip(spans) {
            let screen_x = f64::from(wa.x + og) + (strip_x - cam_x);
            if screen_x + col_w > 0.0 && screen_x < f64::from(viewport.width) {
                let rx = screen_x.round() as i32;
                let rw = col_w.round() as u32;
                for (win_id, ty, th) in col.compute_vertical_tile_spans(
                    self.config.outer_gap,
                    self.config.inner_gap,
                    usable_h,
                ) {
                    let r = self.apply_within_column_tab_inset(
                        col,
                        LogicalRect::new(rx, wa.y + ty, rw, th),
                    );
                    out.push((win_id, r));
                }
            }
        }
        out
    }

    #[must_use]
    pub fn visible_geometries(&self, viewport: LogicalViewport) -> Vec<(WindowId, LogicalRect)> {
        self.visible_geometries_at(viewport, self.camera_x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp_1920() -> LogicalViewport {
        LogicalViewport::new(1920, 1080, 1.0)
    }

    #[test]
    fn consume_and_expel_weave_windows_cleanly_across_columns() {
        let mut strip = ScrollingStrip::new(ScrollingConfig::default(), true);
        let vp = vp_1920();
        strip.insert_column(WindowId::new(1, 1), None, vp, 0);
        strip.insert_column(WindowId::new(2, 1), None, vp, 0);
        strip.insert_column(WindowId::new(3, 1), None, vp, 0);
        // Active is WindowId::new(3, 1) alone in col 2: [1] [2] [3]
        assert_eq!(strip.len(), 3);
        assert_eq!(strip.active_window(), Some(WindowId::new(3, 1)));

        // 1. consume_or_expel_window_left on lone [3] -> consumed into bottom of [2]: [1] [2, 3]
        assert!(strip.consume_or_expel_window_left(vp, 10));
        assert_eq!(strip.len(), 2);
        assert_eq!(strip.columns[1].len(), 2);
        assert_eq!(strip.active_window(), Some(WindowId::new(3, 1)));

        // Verify vertical tile split geometry inside column 1 (2 equal tiles separated by 8px gap)
        let rects = strip.visible_geometries(vp);
        let r2 = rects
            .iter()
            .find(|(w, _)| *w == WindowId::new(2, 1))
            .unwrap()
            .1;
        let r3 = rects
            .iter()
            .find(|(w, _)| *w == WindowId::new(3, 1))
            .unwrap()
            .1;
        assert_eq!(r2.x, r3.x);
        assert_eq!(r2.y + (r2.h as i32) + (strip.gap_px() as i32), r3.y);

        // 2. consume_or_expel_window_left again on multi-tile [2, 3] -> expels [3] left: [1] [3] [2]
        assert!(strip.consume_or_expel_window_left(vp, 20));
        assert_eq!(strip.len(), 3);
        assert_eq!(strip.columns[1].active_win_id(), Some(WindowId::new(3, 1)));
        assert_eq!(strip.columns[2].active_win_id(), Some(WindowId::new(2, 1)));

        // 3. Explicit consume_window_into_column on [3] -> pulls [2] from right into [3, 2]!
        assert!(strip.consume_window_into_column(vp, 30));
        assert_eq!(strip.len(), 2);
        assert_eq!(strip.columns[1].len(), 2);
        // Focus stays on [3] during consume_window_into_column
        assert_eq!(strip.active_window(), Some(WindowId::new(3, 1)));

        // 4. Vertical focus_down -> [2], move_tile_up -> swaps to [2, 3], then expel_window_from_column!
        assert_eq!(strip.focus_down(), Some(WindowId::new(2, 1)));
        assert!(strip.move_tile_up());
        assert_eq!(strip.columns[1].tiles[0].win_id, WindowId::new(2, 1));
        assert!(strip.expel_window_from_column(vp, 40));
        assert_eq!(strip.len(), 3);
        assert_eq!(strip.active_window(), Some(WindowId::new(2, 1)));
    }

    #[test]
    fn preset_window_heights_and_reset_distribute_vertical_space_accurately() {
        let mut strip = ScrollingStrip::new(ScrollingConfig::default(), true);
        let vp = vp_1920();
        strip.insert_column(WindowId::new(1, 1), None, vp, 0);
        strip.insert_column(WindowId::new(2, 1), None, vp, 0);
        // Focus col 0 and consume col 1 into it -> [1, 2]
        strip.focus_prev(vp, 0);
        assert!(strip.consume_window_into_column(vp, 0));

        // Cycle top window height preset -> 2/3 (since current is ~0.5)
        let h_preset = strip.switch_preset_window_height(true, vp).unwrap();
        assert!(h_preset.approx_eq(PresetColumnWidth::Proportion(2.0 / 3.0)));
        let rects = strip.visible_geometries(vp);
        let r1 = rects
            .iter()
            .find(|(w, _)| *w == WindowId::new(1, 1))
            .unwrap()
            .1;
        let r2 = rects
            .iter()
            .find(|(w, _)| *w == WindowId::new(2, 1))
            .unwrap()
            .1;
        assert!(r1.h > r2.h + 200);

        // Reset height back to Auto 50/50 split
        assert!(strip.reset_window_height());
        let rects_after = strip.visible_geometries(vp);
        let a1 = rects_after
            .iter()
            .find(|(w, _)| *w == WindowId::new(1, 1))
            .unwrap()
            .1;
        let a2 = rects_after
            .iter()
            .find(|(w, _)| *w == WindowId::new(2, 1))
            .unwrap()
            .1;
        assert!((a1.h as i32 - a2.h as i32).abs() <= 2);
    }

    #[test]
    fn continuous_three_finger_gesture_tracks_1_to_1_and_projects_velocity_into_column_snap() {
        let mut strip = ScrollingStrip::new(ScrollingConfig::default(), false);
        let vp = vp_1920();
        for id in 1..=4 {
            strip.insert_column(
                WindowId::new(id, 1),
                Some(PresetColumnWidth::Proportion(0.5)),
                vp,
                0,
            );
        }
        strip.active_idx = 0;
        strip.camera_x = 0.0;
        strip.camera_spring.snap_to(0.0);

        let _ = strip.begin_view_offset_gesture(200);
        let live_cam = strip.update_view_offset_gesture(150.0, vp);
        assert!((live_cam - 237.5).abs() < 2.0);

        let snapped_win = strip.end_view_offset_gesture(2600.0, false, vp, 240);
        assert!(
            snapped_win == Some(WindowId::new(3, 1)) || snapped_win == Some(WindowId::new(4, 1))
        );
        assert!(strip.is_camera_animating(248));
    }

    #[test]
    fn toggle_column_tabbed_display_and_indicator_pills_work() {
        let mut strip = ScrollingStrip::new(ScrollingConfig::default(), true);
        let vp = vp_1920();
        strip.insert_column(WindowId::new(1, 1), None, vp, 0);
        strip.insert_column(WindowId::new(2, 1), None, vp, 0);
        assert!(strip.consume_or_expel_window_left(vp, 0));
        assert_eq!(strip.columns[0].len(), 2);

        // Toggle into Tabbed mode -> only active window (WindowId::new(2, 1)) is emitted at full column height
        assert_eq!(
            strip.toggle_column_tabbed_display(),
            Some(ColumnDisplay::Tabbed)
        );
        let vis = strip.visible_geometries(vp);
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].0, WindowId::new(2, 1));
        assert_eq!(vis[0].1.h, vp.height - strip.outer_gap_px() * 2);

        // Two tab-indicator pill segments should be generated on the left edge
        let segs = strip.tab_indicator_segments_at(vp, strip.camera_x);
        assert_eq!(segs.len(), 2);
        assert!(!segs[0].is_active);
        assert!(segs[1].is_active);

        // Clicking first pill hits WindowId::new(1, 1)
        let click_pt = LogicalPoint {
            x: f64::from(segs[0].rect.x) + 1.0,
            y: f64::from(segs[0].rect.y) + 4.0,
        };
        assert_eq!(
            strip.hit_test_tab_indicator(click_pt, vp, 0),
            Some(WindowId::new(1, 1))
        );
    }
}
