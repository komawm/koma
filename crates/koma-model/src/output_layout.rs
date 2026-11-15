#![forbid(unsafe_code)]
//! Pure-Rust 2D multi-output layout, coordinate clamping/edge crossing, output transforms,
//! Retina HiDPI auto-scaling heuristics, and hotplug state persistence.

use std::collections::HashMap;

use koma_proto::WorkspaceId;
use serde::{Deserialize, Serialize};

use crate::{CardinalDir, LogicalPoint, LogicalRect};

/// All 8 Wayland / DRM output transforms (`wl_output.transform`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputTransform {
    #[default]
    Normal,
    #[serde(rename = "90")]
    Rotate90,
    #[serde(rename = "180")]
    Rotate180,
    #[serde(rename = "270")]
    Rotate270,
    Flipped,
    #[serde(rename = "flipped_90", alias = "flipped-90")]
    Flipped90,
    #[serde(rename = "flipped_180", alias = "flipped-180")]
    Flipped180,
    #[serde(rename = "flipped_270", alias = "flipped-270")]
    Flipped270,
}

impl OutputTransform {
    pub fn parse_str(raw: &str) -> Result<Self, String> {
        let normalized = raw.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "" | "normal" => Ok(Self::Normal),
            "90" | "rotate_90" => Ok(Self::Rotate90),
            "180" | "rotate_180" => Ok(Self::Rotate180),
            "270" | "rotate_270" => Ok(Self::Rotate270),
            "flipped" => Ok(Self::Flipped),
            "flipped_90" => Ok(Self::Flipped90),
            "flipped_180" => Ok(Self::Flipped180),
            "flipped_270" => Ok(Self::Flipped270),
            other => Err(format!(
                "invalid monitor `transform = \"{other}\"` (expected normal, 90, 180, 270, flipped, flipped_90, flipped_180, or flipped_270)"
            )),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Rotate90 => "90",
            Self::Rotate180 => "180",
            Self::Rotate270 => "270",
            Self::Flipped => "flipped",
            Self::Flipped90 => "flipped_90",
            Self::Flipped180 => "flipped_180",
            Self::Flipped270 => "flipped_270",
        }
    }

    #[must_use]
    pub const fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Rotate90 | Self::Rotate270 | Self::Flipped90 | Self::Flipped270
        )
    }

    /// Returns `(w, h)` after applying rotation axis swap when 90/270-degree transformed.
    #[must_use]
    pub const fn transform_size(self, w: u32, h: u32) -> (u32, u32) {
        if self.swaps_axes() { (h, w) } else { (w, h) }
    }
}

/// Computes the closest fractional scale exactly representable by `wp-fractional-scale-v1` (`N/120`).
#[must_use]
pub fn closest_representable_scale(scale: f64) -> f64 {
    const DENOM: f64 = 120.0;
    if !scale.is_finite() || scale <= 0.0 {
        return 1.0;
    }
    ((scale.clamp(0.5, 4.0) * DENOM).round() / DENOM).clamp(0.5, 4.0)
}

/// Computes automatic HiDPI scale (`0.25`-step quantized) from physical pixel mode and millimeter
/// dimensions, following Mutter/Wayland physical DPI thresholds and falling back cleanly to `1.0`
/// when physical dimensions are missing/zero (virtual or EDID-less displays).
#[must_use]
pub fn compute_auto_scale(phys_w_px: u32, phys_h_px: u32, phys_w_mm: i32, phys_h_mm: i32) -> f64 {
    const MIN_SCALE_STEPS: i32 = 4; // 1.0
    const MAX_SCALE_STEPS: i32 = 16; // 4.0
    const MOBILE_TARGET_DPI: f64 = 135.0;
    const LARGE_TARGET_DPI: f64 = 110.0;
    const LARGE_MIN_INCHES: f64 = 20.0;
    const MIN_LOGICAL_AREA: f64 = 800.0 * 480.0;

    if phys_w_mm <= 0 || phys_h_mm <= 0 || phys_w_px == 0 || phys_h_px == 0 {
        return 1.0;
    }
    let diag_mm_sq = f64::from(phys_w_mm).powi(2) + f64::from(phys_h_mm).powi(2);
    let diag_inches = diag_mm_sq.sqrt() / 25.4;
    if !diag_inches.is_finite() || diag_inches <= 0.0 {
        return 1.0;
    }

    let diag_px = (f64::from(phys_w_px).powi(2) + f64::from(phys_h_px).powi(2)).sqrt();
    let physical_dpi = diag_px / diag_inches;
    if !physical_dpi.is_finite() || physical_dpi <= 0.0 {
        return 1.0;
    }

    let target_dpi = if diag_inches < LARGE_MIN_INCHES {
        MOBILE_TARGET_DPI
    } else {
        LARGE_TARGET_DPI
    };
    let mut perfect_scale = physical_dpi / target_dpi;
    if physical_dpi >= 192.0 && perfect_scale < 2.0 {
        perfect_scale = 2.0;
    }

    let mut best_scale = 1.0;
    let mut best_diff = f64::INFINITY;
    for step in MIN_SCALE_STEPS..=MAX_SCALE_STEPS {
        let candidate = f64::from(step) / 4.0;
        let logical_w = (f64::from(phys_w_px) / candidate).round();
        let logical_h = (f64::from(phys_h_px) / candidate).round();
        if logical_w * logical_h < MIN_LOGICAL_AREA && step > MIN_SCALE_STEPS {
            continue;
        }
        let diff = (candidate - perfect_scale).abs();
        if diff < best_diff {
            best_diff = diff;
            best_scale = candidate;
        }
    }
    best_scale
}

/// Persisted per-connector display & workspace state restored across hotplug cycles.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedOutputConfig {
    pub active_ws_id: Option<WorkspaceId>,
    pub position: Option<(i32, i32)>,
    pub scale: Option<f64>,
    pub transform: Option<OutputTransform>,
    pub mode: Option<(i32, i32, u32)>,
    pub enabled: bool,
}

impl Default for SavedOutputConfig {
    fn default() -> Self {
        Self {
            active_ws_id: None,
            position: None,
            scale: None,
            transform: None,
            mode: None,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputLayoutEntry {
    pub name: String,
    pub logical_w: u32,
    pub logical_h: u32,
    pub scale: f64,
    pub transform: OutputTransform,
    pub explicit_pos: Option<(i32, i32)>,
    pub resolved_pos: (i32, i32),
    pub enabled: bool,
}

impl OutputLayoutEntry {
    #[must_use]
    pub fn rect(&self) -> LogicalRect {
        LogicalRect::new(
            self.resolved_pos.0,
            self.resolved_pos.1,
            self.logical_w.max(1),
            self.logical_h.max(1),
        )
    }
}

/// Placement and mode parameters supplied when upserting an output in [`OutputLayout`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputLayoutSpec {
    pub logical_w: u32,
    pub logical_h: u32,
    pub scale: f64,
    pub transform: OutputTransform,
    pub explicit_pos: Option<(i32, i32)>,
    pub enabled: bool,
}

/// Global 2D multi-output layout solver with overlap prevention, pointer clamping/edge-crossing,
/// directional neighbor resolution, and hotplug state memory.
#[derive(Debug, Clone, Default)]
pub struct OutputLayout {
    entries: Vec<OutputLayoutEntry>,
    hotplug_memory: HashMap<String, SavedOutputConfig>,
}

impl OutputLayout {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert_output(&mut self, name: impl Into<String>, spec: OutputLayoutSpec) {
        let name = name.into();
        let w = spec.logical_w.max(1);
        let h = spec.logical_h.max(1);
        if let Some(existing) = self.entries.iter_mut().find(|e| e.name == name) {
            existing.logical_w = w;
            existing.logical_h = h;
            existing.scale = spec.scale;
            existing.transform = spec.transform;
            existing.explicit_pos = spec.explicit_pos;
            existing.enabled = spec.enabled;
        } else {
            self.entries.push(OutputLayoutEntry {
                name,
                logical_w: w,
                logical_h: h,
                scale: spec.scale,
                transform: spec.transform,
                explicit_pos: spec.explicit_pos,
                resolved_pos: (0, 0),
                enabled: spec.enabled,
            });
        }
        self.relayout();
    }

    pub fn remove_output(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        let removed = self.entries.len() != before;
        if removed {
            self.relayout();
        }
        removed
    }

    pub fn set_explicit_position(&mut self, name: &str, pos: Option<(i32, i32)>) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| e.name == name) else {
            return false;
        };
        entry.explicit_pos = pos;
        self.relayout();
        true
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| e.name == name) else {
            return false;
        };
        entry.enabled = enabled;
        self.relayout();
        true
    }

    #[must_use]
    pub fn entries(&self) -> &[OutputLayoutEntry] {
        &self.entries
    }

    pub fn enabled_entries(&self) -> impl Iterator<Item = &OutputLayoutEntry> {
        self.entries.iter().filter(|e| e.enabled)
    }

    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&OutputLayoutEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    #[must_use]
    pub fn rect_of(&self, name: &str) -> Option<LogicalRect> {
        self.entries
            .iter()
            .find(|e| e.name == name && e.enabled)
            .map(OutputLayoutEntry::rect)
    }

    pub fn record_hotplug_state(&mut self, name: impl Into<String>, saved: SavedOutputConfig) {
        self.hotplug_memory.insert(name.into(), saved);
    }

    #[must_use]
    pub fn saved_hotplug_state(&self, name: &str) -> Option<&SavedOutputConfig> {
        self.hotplug_memory.get(name)
    }

    #[must_use]
    pub fn saved_hotplug_state_mut(&mut self, name: &str) -> &mut SavedOutputConfig {
        self.hotplug_memory.entry(name.to_owned()).or_default()
    }

    /// Recomputes overlap-free 2D positions: explicit `(x, y)` entries first (resolving collisions
    /// cleanly without overlap), followed by left-to-right auto-placement for unpositioned entries.
    pub fn relayout(&mut self) {
        let mut placed: Vec<(usize, LogicalRect)> = Vec::with_capacity(self.entries.len());

        // Pass 1: explicit-positioned enabled outputs.
        for (idx, entry) in self.entries.iter().enumerate() {
            if !entry.enabled {
                continue;
            }
            if let Some((ex, ey)) = entry.explicit_pos {
                let resolved = resolve_non_overlapping_explicit(
                    ex,
                    ey,
                    entry.logical_w,
                    entry.logical_h,
                    &placed,
                );
                placed.push((idx, resolved));
            }
        }

        // Pass 2: auto-placed enabled outputs (left-to-right along y=0, advancing past overlaps).
        for (idx, entry) in self.entries.iter().enumerate() {
            if !entry.enabled || entry.explicit_pos.is_some() {
                continue;
            }
            let resolved = auto_place_left_to_right(entry.logical_w, entry.logical_h, &placed);
            placed.push((idx, resolved));
        }

        for (idx, rect) in placed {
            self.entries[idx].resolved_pos = (rect.x, rect.y);
        }
    }

    #[must_use]
    pub fn output_at_point(&self, pos: LogicalPoint) -> Option<(&str, LogicalRect)> {
        self.enabled_entries().find_map(|entry| {
            let r = entry.rect();
            if rect_contains_point(r, pos) {
                Some((entry.name.as_str(), r))
            } else {
                None
            }
        })
    }

    /// Clamps `pos` to remain inside at least one enabled output rectangle (projecting onto the
    /// nearest valid boundary point across all active 2D outputs when `pos` lies in dead space).
    #[must_use]
    pub fn clamp_pointer(&self, pos: LogicalPoint) -> LogicalPoint {
        if self.enabled_entries().next().is_none() {
            return LogicalPoint {
                x: pos.x.clamp(0.0, 1919.0),
                y: pos.y.clamp(0.0, 1079.0),
            };
        }
        if self.output_at_point(pos).is_some() {
            return pos;
        }

        let mut best_pt = pos;
        let mut best_dist_sq = f64::INFINITY;
        for entry in self.enabled_entries() {
            let r = entry.rect();
            let candidate = clamp_point_inside_rect(pos, r);
            let dx = pos.x - candidate.x;
            let dy = pos.y - candidate.y;
            let dist_sq = dx * dx + dy * dy;
            if dist_sq < best_dist_sq {
                best_dist_sq = dist_sq;
                best_pt = candidate;
            }
        }
        best_pt
    }

    /// Moves pointer from `from` by `delta`, preserving subpixel positions inside outputs and
    /// bridging across non-contiguous gaps when crossing an output edge toward another aligned output.
    #[must_use]
    pub fn step_pointer(&self, from: LogicalPoint, delta: LogicalPoint) -> LogicalPoint {
        let target = LogicalPoint {
            x: from.x + delta.x,
            y: from.y + delta.y,
        };
        if self.output_at_point(target).is_some() {
            return target;
        }

        if let Some((src_name, src_rect)) = self.output_at_point(from) {
            let src_left = f64::from(src_rect.x);
            let src_top = f64::from(src_rect.y);
            let src_right = f64::from(src_rect.x + src_rect.w as i32);
            let src_bottom = f64::from(src_rect.y + src_rect.h as i32);

            if delta.x > 0.0 && target.x >= src_right {
                let y_cross = target.y.clamp(src_top, (src_bottom - 1.0).max(src_top));
                if let Some(dst) = self
                    .enabled_entries()
                    .filter(|e| e.name != src_name)
                    .map(OutputLayoutEntry::rect)
                    .filter(|r| {
                        f64::from(r.x) >= src_right
                            && y_cross >= f64::from(r.y)
                            && y_cross < f64::from(r.y + r.h as i32)
                    })
                    .min_by_key(|r| r.x)
                {
                    let overshoot = target.x - src_right;
                    return LogicalPoint {
                        x: (f64::from(dst.x) + overshoot)
                            .min((f64::from(dst.x + dst.w as i32) - 0.001).max(f64::from(dst.x))),
                        y: y_cross,
                    };
                }
            } else if delta.x < 0.0 && target.x < src_left {
                let y_cross = target.y.clamp(src_top, (src_bottom - 1.0).max(src_top));
                if let Some(dst) = self
                    .enabled_entries()
                    .filter(|e| e.name != src_name)
                    .map(OutputLayoutEntry::rect)
                    .filter(|r| {
                        f64::from(r.x + r.w as i32) <= src_left
                            && y_cross >= f64::from(r.y)
                            && y_cross < f64::from(r.y + r.h as i32)
                    })
                    .max_by_key(|r| r.x + r.w as i32)
                {
                    let dst_left = f64::from(dst.x);
                    let dst_right = f64::from(dst.x + dst.w as i32);
                    let overshoot = src_left - target.x;
                    return LogicalPoint {
                        x: (dst_right - overshoot)
                            .clamp(dst_left, (dst_right - 0.001).max(dst_left)),
                        y: y_cross,
                    };
                }
            }

            if delta.y > 0.0 && target.y >= src_bottom {
                let x_cross = target.x.clamp(src_left, (src_right - 1.0).max(src_left));
                if let Some(dst) = self
                    .enabled_entries()
                    .filter(|e| e.name != src_name)
                    .map(OutputLayoutEntry::rect)
                    .filter(|r| {
                        f64::from(r.y) >= src_bottom
                            && x_cross >= f64::from(r.x)
                            && x_cross < f64::from(r.x + r.w as i32)
                    })
                    .min_by_key(|r| r.y)
                {
                    let overshoot = target.y - src_bottom;
                    return LogicalPoint {
                        x: x_cross,
                        y: (f64::from(dst.y) + overshoot)
                            .min((f64::from(dst.y + dst.h as i32) - 0.001).max(f64::from(dst.y))),
                    };
                }
            } else if delta.y < 0.0 && target.y < src_top {
                let x_cross = target.x.clamp(src_left, (src_right - 1.0).max(src_left));
                if let Some(dst) = self
                    .enabled_entries()
                    .filter(|e| e.name != src_name)
                    .map(OutputLayoutEntry::rect)
                    .filter(|r| {
                        f64::from(r.y + r.h as i32) <= src_top
                            && x_cross >= f64::from(r.x)
                            && x_cross < f64::from(r.x + r.w as i32)
                    })
                    .max_by_key(|r| r.y + r.h as i32)
                {
                    let dst_top = f64::from(dst.y);
                    let dst_bottom = f64::from(dst.y + dst.h as i32);
                    let overshoot = src_top - target.y;
                    return LogicalPoint {
                        x: x_cross,
                        y: (dst_bottom - overshoot)
                            .clamp(dst_top, (dst_bottom - 0.001).max(dst_top)),
                    };
                }
            }
        }

        self.clamp_pointer(target)
    }

    /// Resolves the best enabled neighbor output strictly in `dir` relative to `from_name`.
    #[must_use]
    pub fn neighbor_in_direction(&self, from_name: &str, dir: CardinalDir) -> Option<&str> {
        let src = self.rect_of(from_name)?;
        let src_cx = f64::from(src.x) + f64::from(src.w) * 0.5;
        let src_cy = f64::from(src.y) + f64::from(src.h) * 0.5;

        let mut best: Option<(&str, bool, i64, f64)> = None;
        for entry in self.enabled_entries() {
            if entry.name == from_name {
                continue;
            }
            let r = entry.rect();
            let dst_cx = f64::from(r.x) + f64::from(r.w) * 0.5;
            let dst_cy = f64::from(r.y) + f64::from(r.h) * 0.5;

            let (in_half_plane, ortho_overlaps, edge_gap) = match dir {
                CardinalDir::Left => (
                    dst_cx < src_cx,
                    ranges_overlap(src.y, src.y + src.h as i32, r.y, r.y + r.h as i32),
                    i64::from(src.x) - i64::from(r.x + r.w as i32),
                ),
                CardinalDir::Right => (
                    dst_cx > src_cx,
                    ranges_overlap(src.y, src.y + src.h as i32, r.y, r.y + r.h as i32),
                    i64::from(r.x) - i64::from(src.x + src.w as i32),
                ),
                CardinalDir::Up => (
                    dst_cy < src_cy,
                    ranges_overlap(src.x, src.x + src.w as i32, r.x, r.x + r.w as i32),
                    i64::from(src.y) - i64::from(r.y + r.h as i32),
                ),
                CardinalDir::Down => (
                    dst_cy > src_cy,
                    ranges_overlap(src.x, src.x + src.w as i32, r.x, r.x + r.w as i32),
                    i64::from(r.y) - i64::from(src.y + src.h as i32),
                ),
            };
            if !in_half_plane {
                continue;
            }
            let dist_sq = (dst_cx - src_cx).powi(2) + (dst_cy - src_cy).powi(2);
            let gap_score = edge_gap.max(0);
            let is_better = match best {
                None => true,
                Some((_, best_overlap, best_gap, best_dist)) => {
                    (ortho_overlaps && !best_overlap)
                        || (ortho_overlaps == best_overlap
                            && (gap_score < best_gap
                                || (gap_score == best_gap && dist_sq < best_dist)))
                }
            };
            if is_better {
                best = Some((entry.name.as_str(), ortho_overlaps, gap_score, dist_sq));
            }
        }
        best.map(|(name, _, _, _)| name)
    }
}

fn rects_overlap(a: LogicalRect, b: LogicalRect) -> bool {
    let ax2 = i64::from(a.x) + i64::from(a.w);
    let ay2 = i64::from(a.y) + i64::from(a.h);
    let bx2 = i64::from(b.x) + i64::from(b.w);
    let by2 = i64::from(b.y) + i64::from(b.h);
    i64::from(a.x) < bx2 && ax2 > i64::from(b.x) && i64::from(a.y) < by2 && ay2 > i64::from(b.y)
}

fn ranges_overlap(a0: i32, a1: i32, b0: i32, b1: i32) -> bool {
    a0 < b1 && a1 > b0
}

fn rect_contains_point(r: LogicalRect, p: LogicalPoint) -> bool {
    p.x >= f64::from(r.x)
        && p.y >= f64::from(r.y)
        && p.x < f64::from(r.x + r.w as i32)
        && p.y < f64::from(r.y + r.h as i32)
}

fn clamp_point_inside_rect(p: LogicalPoint, r: LogicalRect) -> LogicalPoint {
    let min_x = f64::from(r.x);
    let min_y = f64::from(r.y);
    let max_x = f64::from(r.x + r.w.saturating_sub(1) as i32).max(min_x);
    let max_y = f64::from(r.y + r.h.saturating_sub(1) as i32).max(min_y);
    LogicalPoint {
        x: p.x.clamp(min_x, max_x),
        y: p.y.clamp(min_y, max_y),
    }
}

fn overlaps_any(candidate: LogicalRect, placed: &[(usize, LogicalRect)]) -> Option<LogicalRect> {
    placed
        .iter()
        .find(|(_, r)| rects_overlap(candidate, *r))
        .map(|(_, r)| *r)
}

fn resolve_non_overlapping_explicit(
    ex: i32,
    ey: i32,
    w: u32,
    h: u32,
    placed: &[(usize, LogicalRect)],
) -> LogicalRect {
    let mut candidate = LogicalRect::new(ex, ey, w.max(1), h.max(1));
    if overlaps_any(candidate, placed).is_none() {
        return candidate;
    }
    // Deterministically push right past any overlapping rect until overlap-free.
    for _ in 0..=placed.len() {
        if let Some(collider) = overlaps_any(candidate, placed) {
            candidate.x = collider.x.saturating_add(collider.w as i32);
        } else {
            break;
        }
    }
    candidate
}

fn auto_place_left_to_right(w: u32, h: u32, placed: &[(usize, LogicalRect)]) -> LogicalRect {
    let mut x = 0_i32;
    let w = w.max(1);
    let h = h.max(1);
    for _ in 0..=placed.len() {
        let candidate = LogicalRect::new(x, 0, w, h);
        if let Some(collider) = overlaps_any(candidate, placed) {
            x = collider.x.saturating_add(collider.w as i32);
        } else {
            return candidate;
        }
    }
    LogicalRect::new(x, 0, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_eight_transforms_and_dashed_aliases() {
        let cases = [
            ("normal", OutputTransform::Normal),
            ("90", OutputTransform::Rotate90),
            ("180", OutputTransform::Rotate180),
            ("270", OutputTransform::Rotate270),
            ("flipped", OutputTransform::Flipped),
            ("flipped_90", OutputTransform::Flipped90),
            ("flipped-90", OutputTransform::Flipped90),
            ("flipped_180", OutputTransform::Flipped180),
            ("flipped-180", OutputTransform::Flipped180),
            ("flipped_270", OutputTransform::Flipped270),
            ("flipped-270", OutputTransform::Flipped270),
        ];
        for (raw, expected) in cases {
            assert_eq!(OutputTransform::parse_str(raw).unwrap(), expected);
        }
        assert!(OutputTransform::parse_str("bogus").is_err());
        assert_eq!(
            OutputTransform::Rotate90.transform_size(1920, 1080),
            (1080, 1920)
        );
        assert_eq!(
            OutputTransform::Flipped270.transform_size(2560, 1600),
            (1600, 2560)
        );
        assert_eq!(
            OutputTransform::Flipped180.transform_size(2560, 1600),
            (2560, 1600)
        );
    }

    #[test]
    fn auto_scale_selects_retina_hidpi_and_falls_back_to_one_on_unknown_mm() {
        // 14.2" Apple MacBook Pro Liquid Retina XDR: 3024x1964 @ 302x196 mm (~254 DPI) -> 2.0
        assert_eq!(compute_auto_scale(3024, 1964, 302, 196), 2.0);
        // 16.2" Apple MacBook Pro Liquid Retina XDR: 3456x2234 @ 345x223 mm (~254 DPI) -> 2.0
        assert_eq!(compute_auto_scale(3456, 2234, 345, 223), 2.0);
        // 27" 5K Apple Studio Display: 5120x2880 @ 597x336 mm (~218 DPI) -> 2.0
        assert_eq!(compute_auto_scale(5120, 2880, 597, 336), 2.0);
        // 27" 4K desktop display: 3840x2160 @ 597x336 mm (~163 DPI) -> 1.5
        assert_eq!(compute_auto_scale(3840, 2160, 597, 336), 1.5);
        // 24" 1080p desktop display: 1920x1080 @ 527x296 mm (~93 DPI) -> 1.0
        assert_eq!(compute_auto_scale(1920, 1080, 527, 296), 1.0);
        // Virtual / EDID-less output with 0mm -> safe 1.0 fallback
        assert_eq!(compute_auto_scale(1920, 1080, 0, 0), 1.0);
        assert_eq!(compute_auto_scale(3024, 1964, 302, 0), 1.0);
    }

    #[test]
    fn layout_resolves_overlaps_auto_places_left_to_right_and_crosses_edges_smoothly() {
        let mut layout = OutputLayout::new();
        // Two explicit overlapping outputs both requesting (0, 0) -> second shifts to (1920, 0).
        layout.upsert_output(
            "DP-1",
            OutputLayoutSpec {
                logical_w: 1920,
                logical_h: 1080,
                scale: 1.0,
                transform: OutputTransform::Normal,
                explicit_pos: Some((0, 0)),
                enabled: true,
            },
        );
        layout.upsert_output(
            "HDMI-A-1",
            OutputLayoutSpec {
                logical_w: 1920,
                logical_h: 1080,
                scale: 1.0,
                transform: OutputTransform::Normal,
                explicit_pos: Some((0, 0)),
                enabled: true,
            },
        );
        // Unpositioned output auto-places left-to-right at (3840, 0).
        layout.upsert_output(
            "eDP-1",
            OutputLayoutSpec {
                logical_w: 1512,
                logical_h: 982,
                scale: 2.0,
                transform: OutputTransform::Normal,
                explicit_pos: None,
                enabled: true,
            },
        );

        assert_eq!(
            layout.rect_of("DP-1"),
            Some(LogicalRect::new(0, 0, 1920, 1080))
        );
        assert_eq!(
            layout.rect_of("HDMI-A-1"),
            Some(LogicalRect::new(1920, 0, 1920, 1080))
        );
        assert_eq!(
            layout.rect_of("eDP-1"),
            Some(LogicalRect::new(3840, 0, 1512, 982))
        );

        assert_eq!(
            layout.neighbor_in_direction("DP-1", CardinalDir::Right),
            Some("HDMI-A-1")
        );
        assert_eq!(
            layout.neighbor_in_direction("eDP-1", CardinalDir::Left),
            Some("HDMI-A-1")
        );

        // Non-contiguous L-shaped / gapped layout pointer crossing & clamping.
        let mut gapped = OutputLayout::new();
        gapped.upsert_output(
            "Left",
            OutputLayoutSpec {
                logical_w: 1920,
                logical_h: 1080,
                scale: 1.0,
                transform: OutputTransform::Normal,
                explicit_pos: Some((0, 0)),
                enabled: true,
            },
        );
        gapped.upsert_output(
            "Right",
            OutputLayoutSpec {
                logical_w: 2560,
                logical_h: 1440,
                scale: 1.0,
                transform: OutputTransform::Normal,
                explicit_pos: Some((2100, -180)),
                enabled: true,
            },
        );

        let crossed_right = gapped.step_pointer(
            LogicalPoint {
                x: 1918.0,
                y: 500.0,
            },
            LogicalPoint { x: 6.0, y: 0.0 },
        );
        assert_eq!(crossed_right.x, 2104.0);
        assert_eq!(crossed_right.y, 500.0);

        let crossed_left = gapped.step_pointer(
            LogicalPoint {
                x: 2102.0,
                y: 500.0,
            },
            LogicalPoint { x: -6.0, y: 0.0 },
        );
        assert_eq!(crossed_left.x, 1916.0);
        assert_eq!(crossed_left.y, 500.0);

        // Clamping below bottom overhang of Right keeps pointer inside Right rather than invalid space.
        let clamped_overhang = gapped.clamp_pointer(LogicalPoint {
            x: 2050.0,
            y: 1200.0,
        });
        assert_eq!(
            clamped_overhang,
            LogicalPoint {
                x: 2100.0,
                y: 1200.0
            }
        );
    }
}
