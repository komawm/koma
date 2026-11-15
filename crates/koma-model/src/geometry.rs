use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LogicalPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl LogicalRect {
    #[must_use]
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    #[must_use]
    pub fn contains_point(&self, pt: LogicalPoint) -> bool {
        let px = pt.x.round() as i32;
        let py = pt.y.round() as i32;
        px >= self.x
            && py >= self.y
            && px < self.x.saturating_add(self.w as i32)
            && py < self.y.saturating_add(self.h as i32)
    }
}

/// Logical safe-area insets (e.g. MacBook Pro camera notch / rounded screen cutouts)
/// subtracted in addition to `wlr-layer-shell` `exclusive_zone` when computing `work_area`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SafeAreaInsets {
    pub top: i32,
    pub bottom: i32,
    pub left: i32,
    pub right: i32,
}

impl SafeAreaInsets {
    #[must_use]
    pub fn clamped_non_negative(self) -> Self {
        Self {
            top: self.top.max(0),
            bottom: self.bottom.max(0),
            left: self.left.max(0),
            right: self.right.max(0),
        }
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.top <= 0 && self.bottom <= 0 && self.left <= 0 && self.right <= 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LogicalViewport {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_area: Option<LogicalRect>,
}

impl Default for LogicalViewport {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            scale: 1.0,
            work_area: None,
        }
    }
}

impl LogicalViewport {
    #[must_use]
    pub const fn new(width: u32, height: u32, scale: f64) -> Self {
        Self {
            width,
            height,
            scale,
            work_area: None,
        }
    }

    #[must_use]
    pub const fn full_rect(self) -> LogicalRect {
        LogicalRect::new(0, 0, self.width, self.height)
    }

    /// Clamps a raw `non_exclusive_zone` rectangle safely within output logical bounds,
    /// guaranteeing positive non-zero dimensions even if exclusive bars exhaust the output.
    #[must_use]
    pub fn sanitized_work_area(
        self,
        raw_x: i32,
        raw_y: i32,
        raw_w: i32,
        raw_h: i32,
    ) -> LogicalRect {
        let out_w = self.width.max(1) as i32;
        let out_h = self.height.max(1) as i32;
        let x0 = raw_x.clamp(0, out_w - 1);
        let y0 = raw_y.clamp(0, out_h - 1);
        let raw_right = raw_x.saturating_add(raw_w);
        let raw_bottom = raw_y.saturating_add(raw_h);
        let x1 = raw_right.clamp(x0 + 1, out_w);
        let y1 = raw_bottom.clamp(y0 + 1, out_h);
        LogicalRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
    }

    /// Subtracts exclusive edge insets (`top`, `bottom`, `left`, `right`) safely within bounds.
    #[must_use]
    pub fn work_area_from_edges(self, top: i32, bottom: i32, left: i32, right: i32) -> LogicalRect {
        let raw_w = (self.width as i32)
            .saturating_sub(left.max(0))
            .saturating_sub(right.max(0));
        let raw_h = (self.height as i32)
            .saturating_sub(top.max(0))
            .saturating_sub(bottom.max(0));
        self.sanitized_work_area(left.max(0), top.max(0), raw_w, raw_h)
    }

    /// Returns the usable non-exclusive workarea rectangle (falling back to `full_rect()`).
    #[must_use]
    pub fn work_area_rect(self) -> LogicalRect {
        match self.work_area {
            Some(wa) => self.sanitized_work_area(
                wa.x,
                wa.y,
                wa.w.min(i32::MAX as u32) as i32,
                wa.h.min(i32::MAX as u32) as i32,
            ),
            None => self.full_rect(),
        }
    }

    /// Returns a copy of `self` updated with a clamped `non_exclusive_zone` workarea.
    #[must_use]
    pub fn with_work_area_zone(mut self, loc_x: i32, loc_y: i32, width: i32, height: i32) -> Self {
        self.work_area = Some(self.sanitized_work_area(loc_x, loc_y, width, height));
        self
    }

    /// Composes `wlr-layer-shell` `non_exclusive_zone` with monitor `safe_area` insets so
    /// tiled windows avoid both panels and hardware cutouts (such as a camera notch).
    #[must_use]
    pub fn with_work_area_zone_and_safe_area(
        mut self,
        loc_x: i32,
        loc_y: i32,
        width: i32,
        height: i32,
        safe_area: SafeAreaInsets,
    ) -> Self {
        let layer_rect = self.sanitized_work_area(loc_x, loc_y, width, height);
        let safe = safe_area.clamped_non_negative();
        if safe.is_zero() {
            self.work_area = Some(layer_rect);
            return self;
        }
        let out_w = self.width.max(1).min(i32::MAX as u32) as i32;
        let out_h = self.height.max(1).min(i32::MAX as u32) as i32;
        let safe_x0 = safe.left.clamp(0, out_w - 1);
        let safe_y0 = safe.top.clamp(0, out_h - 1);
        let safe_x1 = (out_w - safe.right).clamp(safe_x0 + 1, out_w);
        let safe_y1 = (out_h - safe.bottom).clamp(safe_y0 + 1, out_h);

        let x0 = layer_rect.x.max(safe_x0).clamp(0, out_w - 1);
        let y0 = layer_rect.y.max(safe_y0).clamp(0, out_h - 1);
        let x1 = (layer_rect.x + layer_rect.w.min(i32::MAX as u32) as i32)
            .min(safe_x1)
            .clamp(x0 + 1, out_w);
        let y1 = (layer_rect.y + layer_rect.h.min(i32::MAX as u32) as i32)
            .min(safe_y1)
            .clamp(y0 + 1, out_h);
        self.work_area = Some(LogicalRect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32));
        self
    }
}

/// 9-anchor window-relative reference points (`LUA_API_SPEC.md` §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Anchor9 {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

impl Anchor9 {
    #[must_use]
    pub fn origin_in(self, rect: LogicalRect) -> LogicalPoint {
        let x0 = f64::from(rect.x);
        let y0 = f64::from(rect.y);
        let w = f64::from(rect.w);
        let h = f64::from(rect.h);
        match self {
            Self::TopLeft => LogicalPoint { x: x0, y: y0 },
            Self::Top => LogicalPoint {
                x: x0 + w * 0.5,
                y: y0,
            },
            Self::TopRight => LogicalPoint { x: x0 + w, y: y0 },
            Self::Left => LogicalPoint {
                x: x0,
                y: y0 + h * 0.5,
            },
            Self::Center => LogicalPoint {
                x: x0 + w * 0.5,
                y: y0 + h * 0.5,
            },
            Self::Right => LogicalPoint {
                x: x0 + w,
                y: y0 + h * 0.5,
            },
            Self::BottomLeft => LogicalPoint { x: x0, y: y0 + h },
            Self::Bottom => LogicalPoint {
                x: x0 + w * 0.5,
                y: y0 + h,
            },
            Self::BottomRight => LogicalPoint {
                x: x0 + w,
                y: y0 + h,
            },
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GridLabelError {
    #[error("invalid 15x15 grid_225 label `{0}`; expected A1..O15")]
    InvalidGrid225Label(String),
}

/// Resolution-resilient target coordinate inside a window (`LUA_API_SPEC.md` §5.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AnchorPoint {
    AnchorOffset { anchor: Anchor9, dx: f64, dy: f64 },
    Normalized { u: f64, v: f64 },
    Grid225(String),
}

impl AnchorPoint {
    /// Resolves this relative anchor point into workspace-global logical coordinates.
    pub fn resolve_in(&self, win_rect: LogicalRect) -> Result<LogicalPoint, GridLabelError> {
        match self {
            Self::AnchorOffset { anchor, dx, dy } => {
                let base = anchor.origin_in(win_rect);
                Ok(LogicalPoint {
                    x: base.x + dx,
                    y: base.y + dy,
                })
            }
            Self::Normalized { u, v } => Ok(LogicalPoint {
                x: f64::from(win_rect.x) + u.clamp(0.0, 1.0) * f64::from(win_rect.w),
                y: f64::from(win_rect.y) + v.clamp(0.0, 1.0) * f64::from(win_rect.h),
            }),
            Self::Grid225(label) => resolve_grid_225_center(label, win_rect),
        }
    }
}

/// Resolves a `15x15` (`225`-cell, `A1`..`O15`) grid label tailored to `gemini-decision-1` `choice`.
pub fn resolve_grid_225_center(
    label: &str,
    rect: LogicalRect,
) -> Result<LogicalPoint, GridLabelError> {
    let bytes = label.trim().as_bytes();
    if bytes.len() < 2 || bytes.len() > 3 {
        return Err(GridLabelError::InvalidGrid225Label(label.to_owned()));
    }
    let col_char = bytes[0].to_ascii_uppercase();
    if !(b'A'..=b'O').contains(&col_char) {
        return Err(GridLabelError::InvalidGrid225Label(label.to_owned()));
    }
    let col_idx = u32::from(col_char - b'A');
    let row_num: u32 = label[1..]
        .parse()
        .map_err(|_| GridLabelError::InvalidGrid225Label(label.to_owned()))?;
    if !(1..=15).contains(&row_num) {
        return Err(GridLabelError::InvalidGrid225Label(label.to_owned()));
    }
    let row_idx = row_num - 1;
    let cell_w = f64::from(rect.w) / 15.0;
    let cell_h = f64::from(rect.h) / 15.0;
    Ok(LogicalPoint {
        x: f64::from(rect.x) + (f64::from(col_idx) + 0.5) * cell_w,
        y: f64::from(rect.y) + (f64::from(row_idx) + 0.5) * cell_h,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_zone_subtracts_all_four_edges_without_breaking_bounds() {
        let vp = LogicalViewport::new(1920, 1080, 1.0);
        let top_only = vp.work_area_from_edges(32, 0, 0, 0);
        assert_eq!(top_only, LogicalRect::new(0, 32, 1920, 1048));

        let all_four = vp.work_area_from_edges(32, 48, 60, 80);
        assert_eq!(all_four, LogicalRect::new(60, 32, 1780, 1000));
        assert_eq!(vp.full_rect(), LogicalRect::new(0, 0, 1920, 1080));
    }

    #[test]
    fn exclusive_zone_guards_zero_and_negative_remaining_dimensions() {
        let vp = LogicalViewport::new(1920, 1080, 1.0);
        // Oversized top + bottom + left + right exclusive zones never go <= 0 or escape bounds
        let exhausted = vp.work_area_from_edges(700, 600, 1200, 1000);
        assert!(exhausted.x >= 0 && exhausted.x < 1920);
        assert!(exhausted.y >= 0 && exhausted.y < 1080);
        assert!(exhausted.w >= 1);
        assert!(exhausted.h >= 1);
        assert!(exhausted.x + exhausted.w as i32 <= 1920);
        assert!(exhausted.y + exhausted.h as i32 <= 1080);

        let negative_raw = vp.sanitized_work_area(-40, 2500, -100, -300);
        assert!(negative_raw.w >= 1);
        assert!(negative_raw.h >= 1);
        assert!(negative_raw.x >= 0 && negative_raw.x + negative_raw.w as i32 <= 1920);
        assert!(negative_raw.y >= 0 && negative_raw.y + negative_raw.h as i32 <= 1080);
    }

    #[test]
    fn viewport_work_area_falls_back_to_full_rect_unless_set() {
        let vp = LogicalViewport::new(1920, 1080, 1.5);
        assert_eq!(vp.work_area, None);
        assert_eq!(vp.work_area_rect(), LogicalRect::new(0, 0, 1920, 1080));

        let with_bar = vp.with_work_area_zone(0, 36, 1920, 1044);
        assert_eq!(
            with_bar.work_area_rect(),
            LogicalRect::new(0, 36, 1920, 1044)
        );
    }

    #[test]
    fn safe_area_composes_cleanly_with_layer_shell_exclusive_zone() {
        // 14" MacBook Pro logical size (1512x982) with top camera notch safe_area.top = 38.
        let vp = LogicalViewport::new(1512, 982, 2.0);
        let notch = SafeAreaInsets {
            top: 38,
            bottom: 0,
            left: 0,
            right: 0,
        };

        // No exclusive layer panels -> notch safe_area reserves top 38px.
        let notch_only = vp.with_work_area_zone_and_safe_area(0, 0, 1512, 982, notch);
        assert_eq!(
            notch_only.work_area_rect(),
            LogicalRect::new(0, 38, 1512, 944)
        );

        // Top panel smaller than notch (30px) still leaves notch top=38 protected.
        let short_bar = vp.with_work_area_zone_and_safe_area(0, 30, 1512, 952, notch);
        assert_eq!(
            short_bar.work_area_rect(),
            LogicalRect::new(0, 38, 1512, 944)
        );

        // Taller top panel (48px) + bottom bar (32px) reserves both beyond the notch.
        let tall_bars = vp.with_work_area_zone_and_safe_area(0, 48, 1512, 902, notch);
        assert_eq!(
            tall_bars.work_area_rect(),
            LogicalRect::new(0, 48, 1512, 902)
        );

        // Left/right/bottom safe_area + top layer panel compose via intersection.
        let multi_inset = SafeAreaInsets {
            top: 38,
            bottom: 16,
            left: 12,
            right: 14,
        };
        let composed = vp.with_work_area_zone_and_safe_area(20, 30, 1492, 952, multi_inset);
        assert_eq!(
            composed.work_area_rect(),
            LogicalRect::new(20, 38, 1478, 928)
        );
    }
}
