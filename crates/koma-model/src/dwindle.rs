//! Hyprland-faithful `Dwindle` dynamic binary space partitioning tree (`Tier 0`).
//!
//! Ported directly from `Hyprland/src/layout/algorithm/tiled/dwindle/DwindleAlgorithm.{hpp,cpp}`
//! and `Hyprland/src/layout/supplementary/DragController.cpp` into a safe, leak-free Rust arena
//! (`BTreeMap<DwindleNodeId, DwindleNode>`):
//! - **Dynamic vs Preserved Splits**: By default (`preserve_split = false`), split orientation
//!   recomputes dynamically from `box.h * split_width_multiplier > box.w`. Setting
//!   `preserve_split = true`, `smart_split = true`, or invoking `toggle_split` locks a split node.
//! - **4-Triangle `smart_split` & Interactive Drop Zone**: Divides any hovered tile along its two
//!   diagonals (`|dy * w| < |dx * h|`) so cursor quadrant selects Left, Right, Top, or Bottom
//!   placement (`DwindleAlgorithm.cpp` lines 179-206), with live half-tile `DropZonePreview`
//!   during `Mod + MouseLeft` interactive move grabs!
//! - **`max_size` Auto-Float Guard**: Before splitting `opening_on`, compares client `xdg_toplevel`
//!   `max_size` against the predicted child tile (`DwindleAlgorithm.cpp` lines 104-110); constrained
//!   dialogs/utilities automatically downgrade to centered `FloatingStack` windows.
//! - **Corner-Aware `smart_resizing` & `pseudotile`**: Supports nearest-quadrant `ResizeCorner`
//!   (`DragController.cpp` lines 204-223) outer + inner ancestor `split_ratio` (`[0.1, 1.9]`)
//!   adjustments (`DwindleAlgorithm.cpp` lines 295-378) and centering `pseudotile` windows.

use std::collections::BTreeMap;

use koma_proto::WindowId;
use serde::{Deserialize, Serialize};

use crate::geometry::{LogicalPoint, LogicalRect, LogicalViewport};

pub const MIN_SPLIT_RATIO: f64 = 0.1;
pub const MAX_SPLIT_RATIO: f64 = 1.9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DwindleNodeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardinalDir {
    Left,
    Right,
    Up,
    Down,
}

impl CardinalDir {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "l" | "left" | "west" => Some(Self::Left),
            "r" | "right" | "east" => Some(Self::Right),
            "u" | "up" | "top" | "north" => Some(Self::Up),
            "d" | "dn" | "down" | "bottom" | "south" => Some(Self::Down),
            _ => None,
        }
    }
}

/// Grabbed window corner or edge for interactive `Mod + MouseRight` / `xdg_toplevel.resize`
/// (`Hyprland` `DragController.cpp` lines 204-223 & `eRectCorner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeCorner {
    None,
    TopLeft,
    TopRight,
    BottomRight,
    BottomLeft,
    Left,
    Right,
    Top,
    Bottom,
}

impl ResizeCorner {
    /// Resolves the nearest corner quadrant of `rect` relative to `cursor`
    /// (`Hyprland/src/layout/supplementary/DragController.cpp` lines 204-223).
    #[must_use]
    pub fn nearest_quadrant(rect: LogicalRect, cursor: LogicalPoint) -> Self {
        let mid_x = f64::from(rect.x) + f64::from(rect.w) * 0.5;
        let mid_y = f64::from(rect.y) + f64::from(rect.h) * 0.5;
        match (cursor.x < mid_x, cursor.y < mid_y) {
            (true, true) => Self::TopLeft,
            (false, true) => Self::TopRight,
            (false, false) => Self::BottomRight,
            (true, false) => Self::BottomLeft,
        }
    }

    #[must_use]
    pub const fn has_left(self) -> bool {
        matches!(self, Self::TopLeft | Self::BottomLeft | Self::Left)
    }

    #[must_use]
    pub const fn has_right(self) -> bool {
        matches!(self, Self::TopRight | Self::BottomRight | Self::Right)
    }

    #[must_use]
    pub const fn has_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopRight | Self::Top)
    }

    #[must_use]
    pub const fn has_bottom(self) -> bool {
        matches!(self, Self::BottomLeft | Self::BottomRight | Self::Bottom)
    }

    /// Resolves the grab edge/corner for a pointer at `cursor` within `threshold` logical pixels of
    /// the outer frame `rect`, or `None` when the pointer is not on the frame.
    ///
    /// Corners win over edges, so a 6x6 corner still resizes diagonally. When the frame is thinner
    /// than twice the threshold the nearer edge wins instead of an ambiguous two-edge match. A point
    /// outside the frame by more than `threshold` is never a resize target, so the client area and
    /// the surrounding gaps stay click-through.
    #[must_use]
    pub fn frame_edge_at(rect: LogicalRect, cursor: LogicalPoint, threshold: f64) -> Option<Self> {
        if rect.w == 0 || rect.h == 0 {
            return None;
        }
        let t = threshold.max(1.0);
        let left = f64::from(rect.x);
        let top = f64::from(rect.y);
        let right = left + f64::from(rect.w);
        let bottom = top + f64::from(rect.h);
        if cursor.x < left - t
            || cursor.x > right + t
            || cursor.y < top - t
            || cursor.y > bottom + t
        {
            return None;
        }

        let d_left = (cursor.x - left).abs();
        let d_right = (cursor.x - right).abs();
        let d_top = (cursor.y - top).abs();
        let d_bottom = (cursor.y - bottom).abs();
        let horizontal = match (d_left <= t, d_right <= t) {
            (true, true) if d_left <= d_right => Some(true),
            (true, true) => Some(false),
            (true, false) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
        };
        let vertical = match (d_top <= t, d_bottom <= t) {
            (true, true) if d_top <= d_bottom => Some(true),
            (true, true) => Some(false),
            (true, false) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
        };

        match (horizontal, vertical) {
            (Some(true), Some(true)) => Some(Self::TopLeft),
            (Some(false), Some(true)) => Some(Self::TopRight),
            (Some(true), Some(false)) => Some(Self::BottomLeft),
            (Some(false), Some(false)) => Some(Self::BottomRight),
            (Some(true), None) => Some(Self::Left),
            (Some(false), None) => Some(Self::Right),
            (None, Some(true)) => Some(Self::Top),
            (None, Some(false)) => Some(Self::Bottom),
            (None, None) => None,
        }
    }
}

/// Live 4-quadrant `smart_split` drop zone computed while hovering `DwindleTree` tiles during
/// an interactive `Mod + MouseLeft` window drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropZonePreview {
    /// Hovered target leaf window (`None` when dropping onto an empty `DwindleTree`).
    pub target_window: Option<WindowId>,
    /// Resolved 4-triangle diagonal split direction (`Left`, `Right`, `Up`, `Down`).
    pub split_dir: CardinalDir,
    /// Exact gapped visual preview rectangle where the dropped window will land.
    pub preview_rect: LogicalRect,
}

/// Configuration parameters mirroring Hyprland's `dwindle { ... }` section.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DwindleConfig {
    /// `0` = split follows mouse cursor half, `1` = always left/top, `2` = always right/bottom.
    pub force_split: u8,
    /// Keep split orientation (`split_top`) fixed regardless of container aspect changes.
    pub preserve_split: bool,
    /// Divide hovered tile into 4 diagonal triangles to choose exact split edge from cursor position.
    pub smart_split: bool,
    /// Split the focused tiled window instead of the tile under the mouse cursor.
    pub use_active_for_splits: bool,
    /// Initial `split_ratio` in `[0.1, 1.9]` (`1.0` = 50% / 50%).
    pub default_split_ratio: f64,
    /// Aspect-ratio multiplier in `w > h * split_width_multiplier`.
    pub split_width_multiplier: f64,
    /// Adjust both outer and inner split ratios when resizing (`dwindle:smart_resizing`).
    pub smart_resizing: bool,
    /// Allow `pseudotile` windows to retain their preferred floating size centered inside their tile.
    pub pseudotile: bool,
    /// Inner gap (logical px) between sibling tiles.
    pub inner_gap: u32,
    /// Outer margin (logical px) around the workspace perimeter.
    pub outer_gap: u32,
}

impl Default for DwindleConfig {
    fn default() -> Self {
        Self {
            force_split: 0,
            preserve_split: false,
            smart_split: false,
            use_active_for_splits: false,
            default_split_ratio: 1.0,
            split_width_multiplier: 1.0,
            smart_resizing: true,
            pseudotile: true,
            inner_gap: 6,
            outer_gap: 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DwindleInsertOutcome {
    /// Inserted into the binary tiling tree; returns the newly assigned leaf `LogicalRect`.
    Tiled(LogicalRect),
    /// Client `max_size` cannot fit the predicted tile (`DwindleAlgorithm.cpp` lines 104-110),
    /// so the caller should place it on the `FloatingStack` at `centered_rect`.
    DowngradeToFloating { centered_rect: LogicalRect },
}

#[derive(Debug, Clone)]
enum DwindleNodeKind {
    Split {
        children: [DwindleNodeId; 2],
        /// `false` = side-by-side (Left / Right), `true` = top-and-bottom (Top / Bottom).
        split_top: bool,
        /// `1.0` = equal 50%/50% split; clamped to `[0.1, 1.9]`.
        split_ratio: f64,
        /// Locked when created via `smart_split`, `override_dir`, or `toggle_split`.
        locked_split: bool,
    },
    Leaf {
        win_id: WindowId,
        /// Optional preferred `(width, height)` when `pseudotile` is toggled on for this window.
        pseudotile_size: Option<(u32, u32)>,
    },
}

#[derive(Debug, Clone)]
struct DwindleNode {
    parent: Option<DwindleNodeId>,
    raw_box: LogicalRect,
    visual_box: LogicalRect,
    kind: DwindleNodeKind,
}

/// Arena-allocated binary space partitioning tree implementing Hyprland's `CDwindleAlgorithm`.
#[derive(Debug, Clone)]
pub struct DwindleTree {
    pub config: DwindleConfig,
    next_id: u64,
    root: Option<DwindleNodeId>,
    nodes: BTreeMap<DwindleNodeId, DwindleNode>,
    win_to_leaf: BTreeMap<WindowId, DwindleNodeId>,
    leaf_min_sizes: BTreeMap<WindowId, (u32, u32)>,
}

impl Default for DwindleTree {
    fn default() -> Self {
        Self::new(DwindleConfig::default())
    }
}

impl DwindleTree {
    #[must_use]
    pub fn new(config: DwindleConfig) -> Self {
        Self {
            config,
            next_id: 1,
            root: None,
            nodes: BTreeMap::new(),
            win_to_leaf: BTreeMap::new(),
            leaf_min_sizes: BTreeMap::new(),
        }
    }

    /// Records (or clears when `(0, 0)`) the client-advertised minimum visual size `(min_w, min_h)`
    /// for `win_id` so subsequent `recalculate` passes yield slack from sibling subtrees.
    pub fn set_window_min_size(&mut self, win_id: WindowId, min_w: u32, min_h: u32) -> bool {
        if min_w == 0 && min_h == 0 {
            return self.leaf_min_sizes.remove(&win_id).is_some();
        }
        match self.leaf_min_sizes.get(&win_id) {
            Some(&cur) if cur == (min_w, min_h) => false,
            _ => {
                self.leaf_min_sizes.insert(win_id, (min_w, min_h));
                true
            }
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.win_to_leaf.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.win_to_leaf.len()
    }

    #[must_use]
    pub fn contains(&self, win_id: WindowId) -> bool {
        self.win_to_leaf.contains_key(&win_id)
    }

    #[must_use]
    pub fn rect_of(&self, win_id: WindowId) -> Option<LogicalRect> {
        let leaf_id = *self.win_to_leaf.get(&win_id)?;
        self.nodes.get(&leaf_id).map(|n| n.visual_box)
    }

    #[must_use]
    pub fn raw_rect_of(&self, win_id: WindowId) -> Option<LogicalRect> {
        let leaf_id = *self.win_to_leaf.get(&win_id)?;
        self.nodes.get(&leaf_id).map(|n| n.raw_box)
    }

    #[must_use]
    pub fn pseudotile_size_of(&self, win_id: WindowId) -> Option<(u32, u32)> {
        let leaf_id = *self.win_to_leaf.get(&win_id)?;
        match self.nodes.get(&leaf_id)?.kind {
            DwindleNodeKind::Leaf {
                pseudotile_size, ..
            } => pseudotile_size,
            DwindleNodeKind::Split { .. } => None,
        }
    }

    #[must_use]
    pub fn first_window(&self) -> Option<WindowId> {
        let root_id = self.root?;
        self.leftmost_leaf_under(root_id)
    }

    /// Returns all tiled `(WindowId, LogicalRect)` pairs in deterministic tree order.
    #[must_use]
    pub fn tiled_geometries(&self) -> Vec<(WindowId, LogicalRect)> {
        let mut out = Vec::with_capacity(self.win_to_leaf.len());
        if let Some(root_id) = self.root {
            self.collect_geometries_recursive(root_id, &mut out);
        }
        out
    }

    fn collect_geometries_recursive(
        &self,
        node_id: DwindleNodeId,
        out: &mut Vec<(WindowId, LogicalRect)>,
    ) {
        let Some(node) = self.nodes.get(&node_id) else {
            return;
        };
        match &node.kind {
            DwindleNodeKind::Split { children, .. } => {
                self.collect_geometries_recursive(children[0], out);
                self.collect_geometries_recursive(children[1], out);
            }
            DwindleNodeKind::Leaf { win_id, .. } => {
                out.push((*win_id, node.visual_box));
            }
        }
    }

    fn alloc_node_id(&mut self) -> DwindleNodeId {
        let id = DwindleNodeId(self.next_id);
        self.next_id += 1;
        id
    }

    fn work_area(&self, viewport: LogicalViewport) -> LogicalRect {
        let wa = viewport.work_area_rect();
        let og = self.config.outer_gap;
        let w = wa.w.saturating_sub(og.saturating_mul(2)).max(64);
        let h = wa.h.saturating_sub(og.saturating_mul(2)).max(64);
        let max_x = wa.x.saturating_add((wa.w as i32 - w as i32).max(0));
        let max_y = wa.y.saturating_add((wa.h as i32 - h as i32).max(0));
        LogicalRect::new(
            (wa.x + og as i32).min(max_x),
            (wa.y + og as i32).min(max_y),
            w,
            h,
        )
    }

    /// Predicts the exact 4-quadrant `smart_split` drop zone (`Left`, `Right`, `Up`, `Down`) and
    /// gapped preview rectangle for a cursor hovering at `cursor` (`DwindleAlgorithm.cpp` lines 179-206).
    #[must_use]
    pub fn predict_drop_zone(
        &self,
        cursor: LogicalPoint,
        viewport: LogicalViewport,
    ) -> DropZonePreview {
        let work = self.work_area(viewport);
        let Some(leaf_id) = self.leaf_at_or_closest(cursor) else {
            return DropZonePreview {
                target_window: None,
                split_dir: CardinalDir::Right,
                preview_rect: work,
            };
        };
        let Some(node) = self.nodes.get(&leaf_id) else {
            return DropZonePreview {
                target_window: None,
                split_dir: CardinalDir::Right,
                preview_rect: work,
            };
        };
        let DwindleNodeKind::Leaf { win_id, .. } = node.kind else {
            return DropZonePreview {
                target_window: None,
                split_dir: CardinalDir::Right,
                preview_rect: work,
            };
        };

        let split_dir = four_triangle_quadrant(node.raw_box, cursor);
        let vis = node.visual_box;
        let half_gap = self.config.inner_gap / 2;
        let ratio = self
            .config
            .default_split_ratio
            .clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);

        let preview_rect = match split_dir {
            CardinalDir::Left => {
                let w = ((f64::from(vis.w) * 0.5 * ratio).round() as u32)
                    .saturating_sub(half_gap)
                    .clamp(32, vis.w.max(32));
                LogicalRect::new(vis.x, vis.y, w, vis.h)
            }
            CardinalDir::Right => {
                let first_w = ((f64::from(vis.w) * 0.5 * ratio).round() as u32)
                    .clamp(32, vis.w.saturating_sub(32).max(32));
                let w = vis
                    .w
                    .saturating_sub(first_w)
                    .saturating_sub(half_gap)
                    .max(32);
                let x = vis.x + vis.w as i32 - w as i32;
                LogicalRect::new(x, vis.y, w, vis.h)
            }
            CardinalDir::Up => {
                let h = ((f64::from(vis.h) * 0.5 * ratio).round() as u32)
                    .saturating_sub(half_gap)
                    .clamp(32, vis.h.max(32));
                LogicalRect::new(vis.x, vis.y, vis.w, h)
            }
            CardinalDir::Down => {
                let first_h = ((f64::from(vis.h) * 0.5 * ratio).round() as u32)
                    .clamp(32, vis.h.saturating_sub(32).max(32));
                let h = vis
                    .h
                    .saturating_sub(first_h)
                    .saturating_sub(half_gap)
                    .max(32);
                let y = vis.y + vis.h as i32 - h as i32;
                LogicalRect::new(vis.x, y, vis.w, h)
            }
        };

        DropZonePreview {
            target_window: Some(win_id),
            split_dir,
            preview_rect,
        }
    }

    /// Commits an interactive `Mod + MouseLeft` drop onto `drop_zone`, splitting the hovered
    /// target leaf explicitly along `drop_zone.split_dir` regardless of `use_active_for_splits`.
    pub fn insert_at_drop_zone(
        &mut self,
        win_id: WindowId,
        drop_zone: DropZonePreview,
        viewport: LogicalViewport,
    ) -> LogicalRect {
        if let Some(existing) = self.rect_of(win_id) {
            return existing;
        }
        let target_leaf_id = drop_zone
            .target_window
            .and_then(|tw| self.win_to_leaf.get(&tw).copied())
            .or_else(|| self.root.and_then(|r| self.first_leaf_id_under(r)));

        self.insert_on_leaf_id(
            win_id,
            target_leaf_id,
            viewport,
            None,
            Some(drop_zone.split_dir),
        )
    }

    /// Inserts `win_id` into the Dwindle tree (`CDwindleAlgorithm::addTarget`), enforcing the
    /// `max_size` auto-float check (`DwindleAlgorithm.cpp` lines 104-110), `smart_split` 4-triangle
    /// cursor slope, `force_split`, and `use_active_for_splits`.
    pub fn insert(
        &mut self,
        win_id: WindowId,
        viewport: LogicalViewport,
        focused_win: Option<WindowId>,
        cursor_pos: Option<LogicalPoint>,
        max_size: Option<(u32, u32)>,
        override_dir: Option<CardinalDir>,
    ) -> DwindleInsertOutcome {
        if let Some(existing) = self.rect_of(win_id) {
            return DwindleInsertOutcome::Tiled(existing);
        }

        let work = self.work_area(viewport);

        // Find `opening_on` target leaf (`DwindleAlgorithm.cpp` lines 81-101)
        let opening_on_id = if !self.config.use_active_for_splits {
            if let Some(pt) = cursor_pos {
                self.leaf_at_or_closest(pt)
            } else {
                focused_win
                    .and_then(|fw| self.win_to_leaf.get(&fw).copied())
                    .or_else(|| self.root.and_then(|r| self.first_leaf_id_under(r)))
            }
        } else {
            focused_win
                .and_then(|fw| self.win_to_leaf.get(&fw).copied())
                .or_else(|| cursor_pos.and_then(|pt| self.leaf_at_or_closest(pt)))
                .or_else(|| self.root.and_then(|r| self.first_leaf_id_under(r)))
        };

        // Predict tile dimensions to validate client `xdg_toplevel` `max_size` (`DwindleAlgorithm.cpp` lines 104-110)
        let predicted_size = match opening_on_id.and_then(|id| self.nodes.get(&id)) {
            None => (work.w, work.h),
            Some(target_leaf) => {
                let b = target_leaf.raw_box;
                let mult = self.config.split_width_multiplier.max(0.1);
                let side_by_side = f64::from(b.w) > f64::from(b.h) * mult;
                if side_by_side {
                    ((b.w / 2).max(32), b.h)
                } else {
                    (b.w, (b.h / 2).max(32))
                }
            }
        };

        if let Some((max_w, max_h)) = max_size {
            let violates_w = max_w > 0 && max_w < predicted_size.0;
            let violates_h = max_h > 0 && max_h < predicted_size.1;
            if violates_w || violates_h {
                let fw = if max_w > 0 {
                    max_w.min(work.w).max(64)
                } else {
                    (work.w * 3 / 5).max(120)
                };
                let fh = if max_h > 0 {
                    max_h.min(work.h).max(64)
                } else {
                    (work.h * 3 / 5).max(120)
                };
                let cx = work.x + ((work.w.saturating_sub(fw)) / 2) as i32;
                let cy = work.y + ((work.h.saturating_sub(fh)) / 2) as i32;
                return DwindleInsertOutcome::DowngradeToFloating {
                    centered_rect: LogicalRect::new(cx, cy, fw, fh),
                };
            }
        }

        DwindleInsertOutcome::Tiled(self.insert_on_leaf_id(
            win_id,
            opening_on_id,
            viewport,
            cursor_pos,
            override_dir,
        ))
    }

    fn insert_on_leaf_id(
        &mut self,
        win_id: WindowId,
        opening_on_id: Option<DwindleNodeId>,
        viewport: LogicalViewport,
        cursor_pos: Option<LogicalPoint>,
        override_dir: Option<CardinalDir>,
    ) -> LogicalRect {
        let work = self.work_area(viewport);

        // Empty tree fast-path: first tiled window occupies the entire work area!
        let Some(target_leaf_id) = opening_on_id else {
            let leaf_id = self.alloc_node_id();
            self.nodes.insert(
                leaf_id,
                DwindleNode {
                    parent: None,
                    raw_box: work,
                    visual_box: work,
                    kind: DwindleNodeKind::Leaf {
                        win_id,
                        pseudotile_size: None,
                    },
                },
            );
            self.root = Some(leaf_id);
            self.win_to_leaf.insert(win_id, leaf_id);
            return work;
        };

        let target_box = self.nodes[&target_leaf_id].raw_box;
        let old_parent = self.nodes[&target_leaf_id].parent;

        let new_leaf_id = self.alloc_node_id();
        let split_parent_id = self.alloc_node_id();

        let mult = self.config.split_width_multiplier.max(0.1);
        let default_side_by_side = f64::from(target_box.w) > f64::from(target_box.h) * mult;
        let mut split_top = !default_side_by_side;
        let mut locked_split = self.config.preserve_split;
        let new_first: bool;

        let resolved_dir = override_dir.or_else(|| {
            if self.config.smart_split {
                let cursor = cursor_pos.unwrap_or(LogicalPoint {
                    x: f64::from(target_box.x) + f64::from(target_box.w) * 0.75,
                    y: f64::from(target_box.y) + f64::from(target_box.h) * 0.75,
                });
                Some(four_triangle_quadrant(target_box, cursor))
            } else {
                None
            }
        });

        if let Some(dir) = resolved_dir {
            locked_split = true;
            match dir {
                CardinalDir::Left => {
                    split_top = false;
                    new_first = true;
                }
                CardinalDir::Right => {
                    split_top = false;
                    new_first = false;
                }
                CardinalDir::Up => {
                    split_top = true;
                    new_first = true;
                }
                CardinalDir::Down => {
                    split_top = true;
                    new_first = false;
                }
            }
        } else if self.config.force_split == 0 {
            if let Some(cursor) = cursor_pos {
                if default_side_by_side {
                    new_first = cursor.x < f64::from(target_box.x) + f64::from(target_box.w) * 0.5;
                } else {
                    new_first = cursor.y < f64::from(target_box.y) + f64::from(target_box.h) * 0.5;
                }
            } else {
                new_first = false;
            }
        } else {
            new_first = self.config.force_split == 1;
        }

        let children = if new_first {
            [new_leaf_id, target_leaf_id]
        } else {
            [target_leaf_id, new_leaf_id]
        };

        let ratio = self
            .config
            .default_split_ratio
            .clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);

        self.nodes.insert(
            new_leaf_id,
            DwindleNode {
                parent: Some(split_parent_id),
                raw_box: target_box,
                visual_box: target_box,
                kind: DwindleNodeKind::Leaf {
                    win_id,
                    pseudotile_size: None,
                },
            },
        );
        self.nodes.insert(
            split_parent_id,
            DwindleNode {
                parent: old_parent,
                raw_box: target_box,
                visual_box: target_box,
                kind: DwindleNodeKind::Split {
                    children,
                    split_top,
                    split_ratio: ratio,
                    locked_split,
                },
            },
        );

        if let Some(target_node) = self.nodes.get_mut(&target_leaf_id) {
            target_node.parent = Some(split_parent_id);
        }

        if let Some(parent_id) = old_parent {
            if let Some(parent_node) = self.nodes.get_mut(&parent_id)
                && let DwindleNodeKind::Split { children: pc, .. } = &mut parent_node.kind
            {
                if pc[0] == target_leaf_id {
                    pc[0] = split_parent_id;
                } else if pc[1] == target_leaf_id {
                    pc[1] = split_parent_id;
                }
            }
        } else {
            self.root = Some(split_parent_id);
        }

        self.win_to_leaf.insert(win_id, new_leaf_id);
        self.recalculate(viewport);
        self.rect_of(win_id).unwrap_or(work)
    }

    /// Removes `win_id` from the binary tree (`CDwindleAlgorithm::removeTarget`), collapsing its
    /// parent split node and returning the closest surviving neighbor `WindowId` for focus handoff.
    pub fn remove(&mut self, win_id: WindowId, viewport: LogicalViewport) -> Option<WindowId> {
        let leaf_id = self.win_to_leaf.remove(&win_id)?;
        let removed_node = self.nodes.remove(&leaf_id)?;
        let center = rect_center(removed_node.visual_box);

        let Some(parent_id) = removed_node.parent else {
            self.root = None;
            return None;
        };

        let parent_node = self.nodes.remove(&parent_id)?;
        let DwindleNodeKind::Split { children, .. } = parent_node.kind else {
            return None;
        };
        let sibling_id = if children[0] == leaf_id {
            children[1]
        } else {
            children[0]
        };

        if let Some(sibling_node) = self.nodes.get_mut(&sibling_id) {
            sibling_node.parent = parent_node.parent;
        }

        if let Some(grand_id) = parent_node.parent {
            if let Some(grand_node) = self.nodes.get_mut(&grand_id)
                && let DwindleNodeKind::Split { children: gc, .. } = &mut grand_node.kind
            {
                if gc[0] == parent_id {
                    gc[0] = sibling_id;
                } else if gc[1] == parent_id {
                    gc[1] = sibling_id;
                }
            }
        } else {
            self.root = Some(sibling_id);
        }

        self.recalculate(viewport);
        self.leaf_at_or_closest(center)
            .and_then(|id| match self.nodes.get(&id)?.kind {
                DwindleNodeKind::Leaf { win_id: w, .. } => Some(w),
                DwindleNodeKind::Split { .. } => None,
            })
    }

    /// Recomputes all raw & gapped tile rectangles across `viewport` (`SDwindleNodeData::recalcSizePosRecursive`).
    pub fn recalculate(&mut self, viewport: LogicalViewport) {
        let Some(root_id) = self.root else {
            return;
        };
        let work = self.work_area(viewport);
        if let Some(root_node) = self.nodes.get_mut(&root_id) {
            root_node.raw_box = work;
            root_node.visual_box = work;
        }
        self.recalc_node_recursive(root_id, work, false, false, false, false);
        if !self.leaf_min_sizes.is_empty() {
            // Second pass lets nested nodes settle `split_top` for their resized aspect ratio so
            // bottom-up `subtree_min_size` reads accurate child orientations.
            self.recalc_node_recursive(root_id, work, false, false, false, false);
        }
    }

    fn gap_insets(
        &self,
        gap_left: bool,
        gap_right: bool,
        gap_top: bool,
        gap_bottom: bool,
    ) -> (i32, i32, i32, i32) {
        let inner = self.config.inner_gap;
        let half_gap = (inner / 2) as i32;
        let rem_gap = (inner - inner / 2) as i32;
        (
            if gap_left { half_gap } else { 0 },
            if gap_right { rem_gap } else { 0 },
            if gap_top { half_gap } else { 0 },
            if gap_bottom { rem_gap } else { 0 },
        )
    }

    /// Computes bottom-up minimum raw dimensions `(min_raw_w, min_raw_h)` required so no leaf in
    /// `node_id`'s subtree falls below its advertised client minimum after subtracting inner gaps.
    fn subtree_min_size(
        &self,
        node_id: DwindleNodeId,
        gap_left: bool,
        gap_right: bool,
        gap_top: bool,
        gap_bottom: bool,
    ) -> (u32, u32) {
        let Some(node) = self.nodes.get(&node_id) else {
            return (32, 32);
        };
        match &node.kind {
            DwindleNodeKind::Leaf { win_id, .. } => {
                let (mw, mh) = self.leaf_min_sizes.get(win_id).copied().unwrap_or((0, 0));
                let (il, ir, it, ib) = self.gap_insets(gap_left, gap_right, gap_top, gap_bottom);
                let req_w = if mw > 0 {
                    mw.saturating_add((il + ir) as u32).max(32)
                } else {
                    32
                };
                let req_h = if mh > 0 {
                    mh.saturating_add((it + ib) as u32).max(32)
                } else {
                    32
                };
                (req_w, req_h)
            }
            DwindleNodeKind::Split {
                children,
                split_top,
                ..
            } => {
                let [c0, c1] = *children;
                if !*split_top {
                    let (w0, h0) = self.subtree_min_size(c0, gap_left, true, gap_top, gap_bottom);
                    let (w1, h1) = self.subtree_min_size(c1, true, gap_right, gap_top, gap_bottom);
                    (w0.saturating_add(w1), h0.max(h1))
                } else {
                    let (w0, h0) = self.subtree_min_size(c0, gap_left, gap_right, gap_top, true);
                    let (w1, h1) = self.subtree_min_size(c1, gap_left, gap_right, true, gap_bottom);
                    (w0.max(w1), h0.saturating_add(h1))
                }
            }
        }
    }

    fn recalc_node_recursive(
        &mut self,
        node_id: DwindleNodeId,
        raw_box: LogicalRect,
        gap_left: bool,
        gap_right: bool,
        gap_top: bool,
        gap_bottom: bool,
    ) {
        let cfg = self.config;
        let (inset_left, inset_right, inset_top, inset_bottom) =
            self.gap_insets(gap_left, gap_right, gap_top, gap_bottom);

        let vis_w = (raw_box.w as i32 - inset_left - inset_right).max(32) as u32;
        let vis_h = (raw_box.h as i32 - inset_top - inset_bottom).max(32) as u32;
        let cell_visual =
            LogicalRect::new(raw_box.x + inset_left, raw_box.y + inset_top, vis_w, vis_h);

        let split_info = {
            let Some(node) = self.nodes.get_mut(&node_id) else {
                return;
            };
            node.raw_box = raw_box;

            match &mut node.kind {
                DwindleNodeKind::Leaf {
                    pseudotile_size, ..
                } => {
                    if cfg.pseudotile
                        && let Some((pw, ph)) = *pseudotile_size
                    {
                        let cw = pw.clamp(48, cell_visual.w);
                        let ch = ph.clamp(48, cell_visual.h);
                        let cx = cell_visual.x + ((cell_visual.w - cw) / 2) as i32;
                        let cy = cell_visual.y + ((cell_visual.h - ch) / 2) as i32;
                        node.visual_box = LogicalRect::new(cx, cy, cw, ch);
                        return;
                    }
                    node.visual_box = cell_visual;
                    return;
                }
                DwindleNodeKind::Split {
                    children,
                    split_top,
                    split_ratio,
                    locked_split,
                } => {
                    node.visual_box = cell_visual;
                    if !cfg.preserve_split && !*locked_split && !cfg.smart_split {
                        let mult = cfg.split_width_multiplier.max(0.1);
                        *split_top = f64::from(raw_box.h) * mult > f64::from(raw_box.w);
                    }
                    (
                        *split_top,
                        split_ratio.clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO),
                        *children,
                    )
                }
            }
        };

        let (is_top, ratio, [c0, c1]) = split_info;
        if !is_top {
            let desired = ((f64::from(raw_box.w) * 0.5 * ratio).round() as i32)
                .clamp(32, (raw_box.w as i32 - 32).max(32)) as u32;
            let first_w = if self.leaf_min_sizes.is_empty() {
                desired
            } else {
                let (min_w0, _) = self.subtree_min_size(c0, gap_left, true, gap_top, gap_bottom);
                let (min_w1, _) = self.subtree_min_size(c1, true, gap_right, gap_top, gap_bottom);
                distribute_split_axis(raw_box.w, desired, min_w0, min_w1)
            };
            let second_w = raw_box.w.saturating_sub(first_w).max(32);
            let box0 = LogicalRect::new(raw_box.x, raw_box.y, first_w, raw_box.h);
            let box1 = LogicalRect::new(raw_box.x + first_w as i32, raw_box.y, second_w, raw_box.h);
            self.recalc_node_recursive(c0, box0, gap_left, true, gap_top, gap_bottom);
            self.recalc_node_recursive(c1, box1, true, gap_right, gap_top, gap_bottom);
        } else {
            let desired = ((f64::from(raw_box.h) * 0.5 * ratio).round() as i32)
                .clamp(32, (raw_box.h as i32 - 32).max(32)) as u32;
            let first_h = if self.leaf_min_sizes.is_empty() {
                desired
            } else {
                let (_, min_h0) = self.subtree_min_size(c0, gap_left, gap_right, gap_top, true);
                let (_, min_h1) = self.subtree_min_size(c1, gap_left, gap_right, true, gap_bottom);
                distribute_split_axis(raw_box.h, desired, min_h0, min_h1)
            };
            let second_h = raw_box.h.saturating_sub(first_h).max(32);
            let box0 = LogicalRect::new(raw_box.x, raw_box.y, raw_box.w, first_h);
            let box1 = LogicalRect::new(raw_box.x, raw_box.y + first_h as i32, raw_box.w, second_h);
            self.recalc_node_recursive(c0, box0, gap_left, gap_right, gap_top, true);
            self.recalc_node_recursive(c1, box1, gap_left, gap_right, true, gap_bottom);
        }
    }

    /// Directional geometric focus lookup (`movefocus l/r/u/d`).
    #[must_use]
    pub fn focus_in_direction(&self, from_win: WindowId, dir: CardinalDir) -> Option<WindowId> {
        let leaf_id = *self.win_to_leaf.get(&from_win)?;
        let b = self.nodes.get(&leaf_id)?.raw_box;
        let probe = focal_probe_point(b, dir);
        let hit_id = self.leaf_at_or_closest_excluding(probe, Some(leaf_id))?;
        match self.nodes.get(&hit_id)?.kind {
            DwindleNodeKind::Leaf { win_id, .. } => Some(win_id),
            DwindleNodeKind::Split { .. } => None,
        }
    }

    /// Swaps two adjacent tiled windows in direction `dir` (`swapwindow l/r/u/d`).
    pub fn swap_in_direction(
        &mut self,
        from_win: WindowId,
        dir: CardinalDir,
        viewport: LogicalViewport,
    ) -> bool {
        let Some(target_win) = self.focus_in_direction(from_win, dir) else {
            return false;
        };
        let Some(&leaf_a) = self.win_to_leaf.get(&from_win) else {
            return false;
        };
        let Some(&leaf_b) = self.win_to_leaf.get(&target_win) else {
            return false;
        };

        let pseudo_a = match self.nodes.get(&leaf_a).map(|n| &n.kind) {
            Some(DwindleNodeKind::Leaf {
                pseudotile_size, ..
            }) => *pseudotile_size,
            _ => None,
        };
        let pseudo_b = match self.nodes.get(&leaf_b).map(|n| &n.kind) {
            Some(DwindleNodeKind::Leaf {
                pseudotile_size, ..
            }) => *pseudotile_size,
            _ => None,
        };

        if let Some(na) = self.nodes.get_mut(&leaf_a) {
            na.kind = DwindleNodeKind::Leaf {
                win_id: target_win,
                pseudotile_size: pseudo_b,
            };
        }
        if let Some(nb) = self.nodes.get_mut(&leaf_b) {
            nb.kind = DwindleNodeKind::Leaf {
                win_id: from_win,
                pseudotile_size: pseudo_a,
            };
        }
        self.win_to_leaf.insert(from_win, leaf_b);
        self.win_to_leaf.insert(target_win, leaf_a);
        self.recalculate(viewport);
        true
    }

    /// Moves `win_id` structurally across the binary tree in direction `dir`
    /// (`CDwindleAlgorithm::moveTargetInDirection`, `DwindleAlgorithm.cpp` lines 569-615).
    pub fn move_in_direction(
        &mut self,
        win_id: WindowId,
        dir: CardinalDir,
        viewport: LogicalViewport,
    ) -> bool {
        if self.win_to_leaf.len() <= 1 {
            return false;
        }
        let Some(&leaf_id) = self.win_to_leaf.get(&win_id) else {
            return false;
        };
        let cur_box = self.nodes[&leaf_id].raw_box;
        let pseudo_saved = match self.nodes[&leaf_id].kind {
            DwindleNodeKind::Leaf {
                pseudotile_size, ..
            } => pseudotile_size,
            DwindleNodeKind::Split { .. } => None,
        };

        // Check immediate sibling split boundary (`DwindleAlgorithm.cpp` lines 587-596)
        let mut override_dir = None;
        if let Some(parent_id) = self.nodes[&leaf_id].parent
            && let DwindleNodeKind::Split {
                children,
                split_top,
                ..
            } = self.nodes[&parent_id].kind
        {
            let sibling_id = if children[0] == leaf_id {
                children[1]
            } else {
                children[0]
            };
            let sibling_is_leaf = matches!(
                self.nodes.get(&sibling_id).map(|n| &n.kind),
                Some(DwindleNodeKind::Leaf { .. })
            );
            if sibling_is_leaf {
                let crosses_immediate =
                    (dir == CardinalDir::Up && split_top && children[1] == leaf_id)
                        || (dir == CardinalDir::Down && split_top && children[0] == leaf_id)
                        || (dir == CardinalDir::Left && !split_top && children[1] == leaf_id)
                        || (dir == CardinalDir::Right && !split_top && children[0] == leaf_id);
                if crosses_immediate {
                    override_dir = Some(dir);
                }
            }
        }

        let probe = focal_probe_point(cur_box, dir);
        self.remove(win_id, viewport);
        let _ = self.insert(win_id, viewport, None, Some(probe), None, override_dir);
        if let Some(ps) = pseudo_saved {
            let _ = self.set_pseudotile(win_id, Some(ps), viewport);
        }
        true
    }

    /// Toggles the parent split orientation (`togglesplit`: Top/Bottom <-> Left/Right) and locks it.
    pub fn toggle_split(&mut self, win_id: WindowId, viewport: LogicalViewport) -> bool {
        let Some(&leaf_id) = self.win_to_leaf.get(&win_id) else {
            return false;
        };
        let Some(parent_id) = self.nodes.get(&leaf_id).and_then(|n| n.parent) else {
            return false;
        };
        if let Some(parent_node) = self.nodes.get_mut(&parent_id)
            && let DwindleNodeKind::Split {
                split_top,
                locked_split,
                ..
            } = &mut parent_node.kind
        {
            *split_top = !*split_top;
            *locked_split = true;
            self.recalculate(viewport);
            return true;
        }
        false
    }

    /// Swaps the two halves of `win_id`'s immediate parent split (`swapsplit`).
    pub fn swap_split(&mut self, win_id: WindowId, viewport: LogicalViewport) -> bool {
        let Some(&leaf_id) = self.win_to_leaf.get(&win_id) else {
            return false;
        };
        let Some(parent_id) = self.nodes.get(&leaf_id).and_then(|n| n.parent) else {
            return false;
        };
        if let Some(parent_node) = self.nodes.get_mut(&parent_id)
            && let DwindleNodeKind::Split { children, .. } = &mut parent_node.kind
        {
            children.swap(0, 1);
            self.recalculate(viewport);
            return true;
        }
        false
    }

    /// Adjusts `split_ratio` on `win_id`'s parent split by `delta` (`splitratio +/-delta`, clamped to `[0.1, 1.9]`).
    pub fn adjust_split_ratio(
        &mut self,
        win_id: WindowId,
        delta: f64,
        viewport: LogicalViewport,
    ) -> bool {
        let Some(&leaf_id) = self.win_to_leaf.get(&win_id) else {
            return false;
        };
        let Some(parent_id) = self.nodes.get(&leaf_id).and_then(|n| n.parent) else {
            return false;
        };
        if let Some(parent_node) = self.nodes.get_mut(&parent_id)
            && let DwindleNodeKind::Split { split_ratio, .. } = &mut parent_node.kind
        {
            *split_ratio = (*split_ratio + delta).clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
            self.recalculate(viewport);
            return true;
        }
        false
    }

    /// Corner-aware interactive split-ratio resize (`Mod + MouseRight` & CSD `xdg_toplevel.resize`),
    /// ported directly from `CDwindleAlgorithm::resizeTarget` (`DwindleAlgorithm.cpp` lines 295-378).
    ///
    /// When `smart_resizing` is enabled, identifies both **outer** and **inner** horizontal/vertical
    /// split ancestors matching the grabbed `ResizeCorner` (and workspace perimeter stickiness),
    /// adjusting the outer split divider 1:1 with `(dx_px, dy_px)` while compensating any inner
    /// sibling split so non-grabbed edges remain anchored!
    pub fn resize_target_from_corner(
        &mut self,
        win_id: WindowId,
        dx_px: f64,
        dy_px: f64,
        corner: ResizeCorner,
        viewport: LogicalViewport,
    ) -> bool {
        if corner == ResizeCorner::None {
            return self.resize_target(win_id, dx_px, dy_px, viewport);
        }
        let Some(&leaf_id) = self.win_to_leaf.get(&win_id) else {
            return false;
        };
        let Some(leaf_box) = self.nodes.get(&leaf_id).map(|n| n.raw_box) else {
            return false;
        };

        let work = self.work_area(viewport);
        let sticks = |a: i32, b: i32| (a - b).abs() <= 2;
        let display_left = sticks(leaf_box.x, work.x);
        let display_right = sticks(leaf_box.x + leaf_box.w as i32, work.x + work.w as i32);
        let display_top = sticks(leaf_box.y, work.y);
        let display_bottom = sticks(leaf_box.y + leaf_box.h as i32, work.y + work.h as i32);

        let allowed_dx = if display_left && display_right {
            0.0
        } else {
            dx_px
        };
        let allowed_dy = if display_top && display_bottom {
            0.0
        } else {
            dy_px
        };
        if allowed_dx.abs() <= f64::EPSILON && allowed_dy.abs() <= f64::EPSILON {
            return false;
        }

        if !self.config.smart_resizing {
            let signed_w = if corner.has_left() && !corner.has_right() {
                -allowed_dx
            } else {
                allowed_dx
            };
            let signed_h = if corner.has_top() && !corner.has_bottom() {
                -allowed_dy
            } else {
                allowed_dy
            };
            return self.resize_target(win_id, signed_w, signed_h, viewport);
        }

        let left = corner.has_left() || display_right;
        let right = corner.has_right() || display_left;
        let top = corner.has_top() || display_bottom;
        let bottom = corner.has_bottom() || display_top;

        // `(child_node_id, parent_split_id)`
        let mut h_outer: Option<(DwindleNodeId, DwindleNodeId)> = None;
        let mut h_inner: Option<(DwindleNodeId, DwindleNodeId)> = None;
        let mut v_outer: Option<(DwindleNodeId, DwindleNodeId)> = None;
        let mut v_inner: Option<(DwindleNodeId, DwindleNodeId)> = None;

        let mut cur = leaf_id;
        while let Some(parent_id) = self.nodes.get(&cur).and_then(|n| n.parent) {
            if let Some(DwindleNode {
                kind:
                    DwindleNodeKind::Split {
                        children,
                        split_top,
                        ..
                    },
                ..
            }) = self.nodes.get(&parent_id)
            {
                if *split_top {
                    if v_outer.is_none()
                        && ((top && children[1] == cur) || (bottom && children[0] == cur))
                    {
                        v_outer = Some((cur, parent_id));
                    } else if v_outer.is_none() && v_inner.is_none() {
                        v_inner = Some((cur, parent_id));
                    }
                } else if h_outer.is_none()
                    && ((left && children[1] == cur) || (right && children[0] == cur))
                {
                    h_outer = Some((cur, parent_id));
                } else if h_outer.is_none() && h_inner.is_none() {
                    h_inner = Some((cur, parent_id));
                }
            }
            if h_outer.is_some() && v_outer.is_some() {
                break;
            }
            cur = parent_id;
        }

        let mut changed = false;

        if allowed_dx.abs() > f64::EPSILON
            && let Some((_, outer_pid)) = h_outer
        {
            let orig_inner_w = h_inner
                .and_then(|(in_cid, _)| self.nodes.get(&in_cid))
                .map(|n| f64::from(n.raw_box.w));

            if let Some(pnode) = self.nodes.get_mut(&outer_pid) {
                let pw = f64::from(pnode.raw_box.w.max(64));
                if let DwindleNodeKind::Split { split_ratio, .. } = &mut pnode.kind {
                    *split_ratio = (*split_ratio + allowed_dx * 2.0 / pw)
                        .clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
                    changed = true;
                }
            }
            self.recalculate(viewport);

            if let (Some((in_cid, in_pid)), Some(orig_w)) = (h_inner, orig_inner_w) {
                if let Some(in_pnode) = self.nodes.get_mut(&in_pid) {
                    let in_pw = f64::from(in_pnode.raw_box.w.max(64));
                    if let DwindleNodeKind::Split {
                        children,
                        split_ratio,
                        ..
                    } = &mut in_pnode.kind
                    {
                        let compensated = if children[0] == in_cid {
                            (orig_w - allowed_dx) / in_pw * 2.0
                        } else {
                            2.0 - (orig_w + allowed_dx) / in_pw * 2.0
                        };
                        *split_ratio = compensated.clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
                    }
                }
                self.recalculate(viewport);
            }
        }

        if allowed_dy.abs() > f64::EPSILON
            && let Some((_, outer_pid)) = v_outer
        {
            let orig_inner_h = v_inner
                .and_then(|(in_cid, _)| self.nodes.get(&in_cid))
                .map(|n| f64::from(n.raw_box.h));

            if let Some(pnode) = self.nodes.get_mut(&outer_pid) {
                let ph = f64::from(pnode.raw_box.h.max(64));
                if let DwindleNodeKind::Split { split_ratio, .. } = &mut pnode.kind {
                    *split_ratio = (*split_ratio + allowed_dy * 2.0 / ph)
                        .clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
                    changed = true;
                }
            }
            self.recalculate(viewport);

            if let (Some((in_cid, in_pid)), Some(orig_h)) = (v_inner, orig_inner_h) {
                if let Some(in_pnode) = self.nodes.get_mut(&in_pid) {
                    let in_ph = f64::from(in_pnode.raw_box.h.max(64));
                    if let DwindleNodeKind::Split {
                        children,
                        split_ratio,
                        ..
                    } = &mut in_pnode.kind
                    {
                        let compensated = if children[0] == in_cid {
                            (orig_h - allowed_dy) / in_ph * 2.0
                        } else {
                            2.0 - (orig_h + allowed_dy) / in_ph * 2.0
                        };
                        *split_ratio = compensated.clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
                    }
                }
                self.recalculate(viewport);
            }
        }

        changed
    }

    /// Resizes `win_id` by `(dx_px, dy_px)` across orthogonal horizontal & vertical ancestor splits
    /// (`CDwindleAlgorithm::resizeTarget` with `CORNER_NONE` for keyboard `WindowResizeDelta`).
    pub fn resize_target(
        &mut self,
        win_id: WindowId,
        dx_px: f64,
        dy_px: f64,
        viewport: LogicalViewport,
    ) -> bool {
        let Some(&leaf_id) = self.win_to_leaf.get(&win_id) else {
            return false;
        };
        let mut h_split_ancestor: Option<(DwindleNodeId, bool)> = None;
        let mut v_split_ancestor: Option<(DwindleNodeId, bool)> = None;

        let mut cur = leaf_id;
        while let Some(parent_id) = self.nodes.get(&cur).and_then(|n| n.parent) {
            if let Some(DwindleNode {
                kind:
                    DwindleNodeKind::Split {
                        children,
                        split_top,
                        ..
                    },
                ..
            }) = self.nodes.get(&parent_id)
            {
                let is_first_child = children[0] == cur;
                if !*split_top && h_split_ancestor.is_none() {
                    h_split_ancestor = Some((parent_id, is_first_child));
                } else if *split_top && v_split_ancestor.is_none() {
                    v_split_ancestor = Some((parent_id, is_first_child));
                }
            }
            if h_split_ancestor.is_some() && v_split_ancestor.is_some() {
                break;
            }
            cur = parent_id;
        }

        let mut changed = false;
        if dx_px.abs() > f64::EPSILON
            && let Some((pid, is_first)) = h_split_ancestor
            && let Some(pnode) = self.nodes.get_mut(&pid)
        {
            let pw = f64::from(pnode.raw_box.w.max(64));
            let signed = if is_first { dx_px } else { -dx_px };
            if let DwindleNodeKind::Split { split_ratio, .. } = &mut pnode.kind {
                *split_ratio =
                    (*split_ratio + signed * 2.0 / pw).clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
                changed = true;
            }
        }
        if dy_px.abs() > f64::EPSILON
            && let Some((pid, is_first)) = v_split_ancestor
            && let Some(pnode) = self.nodes.get_mut(&pid)
        {
            let ph = f64::from(pnode.raw_box.h.max(64));
            let signed = if is_first { dy_px } else { -dy_px };
            if let DwindleNodeKind::Split { split_ratio, .. } = &mut pnode.kind {
                *split_ratio =
                    (*split_ratio + signed * 2.0 / ph).clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
                changed = true;
            }
        }

        if changed {
            self.recalculate(viewport);
        }
        changed
    }

    /// Toggles or sets `pseudotile` preferred dimensions on a tiled leaf window.
    pub fn set_pseudotile(
        &mut self,
        win_id: WindowId,
        size: Option<(u32, u32)>,
        viewport: LogicalViewport,
    ) -> bool {
        let Some(&leaf_id) = self.win_to_leaf.get(&win_id) else {
            return false;
        };
        if let Some(node) = self.nodes.get_mut(&leaf_id)
            && let DwindleNodeKind::Leaf {
                pseudotile_size, ..
            } = &mut node.kind
        {
            *pseudotile_size = size;
            self.recalculate(viewport);
            return true;
        }
        false
    }

    #[must_use]
    pub fn is_pseudotiled(&self, win_id: WindowId) -> bool {
        let Some(&leaf_id) = self.win_to_leaf.get(&win_id) else {
            return false;
        };
        matches!(
            self.nodes.get(&leaf_id).map(|n| &n.kind),
            Some(DwindleNodeKind::Leaf {
                pseudotile_size: Some(_),
                ..
            })
        )
    }

    fn first_leaf_id_under(&self, node_id: DwindleNodeId) -> Option<DwindleNodeId> {
        let node = self.nodes.get(&node_id)?;
        match &node.kind {
            DwindleNodeKind::Leaf { .. } => Some(node_id),
            DwindleNodeKind::Split { children, .. } => self.first_leaf_id_under(children[0]),
        }
    }

    fn leftmost_leaf_under(&self, node_id: DwindleNodeId) -> Option<WindowId> {
        let leaf_id = self.first_leaf_id_under(node_id)?;
        match self.nodes.get(&leaf_id)?.kind {
            DwindleNodeKind::Leaf { win_id, .. } => Some(win_id),
            DwindleNodeKind::Split { .. } => None,
        }
    }

    fn leaf_at_or_closest(&self, pt: LogicalPoint) -> Option<DwindleNodeId> {
        self.leaf_at_or_closest_excluding(pt, None)
    }

    fn leaf_at_or_closest_excluding(
        &self,
        pt: LogicalPoint,
        exclude: Option<DwindleNodeId>,
    ) -> Option<DwindleNodeId> {
        let mut best: Option<(DwindleNodeId, f64)> = None;
        for (&nid, node) in &self.nodes {
            if Some(nid) == exclude || !matches!(node.kind, DwindleNodeKind::Leaf { .. }) {
                continue;
            }
            let d2 = point_to_rect_dist_sq(pt, node.raw_box);
            if d2 == 0.0 {
                return Some(nid);
            }
            if best.is_none_or(|(_, cur_d2)| d2 < cur_d2) {
                best = Some((nid, d2));
            }
        }
        best.map(|(id, _)| id)
    }
}

/// Evaluates Hyprland's 4-triangle diagonal quadrant slope (`DwindleAlgorithm.cpp` lines 179-206).
fn four_triangle_quadrant(target_box: LogicalRect, cursor: LogicalPoint) -> CardinalDir {
    let center_x = f64::from(target_box.x) + f64::from(target_box.w) * 0.5;
    let center_y = f64::from(target_box.y) + f64::from(target_box.h) * 0.5;
    let dx = cursor.x - center_x;
    let dy = cursor.y - center_y;
    let w_f = f64::from(target_box.w.max(1));
    let h_f = f64::from(target_box.h.max(1));

    if (dy * w_f).abs() < (dx * h_f).abs() {
        if dx < 0.0 {
            CardinalDir::Left
        } else {
            CardinalDir::Right
        }
    } else if dy < 0.0 {
        CardinalDir::Up
    } else {
        CardinalDir::Down
    }
}

fn rect_center(r: LogicalRect) -> LogicalPoint {
    LogicalPoint {
        x: f64::from(r.x) + f64::from(r.w) * 0.5,
        y: f64::from(r.y) + f64::from(r.h) * 0.5,
    }
}

fn focal_probe_point(r: LogicalRect, dir: CardinalDir) -> LogicalPoint {
    let cx = f64::from(r.x) + f64::from(r.w) * 0.5;
    let cy = f64::from(r.y) + f64::from(r.h) * 0.5;
    match dir {
        CardinalDir::Left => LogicalPoint {
            x: f64::from(r.x) - 4.0,
            y: cy,
        },
        CardinalDir::Right => LogicalPoint {
            x: f64::from(r.x + r.w as i32) + 4.0,
            y: cy,
        },
        CardinalDir::Up => LogicalPoint {
            x: cx,
            y: f64::from(r.y) - 4.0,
        },
        CardinalDir::Down => LogicalPoint {
            x: cx,
            y: f64::from(r.y + r.h as i32) + 4.0,
        },
    }
}

fn point_to_rect_dist_sq(pt: LogicalPoint, r: LogicalRect) -> f64 {
    let min_x = f64::from(r.x);
    let max_x = f64::from(r.x + r.w as i32);
    let min_y = f64::from(r.y);
    let max_y = f64::from(r.y + r.h as i32);
    let dx = if pt.x < min_x {
        min_x - pt.x
    } else if pt.x > max_x {
        pt.x - max_x
    } else {
        0.0
    };
    let dy = if pt.y < min_y {
        min_y - pt.y
    } else if pt.y > max_y {
        pt.y - max_y
    } else {
        0.0
    };
    dx * dx + dy * dy
}

fn distribute_split_axis(avail: u32, desired: u32, min0: u32, min1: u32) -> u32 {
    if avail <= 64 {
        return (avail / 2).max(32).min(avail.max(1));
    }
    let max_span = avail.saturating_sub(32).max(32);
    let lower = min0.min(max_span).max(32);
    let upper = avail.saturating_sub(min1).max(32).min(max_span);
    if lower <= upper {
        desired.clamp(lower, upper)
    } else {
        desired.clamp(32, max_span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp() -> LogicalViewport {
        LogicalViewport::new(1920, 1080, 1.0)
    }

    #[test]
    fn fibonacci_dwindle_progression_splits_side_then_top_then_side() {
        let cfg = DwindleConfig {
            force_split: 2, // always right/bottom for deterministic Fibonacci dwindling
            outer_gap: 0,
            inner_gap: 0,
            ..DwindleConfig::default()
        };

        let mut tree = DwindleTree::new(cfg);
        let w1 = WindowId::new(1, 1);
        let w2 = WindowId::new(2, 1);
        let w3 = WindowId::new(3, 1);
        let w4 = WindowId::new(4, 1);

        assert!(matches!(
            tree.insert(w1, vp(), None, None, None, None),
            DwindleInsertOutcome::Tiled(_)
        ));
        assert_eq!(
            tree.rect_of(w1).unwrap(),
            LogicalRect::new(0, 0, 1920, 1080)
        );

        // 2nd window: 1920 > 1080 -> split side-by-side (960x1080 each)
        let _ = tree.insert(
            w2,
            vp(),
            Some(w1),
            Some(LogicalPoint {
                x: 1400.0,
                y: 540.0,
            }),
            None,
            None,
        );
        assert_eq!(tree.rect_of(w1).unwrap(), LogicalRect::new(0, 0, 960, 1080));
        assert_eq!(
            tree.rect_of(w2).unwrap(),
            LogicalRect::new(960, 0, 960, 1080)
        );

        // 3rd window on w2 (960 < 1080): splits top-and-bottom (960x540 each)
        let _ = tree.insert(
            w3,
            vp(),
            Some(w2),
            Some(LogicalPoint {
                x: 1400.0,
                y: 800.0,
            }),
            None,
            None,
        );
        assert_eq!(
            tree.rect_of(w2).unwrap(),
            LogicalRect::new(960, 0, 960, 540)
        );
        assert_eq!(
            tree.rect_of(w3).unwrap(),
            LogicalRect::new(960, 540, 960, 540)
        );

        // 4th window on w3 (960 > 540): splits side-by-side (480x540 each) — classic Fibonacci spiral!
        let _ = tree.insert(
            w4,
            vp(),
            Some(w3),
            Some(LogicalPoint {
                x: 1600.0,
                y: 800.0,
            }),
            None,
            None,
        );
        assert_eq!(
            tree.rect_of(w3).unwrap(),
            LogicalRect::new(960, 540, 480, 540)
        );
        assert_eq!(
            tree.rect_of(w4).unwrap(),
            LogicalRect::new(1440, 540, 480, 540)
        );

        // Directional navigation works across all quadrants
        assert_eq!(tree.focus_in_direction(w4, CardinalDir::Left), Some(w3));
        assert_eq!(tree.focus_in_direction(w3, CardinalDir::Left), Some(w1));
        assert_eq!(tree.focus_in_direction(w3, CardinalDir::Up), Some(w2));
    }

    #[test]
    fn max_size_dialog_automatically_downgrades_to_centered_floating() {
        let mut tree = DwindleTree::default();
        let w1 = WindowId::new(1, 1);
        let dialog = WindowId::new(2, 1);
        let _ = tree.insert(w1, vp(), None, None, None, None);

        let res = tree.insert(dialog, vp(), Some(w1), None, Some((420, 260)), None);
        match res {
            DwindleInsertOutcome::DowngradeToFloating { centered_rect } => {
                assert_eq!(centered_rect.w, 420);
                assert_eq!(centered_rect.h, 260);
                assert!(!tree.contains(dialog));
            }
            DwindleInsertOutcome::Tiled(_) => panic!("expected max_size dialog to auto-float"),
        }
    }

    #[test]
    fn smart_split_four_triangle_places_window_on_top_edge() {
        let cfg = DwindleConfig {
            smart_split: true,
            outer_gap: 0,
            inner_gap: 0,
            ..DwindleConfig::default()
        };

        let mut tree = DwindleTree::new(cfg);
        let w1 = WindowId::new(1, 1);
        let w2 = WindowId::new(2, 1);
        let _ = tree.insert(w1, vp(), None, None, None, None);

        // Cursor near top-center (960, 80) -> top triangle -> vertical split with w2 on TOP!
        let _ = tree.insert(
            w2,
            vp(),
            Some(w1),
            Some(LogicalPoint { x: 960.0, y: 80.0 }),
            None,
            None,
        );
        assert_eq!(tree.rect_of(w2).unwrap(), LogicalRect::new(0, 0, 1920, 540));
        assert_eq!(
            tree.rect_of(w1).unwrap(),
            LogicalRect::new(0, 540, 1920, 540)
        );
    }

    #[test]
    fn drop_zone_preview_and_corner_smart_resizing_track_cursor_accurately() {
        let cfg = DwindleConfig {
            outer_gap: 0,
            inner_gap: 0,
            ..DwindleConfig::default()
        };

        let mut tree = DwindleTree::new(cfg);
        let w1 = WindowId::new(1, 1);
        let w2 = WindowId::new(2, 1);
        let w3 = WindowId::new(3, 1);

        let _ = tree.insert(w1, vp(), None, None, None, None);
        let dz = tree.predict_drop_zone(
            LogicalPoint {
                x: 1800.0,
                y: 540.0,
            },
            vp(),
        );
        assert_eq!(dz.target_window, Some(w1));
        assert_eq!(dz.split_dir, CardinalDir::Right);
        assert_eq!(dz.preview_rect, LogicalRect::new(960, 0, 960, 1080));

        let _ = tree.insert_at_drop_zone(w2, dz, vp());
        assert_eq!(tree.rect_of(w1).unwrap(), LogicalRect::new(0, 0, 960, 1080));
        assert_eq!(
            tree.rect_of(w2).unwrap(),
            LogicalRect::new(960, 0, 960, 1080)
        );

        // Drop w3 onto bottom quadrant of w2
        let dz_bottom = tree.predict_drop_zone(
            LogicalPoint {
                x: 1440.0,
                y: 1020.0,
            },
            vp(),
        );
        assert_eq!(dz_bottom.target_window, Some(w2));
        assert_eq!(dz_bottom.split_dir, CardinalDir::Down);
        let _ = tree.insert_at_drop_zone(w3, dz_bottom, vp());
        assert_eq!(
            tree.rect_of(w2).unwrap(),
            LogicalRect::new(960, 0, 960, 540)
        );
        assert_eq!(
            tree.rect_of(w3).unwrap(),
            LogicalRect::new(960, 540, 960, 540)
        );

        // Grab TopLeft corner of w3 and drag (-160, -100): divider moves left by 160 & up by 100!
        assert!(tree.resize_target_from_corner(w3, -160.0, -100.0, ResizeCorner::TopLeft, vp()));
        assert_eq!(tree.rect_of(w1).unwrap().w, 800);
        assert_eq!(tree.rect_of(w3).unwrap().x, 800);
        assert_eq!(tree.rect_of(w3).unwrap().w, 1120);
        assert_eq!(tree.rect_of(w2).unwrap().h, 440);
        assert_eq!(tree.rect_of(w3).unwrap().y, 440);
        assert_eq!(tree.rect_of(w3).unwrap().h, 640);
    }

    #[test]
    fn frame_edge_at_maps_all_eight_directions_and_rejects_interior_and_far_points() {
        let rect = LogicalRect::new(100, 100, 400, 300);
        let t = 8.0;
        let cases = [
            (LogicalPoint { x: 101.0, y: 102.0 }, ResizeCorner::TopLeft),
            (LogicalPoint { x: 499.0, y: 102.0 }, ResizeCorner::TopRight),
            (
                LogicalPoint { x: 101.0, y: 399.0 },
                ResizeCorner::BottomLeft,
            ),
            (
                LogicalPoint { x: 499.0, y: 399.0 },
                ResizeCorner::BottomRight,
            ),
            (LogicalPoint { x: 100.0, y: 250.0 }, ResizeCorner::Left),
            (LogicalPoint { x: 500.0, y: 250.0 }, ResizeCorner::Right),
            (LogicalPoint { x: 300.0, y: 100.0 }, ResizeCorner::Top),
            (LogicalPoint { x: 300.0, y: 400.0 }, ResizeCorner::Bottom),
            // Just outside the frame, still inside the threshold, is still a grab.
            (LogicalPoint { x: 296.0, y: 405.0 }, ResizeCorner::Bottom),
            // Interior of the frame is client area, never a grab.
            (LogicalPoint { x: 300.0, y: 250.0 }, ResizeCorner::None),
            // Beyond the threshold on one axis only.
            (LogicalPoint { x: 300.0, y: 412.0 }, ResizeCorner::None),
            (
                LogicalPoint {
                    x: 300.0,
                    y: 100.0 - 12.0,
                },
                ResizeCorner::None,
            ),
        ];
        for (cursor, expected) in cases {
            let got = ResizeCorner::frame_edge_at(rect, cursor, t).unwrap_or(ResizeCorner::None);
            assert_eq!(got, expected, "cursor {cursor:?}");
        }
    }

    #[test]
    fn frame_edge_at_prefers_the_nearer_edge_when_the_frame_is_thinner_than_two_thresholds() {
        // A 10px wide tile with an 8px threshold: both vertical edges are within reach of the
        // centre, so the nearer one must win instead of reporting a corner.
        let rect = LogicalRect::new(0, 0, 10, 200);
        assert_eq!(
            ResizeCorner::frame_edge_at(rect, LogicalPoint { x: 1.0, y: 100.0 }, 8.0),
            Some(ResizeCorner::Left)
        );
        assert_eq!(
            ResizeCorner::frame_edge_at(rect, LogicalPoint { x: 9.0, y: 100.0 }, 8.0),
            Some(ResizeCorner::Right)
        );
        // A degenerate frame can never be a resize target.
        assert_eq!(
            ResizeCorner::frame_edge_at(
                LogicalRect::new(0, 0, 0, 0),
                LogicalPoint { x: 0.0, y: 0.0 },
                8.0
            ),
            None
        );
    }
}
