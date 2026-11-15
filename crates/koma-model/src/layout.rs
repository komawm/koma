//! Workspace layout modes (`Dwindle`, `Scrolling`, `Deck`) and the Z-ordered `FloatingStack`
//! (`Hyprland` `CSpace` dual-layer `ITiledAlgorithm` + `IFloatingAlgorithm` architecture).

use std::collections::VecDeque;

use koma_proto::WindowId;
use serde::{Deserialize, Serialize};

use crate::{
    dwindle::ResizeCorner,
    geometry::{LogicalPoint, LogicalRect, LogicalViewport},
};

pub use crate::scrolling::{
    CenterFocusedColumn, EdgeAutoScrollConfig, EdgeScrollStep, GESTURE_DECELERATION_TOUCHPAD,
    PresetColumnWidth, SWIPE_HISTORY_WINDOW_MS, ScrollingColumn, ScrollingConfig,
    ScrollingDropTarget, ScrollingDropZone, ScrollingStrip, ScrollingTile, SwipeTracker,
    TileHeight, VIEW_GESTURE_WORKING_AREA_MOVEMENT,
};

/// Per-workspace primary tiled layout engine (`koma.workspace({ layout = "dwindle" | "scrolling" | "deck" })`).
///
/// - **`Dwindle` (Default for `Headed` human workspaces)**: Hyprland-style Fibonacci dwindling binary
///   tree (`general:layout = "dwindle"`, accepting `"bsp"` as a backwards-compatible alias).
/// - **`Scrolling` (Optional per-workspace)**: Niri / Hyprland 0.54+ infinite horizontal column strip.
/// - **`Deck` (Default for `Headless` Agent workspaces)**: 100% full-viewport active window + `Stash`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutMode {
    #[default]
    #[serde(alias = "bsp")]
    Dwindle,
    Scrolling,
    Deck,
}

/// Z-ordered floating window entry managed by `FloatingStack` (`Hyprland` `CDefaultFloatingAlgorithm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingEntry {
    pub win_id: WindowId,
    pub rect: LogicalRect,
}

/// Per-workspace floating layer rendered above tiled windows (`CSpace` dual-layer model).
#[derive(Debug, Clone, Default)]
pub struct FloatingStack {
    /// Ordered bottom-to-top (`entries.last()` is topmost / most recently raised).
    entries: Vec<FloatingEntry>,
}

impl FloatingStack {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn contains(&self, win_id: WindowId) -> bool {
        self.entries.iter().any(|e| e.win_id == win_id)
    }

    #[must_use]
    pub fn topmost(&self) -> Option<WindowId> {
        self.entries.last().map(|e| e.win_id)
    }

    #[must_use]
    pub fn rect_of(&self, win_id: WindowId) -> Option<LogicalRect> {
        self.entries
            .iter()
            .find(|e| e.win_id == win_id)
            .map(|e| e.rect)
    }

    /// Computes a centered cascade rectangle inside `viewport` when floating a window without an
    /// explicit rule position.
    #[must_use]
    pub fn default_centered_rect(
        &self,
        viewport: LogicalViewport,
        preferred_size: Option<(u32, u32)>,
    ) -> LogicalRect {
        let wa = viewport.work_area_rect();
        let (pw, ph) = preferred_size.unwrap_or(((wa.w * 3 / 5).max(320), (wa.h * 3 / 5).max(240)));
        let w = pw.clamp(120, wa.w.saturating_sub(32).max(120));
        let h = ph.clamp(90, wa.h.saturating_sub(32).max(90));
        let cascade = ((self.entries.len() % 5) as i32) * 24;
        let base_x = wa.x + ((wa.w.saturating_sub(w)) / 2) as i32;
        let base_y = wa.y + ((wa.h.saturating_sub(h)) / 2) as i32;
        let min_x = wa.x + 8;
        let min_y = wa.y + 8;
        let max_x = wa.x + (wa.w.saturating_sub(w + 8)) as i32;
        let max_y = wa.y + (wa.h.saturating_sub(h + 8)) as i32;
        LogicalRect::new(
            (base_x + cascade).clamp(min_x, max_x.max(min_x)),
            (base_y + cascade).clamp(min_y, max_y.max(min_y)),
            w,
            h,
        )
    }

    /// Computes the detached floating drag card rectangle when untiling a `Dwindle`/`Scrolling`
    /// window during `Mod + MouseLeft` drag, preserving proportional grab hotspot (`MAPDRAGHOTSPOT`
    /// in `Hyprland/src/layout/supplementary/DragController.cpp` lines 97-111).
    #[must_use]
    pub fn detached_tile_drag_rect(
        old_tile_rect: LogicalRect,
        cursor: LogicalPoint,
        min_size: (u32, u32),
        scale_ratio: f64,
    ) -> (LogicalRect, LogicalPoint) {
        let ratio = scale_ratio.clamp(0.5, 1.0);
        let new_w = ((f64::from(old_tile_rect.w) * ratio).round() as u32).max(min_size.0.max(160));
        let new_h = ((f64::from(old_tile_rect.h) * ratio).round() as u32).max(min_size.1.max(120));
        let rel_x = ((cursor.x - f64::from(old_tile_rect.x)) / f64::from(old_tile_rect.w.max(1)))
            .clamp(0.0, 1.0);
        let rel_y = ((cursor.y - f64::from(old_tile_rect.y)) / f64::from(old_tile_rect.h.max(1)))
            .clamp(0.0, 1.0);
        let hotspot = LogicalPoint {
            x: rel_x * f64::from(new_w),
            y: rel_y * f64::from(new_h),
        };
        let rect = LogicalRect::new(
            (cursor.x - hotspot.x).round() as i32,
            (cursor.y - hotspot.y).round() as i32,
            new_w,
            new_h,
        );
        (rect, hotspot)
    }

    /// Computes drift-free anchored corner/edge resize geometry for a floating window from its
    /// grab-start `initial` box and cumulative `(total_dx, total_dy)` pointer delta.
    #[must_use]
    pub fn compute_corner_resize_rect(
        initial: LogicalRect,
        corner: ResizeCorner,
        total_dx: f64,
        total_dy: f64,
        min_size: (u32, u32),
        max_size: (u32, u32),
        viewport: LogicalViewport,
    ) -> LogicalRect {
        let wa = viewport.work_area_rect();
        let min_w = min_size.0.max(96) as i32;
        let min_h = min_size.1.max(72) as i32;
        let max_w = if max_size.0 > 0 {
            (max_size.0 as i32).max(min_w)
        } else {
            (wa.w.max(96) as i32).max(min_w)
        };
        let max_h = if max_size.1 > 0 {
            (max_size.1 as i32).max(min_h)
        } else {
            (wa.h.max(72) as i32).max(min_h)
        };

        let right_edge = initial.x + initial.w as i32;
        let bottom_edge = initial.y + initial.h as i32;
        let dx_i = total_dx.round() as i32;
        let dy_i = total_dy.round() as i32;

        let (new_x, new_w) = if corner.has_left() {
            let w = (initial.w as i32 - dx_i).clamp(min_w, max_w);
            (right_edge - w, w)
        } else if corner.has_right() {
            let w = (initial.w as i32 + dx_i).clamp(min_w, max_w);
            (initial.x, w)
        } else {
            (initial.x, initial.w as i32)
        };

        let (new_y, new_h) = if corner.has_top() {
            let h = (initial.h as i32 - dy_i).clamp(min_h, max_h);
            (bottom_edge - h, h)
        } else if corner.has_bottom() {
            let h = (initial.h as i32 + dy_i).clamp(min_h, max_h);
            (initial.y, h)
        } else {
            (initial.y, initial.h as i32)
        };

        LogicalRect::new(new_x, new_y, new_w as u32, new_h as u32)
    }

    pub fn push_top(&mut self, win_id: WindowId, rect: LogicalRect) {
        self.remove(win_id);
        self.entries.push(FloatingEntry { win_id, rect });
    }

    pub fn raise_to_top(&mut self, win_id: WindowId) -> bool {
        let Some(pos) = self.entries.iter().position(|e| e.win_id == win_id) else {
            return false;
        };
        if pos + 1 < self.entries.len() {
            let entry = self.entries.remove(pos);
            self.entries.push(entry);
        }
        true
    }

    pub fn set_rect(&mut self, win_id: WindowId, rect: LogicalRect) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.win_id == win_id) {
            entry.rect = rect;
            return true;
        }
        false
    }

    pub fn move_to_clamped(
        &mut self,
        win_id: WindowId,
        x: i32,
        y: i32,
        viewport: LogicalViewport,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| e.win_id == win_id) else {
            return false;
        };
        let wa = viewport.work_area_rect();
        let max_x = (wa.x + wa.w as i32 - 48).max(wa.x);
        let max_y = (wa.y + wa.h as i32 - 48).max(wa.y);
        let min_x = wa.x - (entry.rect.w as i32 - 48);
        let min_y = wa.y - (entry.rect.h as i32 - 48).min(0);
        entry.rect.x = x.clamp(min_x, max_x);
        entry.rect.y = y.clamp(min_y, max_y);
        true
    }

    pub fn move_by(
        &mut self,
        win_id: WindowId,
        dx: i32,
        dy: i32,
        viewport: LogicalViewport,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| e.win_id == win_id) else {
            return false;
        };
        let wa = viewport.work_area_rect();
        let max_x = (wa.x + wa.w as i32 - 48).max(wa.x);
        let max_y = (wa.y + wa.h as i32 - 48).max(wa.y);
        let min_x = wa.x - (entry.rect.w as i32 - 48);
        entry.rect.x = (entry.rect.x + dx).clamp(min_x, max_x);
        entry.rect.y = (entry.rect.y + dy).clamp(wa.y, max_y);
        true
    }

    pub fn resize_by(
        &mut self,
        win_id: WindowId,
        dw: i32,
        dh: i32,
        viewport: LogicalViewport,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| e.win_id == win_id) else {
            return false;
        };
        let wa = viewport.work_area_rect();
        entry.rect.w = (entry.rect.w as i32 + dw).clamp(96, wa.w.max(96) as i32) as u32;
        entry.rect.h = (entry.rect.h as i32 + dh).clamp(72, wa.h.max(72) as i32) as u32;
        true
    }

    pub fn clamp_to_viewport(&mut self, viewport: LogicalViewport) {
        let wa = viewport.work_area_rect();
        for entry in &mut self.entries {
            entry.rect.w = entry.rect.w.min(wa.w.max(96));
            entry.rect.h = entry.rect.h.min(wa.h.max(72));
            let max_x = wa.x + wa.w.saturating_sub(entry.rect.w) as i32;
            let max_y = wa.y + wa.h.saturating_sub(entry.rect.h) as i32;
            entry.rect.x = entry.rect.x.clamp(wa.x, max_x.max(wa.x));
            entry.rect.y = entry.rect.y.clamp(wa.y, max_y.max(wa.y));
        }
    }

    pub fn remove(&mut self, win_id: WindowId) -> Option<FloatingEntry> {
        let pos = self.entries.iter().position(|e| e.win_id == win_id)?;
        Some(self.entries.remove(pos))
    }

    pub fn iter_bottom_to_top(&self) -> impl DoubleEndedIterator<Item = &FloatingEntry> {
        self.entries.iter()
    }
}

/// Single-window active `Deck` + background `Stash` container (`ARCHITECTURE.md` §3.2).
#[derive(Debug, Clone, Default)]
pub struct DeckStashStack {
    active_deck: Option<WindowId>,
    stash: VecDeque<WindowId>,
}

impl DeckStashStack {
    #[must_use]
    pub const fn active(&self) -> Option<WindowId> {
        self.active_deck
    }

    #[must_use]
    pub fn stash_windows(&self) -> &VecDeque<WindowId> {
        self.stash_slice()
    }

    const fn stash_slice(&self) -> &VecDeque<WindowId> {
        &self.stash
    }

    pub fn push_deck_top(&mut self, win_id: WindowId) {
        self.remove(win_id);
        if let Some(prev) = self.active_deck.replace(win_id) {
            self.stash.push_front(prev);
        }
    }

    pub fn push_stash_background(&mut self, win_id: WindowId) {
        self.remove(win_id);
        if self.active_deck.is_none() {
            self.active_deck = Some(win_id);
        } else {
            self.stash.push_back(win_id);
        }
    }

    pub fn activate(&mut self, win_id: WindowId) -> bool {
        if self.active_deck == Some(win_id) {
            return true;
        }
        if let Some(pos) = self.stash.iter().position(|&w| w == win_id) {
            self.stash.remove(pos);
            if let Some(prev) = self.active_deck.replace(win_id) {
                self.stash.push_front(prev);
            }
            return true;
        }
        false
    }

    pub fn cycle_next(&mut self) -> Option<WindowId> {
        let next = self.stash.pop_front()?;
        if let Some(prev) = self.active_deck.replace(next) {
            self.stash.push_back(prev);
        }
        self.active_deck
    }

    pub fn cycle_prev(&mut self) -> Option<WindowId> {
        let prev_back = self.stash.pop_back()?;
        if let Some(prev) = self.active_deck.replace(prev_back) {
            self.stash.push_front(prev);
        }
        self.active_deck
    }

    pub fn remove(&mut self, win_id: WindowId) -> bool {
        if self.active_deck == Some(win_id) {
            self.active_deck = self.stash.pop_front();
            return true;
        }
        if let Some(pos) = self.stash.iter().position(|&w| w == win_id) {
            self.stash.remove(pos);
            return true;
        }
        false
    }

    #[must_use]
    pub fn active_geometry(&self, viewport: LogicalViewport) -> Option<(WindowId, LogicalRect)> {
        self.active_deck
            .map(|win_id| (win_id, viewport.work_area_rect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_corner_resize_rect_never_panics_when_work_area_smaller_than_min_size() {
        let tiny_vp = LogicalViewport::new(64, 48, 1.0).with_work_area_zone(0, 0, 64, 48);
        let initial = LogicalRect::new(0, 0, 200, 150);
        let resized = FloatingStack::compute_corner_resize_rect(
            initial,
            ResizeCorner::BottomRight,
            -32.0,
            -24.0,
            (200, 150),
            (0, 0),
            tiny_vp,
        );
        assert_eq!(resized.w, 200);
        assert_eq!(resized.h, 150);

        let grown = FloatingStack::compute_corner_resize_rect(
            initial,
            ResizeCorner::TopLeft,
            -40.0,
            -30.0,
            (240, 180),
            (0, 0),
            tiny_vp,
        );
        assert!(grown.w >= 240);
        assert!(grown.h >= 180);
    }
}
