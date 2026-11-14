//! Symmetrical first-class workspace container implementing Hyprland's `CSpace` dual-layer
//! (`Tiled`: `Dwindle` | `Scrolling` | `Deck` + `Floating`: `FloatingStack`) architecture
//! and `special:<name>` Scratchpad overlay semantics.

use koma_proto::{OutputId, SeatId, WindowId, WorkspaceId};

use crate::{
    clock::{AdaptiveFrameClock, FrameClockConfig},
    dwindle::{CardinalDir, DwindleConfig, DwindleInsertOutcome, DwindleTree},
    geometry::{LogicalPoint, LogicalRect, LogicalViewport},
    layout::{DeckStashStack, FloatingStack, LayoutMode},
    lease::WorkspaceLease,
    scrolling::{ScrollingConfig, ScrollingStrip},
};

pub const SPECIAL_WORKSPACE_PREFIX: &str = "special:";
pub const DEFAULT_SCRATCHPAD_NAME: &str = "special:magic";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentationTarget {
    Physical(OutputId),
    Offscreen { lazy_fbo_slot: u32 },
}

/// Symmetrical workspace container owning both a tiled layout engine (`DwindleTree`,
/// `ScrollingStrip`, or `DeckStashStack`) and a Z-ordered `FloatingStack` (`CSpace`).
#[derive(Debug)]
pub struct WorkspaceState {
    pub id: WorkspaceId,
    pub name: String,
    /// `true` for Scratchpad overlay workspaces (`name` prefixed with `"special:"`).
    pub is_special: bool,
    pub vseat_id: SeatId,
    pub viewport: LogicalViewport,
    pub presentation: PresentationTarget,
    pub layout_mode: LayoutMode,
    pub dwindle: DwindleTree,
    pub deck: DeckStashStack,
    pub scrolling: ScrollingStrip,
    pub floating: FloatingStack,
    pub focused_window: Option<WindowId>,
    pub fullscreen_window: Option<WindowId>,
    pub lease: WorkspaceLease,
    pub frame_clock: AdaptiveFrameClock,
}

impl WorkspaceState {
    /// Normalizes a Scratchpad identifier (`"magic"` or `"special:magic"`) into `"special:magic"`.
    #[must_use]
    pub fn normalize_special_name(raw: Option<&str>) -> String {
        let trimmed = raw
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("magic");
        if trimmed.starts_with(SPECIAL_WORKSPACE_PREFIX) {
            trimmed.to_owned()
        } else {
            format!("{SPECIAL_WORKSPACE_PREFIX}{trimmed}")
        }
    }

    #[must_use]
    pub fn new(
        id: WorkspaceId,
        name: impl Into<String>,
        vseat_id: SeatId,
        viewport: LogicalViewport,
        presentation: PresentationTarget,
        layout_mode: LayoutMode,
        now_ms: u64,
    ) -> Self {
        let name_str = name.into();
        let is_special = name_str.starts_with(SPECIAL_WORKSPACE_PREFIX);
        let is_headed = matches!(presentation, PresentationTarget::Physical(_));

        let mut dwindle_cfg = DwindleConfig::default();
        let mut scrolling_cfg = ScrollingConfig::default();
        if is_special {
            // Frame Scratchpad overlay workspaces with generous outer margins (`Hyprland` `special:magic`)
            dwindle_cfg.outer_gap = 48;
            dwindle_cfg.inner_gap = 10;
            scrolling_cfg.outer_gap = 48;
            scrolling_cfg.inner_gap = 10;
        }

        Self {
            id,
            name: name_str,
            is_special,
            vseat_id,
            viewport,
            presentation,
            layout_mode,
            dwindle: DwindleTree::new(dwindle_cfg),
            deck: DeckStashStack::default(),
            scrolling: ScrollingStrip::new(scrolling_cfg, !is_headed),
            floating: FloatingStack::default(),
            focused_window: None,
            fullscreen_window: None,
            lease: WorkspaceLease::default(),
            frame_clock: AdaptiveFrameClock::new(is_headed, FrameClockConfig::default(), now_ms),
        }
    }

    /// Currently active/focused window in this workspace (across both `FloatingStack` and Tiled).
    #[must_use]
    pub fn active_window(&self) -> Option<WindowId> {
        self.focused_window
            .or_else(|| self.floating.topmost())
            .or_else(|| match self.layout_mode {
                LayoutMode::Dwindle => self.dwindle.first_window(),
                LayoutMode::Scrolling => self.scrolling.active_window(),
                LayoutMode::Deck => self.deck.active(),
            })
    }

    /// Inserts a newly mapped window into this workspace (`CSpace::add`), automatically applying
    /// `Dwindle`'s `max_size` auto-float downgrade check and returning `is_floating`.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_window(
        &mut self,
        win_id: WindowId,
        want_floating: bool,
        explicit_float_rect: Option<LogicalRect>,
        max_size: Option<(u32, u32)>,
        preferred_size: Option<(u32, u32)>,
        cursor_pos: Option<LogicalPoint>,
        open_in_stash: bool,
        focus_new: bool,
        now_ms: u64,
    ) -> bool {
        if want_floating {
            let rect = explicit_float_rect.unwrap_or_else(|| {
                self.floating
                    .default_centered_rect(self.viewport, preferred_size.or(max_size))
            });
            self.floating.push_top(win_id, rect);
            if focus_new || self.focused_window.is_none() {
                self.focused_window = Some(win_id);
            }
            return true;
        }

        match self.layout_mode {
            LayoutMode::Dwindle => {
                match self.dwindle.insert(
                    win_id,
                    self.viewport,
                    self.focused_window,
                    cursor_pos,
                    max_size,
                    None,
                ) {
                    DwindleInsertOutcome::Tiled(_) => {
                        self.deck.push_deck_top(win_id);
                        if focus_new || self.focused_window.is_none() {
                            self.focused_window = Some(win_id);
                        }
                        false
                    }
                    DwindleInsertOutcome::DowngradeToFloating { centered_rect } => {
                        self.floating.push_top(win_id, centered_rect);
                        if focus_new || self.focused_window.is_none() {
                            self.focused_window = Some(win_id);
                        }
                        true
                    }
                }
            }
            LayoutMode::Scrolling => {
                self.scrolling
                    .insert_column(win_id, None, self.viewport, now_ms);
                self.deck.push_deck_top(win_id);
                if focus_new || self.focused_window.is_none() {
                    self.focused_window = Some(win_id);
                }
                false
            }
            LayoutMode::Deck => {
                if open_in_stash {
                    self.deck.push_stash_background(win_id);
                } else {
                    self.deck.push_deck_top(win_id);
                }
                if (focus_new && !open_in_stash) || self.focused_window.is_none() {
                    self.focused_window = self.deck.active();
                }
                false
            }
        }
    }

    /// Removes `win_id` from whichever layer (`FloatingStack` or Tiled) owns it and updates focus.
    pub fn remove_window(&mut self, win_id: WindowId, now_ms: u64) -> Option<WindowId> {
        if self.fullscreen_window == Some(win_id) {
            self.fullscreen_window = None;
        }
        let _ = self.deck.remove(win_id);
        let was_floating = self.floating.remove(win_id).is_some();
        let next_tiled = if !was_floating {
            let d_next = self.dwindle.remove(win_id, self.viewport);
            let s_next = self.scrolling.remove(win_id, self.viewport, now_ms);
            match self.layout_mode {
                LayoutMode::Dwindle => d_next,
                LayoutMode::Scrolling => s_next,
                LayoutMode::Deck => self.deck.active(),
            }
        } else {
            None
        };

        if self.focused_window == Some(win_id) {
            self.focused_window = if was_floating {
                self.floating.topmost().or_else(|| match self.layout_mode {
                    LayoutMode::Dwindle => self.dwindle.first_window(),
                    LayoutMode::Scrolling => self.scrolling.active_window(),
                    LayoutMode::Deck => self.deck.active(),
                })
            } else {
                next_tiled.or_else(|| self.floating.topmost())
            };
        }
        self.focused_window
    }

    /// Toggles `win_id` between `FloatingStack` and the active Tiled layout (`CSpace::toggleTargetFloating`).
    pub fn toggle_floating(
        &mut self,
        win_id: WindowId,
        preferred_size: Option<(u32, u32)>,
        cursor_pos: Option<LogicalPoint>,
        now_ms: u64,
    ) -> Option<bool> {
        if self.fullscreen_window == Some(win_id) {
            self.fullscreen_window = None;
        }
        if self.floating.remove(win_id).is_some() {
            let is_float = self.insert_window(
                win_id,
                false,
                None,
                None,
                preferred_size,
                cursor_pos,
                false,
                true,
                now_ms,
            );
            Some(is_float)
        } else if self.dwindle.contains(win_id)
            || self.scrolling.contains(win_id)
            || self.deck.active() == Some(win_id)
            || self.deck.stash_windows().contains(&win_id)
        {
            let _ = self.dwindle.remove(win_id, self.viewport);
            let _ = self.scrolling.remove(win_id, self.viewport, now_ms);
            let _ = self.deck.remove(win_id);
            let rect = self
                .floating
                .default_centered_rect(self.viewport, preferred_size);
            self.floating.push_top(win_id, rect);
            self.focused_window = Some(win_id);
            Some(true)
        } else {
            None
        }
    }

    /// Toggles workspace fullscreen state for `win_id`, preserving underlying tiled/floating rects.
    pub fn toggle_fullscreen(&mut self, win_id: WindowId) -> bool {
        if self.fullscreen_window == Some(win_id) {
            self.fullscreen_window = None;
            false
        } else {
            self.fullscreen_window = Some(win_id);
            self.focused_window = Some(win_id);
            true
        }
    }

    /// Toggles `pseudotile` on `win_id` inside `DwindleTree`.
    pub fn toggle_pseudotile(&mut self, win_id: WindowId, preferred_size: (u32, u32)) -> bool {
        if self.dwindle.is_pseudotiled(win_id) {
            let _ = self.dwindle.set_pseudotile(win_id, None, self.viewport);
            false
        } else {
            self.dwindle
                .set_pseudotile(win_id, Some(preferred_size), self.viewport)
        }
    }

    /// Focuses `win_id` (raising it to top if in `FloatingStack`, or scrolling/activating in Tiled).
    pub fn activate_window(&mut self, win_id: WindowId, now_ms: u64) -> bool {
        if self.floating.raise_to_top(win_id) {
            self.focused_window = Some(win_id);
            return true;
        }
        let mut found = false;
        if self.dwindle.contains(win_id) {
            found = true;
        }
        if self.scrolling.activate(win_id, self.viewport, now_ms) {
            found = true;
        }
        if self.deck.activate(win_id) {
            found = true;
        }
        if found {
            self.focused_window = Some(win_id);
        }
        found
    }

    /// Directional focus navigation (`movefocus l/r/u/d`).
    pub fn focus_direction(&mut self, dir: CardinalDir, now_ms: u64) -> Option<WindowId> {
        let cur = self.active_window()?;
        if self.floating.contains(cur) {
            return Some(cur);
        }
        let next = match self.layout_mode {
            LayoutMode::Dwindle => self.dwindle.focus_in_direction(cur, dir),
            LayoutMode::Scrolling => match dir {
                CardinalDir::Left => self.scrolling.focus_prev(self.viewport, now_ms),
                CardinalDir::Right => self.scrolling.focus_next(self.viewport, now_ms),
                CardinalDir::Up => self.scrolling.focus_up(),
                CardinalDir::Down => self.scrolling.focus_down(),
            },
            LayoutMode::Deck => match dir {
                CardinalDir::Left | CardinalDir::Up => self.deck.cycle_prev(),
                CardinalDir::Right | CardinalDir::Down => self.deck.cycle_next(),
            },
        };
        if let Some(target) = next {
            self.focused_window = Some(target);
            let _ = self.deck.activate(target);
        }
        self.focused_window
    }

    /// Directional window move (`movewindow l/r/u/d` & `move_column_left/right` in `Scrolling`).
    pub fn move_window_direction(&mut self, dir: CardinalDir, now_ms: u64) -> bool {
        let Some(cur) = self.active_window() else {
            return false;
        };
        if self.floating.contains(cur) {
            let step = 48;
            let (dx, dy) = match dir {
                CardinalDir::Left => (-step, 0),
                CardinalDir::Right => (step, 0),
                CardinalDir::Up => (0, -step),
                CardinalDir::Down => (0, step),
            };
            return self.floating.move_by(cur, dx, dy, self.viewport);
        }
        match self.layout_mode {
            LayoutMode::Dwindle => self.dwindle.move_in_direction(cur, dir, self.viewport),
            LayoutMode::Scrolling => match dir {
                CardinalDir::Left => self.scrolling.move_column_left(self.viewport, now_ms),
                CardinalDir::Right => self.scrolling.move_column_right(self.viewport, now_ms),
                CardinalDir::Up => self.scrolling.move_tile_up(),
                CardinalDir::Down => self.scrolling.move_tile_down(),
            },
            LayoutMode::Deck => false,
        }
    }

    /// Switches `layout_mode` dynamically, migrating existing tiled windows losslessly!
    pub fn set_layout_mode(&mut self, next_mode: LayoutMode, now_ms: u64) {
        if self.layout_mode == next_mode {
            self.recalculate(now_ms);
            return;
        }
        let mut tiled_ids: Vec<WindowId> = Vec::new();
        for (w, _) in self.dwindle.tiled_geometries() {
            if !tiled_ids.contains(&w) {
                tiled_ids.push(w);
            }
        }
        for wid in self.scrolling.all_window_ids() {
            if !tiled_ids.contains(&wid) {
                tiled_ids.push(wid);
            }
        }
        if let Some(active_d) = self.deck.active()
            && !tiled_ids.contains(&active_d)
            && !self.floating.contains(active_d)
        {
            tiled_ids.push(active_d);
        }
        for &w in self.deck.stash_windows() {
            if !tiled_ids.contains(&w) && !self.floating.contains(w) {
                tiled_ids.push(w);
            }
        }

        self.layout_mode = next_mode;
        match next_mode {
            LayoutMode::Dwindle => {
                for wid in tiled_ids {
                    if !self.dwindle.contains(wid) {
                        let _ = self.dwindle.insert(
                            wid,
                            self.viewport,
                            self.focused_window,
                            None,
                            None,
                            None,
                        );
                    }
                }
            }
            LayoutMode::Scrolling => {
                for wid in tiled_ids {
                    if !self.scrolling.contains(wid) {
                        self.scrolling
                            .insert_column(wid, None, self.viewport, now_ms);
                    }
                }
            }
            LayoutMode::Deck => {
                for wid in tiled_ids {
                    if self.deck.active() != Some(wid) && !self.deck.stash_windows().contains(&wid)
                    {
                        self.deck.push_stash_background(wid);
                    }
                }
            }
        }
        self.recalculate(now_ms);
    }

    /// Recomputes all tiled & floating geometries after a monitor mode or gap update.
    pub fn recalculate(&mut self, now_ms: u64) {
        self.dwindle.recalculate(self.viewport);
        self.scrolling.ensure_active_visible(self.viewport, now_ms);
        self.floating.clamp_to_viewport(self.viewport);
    }

    /// Returns layout-space target geometries (`(WindowId, LogicalRect, is_floating)`) for
    /// synchronizing `WindowAnimationTable` and `xdg_toplevel.configure` sizes.
    ///
    /// Note: For `LayoutMode::Scrolling`, tiled columns are returned in **strip space** (`x` before
    /// subtracting `camera_x`) so column width/swap animations stay orthogonal to `camera_spring`!
    #[must_use]
    pub fn compute_animation_target_geometries(&self) -> Vec<(WindowId, LogicalRect, bool)> {
        if let Some(fs_win) = self.fullscreen_window {
            return vec![(fs_win, self.viewport.full_rect(), false)];
        }

        let mut out: Vec<(WindowId, LogicalRect, bool)> = match self.layout_mode {
            LayoutMode::Dwindle => self
                .dwindle
                .tiled_geometries()
                .into_iter()
                .map(|(w, r)| (w, r, false))
                .collect(),
            LayoutMode::Scrolling => self
                .scrolling
                .strip_geometries(self.viewport)
                .into_iter()
                .map(|(w, r)| (w, r, false))
                .collect(),
            LayoutMode::Deck => self
                .deck
                .active_geometry(self.viewport)
                .into_iter()
                .map(|(w, r)| (w, r, false))
                .collect(),
        };

        for entry in self.floating.iter_bottom_to_top() {
            out.push((entry.win_id, entry.rect, true));
        }

        out
    }

    /// Returns all currently visible `(WindowId, LogicalRect, is_floating)` items in **bottom-to-top**
    /// Z-order at `now_ms` (`Scrolling` columns shifted by the live `camera_spring` position!).
    #[must_use]
    pub fn compute_visible_geometries_at(&self, now_ms: u64) -> Vec<(WindowId, LogicalRect, bool)> {
        if let Some(fs_win) = self.fullscreen_window {
            return vec![(fs_win, self.viewport.full_rect(), false)];
        }

        let mut out: Vec<(WindowId, LogicalRect, bool)> = match self.layout_mode {
            LayoutMode::Dwindle => self
                .dwindle
                .tiled_geometries()
                .into_iter()
                .map(|(w, r)| (w, r, false))
                .collect(),
            LayoutMode::Scrolling => {
                let cam_x = self.scrolling.peek_camera_x(now_ms);
                self.scrolling
                    .visible_geometries_at(self.viewport, cam_x)
                    .into_iter()
                    .map(|(w, r)| (w, r, false))
                    .collect()
            }
            LayoutMode::Deck => self
                .deck
                .active_geometry(self.viewport)
                .into_iter()
                .map(|(w, r)| (w, r, false))
                .collect(),
        };

        for entry in self.floating.iter_bottom_to_top() {
            out.push((entry.win_id, entry.rect, true));
        }

        out
    }

    /// Returns visible `(WindowId, LogicalRect, is_floating)` items at the settled target camera.
    #[must_use]
    pub fn compute_visible_geometries(&self) -> Vec<(WindowId, LogicalRect, bool)> {
        if let Some(fs_win) = self.fullscreen_window {
            return vec![(fs_win, self.viewport.full_rect(), false)];
        }

        let mut out: Vec<(WindowId, LogicalRect, bool)> = match self.layout_mode {
            LayoutMode::Dwindle => self
                .dwindle
                .tiled_geometries()
                .into_iter()
                .map(|(w, r)| (w, r, false))
                .collect(),
            LayoutMode::Scrolling => self
                .scrolling
                .visible_geometries(self.viewport)
                .into_iter()
                .map(|(w, r)| (w, r, false))
                .collect(),
            LayoutMode::Deck => self
                .deck
                .active_geometry(self.viewport)
                .into_iter()
                .map(|(w, r)| (w, r, false))
                .collect(),
        };

        for entry in self.floating.iter_bottom_to_top() {
            out.push((entry.win_id, entry.rect, true));
        }

        out
    }
}
