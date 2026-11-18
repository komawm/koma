//! Symmetrical first-class workspace container implementing Hyprland's `CSpace` dual-layer
//! (`Tiled`: `Dwindle` | `Scrolling` | `Deck` + `Floating`: `FloatingStack`) architecture
//! and `special:<name>` Scratchpad overlay semantics.

use std::collections::BTreeMap;

use koma_proto::{OutputId, SeatId, WindowId, WorkspaceId};

use crate::{
    clock::{AdaptiveFrameClock, FrameClockConfig},
    dwindle::{CardinalDir, DwindleConfig, DwindleInsertOutcome, DwindleTree},
    geometry::{LogicalPoint, LogicalRect, LogicalViewport, SizeHints},
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

/// Per-window presentation override owned by a `WorkspaceState`.
///
/// This is the single source of truth for both special window modes: it decides the geometry of
/// the window and every consumer (xdg `Maximized`/`Fullscreen` state bits, foreign-toplevel and IPC
/// surfaces, the layer-shell visibility policy) derives its answer from it. The two modes are
/// mutually exclusive by construction because only one window can hold at most one mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowPresentation {
    /// Covers `viewport.full_rect()` (gaps and exclusive zones included) and hides the
    /// `Top`/`Overlay` layer tiers on the hosting output.
    Fullscreen,
    /// Covers `viewport.work_area_rect()` inset by the active layout's outer gap; panels stay.
    Maximized,
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
    /// Single source of truth for `Fullscreen` / `Maximized` presentation of at most one window.
    pub window_presentation: Option<(WindowId, WindowPresentation)>,
    pub lease: WorkspaceLease,
    pub frame_clock: AdaptiveFrameClock,
    /// Keeps this workspace visible in `ext-workspace-v1` status bars while it holds no window
    /// (`koma.workspace({ persistent = true })`). Empty workspaces are hidden by default so a bar
    /// lists only the workspaces actually in use instead of the whole configured strip.
    pub persistent: bool,
    /// Per-window `xdg_toplevel` min/max size constraints, applied to every tiled geometry this
    /// workspace reports (see [`SizeHints`]). Floating windows keep their own placement.
    size_hints: BTreeMap<WindowId, SizeHints>,
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
            window_presentation: None,
            lease: WorkspaceLease::default(),
            frame_clock: AdaptiveFrameClock::new(is_headed, FrameClockConfig::default(), now_ms),
            persistent: false,
            size_hints: BTreeMap::new(),
        }
    }

    /// Records (or clears) the `xdg_toplevel` size constraints advertised by `win_id`.
    ///
    /// Returns `true` when the stored hints changed, so callers can re-run
    /// `sync_workspace_geometries` and re-configure a client that updated its minimum size after a
    /// configure (Chromium does this on every settled resize).
    pub fn set_size_hints(&mut self, win_id: WindowId, hints: SizeHints) -> bool {
        if hints.is_unconstrained() {
            return self.size_hints.remove(&win_id).is_some();
        }
        match self.size_hints.get(&win_id) {
            Some(existing) if *existing == hints => false,
            _ => {
                self.size_hints.insert(win_id, hints);
                true
            }
        }
    }

    #[must_use]
    pub fn size_hints(&self, win_id: WindowId) -> Option<SizeHints> {
        self.size_hints.get(&win_id).copied()
    }

    /// Clamps every presented/tiled geometry to the owning client's advertised size constraints.
    ///
    /// A layout box narrower/shorter than a client's minimum cannot be honoured by that client: it
    /// keeps its minimum content size and commits a cropped buffer, which the compositor then masks
    /// to the smaller tile ("clipped instead of resized"). Growing the reported tile to the minimum
    /// makes the layout honest again.
    ///
    /// Fullscreen/maximize presentation rects go through the same clamp, so a presentation never
    /// hands a client a size below its `xdg_toplevel` minimum.
    ///
    /// When the workspace genuinely cannot fit both neighbours' minimums the clamped tile overlaps
    /// its sibling (the focused window stays complete and usable). Propagating minimums through the
    /// split ratios so the sibling yields space is a follow-up, not a regression of this fix.
    fn clamp_tiled_size_hints(&self, out: &mut [(WindowId, LogicalRect, bool)]) {
        if self.size_hints.is_empty() {
            return;
        }
        for (win_id, rect, is_floating) in out {
            if *is_floating {
                continue;
            }
            if let Some(hints) = self.size_hints.get(win_id) {
                *rect = hints.clamp_rect(*rect);
            }
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
        if self.presented_window_id() == Some(win_id) {
            self.window_presentation = None;
        }
        let _ = self.deck.remove(win_id);
        self.size_hints.remove(&win_id);
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
        if self.presented_window_id() == Some(win_id) {
            self.window_presentation = None;
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

    /// `WindowId` currently presented `Fullscreen` or `Maximized` on this workspace.
    #[must_use]
    pub fn presented_window_id(&self) -> Option<WindowId> {
        self.window_presentation.map(|(win_id, _)| win_id)
    }

    /// Presentation mode currently applied to `win_id`, if any.
    #[must_use]
    pub fn presentation_of(&self, win_id: WindowId) -> Option<WindowPresentation> {
        self.window_presentation
            .filter(|(presented, _)| *presented == win_id)
            .map(|(_, mode)| mode)
    }

    /// `WindowId` currently presented `Fullscreen` on this workspace, if any.
    #[must_use]
    pub fn fullscreen_window(&self) -> Option<WindowId> {
        self.window_presentation
            .filter(|(_, mode)| *mode == WindowPresentation::Fullscreen)
            .map(|(win_id, _)| win_id)
    }

    /// `WindowId` currently presented `Maximized` on this workspace, if any.
    #[must_use]
    pub fn maximized_window(&self) -> Option<WindowId> {
        self.window_presentation
            .filter(|(_, mode)| *mode == WindowPresentation::Maximized)
            .map(|(win_id, _)| win_id)
    }

    /// Whether `win_id` is the workspace's `Fullscreen` window.
    #[must_use]
    pub fn is_fullscreen(&self, win_id: WindowId) -> bool {
        self.presentation_of(win_id) == Some(WindowPresentation::Fullscreen)
    }

    /// Whether `win_id` is the workspace's `Maximized` window.
    #[must_use]
    pub fn is_maximized(&self, win_id: WindowId) -> bool {
        self.presentation_of(win_id) == Some(WindowPresentation::Maximized)
    }

    /// Sets or clears `Fullscreen` on `win_id`, clearing any other presentation mode on this
    /// workspace (the two modes are mutually exclusive). Returns the resulting state.
    pub fn set_fullscreen(&mut self, win_id: WindowId, fullscreen: bool) -> bool {
        if self.is_fullscreen(win_id) == fullscreen {
            return fullscreen;
        }
        if fullscreen {
            self.window_presentation = Some((win_id, WindowPresentation::Fullscreen));
            self.focused_window = Some(win_id);
        } else {
            self.window_presentation = None;
        }
        fullscreen
    }

    /// Sets or clears `Maximized` on `win_id`, clearing any other presentation mode on this
    /// workspace. Returns the resulting state.
    pub fn set_maximized(&mut self, win_id: WindowId, maximized: bool) -> bool {
        if self.is_maximized(win_id) == maximized {
            return maximized;
        }
        if maximized {
            self.window_presentation = Some((win_id, WindowPresentation::Maximized));
            self.focused_window = Some(win_id);
        } else {
            self.window_presentation = None;
        }
        maximized
    }

    /// Toggles workspace fullscreen state for `win_id`, preserving underlying tiled/floating rects.
    /// An active `Maximized` state on any window is cleared.
    pub fn toggle_fullscreen(&mut self, win_id: WindowId) -> bool {
        self.set_fullscreen(win_id, !self.is_fullscreen(win_id))
    }

    /// Toggles workspace maximize state for `win_id`. An active `Fullscreen` state is cleared.
    pub fn toggle_maximized(&mut self, win_id: WindowId) -> bool {
        self.set_maximized(win_id, !self.is_maximized(win_id))
    }

    /// The active layout's outer gap, used by `maximized_rect`.
    #[must_use]
    pub fn outer_gap(&self) -> u32 {
        match self.layout_mode {
            LayoutMode::Dwindle | LayoutMode::Deck => self.dwindle.config.outer_gap,
            LayoutMode::Scrolling => self.scrolling.config.outer_gap,
        }
    }

    /// Rect used by `WindowPresentation::Maximized`: the non-exclusive work area inset by the
    /// active layout's outer gap, so the bar stays visible while tiling gaps stay uniform.
    #[must_use]
    pub fn maximized_rect(&self) -> LogicalRect {
        let wa = self.viewport.work_area_rect();
        let gap = self.outer_gap();
        let w = wa.w.saturating_sub(gap.saturating_mul(2)).max(1);
        let h = wa.h.saturating_sub(gap.saturating_mul(2)).max(1);
        LogicalRect::new(
            wa.x.saturating_add(gap as i32),
            wa.y.saturating_add(gap as i32),
            w,
            h,
        )
    }

    /// Geometry of the presenting window (`Fullscreen` or `Maximized`), if any.
    #[must_use]
    pub fn presentation_geometry(&self) -> Option<(WindowId, LogicalRect)> {
        self.window_presentation.map(|(win_id, mode)| {
            let rect = match mode {
                WindowPresentation::Fullscreen => self.viewport.full_rect(),
                WindowPresentation::Maximized => self.maximized_rect(),
            };
            (win_id, rect)
        })
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
        if let Some((win_id, rect)) = self.presentation_geometry() {
            let mut out = vec![(win_id, rect, false)];
            self.clamp_tiled_size_hints(&mut out);
            return out;
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

        self.clamp_tiled_size_hints(&mut out);
        out
    }

    /// Returns all currently visible `(WindowId, LogicalRect, is_floating)` items in **bottom-to-top**
    /// Z-order at `now_ms` (`Scrolling` columns shifted by the live `camera_spring` position!).
    #[must_use]
    pub fn compute_visible_geometries_at(&self, now_ms: u64) -> Vec<(WindowId, LogicalRect, bool)> {
        if let Some((win_id, rect)) = self.presentation_geometry() {
            let mut out = vec![(win_id, rect, false)];
            self.clamp_tiled_size_hints(&mut out);
            return out;
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

        self.clamp_tiled_size_hints(&mut out);
        out
    }

    /// Returns visible `(WindowId, LogicalRect, is_floating)` items at the settled target camera.
    #[must_use]
    pub fn compute_visible_geometries(&self) -> Vec<(WindowId, LogicalRect, bool)> {
        if let Some((win_id, rect)) = self.presentation_geometry() {
            let mut out = vec![(win_id, rect, false)];
            self.clamp_tiled_size_hints(&mut out);
            return out;
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

        self.clamp_tiled_size_hints(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use koma_proto::OutputId;

    use super::*;

    #[test]
    fn exclusive_zone_shrinks_dwindle_scrolling_and_deck_while_fullscreen_uses_full_output() {
        let base_vp = LogicalViewport::new(1920, 1080, 1.0);
        let wa = base_vp.work_area_from_edges(36, 44, 60, 40);
        let vp_with_bars = base_vp.with_work_area_zone(wa.x, wa.y, wa.w as i32, wa.h as i32);

        let mut ws = WorkspaceState::new(
            WorkspaceId::new("main"),
            "main",
            SeatId::new("seat0"),
            vp_with_bars,
            PresentationTarget::Physical(OutputId::new("eDP-1")),
            LayoutMode::Dwindle,
            0,
        );
        ws.dwindle.config.outer_gap = 0;
        ws.dwindle.config.inner_gap = 0;
        let w1 = WindowId::new(1, 1);
        ws.insert_window(w1, false, None, None, None, None, false, true, 0);

        let tiled = ws.compute_visible_geometries();
        assert_eq!(
            tiled,
            vec![(w1, LogicalRect::new(60, 36, 1820, 1000), false)]
        );

        ws.set_layout_mode(LayoutMode::Deck, 1);
        let deck_geos = ws.compute_visible_geometries();
        assert_eq!(
            deck_geos,
            vec![(w1, LogicalRect::new(60, 36, 1820, 1000), false)]
        );

        ws.set_layout_mode(LayoutMode::Scrolling, 2);
        let mut s_cfg = ws.scrolling.config.clone();
        s_cfg.outer_gap = 0;
        ws.scrolling.update_config(s_cfg, vp_with_bars, 2);
        let scroll_geos = ws.compute_visible_geometries();
        assert_eq!(scroll_geos[0].1.y, 36);
        assert_eq!(scroll_geos[0].1.h, 1000);
        assert!(scroll_geos[0].1.x >= 60);
        assert!(scroll_geos[0].1.x + scroll_geos[0].1.w as i32 <= 60 + 1820);

        // Fullscreen spans full physical output bounds regardless of panels
        ws.set_fullscreen(w1, true);
        assert_eq!(
            ws.compute_visible_geometries(),
            vec![(w1, LogicalRect::new(0, 0, 1920, 1080), false)]
        );
    }

    /// `komawm/koma#33`: a dwindle split may not hand a client a tile narrower than the
    /// `xdg_toplevel.set_min_size` it advertised, or Chromium/Firefox keep their minimum content
    /// size and commit a cropped buffer that the compositor then masks ("clipped, not resized").
    #[test]
    fn size_hints_clamp_tiled_geometry_to_the_client_minimum() {
        let vp = LogicalViewport::new(800, 600, 1.0);
        let mut ws = WorkspaceState::new(
            WorkspaceId::new("main"),
            "main",
            SeatId::new("seat0"),
            vp,
            PresentationTarget::Physical(OutputId::new("eDP-1")),
            LayoutMode::Dwindle,
            0,
        );
        ws.dwindle.config.outer_gap = 0;
        ws.dwindle.config.inner_gap = 0;
        ws.dwindle.config.smart_split = false;
        ws.dwindle.config.split_width_multiplier = 0.1;

        let browser = WindowId::new(1, 1);
        let terminal = WindowId::new(2, 1);
        ws.insert_window(browser, false, None, None, None, None, false, true, 0);
        assert!(ws.set_size_hints(browser, SizeHints::new(500, 87, 0, 0)));
        assert!(!ws.set_size_hints(browser, SizeHints::new(500, 87, 0, 0)));
        ws.insert_window(terminal, false, None, None, None, None, false, true, 1);

        // The raw dwindle box is a 400 px half, but the reported/configured tile must be the
        // 500 px minimum the client can actually honour.
        let targets = ws.compute_animation_target_geometries();
        let (_, browser_rect, browser_float) = targets
            .iter()
            .find(|(w, _, _)| *w == browser)
            .copied()
            .expect("browser geometry");
        assert!(!browser_float);
        assert_eq!(browser_rect.w, 500);
        assert_eq!(browser_rect.h, 600);
        assert_eq!(ws.dwindle.rect_of(browser).map(|r| r.w), Some(400));

        // The visible-geometry, settled and animated sampling paths must agree, otherwise the
        // renderer would draw the client back into the too-small box.
        for (w, rect, _) in ws.compute_visible_geometries() {
            if w == browser {
                assert_eq!(rect.w, 500);
            }
        }
        for (w, rect, _) in ws.compute_visible_geometries_at(2) {
            if w == browser {
                assert_eq!(rect.w, 500);
            }
        }

        // Unconstrained clients keep the raw layout box untouched.
        let (_, terminal_rect, _) = targets
            .iter()
            .find(|(w, _, _)| *w == terminal)
            .copied()
            .expect("terminal geometry");
        assert_eq!(terminal_rect.w, 400);

        // Closing the browser drops its hints.
        let _ = ws.remove_window(browser, 3);
        assert!(ws.size_hints(browser).is_none());

        // Floating windows are placed by the floating stack and never clamped by tiled hints.
        let dialog = WindowId::new(3, 1);
        ws.insert_window(dialog, true, None, None, None, None, false, true, 4);
        assert!(ws.set_size_hints(dialog, SizeHints::new(900, 700, 0, 0)));
        let (_, float_rect, float) = ws
            .compute_visible_geometries()
            .into_iter()
            .find(|(w, _, _)| *w == dialog)
            .expect("dialog geometry");
        assert!(float);
        assert!(float_rect.w < 900, "floating rect keeps its own size");
    }

    #[test]
    fn maximized_window_uses_gapped_work_area_and_excludes_siblings_from_the_frame() {
        let base_vp = LogicalViewport::new(1920, 1080, 1.0);
        // 1820x1000 work area at (60, 36): 60 px left, 36 px top, 60 px right, 44 px bottom edges.
        let wa = base_vp.work_area_from_edges(36, 44, 60, 40);
        let vp_with_bars = base_vp.with_work_area_zone(wa.x, wa.y, wa.w as i32, wa.h as i32);

        let mut ws = WorkspaceState::new(
            WorkspaceId::new("main"),
            "main",
            SeatId::new("seat0"),
            vp_with_bars,
            PresentationTarget::Physical(OutputId::new("eDP-1")),
            LayoutMode::Dwindle,
            0,
        );
        ws.dwindle.config.outer_gap = 10;
        ws.dwindle.config.inner_gap = 6;
        let w1 = WindowId::new(1, 1);
        let w2 = WindowId::new(2, 1);
        ws.insert_window(w1, false, None, None, None, None, false, true, 0);
        ws.insert_window(w2, false, None, None, None, None, false, true, 0);
        assert_eq!(ws.compute_animation_target_geometries().len(), 2);

        assert!(ws.toggle_maximized(w1));
        assert_eq!(ws.presentation_of(w1), Some(WindowPresentation::Maximized));
        assert!(ws.is_maximized(w1));
        assert!(!ws.is_maximized(w2));
        assert!(ws.fullscreen_window().is_none());

        let maximized = ws.maximized_rect();
        // 1820x1000 work area inset by the 10 px outer gap on every side.
        assert_eq!(maximized, LogicalRect::new(70, 46, 1800, 980));
        assert_eq!(
            ws.compute_animation_target_geometries(),
            vec![(w1, maximized, false)],
            "maximize presents only the maximized window, front-most, with no siblings",
        );

        // Maximize is not fullscreen: the bar inset and gaps stay reserved.
        let fullscreen = ws.viewport.full_rect();
        assert_eq!(fullscreen, LogicalRect::new(0, 0, 1920, 1080));
        assert_ne!(maximized, fullscreen);
        assert_eq!(maximized.y - fullscreen.y, 46);
        assert_eq!(fullscreen.h - maximized.h, 100);
        assert_eq!(maximized.x, 70);
        assert_eq!(maximized.w + 120, 1920);

        // Fullscreen supersedes maximize; toggling maximize off restores the dwindle tiles.
        assert!(ws.toggle_fullscreen(w1));
        assert_eq!(
            ws.compute_animation_target_geometries(),
            vec![(w1, fullscreen, false)]
        );
        assert!(!ws.is_maximized(w1));
        assert!(!ws.toggle_fullscreen(w1));
        assert_eq!(ws.window_presentation, None);
        let restored = ws.compute_animation_target_geometries();
        assert_eq!(restored.len(), 2, "both dwindle tiles return after exiting");
        assert!(
            restored
                .iter()
                .any(|(id, rect, _)| *id == w1 && *rect != maximized)
        );
    }
}
