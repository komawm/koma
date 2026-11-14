//! Responsive `16:9` multi-workspace overview grid solver & 2D keyboard/pointer navigation (`Mod + Tab`).

use koma_model::LogicalRect;
use koma_proto::WorkspaceId;

pub const GRID_OUTER_PADDING: u32 = 48;
pub const GRID_GUTTER: u32 = 24;
pub const CARD_HEADER_HEIGHT: u32 = 30;
pub const CARD_INNER_PAD: u32 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverviewCardSlot {
    pub index: usize,
    pub ws_id: WorkspaceId,
    pub card_rect: LogicalRect,
    pub header_rect: LogicalRect,
    pub thumb_rect: LogicalRect,
    pub is_selected: bool,
}

#[derive(Debug, Clone)]
pub struct OverviewGridState {
    pub is_open: bool,
    pub cols: usize,
    pub rows: usize,
    pub selected_idx: usize,
    pub slots: Vec<OverviewCardSlot>,
}

impl Default for OverviewGridState {
    fn default() -> Self {
        Self {
            is_open: false,
            cols: 1,
            rows: 1,
            selected_idx: 0,
            slots: Vec::new(),
        }
    }
}

impl OverviewGridState {
    /// Computes centered, aspect-ratio-preserving (`16:9` thumbnail + header) card slots for `workspaces`.
    pub fn recompute_layout(&mut self, screen_w: u32, screen_h: u32, workspaces: &[WorkspaceId]) {
        let n = workspaces.len();
        if n == 0 {
            self.slots.clear();
            self.cols = 1;
            self.rows = 1;
            self.selected_idx = 0;
            return;
        }

        self.selected_idx = self.selected_idx.min(n - 1);
        let cols = optimal_column_count(n);
        let rows = n.div_ceil(cols);
        self.cols = cols;
        self.rows = rows;

        let avail_w = screen_w.saturating_sub(GRID_OUTER_PADDING * 2);
        let avail_h = screen_h.saturating_sub(GRID_OUTER_PADDING * 2);

        let max_cell_w =
            avail_w.saturating_sub(GRID_GUTTER * (cols.saturating_sub(1) as u32)) / (cols as u32);
        let max_cell_h =
            avail_h.saturating_sub(GRID_GUTTER * (rows.saturating_sub(1) as u32)) / (rows as u32);

        // Fit 16:9 thumbnail + header + padding inside `(max_cell_w, max_cell_h)` without distortion
        let max_thumb_w_by_w = max_cell_w.saturating_sub(CARD_INNER_PAD * 2);
        let max_thumb_h_by_h = max_cell_h.saturating_sub(CARD_HEADER_HEIGHT + CARD_INNER_PAD * 2);
        let thumb_w = max_thumb_w_by_w.min((max_thumb_h_by_h * 16) / 9).max(96);
        let thumb_h = ((thumb_w * 9) / 16).max(54);

        let card_w = thumb_w + CARD_INNER_PAD * 2;
        let card_h = thumb_h + CARD_HEADER_HEIGHT + CARD_INNER_PAD * 2;

        let total_grid_h = card_h * (rows as u32) + GRID_GUTTER * (rows.saturating_sub(1) as u32);
        let start_y = (screen_h.saturating_sub(total_grid_h) / 2) as i32;

        let mut slots = Vec::with_capacity(n);
        for (idx, ws_id) in workspaces.iter().enumerate() {
            let r = idx / cols;
            let c = idx % cols;
            let cols_in_this_row = if r + 1 == rows && !n.is_multiple_of(cols) {
                n % cols
            } else {
                cols
            };
            let row_total_w = card_w * (cols_in_this_row as u32)
                + GRID_GUTTER * (cols_in_this_row.saturating_sub(1) as u32);
            let row_start_x = (screen_w.saturating_sub(row_total_w) / 2) as i32;

            let cx = row_start_x + (c as i32) * ((card_w + GRID_GUTTER) as i32);
            let cy = start_y + (r as i32) * ((card_h + GRID_GUTTER) as i32);

            let card_rect = LogicalRect::new(cx, cy, card_w, card_h);
            let header_rect = LogicalRect::new(
                cx + CARD_INNER_PAD as i32,
                cy + CARD_INNER_PAD as i32,
                thumb_w,
                CARD_HEADER_HEIGHT,
            );
            let thumb_rect = LogicalRect::new(
                cx + CARD_INNER_PAD as i32,
                cy + (CARD_INNER_PAD + CARD_HEADER_HEIGHT) as i32,
                thumb_w,
                thumb_h,
            );

            slots.push(OverviewCardSlot {
                index: idx,
                ws_id: ws_id.clone(),
                card_rect,
                header_rect,
                thumb_rect,
                is_selected: idx == self.selected_idx,
            });
        }
        self.slots = slots;
    }

    /// Moves selection by `(dx, dy)` for `H/J/K/L` or arrow keys.
    pub fn move_selection(&mut self, dx: isize, dy: isize) {
        if self.slots.is_empty() {
            return;
        }
        let n = self.slots.len() as isize;
        let cols = self.cols.max(1) as isize;
        let cur = self.selected_idx as isize;
        let next = (cur + dx + dy * cols).clamp(0, n - 1) as usize;
        self.set_selected(next);
    }

    pub fn set_selected(&mut self, idx: usize) {
        if idx < self.slots.len() {
            self.selected_idx = idx;
            for s in &mut self.slots {
                s.is_selected = s.index == idx;
            }
        }
    }

    #[must_use]
    pub fn hit_test(&self, px: f64, py: f64) -> Option<&OverviewCardSlot> {
        let pt = koma_model::LogicalPoint { x: px, y: py };
        self.slots.iter().find(|s| s.card_rect.contains_point(pt))
    }

    #[must_use]
    pub fn selected_workspace(&self) -> Option<&WorkspaceId> {
        self.slots.get(self.selected_idx).map(|s| &s.ws_id)
    }
}

const fn optimal_column_count(n: usize) -> usize {
    match n {
        0 | 1 => 1,
        2 => 2,
        3 | 4 => 2,
        5 | 6 => 3,
        7..=9 => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lays_out_five_workspaces_without_overlap_and_navigates_2d() {
        let mut grid = OverviewGridState::default();
        let ws: Vec<WorkspaceId> = (1..=5)
            .map(|i| WorkspaceId::new(format!("ws_{i:02}")))
            .collect();
        grid.recompute_layout(2560, 1600, &ws);

        assert_eq!(grid.cols, 3);
        assert_eq!(grid.rows, 2);
        assert_eq!(grid.slots.len(), 5);

        // Verify pairwise non-overlapping card bounding boxes
        for i in 0..grid.slots.len() {
            for j in (i + 1)..grid.slots.len() {
                let a = grid.slots[i].card_rect;
                let b = grid.slots[j].card_rect;
                let disjoint = a.x + (a.w as i32) <= b.x
                    || b.x + (b.w as i32) <= a.x
                    || a.y + (a.h as i32) <= b.y
                    || b.y + (b.h as i32) <= a.y;
                assert!(disjoint, "cards {i} and {j} must not overlap");
            }
        }

        // Move right -> down -> verify ws_05 selected
        grid.move_selection(1, 0);
        assert_eq!(grid.selected_workspace(), Some(&WorkspaceId::from("ws_02")));
        grid.move_selection(0, 1);
        assert_eq!(grid.selected_workspace(), Some(&WorkspaceId::from("ws_05")));
    }
}
