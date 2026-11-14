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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LogicalViewport {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

impl Default for LogicalViewport {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            scale: 1.0,
        }
    }
}

impl LogicalViewport {
    #[must_use]
    pub const fn full_rect(self) -> LogicalRect {
        LogicalRect::new(0, 0, self.width, self.height)
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
