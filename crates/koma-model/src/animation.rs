//! Analytical 2nd-order `Spring` physics engine, velocity-preserving `WindowAnimationTable`
//! (window open/close + Dwindle split-divider / geometry transitions), and `Dual Kawase Blur`
//! configuration & damage-expansion math (`Tier 0`).
//!
//! Modeled directly on:
//! - `niri/src/animation/spring.rs` (`libadwaita` / `RBBSpringAnimation` analytical ODE solver
//!   extended with exact closed-form first derivative `velocity_at` for $C^1$-continuous retargeting),
//! - `Hyprland` `decoration:blur` (`Dual Kawase` `size`, `passes`, `noise`, `contrast`, `brightness`,
//!   `vibrancy`, `xray`) and `BlurDamage.cpp` ($R = \text{size} \times 2^{\text{passes}}$).

use std::{collections::BTreeMap, time::Duration};

use koma_proto::WindowId;
use serde::{Deserialize, Serialize};

use crate::geometry::{LogicalRect, LogicalViewport};

/// Declarative spring animation configuration (`koma.appearance({ animations = { ... } })`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnimationConfig {
    pub enabled: bool,
    /// `1.0` = critically damped (zero overshoot, Niri default); `< 1.0` (e.g. `0.82`) = bouncy Hyprland spring.
    pub damping_ratio: f64,
    /// Spring stiffness $k$ (`800.0` = snappy ~180 ms settle, Niri default).
    pub stiffness: f64,
    /// Rest convergence threshold $\epsilon$.
    pub epsilon: f64,
    /// Initial centered scale factor when a new window maps (`0.88` -> `1.0`).
    pub open_scale_from: f64,
    /// Target centered scale factor when a closing window fades out (`1.0` -> `0.92`).
    pub close_scale_to: f64,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            damping_ratio: 1.0,
            stiffness: 800.0,
            epsilon: 0.0005,
            open_scale_from: 0.88,
            close_scale_to: 0.92,
        }
    }
}

impl AnimationConfig {
    #[must_use]
    pub fn spring_params(self) -> SpringParams {
        SpringParams::new(self.damping_ratio, self.stiffness, self.epsilon)
    }
}

/// Declarative multi-pass `4-tap Dual Kawase Blur` configuration (`Hyprland` `decoration:blur`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BlurConfig {
    pub enabled: bool,
    /// Kawase sample offset radius (`1..=24`, Hyprland `blur:size`, default `6`).
    pub size: u32,
    /// Downsample/upsample FBO pyramid depth (`1..=6`, Hyprland `blur:passes`, default `3`).
    pub passes: u8,
    /// Post-blur anti-banding dithering noise (`0.0..=0.2`, Hyprland default `0.0117`).
    pub noise: f32,
    /// Pre/post contrast multiplier (`0.1..=2.0`, Hyprland default `0.8916`).
    pub contrast: f32,
    /// Post-blur brightness multiplier (`0.1..=2.0`, Hyprland default `0.8172`).
    pub brightness: f32,
    /// HSL perceived-luminance saturation boost (`0.0..=1.0`, Hyprland default `0.1696`).
    pub vibrancy: f32,
    /// Sample cached blurred wallpaper backdrop rather than re-blurring intermediate tiled siblings.
    pub xray: bool,
    /// Apply frosted-glass blur behind windows even when their configured opacity is `1.0`.
    pub ignore_opacity: bool,
}

impl Default for BlurConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            size: 6,
            passes: 3,
            noise: 0.0117,
            contrast: 0.8916,
            brightness: 0.8172,
            vibrancy: 0.1696,
            xray: true,
            ignore_opacity: false,
        }
    }
}

impl BlurConfig {
    /// Effective spatial receptive-field radius in logical pixels ($R = \text{size} \times 2^{\text{passes}}$).
    ///
    /// Any surface damage inside `viewport` within `R` pixels of a blurred window can affect its
    /// Dual Kawase mip chain (`Hyprland` `BlurDamage.cpp`).
    #[must_use]
    pub const fn damage_expansion_radius_px(&self) -> i32 {
        if !self.enabled {
            return 0;
        }
        let clamped_passes = if self.passes < 1 {
            1
        } else if self.passes > 6 {
            6
        } else {
            self.passes
        };
        let clamped_size = if self.size < 1 {
            1
        } else if self.size > 24 {
            24
        } else {
            self.size
        };
        (clamped_size as i32).saturating_mul(1_i32 << clamped_passes)
    }

    /// Expands a damaged `LogicalRect` outward by `damage_expansion_radius_px`, clamped to `viewport`.
    #[must_use]
    pub fn expand_damage_rect(&self, rect: LogicalRect, viewport: LogicalViewport) -> LogicalRect {
        let r = self.damage_expansion_radius_px();
        if r <= 0 {
            return rect;
        }
        let x0 = (rect.x - r).max(0);
        let y0 = (rect.y - r).max(0);
        let x1 = (rect.x + rect.width as i32 + r).min(viewport.width as i32);
        let y1 = (rect.y + rect.height as i32 + r).min(viewport.height as i32);
        LogicalRect::new(x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
    }
}

/// Physical parameters of a unit-mass damped harmonic oscillator $m\ddot{x} + b\dot{x} + kx = 0$
/// (`niri/src/animation/spring.rs`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpringParams {
    pub damping: f64,
    pub mass: f64,
    pub stiffness: f64,
    pub epsilon: f64,
}

impl SpringParams {
    #[must_use]
    pub fn new(damping_ratio: f64, stiffness: f64, epsilon: f64) -> Self {
        let ratio = damping_ratio.max(0.05);
        let k = stiffness.max(1.0);
        let eps = epsilon.max(1e-6);
        let mass = 1.0;
        let critical_damping = 2.0 * (mass * k).sqrt();
        Self {
            damping: ratio * critical_damping,
            mass,
            stiffness: k,
            epsilon: eps,
        }
    }
}

/// Closed-form 2nd-order spring supporting exact position AND velocity evaluation at arbitrary `t`.
#[derive(Debug, Clone, Copy)]
pub struct Spring {
    pub from: f64,
    pub to: f64,
    pub initial_velocity: f64,
    pub params: SpringParams,
}

impl Spring {
    #[must_use]
    pub fn value_at(&self, t: Duration) -> f64 {
        self.sample_pos_vel(t.as_secs_f64()).0
    }

    #[must_use]
    pub fn velocity_at(&self, t: Duration) -> f64 {
        self.sample_pos_vel(t.as_secs_f64()).1
    }

    /// Evaluates analytical `(position, velocity)` at elapsed seconds `t >= 0` across critically
    /// damped, underdamped, and overdamped regimes.
    #[must_use]
    pub fn sample_pos_vel(&self, t: f64) -> (f64, f64) {
        if t <= 0.0 {
            return (self.from, self.initial_velocity);
        }
        let b = self.params.damping;
        let m = self.params.mass;
        let k = self.params.stiffness;
        let v0 = self.initial_velocity;

        let beta = b / (2.0 * m);
        let omega0 = (k / m).sqrt();
        let x0 = self.from - self.to;

        if x0.abs() <= self.params.epsilon && v0.abs() <= self.params.epsilon {
            return (self.to, 0.0);
        }

        let envelope = (-beta * t).exp();
        if !envelope.is_finite() || envelope < 1e-9 {
            return (self.to, 0.0);
        }

        if (beta - omega0).abs() <= f64::from(f32::EPSILON) {
            // Critically damped: f(t) = x0 + c2 * t, where c2 = beta * x0 + v0
            let c2 = beta * x0 + v0;
            let f = x0 + c2 * t;
            let df = c2;
            let pos = self.to + envelope * f;
            let vel = envelope * (df - beta * f);
            (pos, vel)
        } else if beta < omega0 {
            // Underdamped: f(t) = x0 * cos(w1*t) + c2 * sin(w1*t)
            let omega1 = (omega0 * omega0 - beta * beta).sqrt();
            let c2 = (beta * x0 + v0) / omega1;
            let (sin_wt, cos_wt) = (omega1 * t).sin_cos();
            let f = x0 * cos_wt + c2 * sin_wt;
            let df = -omega1 * x0 * sin_wt + omega1 * c2 * cos_wt;
            let pos = self.to + envelope * f;
            let vel = envelope * (df - beta * f);
            (pos, vel)
        } else {
            // Overdamped: f(t) = x0 * cosh(w2*t) + c2 * sinh(w2*t)
            let omega2 = (beta * beta - omega0 * omega0).sqrt();
            let c2 = (beta * x0 + v0) / omega2;
            let wt = (omega2 * t).min(40.0);
            let sinh_wt = wt.sinh();
            let cosh_wt = wt.cosh();
            let f = x0 * cosh_wt + c2 * sinh_wt;
            let df = omega2 * x0 * sinh_wt + omega2 * c2 * cosh_wt;
            let pos = self.to + envelope * f;
            let vel = envelope * (df - beta * f);
            if pos.is_finite() && vel.is_finite() {
                (pos, vel)
            } else {
                (self.to, 0.0)
            }
        }
    }

    /// Estimates settling duration (`niri/src/animation/spring.rs` lines 47-105).
    #[must_use]
    pub fn duration(&self) -> Duration {
        let beta = self.params.damping / (2.0 * self.params.mass);
        if beta <= f64::EPSILON {
            return Duration::from_secs(2);
        }
        let span = (self.to - self.from)
            .abs()
            .max(self.initial_velocity.abs() * 0.05);
        if span <= self.params.epsilon {
            return Duration::ZERO;
        }
        let est_secs = (-(self.params.epsilon / span.max(1.0)).ln() / beta).clamp(0.0, 2.5);
        Duration::from_secs_f64(est_secs)
    }
}

/// Velocity-preserving scalar spring channel for smooth mid-flight retargeting.
#[derive(Debug, Clone, Copy)]
pub struct SpringTrack {
    spring: Spring,
    start_ms: u64,
    settled: bool,
}

impl SpringTrack {
    #[must_use]
    pub fn settled_at(value: f64, params: SpringParams) -> Self {
        Self {
            spring: Spring {
                from: value,
                to: value,
                initial_velocity: 0.0,
                params,
            },
            start_ms: 0,
            settled: true,
        }
    }

    #[must_use]
    pub fn from_to(from: f64, to: f64, now_ms: u64, params: SpringParams) -> Self {
        let settled = (to - from).abs() <= params.epsilon;
        Self {
            spring: Spring {
                from,
                to,
                initial_velocity: 0.0,
                params,
            },
            start_ms: now_ms,
            settled,
        }
    }

    #[must_use]
    pub const fn target(&self) -> f64 {
        self.spring.to
    }

    #[must_use]
    pub const fn is_settled(&self) -> bool {
        self.settled
    }

    pub fn snap_to(&mut self, value: f64) {
        self.spring.from = value;
        self.spring.to = value;
        self.spring.initial_velocity = 0.0;
        self.settled = true;
    }

    /// Launches the spring from explicit `(from, initial_velocity)` toward `to` at `now_ms`
    /// (`C^1`-continuous hand-off on touchpad `GestureSwipeEnd` release velocity projection!).
    pub fn launch_from_velocity(
        &mut self,
        from: f64,
        to: f64,
        initial_velocity: f64,
        now_ms: u64,
        params: SpringParams,
    ) {
        let v0 = if initial_velocity.is_finite() {
            initial_velocity
        } else {
            0.0
        };
        self.spring = Spring {
            from,
            to,
            initial_velocity: v0,
            params,
        };
        self.start_ms = now_ms;
        self.settled = (to - from).abs() <= params.epsilon && v0.abs() <= params.epsilon * 10.0;
    }

    /// Retargets the channel to `new_to` at `now_ms`, inheriting instantaneous velocity `v(t)`!
    pub fn retarget(&mut self, new_to: f64, now_ms: u64, params: SpringParams) {
        if (self.spring.to - new_to).abs() <= params.epsilon {
            return;
        }
        let (cur_pos, cur_vel) = if self.settled {
            (self.spring.to, 0.0)
        } else {
            let dt = now_ms.saturating_sub(self.start_ms) as f64 / 1000.0;
            self.spring.sample_pos_vel(dt)
        };
        self.spring = Spring {
            from: cur_pos,
            to: new_to,
            initial_velocity: cur_vel,
            params,
        };
        self.start_ms = now_ms;
        self.settled =
            (new_to - cur_pos).abs() <= params.epsilon && cur_vel.abs() <= params.epsilon * 10.0;
    }

    /// Samples the current value at `now_ms`, marking `settled = true` once converged.
    pub fn sample(&mut self, now_ms: u64) -> f64 {
        if self.settled {
            return self.spring.to;
        }
        let dt = now_ms.saturating_sub(self.start_ms) as f64 / 1000.0;
        let (pos, vel) = self.spring.sample_pos_vel(dt);
        if (self.spring.to - pos).abs() <= self.spring.params.epsilon * 50.0
            && vel.abs() <= self.spring.params.epsilon * 250.0
        {
            self.settled = true;
            return self.spring.to;
        }
        pos
    }
}

#[derive(Debug, Clone, Copy)]
struct WindowSpringEntry {
    x: SpringTrack,
    y: SpringTrack,
    w: SpringTrack,
    h: SpringTrack,
    scale: SpringTrack,
    opacity: SpringTrack,
    closing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampledWindowVisual {
    pub rect: LogicalRect,
    pub opacity_multiplier: f32,
    pub is_animating: bool,
}

/// Manages open/close pop-in & Dwindle/Scrolling/Floating geometry springs across windows,
/// automatically bypassing animations (`0 ms` snap) on `Headless` Agent workspaces.
#[derive(Debug, Clone, Default)]
pub struct WindowAnimationTable {
    entries: BTreeMap<WindowId, WindowSpringEntry>,
}

impl WindowAnimationTable {
    /// Registers a newly mapped window with centered open scale (`open_scale_from -> 1.0`) and
    /// opacity (`0.0 -> 1.0`) springs unless `instant_bypass` is true.
    pub fn on_window_opened(
        &mut self,
        win_id: WindowId,
        target_rect: LogicalRect,
        now_ms: u64,
        cfg: AnimationConfig,
        instant_bypass: bool,
    ) {
        let params = cfg.spring_params();
        if !cfg.enabled || instant_bypass {
            self.entries.insert(
                win_id,
                WindowSpringEntry {
                    x: SpringTrack::settled_at(f64::from(target_rect.x), params),
                    y: SpringTrack::settled_at(f64::from(target_rect.y), params),
                    w: SpringTrack::settled_at(f64::from(target_rect.width), params),
                    h: SpringTrack::settled_at(f64::from(target_rect.height), params),
                    scale: SpringTrack::settled_at(1.0, params),
                    opacity: SpringTrack::settled_at(1.0, params),
                    closing: false,
                },
            );
            return;
        }

        let start_scale = cfg.open_scale_from.clamp(0.5, 1.0);
        self.entries.insert(
            win_id,
            WindowSpringEntry {
                x: SpringTrack::settled_at(f64::from(target_rect.x), params),
                y: SpringTrack::settled_at(f64::from(target_rect.y), params),
                w: SpringTrack::settled_at(f64::from(target_rect.width), params),
                h: SpringTrack::settled_at(f64::from(target_rect.height), params),
                scale: SpringTrack::from_to(start_scale, 1.0, now_ms, params),
                opacity: SpringTrack::from_to(0.0, 1.0, now_ms, params),
                closing: false,
            },
        );
    }

    /// Smoothly retargets `win_id`'s `(x, y, width, height)` springs when a Dwindle split,
    /// swap, resize, or Floating toggle changes its layout rectangle!
    pub fn retarget_geometry(
        &mut self,
        win_id: WindowId,
        target_rect: LogicalRect,
        now_ms: u64,
        cfg: AnimationConfig,
        instant_bypass: bool,
    ) {
        let params = cfg.spring_params();
        let Some(entry) = self.entries.get_mut(&win_id) else {
            self.on_window_opened(win_id, target_rect, now_ms, cfg, true);
            return;
        };
        if !cfg.enabled || instant_bypass {
            entry.x.snap_to(f64::from(target_rect.x));
            entry.y.snap_to(f64::from(target_rect.y));
            entry.w.snap_to(f64::from(target_rect.width));
            entry.h.snap_to(f64::from(target_rect.height));
            return;
        }

        entry.x.retarget(f64::from(target_rect.x), now_ms, params);
        entry.y.retarget(f64::from(target_rect.y), now_ms, params);
        entry
            .w
            .retarget(f64::from(target_rect.width), now_ms, params);
        entry
            .h
            .retarget(f64::from(target_rect.height), now_ms, params);
    }

    /// Starts a close shrink/fade-out spring (`1.0 -> close_scale_to`, `opacity -> 0.0`), or
    /// removes immediately if animations are disabled.
    pub fn remove_immediately(&mut self, win_id: WindowId) {
        self.entries.remove(&win_id);
    }

    /// Evaluates the interpolated visual rectangle & opacity multiplier for `win_id` at `now_ms`.
    pub fn sample_visual(
        &mut self,
        win_id: WindowId,
        fallback_rect: LogicalRect,
        now_ms: u64,
    ) -> SampledWindowVisual {
        let Some(entry) = self.entries.get_mut(&win_id) else {
            return SampledWindowVisual {
                rect: fallback_rect,
                opacity_multiplier: 1.0,
                is_animating: false,
            };
        };

        let rx = entry.x.sample(now_ms);
        let ry = entry.y.sample(now_ms);
        let rw = entry.w.sample(now_ms).max(32.0);
        let rh = entry.h.sample(now_ms).max(32.0);
        let scale = entry.scale.sample(now_ms).clamp(0.2, 1.5);
        let alpha = entry.opacity.sample(now_ms).clamp(0.0, 1.0) as f32;

        // Apply centered open/close scale around `(rx + rw/2, ry + rh/2)`
        let scaled_w = (rw * scale).round().max(24.0) as u32;
        let scaled_h = (rh * scale).round().max(24.0) as u32;
        let cx = rx + rw * 0.5;
        let cy = ry + rh * 0.5;
        let sx = (cx - f64::from(scaled_w) * 0.5).round() as i32;
        let sy = (cy - f64::from(scaled_h) * 0.5).round() as i32;

        let is_animating = !(entry.x.is_settled()
            && entry.y.is_settled()
            && entry.w.is_settled()
            && entry.h.is_settled()
            && entry.scale.is_settled()
            && entry.opacity.is_settled());

        let _ = entry.closing;
        SampledWindowVisual {
            rect: LogicalRect::new(sx, sy, scaled_w, scaled_h),
            opacity_multiplier: alpha,
            is_animating,
        }
    }

    /// Returns `true` if any window still has an unsettled spring in flight (requiring next VBlank redraw).
    pub fn has_active_animations(&mut self, now_ms: u64) -> bool {
        let mut active = false;
        for entry in self.entries.values_mut() {
            let _ = entry.x.sample(now_ms);
            let _ = entry.y.sample(now_ms);
            let _ = entry.w.sample(now_ms);
            let _ = entry.h.sample(now_ms);
            let _ = entry.scale.sample(now_ms);
            let _ = entry.opacity.sample(now_ms);
            if !(entry.x.is_settled()
                && entry.y.is_settled()
                && entry.w.is_settled()
                && entry.h.is_settled()
                && entry.scale.is_settled()
                && entry.opacity.is_settled())
            {
                active = true;
            }
        }
        active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_preserves_continuous_velocity_across_mid_flight_retarget() {
        let cfg = AnimationConfig::default();
        let params = cfg.spring_params();
        let mut track = SpringTrack::from_to(0.0, 960.0, 0, params);

        let v_before = track.spring.velocity_at(Duration::from_millis(45));
        let x_before = track.sample(45);
        assert!(x_before > 50.0 && x_before < 960.0);
        assert!(v_before > 100.0);

        // Retarget mid-flight at t = 45 ms to 480.0: position and velocity must match continuously!
        track.retarget(480.0, 45, params);
        assert!((track.spring.from - x_before).abs() < 1e-6);
        assert!((track.spring.initial_velocity - v_before).abs() < 1e-6);

        // Settles cleanly at 480.0 by t = 600 ms
        assert_eq!(track.sample(600), 480.0);
        assert!(track.is_settled());
    }

    #[test]
    fn kawase_blur_damage_expansion_scales_with_two_to_the_passes() {
        let blur = BlurConfig {
            enabled: true,
            size: 6,
            passes: 3,
            ..BlurConfig::default()
        };
        // 6 * 2^3 = 48 px expansion radius
        assert_eq!(blur.damage_expansion_radius_px(), 48);
        let vp = LogicalViewport {
            width: 1920,
            height: 1080,
            scale: 1.0,
        };
        let expanded = blur.expand_damage_rect(LogicalRect::new(100, 100, 200, 150), vp);
        assert_eq!(expanded, LogicalRect::new(52, 52, 296, 246));
    }
}
