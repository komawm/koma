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

/// Declarative window border configuration (`koma.appearance({ border = { ... } })`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BorderConfig {
    pub enabled: bool,
    pub width: f32,
    pub active_color: String,
    pub inactive_color: String,
    pub agent_leased_color: String,
    pub human_preempt_color: String,
    /// Optional gradient end color for active windows (defaults to `active_color` when `None`).
    pub active_color_end: Option<String>,
    /// Optional gradient end color for inactive windows (defaults to `inactive_color` when `None`).
    pub inactive_color_end: Option<String>,
    /// Linear gradient angle in degrees (`45.0` = top-left to bottom-right).
    pub gradient_angle: f32,
    /// Continuous gradient rotation speed in revolutions per minute (`0.0` = static gradient).
    pub gradient_rotate_rpm: f32,
}

impl Default for BorderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            width: 2.0,
            active_color: "#58a6ff".to_owned(),
            inactive_color: "#30363d".to_owned(),
            agent_leased_color: "#d2a8ff".to_owned(),
            human_preempt_color: "#f0883e".to_owned(),
            active_color_end: None,
            inactive_color_end: None,
            gradient_angle: 45.0,
            gradient_rotate_rpm: 0.0,
        }
    }
}

impl BorderConfig {
    /// Returns `true` when animated gradient rotation (`gradient_rotate_rpm != 0`) is enabled.
    #[must_use]
    pub fn is_gradient_rotating(&self) -> bool {
        self.enabled
            && self.gradient_rotate_rpm.is_finite()
            && self.gradient_rotate_rpm.abs() > 1e-4
    }

    /// Computes effective border gradient angle in radians at timestamp `now_ms`.
    #[must_use]
    pub fn effective_gradient_angle_rad(&self, now_ms: u64) -> f32 {
        let base_deg = if self.gradient_angle.is_finite() {
            self.gradient_angle
        } else {
            45.0
        };
        let spin_deg = if self.is_gradient_rotating() {
            ((now_ms as f64) * 1e-3 * f64::from(self.gradient_rotate_rpm) * 6.0).rem_euclid(360.0)
                as f32
        } else {
            0.0
        };
        (base_deg + spin_deg).to_radians()
    }
}

/// Declarative Niri-style focus ring configuration (`koma.appearance({ focus_ring = { ... } })`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FocusRingConfig {
    pub enabled: bool,
    pub width: f32,
    pub active_color: String,
    pub inactive_color: String,
}

impl Default for FocusRingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            width: 2.0,
            active_color: "#58a6ff".to_owned(),
            inactive_color: "#30363d".to_owned(),
        }
    }
}

/// Declarative drop shadow configuration (`koma.appearance({ shadow = { ... } })`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShadowConfig {
    pub enabled: bool,
    pub range: u32,
    pub render_power: u8,
    pub sharp: bool,
    pub color: String,
    pub inactive_color: Option<String>,
    pub offset_x: f32,
    pub offset_y: f32,
    pub scale: f32,
}

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            range: 16,
            render_power: 3,
            sharp: false,
            color: "#00000073".to_owned(),
            inactive_color: None,
            offset_x: 0.0,
            offset_y: 4.0,
            scale: 1.0,
        }
    }
}

/// Declarative window brightness, contrast, and inactive-window dimming configuration
/// (`koma.appearance({ dim = { ... } })`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DimConfig {
    pub dim_inactive: bool,
    pub dim_strength: f32,
    pub brightness: f32,
    pub contrast: f32,
}

impl Default for DimConfig {
    fn default() -> Self {
        Self {
            dim_inactive: false,
            dim_strength: 0.15,
            brightness: 1.0,
            contrast: 1.0,
        }
    }
}

/// Parses `#RRGGBB`, `#RRGGBBAA`, `0xRRGGBB`, `0xAARRGGBB`, or `rgba(r, g, b, a)` into
/// normalized `[f32; 4]` (`0.0..=1.0`), returning `fallback` on invalid input.
#[must_use]
pub fn parse_hex_color(s: &str, fallback: [f32; 4]) -> [f32; 4] {
    let trimmed = s.trim();
    if !trimmed.is_ascii() {
        return fallback;
    }

    if let Some(hex) = trimmed.strip_prefix('#') {
        return match hex.len() {
            6 => {
                let Ok(r) = u8::from_str_radix(&hex[0..2], 16) else {
                    return fallback;
                };
                let Ok(g) = u8::from_str_radix(&hex[2..4], 16) else {
                    return fallback;
                };
                let Ok(b) = u8::from_str_radix(&hex[4..6], 16) else {
                    return fallback;
                };
                [
                    f32::from(r) / 255.0,
                    f32::from(g) / 255.0,
                    f32::from(b) / 255.0,
                    1.0,
                ]
            }
            8 => {
                let Ok(r) = u8::from_str_radix(&hex[0..2], 16) else {
                    return fallback;
                };
                let Ok(g) = u8::from_str_radix(&hex[2..4], 16) else {
                    return fallback;
                };
                let Ok(b) = u8::from_str_radix(&hex[4..6], 16) else {
                    return fallback;
                };
                let Ok(a) = u8::from_str_radix(&hex[6..8], 16) else {
                    return fallback;
                };
                [
                    f32::from(r) / 255.0,
                    f32::from(g) / 255.0,
                    f32::from(b) / 255.0,
                    f32::from(a) / 255.0,
                ]
            }
            _ => fallback,
        };
    }

    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return match hex.len() {
            6 => {
                let Ok(r) = u8::from_str_radix(&hex[0..2], 16) else {
                    return fallback;
                };
                let Ok(g) = u8::from_str_radix(&hex[2..4], 16) else {
                    return fallback;
                };
                let Ok(b) = u8::from_str_radix(&hex[4..6], 16) else {
                    return fallback;
                };
                [
                    f32::from(r) / 255.0,
                    f32::from(g) / 255.0,
                    f32::from(b) / 255.0,
                    1.0,
                ]
            }
            8 => {
                let Ok(a) = u8::from_str_radix(&hex[0..2], 16) else {
                    return fallback;
                };
                let Ok(r) = u8::from_str_radix(&hex[2..4], 16) else {
                    return fallback;
                };
                let Ok(g) = u8::from_str_radix(&hex[4..6], 16) else {
                    return fallback;
                };
                let Ok(b) = u8::from_str_radix(&hex[6..8], 16) else {
                    return fallback;
                };
                [
                    f32::from(r) / 255.0,
                    f32::from(g) / 255.0,
                    f32::from(b) / 255.0,
                    f32::from(a) / 255.0,
                ]
            }
            _ => fallback,
        };
    }

    let lower = trimmed.to_ascii_lowercase();
    if let Some(inner) = lower
        .strip_prefix("rgba(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let inner = inner.trim();
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() == 1 && inner.len() == 8 {
            let Ok(r) = u8::from_str_radix(&inner[0..2], 16) else {
                return fallback;
            };
            let Ok(g) = u8::from_str_radix(&inner[2..4], 16) else {
                return fallback;
            };
            let Ok(b) = u8::from_str_radix(&inner[4..6], 16) else {
                return fallback;
            };
            let Ok(a) = u8::from_str_radix(&inner[6..8], 16) else {
                return fallback;
            };
            return [
                f32::from(r) / 255.0,
                f32::from(g) / 255.0,
                f32::from(b) / 255.0,
                f32::from(a) / 255.0,
            ];
        }
        if parts.len() == 4 {
            let Ok(r_raw) = parts[0].parse::<f32>() else {
                return fallback;
            };
            let Ok(g_raw) = parts[1].parse::<f32>() else {
                return fallback;
            };
            let Ok(b_raw) = parts[2].parse::<f32>() else {
                return fallback;
            };
            let Ok(a_raw) = parts[3].parse::<f32>() else {
                return fallback;
            };
            if !r_raw.is_finite()
                || !g_raw.is_finite()
                || !b_raw.is_finite()
                || !a_raw.is_finite()
                || r_raw < 0.0
                || g_raw < 0.0
                || b_raw < 0.0
                || a_raw < 0.0
            {
                return fallback;
            }
            let rgb_is_byte_scale = r_raw > 1.0
                || g_raw > 1.0
                || b_raw > 1.0
                || (!parts[0].contains('.') && !parts[1].contains('.') && !parts[2].contains('.'));
            let (r, g, b) = if rgb_is_byte_scale {
                (r_raw / 255.0, g_raw / 255.0, b_raw / 255.0)
            } else {
                (r_raw, g_raw, b_raw)
            };
            let a = if a_raw > 1.0 { a_raw / 255.0 } else { a_raw };
            return [
                r.clamp(0.0, 1.0),
                g.clamp(0.0, 1.0),
                b.clamp(0.0, 1.0),
                a.clamp(0.0, 1.0),
            ];
        }
    }

    fallback
}

/// Solver family for `koma.appearance({ animations = { solver = ... } })`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimationSolverKind {
    /// Damped Hooke's Law Semi-Implicit / Symplectic Euler (`m*x'' + c*x' + k*(x - target) = 0`).
    #[default]
    #[serde(alias = "hooke", alias = "hooke_spring")]
    Spring,
    /// Closed-form analytical 2nd-order spring (`niri/src/animation/spring.rs`).
    #[serde(alias = "analytical")]
    AnalyticalSpring,
}

/// Per-channel animation tuning (`windows_open`, `windows_close`, `windows_move`, `fade`, `border`, `workspaces`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AnimationChannelConfig {
    pub enabled: Option<bool>,
    pub curve: Option<String>,
    pub duration_ms: Option<u32>,
    pub style: Option<String>,
}

/// Declarative animation configuration (`koma.appearance({ animations = { ... } })`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnimationConfig {
    pub enabled: bool,
    /// `1.0` = critically damped (zero overshoot, Niri default); `< 1.0` (e.g. `0.82`) = bouncy Hyprland spring.
    pub damping_ratio: f64,
    /// Spring stiffness $k$ (`800.0` = snappy ~180 ms settle, Niri default).
    pub stiffness: f64,
    /// Optional explicit mass $m$ for Hooke semi-implicit Euler springs (defaults to `1.0`).
    pub mass: Option<f64>,
    /// Optional explicit damping $c$ for Hooke semi-implicit Euler springs (overrides `damping_ratio` when set).
    pub damping: Option<f64>,
    /// Default spring integration solver (`"spring"` = Hooke semi-implicit Euler, `"analytical_spring"`).
    pub solver: AnimationSolverKind,
    /// Rest convergence threshold $\epsilon$.
    pub epsilon: f64,
    /// Initial centered scale factor when a new window maps (`0.88` -> `1.0`).
    pub open_scale_from: f64,
    /// Target centered scale factor when a closing window fades out (`1.0` -> `0.92`).
    pub close_scale_to: f64,
    /// Named curves registered via `koma.bezier(...)`, `koma.spring(...)`, or inline `curves = { ... }`.
    pub curves: BTreeMap<String, CurveSpec>,
    #[serde(alias = "windows_in")]
    pub windows_open: AnimationChannelConfig,
    #[serde(alias = "windows_out")]
    pub windows_close: AnimationChannelConfig,
    pub windows_move: AnimationChannelConfig,
    pub fade: AnimationChannelConfig,
    pub border: AnimationChannelConfig,
    pub workspaces: AnimationChannelConfig,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            damping_ratio: 1.0,
            stiffness: 800.0,
            mass: None,
            damping: None,
            solver: AnimationSolverKind::Spring,
            epsilon: 0.0005,
            open_scale_from: 0.88,
            close_scale_to: 0.92,
            curves: BTreeMap::new(),
            windows_open: AnimationChannelConfig::default(),
            windows_close: AnimationChannelConfig::default(),
            windows_move: AnimationChannelConfig::default(),
            fade: AnimationChannelConfig::default(),
            border: AnimationChannelConfig::default(),
            workspaces: AnimationChannelConfig::default(),
        }
    }
}

impl AnimationConfig {
    #[must_use]
    pub fn spring_params(&self) -> SpringParams {
        SpringParams::new(self.damping_ratio, self.stiffness, self.epsilon)
    }

    #[must_use]
    pub fn hooke_spring_params(&self) -> HookeSpringParams {
        let mass = self.mass.unwrap_or(1.0);
        if let Some(damping) = self.damping {
            HookeSpringParams::from_physical(mass, damping, self.stiffness, self.epsilon)
        } else {
            HookeSpringParams::from_ratio(self.damping_ratio, self.stiffness, mass, self.epsilon)
        }
    }

    #[must_use]
    pub fn default_curve_spec(&self) -> CurveSpec {
        match self.solver {
            AnimationSolverKind::Spring => CurveSpec::Spring(self.hooke_spring_params()),
            AnimationSolverKind::AnalyticalSpring => {
                CurveSpec::AnalyticalSpring(self.spring_params())
            }
        }
    }

    #[must_use]
    pub fn resolve_curve(
        &self,
        name: &str,
        duration_override_ms: Option<u32>,
    ) -> Option<CurveSpec> {
        let mut spec = self
            .curves
            .get(name)
            .cloned()
            .or_else(|| resolve_named_curve(name))?;
        if let Some(ms) = duration_override_ms
            && let CurveSpec::Bezier {
                ref mut duration_ms,
                ..
            } = spec
        {
            *duration_ms = ms.clamp(1, 5000);
        }
        Some(spec)
    }

    #[must_use]
    pub fn channel_curve(
        &self,
        channel: &AnimationChannelConfig,
        override_name: Option<&str>,
    ) -> CurveSpec {
        if !self.enabled || channel.enabled == Some(false) {
            return CurveSpec::Instant;
        }
        if let Some(name) = override_name.or(channel.curve.as_deref())
            && let Some(spec) = self.resolve_curve(name, channel.duration_ms)
        {
            return spec;
        }
        self.default_curve_spec()
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
        let x1 = (rect.x + rect.w as i32 + r).min(viewport.width as i32);
        let y1 = (rect.y + rect.h as i32 + r).min(viewport.height as i32);
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

/// Physical parameters of a damped Hooke's Law oscillator $m\ddot{x} + c\dot{x} + k(x - x_{\text{target}}) = 0$.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HookeSpringParams {
    pub mass: f64,
    pub damping: f64,
    pub stiffness: f64,
    pub epsilon: f64,
}

impl Default for HookeSpringParams {
    fn default() -> Self {
        Self::from_ratio(1.0, 800.0, 1.0, 0.0005)
    }
}

impl HookeSpringParams {
    /// Constructs Hooke spring parameters directly from `(mass, damping, stiffness, epsilon)`.
    #[must_use]
    pub fn from_physical(mass: f64, damping: f64, stiffness: f64, epsilon: f64) -> Self {
        let m = if mass.is_finite() {
            mass.max(0.01)
        } else {
            1.0
        };
        let k = if stiffness.is_finite() {
            stiffness.max(1.0)
        } else {
            800.0
        };
        let c = if damping.is_finite() {
            damping.max(0.0)
        } else {
            2.0 * (m * k).sqrt()
        };
        let eps = if epsilon.is_finite() {
            epsilon.max(1e-6)
        } else {
            0.0005
        };
        Self {
            mass: m,
            damping: c,
            stiffness: k,
            epsilon: eps,
        }
    }

    /// Constructs Hooke spring parameters from damping ratio $\zeta$ using $c = 2\zeta\sqrt{mk}$.
    #[must_use]
    pub fn from_ratio(damping_ratio: f64, stiffness: f64, mass: f64, epsilon: f64) -> Self {
        let m = if mass.is_finite() {
            mass.max(0.01)
        } else {
            1.0
        };
        let k = if stiffness.is_finite() {
            stiffness.max(1.0)
        } else {
            800.0
        };
        let zeta = if damping_ratio.is_finite() {
            damping_ratio.max(0.0)
        } else {
            1.0
        };
        let eps = if epsilon.is_finite() {
            epsilon.max(1e-6)
        } else {
            0.0005
        };
        Self {
            mass: m,
            damping: 2.0 * zeta * (m * k).sqrt(),
            stiffness: k,
            epsilon: eps,
        }
    }

    #[must_use]
    pub fn is_at_rest(&self, x: f64, v: f64, target: f64) -> bool {
        (x - target).abs() <= self.epsilon * 50.0 && v.abs() <= self.epsilon * 250.0
    }

    /// Estimates convergence time in milliseconds over `span_px` using symplectic Euler probing
    /// at `8 ms` steps up to `2500 ms` ($O(1)$ bounded time and $O(1)$ space).
    #[must_use]
    pub fn estimated_settle_ms(&self, span_px: f64) -> u64 {
        let target = if span_px.is_finite() {
            span_px.abs().max(256.0)
        } else {
            512.0
        };
        let mut probe = HookeSpringTrack::from_to(0.0, target, 0, *self);
        for t_ms in (8_u64..=2_500).step_by(8) {
            let _ = probe.sample(t_ms);
            if probe.is_settled() {
                return t_ms;
            }
        }
        2_500
    }
}

/// Damped Hooke's Law Semi-Implicit (Symplectic) Euler scalar spring track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HookeSpringTrack {
    pub x: f64,
    pub v: f64,
    pub target: f64,
    pub params: HookeSpringParams,
    last_ms: u64,
    settled: bool,
}

impl HookeSpringTrack {
    #[must_use]
    pub fn settled_at(value: f64, params: HookeSpringParams) -> Self {
        Self {
            x: value,
            v: 0.0,
            target: value,
            params,
            last_ms: 0,
            settled: true,
        }
    }

    #[must_use]
    pub fn from_to(from: f64, to: f64, now_ms: u64, params: HookeSpringParams) -> Self {
        let settled = params.is_at_rest(from, 0.0, to);
        Self {
            x: if settled { to } else { from },
            v: 0.0,
            target: to,
            params,
            last_ms: now_ms,
            settled,
        }
    }

    #[must_use]
    pub const fn position(&self) -> f64 {
        self.x
    }

    #[must_use]
    pub const fn velocity(&self) -> f64 {
        self.v
    }

    #[must_use]
    pub const fn target(&self) -> f64 {
        self.target
    }

    #[must_use]
    pub const fn is_settled(&self) -> bool {
        self.settled
    }

    pub fn snap_to(&mut self, value: f64) {
        self.x = value;
        self.v = 0.0;
        self.target = value;
        self.settled = true;
    }

    /// Launches the Hooke track from explicit `(from, v0)` toward `to` at `now_ms`.
    pub fn launch_from_velocity(
        &mut self,
        from: f64,
        to: f64,
        v0: f64,
        now_ms: u64,
        params: HookeSpringParams,
    ) {
        let vel = if v0.is_finite() { v0 } else { 0.0 };
        self.x = from;
        self.v = vel;
        self.target = to;
        self.params = params;
        self.last_ms = now_ms;
        if params.is_at_rest(from, vel, to) {
            self.x = to;
            self.v = 0.0;
            self.settled = true;
        } else {
            self.settled = false;
        }
    }

    /// Advances the spring by `dt_secs` using symplectic Euler sub-stepping ($O(1)$ time, $O(1)$ space).
    pub fn step_dt(&mut self, dt_secs: f64) {
        if self.settled || !dt_secs.is_finite() || dt_secs <= 0.0 {
            return;
        }
        let total_dt = dt_secs.clamp(0.0, 0.1);
        if total_dt <= 0.0 {
            return;
        }
        let steps = ((total_dt / (1.0 / 240.0)).ceil() as usize).clamp(1, 24);
        let sub_dt = total_dt / (steps as f64);
        let m = self.params.mass;
        let c = self.params.damping;
        let k = self.params.stiffness;

        for _ in 0..steps {
            let accel = (-k * (self.x - self.target) - c * self.v) / m;
            self.v += accel * sub_dt;
            self.x += self.v * sub_dt;
        }

        if !self.x.is_finite()
            || !self.v.is_finite()
            || self.params.is_at_rest(self.x, self.v, self.target)
        {
            self.x = self.target;
            self.v = 0.0;
            self.settled = true;
        }
    }

    fn advance_to(&mut self, now_ms: u64) {
        if self.settled {
            self.last_ms = self.last_ms.max(now_ms);
            return;
        }
        if now_ms <= self.last_ms {
            return;
        }
        let mut remaining = (now_ms - self.last_ms) as f64 / 1000.0;
        self.last_ms = now_ms;
        for _ in 0..50 {
            if self.settled || remaining <= 0.0 {
                break;
            }
            let chunk = remaining.min(0.1);
            self.step_dt(chunk);
            remaining -= chunk;
        }
    }

    /// Retargets to `new_target` at `now_ms`, preserving live `x` and `v` momentum.
    pub fn retarget(&mut self, new_target: f64, now_ms: u64, params: HookeSpringParams) {
        self.advance_to(now_ms);
        self.target = new_target;
        self.params = params;
        if params.is_at_rest(self.x, self.v, new_target) {
            self.x = new_target;
            self.v = 0.0;
            self.settled = true;
        } else {
            self.settled = false;
        }
    }

    pub fn sample(&mut self, now_ms: u64) -> f64 {
        self.advance_to(now_ms);
        self.x
    }

    pub fn sample_pos_vel(&mut self, now_ms: u64) -> (f64, f64) {
        self.advance_to(now_ms);
        (self.x, self.v)
    }
}

/// Parametric cubic Bezier easing curve $B(u) = (X(u), Y(u))$ with control points `(0,0)`, `(x1,y1)`, `(x2,y2)`, `(1,1)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CubicBezier {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
}

impl CubicBezier {
    #[must_use]
    pub fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> Self {
        let sx1 = if x1.is_finite() {
            x1.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let sy1 = if y1.is_finite() {
            y1.clamp(-2.0, 3.0)
        } else {
            0.0
        };
        let sx2 = if x2.is_finite() {
            x2.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let sy2 = if y2.is_finite() {
            y2.clamp(-2.0, 3.0)
        } else {
            1.0
        };
        Self {
            x1: sx1,
            y1: sy1,
            x2: sx2,
            y2: sy2,
        }
    }

    #[must_use]
    fn sample_axis(u: f64, p1: f64, p2: f64) -> f64 {
        let c = 3.0 * p1;
        let b = 3.0 * (p2 - p1) - c;
        let a = 1.0 - c - b;
        ((a * u + b) * u + c) * u
    }

    #[must_use]
    fn sample_deriv_axis(u: f64, p1: f64, p2: f64) -> f64 {
        let c = 3.0 * p1;
        let b = 3.0 * (p2 - p1) - c;
        let a = 1.0 - c - b;
        (3.0 * a * u + 2.0 * b) * u + c
    }

    /// Solves $X(u) = t$ via 8-iteration Newton-Raphson with 12-iteration bisection fallback ($O(1)$ time, $O(1)$ space).
    #[must_use]
    pub fn solve_u(&self, t_norm: f64) -> f64 {
        if t_norm <= 0.0 {
            return 0.0;
        }
        if t_norm >= 1.0 {
            return 1.0;
        }
        let mut u = t_norm;
        for _ in 0..8 {
            let err = Self::sample_axis(u, self.x1, self.x2) - t_norm;
            if err.abs() < 1e-7 {
                return u;
            }
            let dx = Self::sample_deriv_axis(u, self.x1, self.x2);
            if dx.abs() < 1e-6 {
                break;
            }
            u = (u - err / dx).clamp(0.0, 1.0);
        }
        if (Self::sample_axis(u, self.x1, self.x2) - t_norm).abs() < 1e-6 {
            return u;
        }
        let mut lo = 0.0;
        let mut hi = 1.0;
        for _ in 0..12 {
            let mid = 0.5 * (lo + hi);
            if Self::sample_axis(mid, self.x1, self.x2) < t_norm {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        0.5 * (lo + hi)
    }

    /// Evaluates eased progress $Y(u(t))$ (can exceed `[0, 1]` for overshoot curves).
    #[must_use]
    pub fn sample(&self, t_norm: f64) -> f64 {
        if t_norm <= 0.0 {
            return 0.0;
        }
        if t_norm >= 1.0 {
            return 1.0;
        }
        let u = self.solve_u(t_norm);
        Self::sample_axis(u, self.y1, self.y2)
    }

    /// Evaluates normalized derivative $\frac{dY}{dt} = \frac{Y'(u)}{X'(u)}$ via the chain rule.
    #[must_use]
    pub fn velocity_norm(&self, t_norm: f64) -> f64 {
        let clamped = t_norm.clamp(0.0, 1.0);
        let u = self.solve_u(clamped);
        let dx = Self::sample_deriv_axis(u, self.x1, self.x2);
        let dy = Self::sample_deriv_axis(u, self.y1, self.y2);
        if dx.abs() < 1e-5 {
            if dy >= 0.0 { 0.0 } else { -0.0 }
        } else {
            dy / dx
        }
    }
}

/// Time-bounded scalar track driven by a `CubicBezier` curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BezierTrack {
    pub from: f64,
    pub to: f64,
    pub bezier: CubicBezier,
    pub duration_ms: u32,
    start_ms: u64,
    settled: bool,
}

impl BezierTrack {
    #[must_use]
    pub fn settled_at(value: f64, bezier: CubicBezier, duration_ms: u32) -> Self {
        Self {
            from: value,
            to: value,
            bezier,
            duration_ms: duration_ms.clamp(1, 5000),
            start_ms: 0,
            settled: true,
        }
    }

    #[must_use]
    pub fn from_to(from: f64, to: f64, now_ms: u64, bezier: CubicBezier, duration_ms: u32) -> Self {
        let dur = duration_ms.clamp(1, 5000);
        let settled = (to - from).abs() <= 1e-4;
        Self {
            from: if settled { to } else { from },
            to,
            bezier,
            duration_ms: dur,
            start_ms: now_ms,
            settled,
        }
    }

    #[must_use]
    pub const fn target(&self) -> f64 {
        self.to
    }

    #[must_use]
    pub const fn is_settled(&self) -> bool {
        self.settled
    }

    pub fn snap_to(&mut self, value: f64) {
        self.from = value;
        self.to = value;
        self.settled = true;
    }

    pub fn retarget(&mut self, new_to: f64, now_ms: u64, bezier: CubicBezier, duration_ms: u32) {
        let (cur_pos, _) = self.sample_pos_vel(now_ms);
        self.from = cur_pos;
        self.to = new_to;
        self.bezier = bezier;
        self.duration_ms = duration_ms.clamp(1, 5000);
        self.start_ms = now_ms;
        self.settled = (new_to - cur_pos).abs() <= 1e-4;
    }

    pub fn sample_pos_vel(&mut self, now_ms: u64) -> (f64, f64) {
        if self.settled {
            return (self.to, 0.0);
        }
        let elapsed_ms = now_ms.saturating_sub(self.start_ms);
        if elapsed_ms >= u64::from(self.duration_ms) {
            self.settled = true;
            return (self.to, 0.0);
        }
        let t_norm = (elapsed_ms as f64) / f64::from(self.duration_ms);
        let span = self.to - self.from;
        let pos = self.from + span * self.bezier.sample(t_norm);
        let vel = span * self.bezier.velocity_norm(t_norm) / (f64::from(self.duration_ms) / 1000.0);
        (pos, vel)
    }

    pub fn sample(&mut self, now_ms: u64) -> f64 {
        self.sample_pos_vel(now_ms).0
    }
}

/// Unified curve descriptor deserializable from Lua tables (`spring`, `analytical_spring`, `bezier`, `instant`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CurveSpec {
    Spring(HookeSpringParams),
    AnalyticalSpring(SpringParams),
    Bezier {
        bezier: CubicBezier,
        duration_ms: u32,
    },
    Instant,
}

impl Serialize for CurveSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            Self::Spring(p) => {
                let mut map = serializer.serialize_map(Some(5))?;
                map.serialize_entry("type", "spring")?;
                map.serialize_entry("mass", &p.mass)?;
                map.serialize_entry("damping", &p.damping)?;
                map.serialize_entry("stiffness", &p.stiffness)?;
                map.serialize_entry("epsilon", &p.epsilon)?;
                map.end()
            }
            Self::AnalyticalSpring(p) => {
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("type", "analytical_spring")?;
                map.serialize_entry(
                    "damping_ratio",
                    &(p.damping / (2.0 * (p.mass * p.stiffness).sqrt())),
                )?;
                map.serialize_entry("stiffness", &p.stiffness)?;
                map.serialize_entry("epsilon", &p.epsilon)?;
                map.end()
            }
            Self::Bezier {
                bezier,
                duration_ms,
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "bezier")?;
                map.serialize_entry("points", &[bezier.x1, bezier.y1, bezier.x2, bezier.y2])?;
                map.serialize_entry("duration_ms", duration_ms)?;
                map.end()
            }
            Self::Instant => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", "instant")?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for CurveSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct CurveVisitor;

        impl<'de> serde::de::Visitor<'de> for CurveVisitor {
            type Value = CurveSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(
                    f,
                    "curve preset string or table with `type = \"spring\"|\"analytical_spring\"|\"bezier\"|\"instant\"`"
                )
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                resolve_named_curve(v)
                    .ok_or_else(|| E::custom(format!("unknown curve preset `{v}`")))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut kind: Option<String> = None;
                let mut mass: Option<f64> = None;
                let mut damping: Option<f64> = None;
                let mut damping_ratio: Option<f64> = None;
                let mut stiffness: Option<f64> = None;
                let mut epsilon: Option<f64> = None;
                let mut points: Option<[f64; 4]> = None;
                let mut x1: Option<f64> = None;
                let mut y1: Option<f64> = None;
                let mut x2: Option<f64> = None;
                let mut y2: Option<f64> = None;
                let mut duration_ms: Option<u32> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" | "kind" => kind = Some(map.next_value()?),
                        "mass" => mass = Some(map.next_value()?),
                        "damping" => damping = Some(map.next_value()?),
                        "damping_ratio" => damping_ratio = Some(map.next_value()?),
                        "stiffness" => stiffness = Some(map.next_value()?),
                        "epsilon" => epsilon = Some(map.next_value()?),
                        "points" => points = Some(map.next_value()?),
                        "x1" => x1 = Some(map.next_value()?),
                        "y1" => y1 = Some(map.next_value()?),
                        "x2" => x2 = Some(map.next_value()?),
                        "y2" => y2 = Some(map.next_value()?),
                        "duration_ms" => duration_ms = Some(map.next_value()?),
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let inferred = match kind.as_deref() {
                    Some(k) => k.trim().to_ascii_lowercase(),
                    None if points.is_some() || (x1.is_some() && x2.is_some()) => {
                        "bezier".to_owned()
                    }
                    None => "spring".to_owned(),
                };

                match inferred.as_str() {
                    "instant" => Ok(CurveSpec::Instant),
                    "analytical_spring" | "analytical" => {
                        Ok(CurveSpec::AnalyticalSpring(SpringParams::new(
                            damping_ratio.unwrap_or(1.0),
                            stiffness.unwrap_or(800.0),
                            epsilon.unwrap_or(0.0005),
                        )))
                    }
                    "bezier" => {
                        let [px1, py1, px2, py2] = points.unwrap_or([
                            x1.unwrap_or(0.22),
                            y1.unwrap_or(1.0),
                            x2.unwrap_or(0.36),
                            y2.unwrap_or(1.0),
                        ]);
                        Ok(CurveSpec::Bezier {
                            bezier: CubicBezier::new(px1, py1, px2, py2),
                            duration_ms: duration_ms.unwrap_or(250).clamp(1, 5000),
                        })
                    }
                    "spring" | "hooke" | "hooke_spring" => {
                        let m = mass.unwrap_or(1.0);
                        let k = stiffness.unwrap_or(800.0);
                        let eps = epsilon.unwrap_or(0.0005);
                        let params = if let Some(c) = damping {
                            HookeSpringParams::from_physical(m, c, k, eps)
                        } else {
                            HookeSpringParams::from_ratio(damping_ratio.unwrap_or(1.0), k, m, eps)
                        };
                        Ok(CurveSpec::Spring(params))
                    }
                    other => Err(serde::de::Error::custom(format!(
                        "unsupported curve type `{other}`"
                    ))),
                }
            }
        }

        deserializer.deserialize_any(CurveVisitor)
    }
}

/// Resolves built-in named Bezier and Hooke spring presets.
#[must_use]
pub fn resolve_named_curve(name: &str) -> Option<CurveSpec> {
    match name.trim().to_ascii_lowercase().as_str() {
        "linear" => Some(CurveSpec::Bezier {
            bezier: CubicBezier::new(0.0, 0.0, 1.0, 1.0),
            duration_ms: 200,
        }),
        "ease_out" | "default" => Some(CurveSpec::Bezier {
            bezier: CubicBezier::new(0.22, 1.0, 0.36, 1.0),
            duration_ms: 250,
        }),
        "md3_decel" => Some(CurveSpec::Bezier {
            bezier: CubicBezier::new(0.05, 0.7, 0.1, 1.0),
            duration_ms: 280,
        }),
        "overshot" => Some(CurveSpec::Bezier {
            bezier: CubicBezier::new(0.05, 0.9, 0.1, 1.1),
            duration_ms: 280,
        }),
        "fluent" => Some(CurveSpec::Bezier {
            bezier: CubicBezier::new(0.1, 0.9, 0.2, 1.0),
            duration_ms: 220,
        }),
        "snappy_spring" => Some(CurveSpec::Spring(HookeSpringParams::from_physical(
            1.0, 36.0, 420.0, 0.0005,
        ))),
        "bouncy_spring" => Some(CurveSpec::Spring(HookeSpringParams::from_physical(
            1.0, 20.0, 320.0, 0.0005,
        ))),
        "gentle_spring" => Some(CurveSpec::Spring(HookeSpringParams::from_physical(
            1.2, 28.0, 180.0, 0.0005,
        ))),
        "instant" | "none" => Some(CurveSpec::Instant),
        _ => None,
    }
}

/// Unified velocity-preserving animation track (`HookeSpringTrack`, `SpringTrack`, `BezierTrack`, or `Instant`).
#[derive(Debug, Clone, Copy)]
pub enum MotionTrack {
    Hooke(HookeSpringTrack),
    Analytical(SpringTrack),
    Bezier(BezierTrack),
    Instant { value: f64 },
}

impl MotionTrack {
    #[must_use]
    pub fn settled_at(value: f64, spec: &CurveSpec) -> Self {
        match *spec {
            CurveSpec::Spring(p) => Self::Hooke(HookeSpringTrack::settled_at(value, p)),
            CurveSpec::AnalyticalSpring(p) => Self::Analytical(SpringTrack::settled_at(value, p)),
            CurveSpec::Bezier {
                bezier,
                duration_ms,
            } => Self::Bezier(BezierTrack::settled_at(value, bezier, duration_ms)),
            CurveSpec::Instant => Self::Instant { value },
        }
    }

    #[must_use]
    pub fn from_to(
        from: f64,
        to: f64,
        now_ms: u64,
        spec: &CurveSpec,
        duration_override_ms: Option<u32>,
    ) -> Self {
        match *spec {
            CurveSpec::Spring(p) => Self::Hooke(HookeSpringTrack::from_to(from, to, now_ms, p)),
            CurveSpec::AnalyticalSpring(p) => {
                Self::Analytical(SpringTrack::from_to(from, to, now_ms, p))
            }
            CurveSpec::Bezier {
                bezier,
                duration_ms,
            } => Self::Bezier(BezierTrack::from_to(
                from,
                to,
                now_ms,
                bezier,
                duration_override_ms.unwrap_or(duration_ms),
            )),
            CurveSpec::Instant => Self::Instant { value: to },
        }
    }

    pub fn launch_from_velocity(
        &mut self,
        from: f64,
        to: f64,
        initial_velocity: f64,
        now_ms: u64,
        spec: &CurveSpec,
        duration_override_ms: Option<u32>,
    ) {
        match *spec {
            CurveSpec::Spring(p) => {
                let mut tr = HookeSpringTrack::settled_at(from, p);
                tr.launch_from_velocity(from, to, initial_velocity, now_ms, p);
                *self = Self::Hooke(tr);
            }
            CurveSpec::AnalyticalSpring(p) => {
                let mut tr = SpringTrack::settled_at(from, p);
                tr.launch_from_velocity(from, to, initial_velocity, now_ms, p);
                *self = Self::Analytical(tr);
            }
            CurveSpec::Bezier {
                bezier,
                duration_ms,
            } => {
                *self = Self::Bezier(BezierTrack::from_to(
                    from,
                    to,
                    now_ms,
                    bezier,
                    duration_override_ms.unwrap_or(duration_ms),
                ));
            }
            CurveSpec::Instant => {
                *self = Self::Instant { value: to };
            }
        }
    }

    /// Retargets to `new_to` at `now_ms`, handing off instantaneous `(position, velocity)` across solvers.
    pub fn retarget(
        &mut self,
        new_to: f64,
        now_ms: u64,
        spec: &CurveSpec,
        duration_override_ms: Option<u32>,
    ) {
        match (self, *spec) {
            (Self::Hooke(tr), CurveSpec::Spring(p)) => tr.retarget(new_to, now_ms, p),
            (Self::Analytical(tr), CurveSpec::AnalyticalSpring(p)) => {
                tr.retarget(new_to, now_ms, p)
            }
            (
                Self::Bezier(tr),
                CurveSpec::Bezier {
                    bezier,
                    duration_ms,
                },
            ) => tr.retarget(
                new_to,
                now_ms,
                bezier,
                duration_override_ms.unwrap_or(duration_ms),
            ),
            (this, target_spec) => {
                let (cur_pos, cur_vel) = this.sample_pos_vel(now_ms);
                this.launch_from_velocity(
                    cur_pos,
                    new_to,
                    cur_vel,
                    now_ms,
                    &target_spec,
                    duration_override_ms,
                );
            }
        }
    }

    #[must_use]
    pub const fn target(&self) -> f64 {
        match self {
            Self::Hooke(t) => t.target(),
            Self::Analytical(t) => t.target(),
            Self::Bezier(t) => t.target(),
            Self::Instant { value } => *value,
        }
    }

    #[must_use]
    pub const fn is_settled(&self) -> bool {
        match self {
            Self::Hooke(t) => t.is_settled(),
            Self::Analytical(t) => t.is_settled(),
            Self::Bezier(t) => t.is_settled(),
            Self::Instant { .. } => true,
        }
    }

    pub fn snap_to(&mut self, value: f64) {
        match self {
            Self::Hooke(t) => t.snap_to(value),
            Self::Analytical(t) => t.snap_to(value),
            Self::Bezier(t) => t.snap_to(value),
            Self::Instant { value: v } => *v = value,
        }
    }

    pub fn sample(&mut self, now_ms: u64) -> f64 {
        match self {
            Self::Hooke(t) => t.sample(now_ms),
            Self::Analytical(t) => t.sample(now_ms),
            Self::Bezier(t) => t.sample(now_ms),
            Self::Instant { value } => *value,
        }
    }

    pub fn sample_pos_vel(&mut self, now_ms: u64) -> (f64, f64) {
        match self {
            Self::Hooke(t) => t.sample_pos_vel(now_ms),
            Self::Analytical(t) => {
                if t.settled {
                    (t.spring.to, 0.0)
                } else {
                    let dt = now_ms.saturating_sub(t.start_ms) as f64 / 1000.0;
                    let res = t.spring.sample_pos_vel(dt);
                    let _ = t.sample(now_ms);
                    res
                }
            }
            Self::Bezier(t) => t.sample_pos_vel(now_ms),
            Self::Instant { value } => (*value, 0.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideEdge {
    Top,
    Bottom,
    Left,
    Right,
}

/// Hyprland-style window open animation styles (`"popin 85%"`, `"slide [edge]"`, `"gnomed"`, `"fade"`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowOpenStyle {
    Popin(Option<f64>),
    Slide(Option<SlideEdge>),
    Gnomed,
    Fade,
    None,
}

impl WindowOpenStyle {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split_whitespace();
        let head = parts.next()?.to_ascii_lowercase();
        match head.as_str() {
            "popin" => {
                let pct = parts.next().and_then(|s| {
                    if let Some(stripped) = s.strip_suffix('%') {
                        stripped
                            .parse::<f64>()
                            .ok()
                            .map(|v| (v / 100.0).clamp(0.1, 1.0))
                    } else {
                        s.parse::<f64>().ok().map(|v| v.clamp(0.1, 1.0))
                    }
                });
                Some(Self::Popin(pct))
            }
            "slide" => {
                let edge = parts
                    .next()
                    .and_then(|s| match s.to_ascii_lowercase().as_str() {
                        "top" | "up" => Some(SlideEdge::Top),
                        "bottom" | "down" => Some(SlideEdge::Bottom),
                        "left" => Some(SlideEdge::Left),
                        "right" => Some(SlideEdge::Right),
                        _ => None,
                    });
                Some(Self::Slide(edge))
            }
            "gnomed" | "gnome" => Some(Self::Gnomed),
            "fade" => Some(Self::Fade),
            "none" | "off" => Some(Self::None),
            _ => None,
        }
    }
}

fn nearest_viewport_edge(rect: LogicalRect, viewport: LogicalViewport) -> SlideEdge {
    let left_dist = rect.x;
    let top_dist = rect.y;
    let right_dist = viewport.width as i32 - (rect.x + rect.w as i32);
    let bottom_dist = viewport.height as i32 - (rect.y + rect.h as i32);
    let candidates = [
        (left_dist, SlideEdge::Left),
        (right_dist, SlideEdge::Right),
        (top_dist, SlideEdge::Top),
        (bottom_dist, SlideEdge::Bottom),
    ];
    candidates
        .into_iter()
        .min_by_key(|(d, _)| *d)
        .map_or(SlideEdge::Bottom, |(_, edge)| edge)
}

#[derive(Debug, Clone, Copy)]
struct WindowSpringEntry {
    x: MotionTrack,
    y: MotionTrack,
    w: MotionTrack,
    h: MotionTrack,
    scale: MotionTrack,
    opacity: MotionTrack,
    focus_factor: MotionTrack,
    instant_override: bool,
    closing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampledWindowVisual {
    pub rect: LogicalRect,
    pub opacity_multiplier: f32,
    pub focus_factor: f32,
    pub is_animating: bool,
}

/// Manages open/close pop-in/slide/gnomed & Dwindle/Scrolling/Floating geometry animations across
/// windows, automatically bypassing animations (`0 ms` snap) on `Headless` Agent workspaces.
#[derive(Debug, Clone, Default)]
pub struct WindowAnimationTable {
    entries: BTreeMap<WindowId, WindowSpringEntry>,
}

impl WindowAnimationTable {
    /// Registers a newly mapped window with configured `windows_open` / `fade` curves and optional
    /// per-window style/curve override unless `instant_bypass` is true.
    #[allow(clippy::too_many_arguments)]
    pub fn on_window_opened(
        &mut self,
        win_id: WindowId,
        target_rect: LogicalRect,
        viewport: LogicalViewport,
        now_ms: u64,
        cfg: &AnimationConfig,
        style_override: Option<&str>,
        instant_bypass: bool,
    ) {
        let parsed_style = style_override.and_then(WindowOpenStyle::parse).or_else(|| {
            cfg.windows_open
                .style
                .as_deref()
                .and_then(WindowOpenStyle::parse)
        });
        let explicit_curve_override = if parsed_style.is_none() {
            style_override
        } else {
            None
        };
        let instant_override = matches!(parsed_style, Some(WindowOpenStyle::None));
        let open_curve = cfg.channel_curve(&cfg.windows_open, explicit_curve_override);
        let fade_curve = cfg.channel_curve(&cfg.fade, None);
        let border_curve = cfg.channel_curve(&cfg.border, None);

        if !cfg.enabled || instant_bypass || instant_override {
            self.entries.insert(
                win_id,
                WindowSpringEntry {
                    x: MotionTrack::settled_at(f64::from(target_rect.x), &open_curve),
                    y: MotionTrack::settled_at(f64::from(target_rect.y), &open_curve),
                    w: MotionTrack::settled_at(f64::from(target_rect.w), &open_curve),
                    h: MotionTrack::settled_at(f64::from(target_rect.h), &open_curve),
                    scale: MotionTrack::settled_at(1.0, &open_curve),
                    opacity: MotionTrack::settled_at(1.0, &fade_curve),
                    focus_factor: MotionTrack::settled_at(1.0, &border_curve),
                    instant_override,
                    closing: false,
                },
            );
            return;
        }

        let style = parsed_style.unwrap_or(WindowOpenStyle::Popin(None));
        let mut x = MotionTrack::settled_at(f64::from(target_rect.x), &open_curve);
        let mut y = MotionTrack::settled_at(f64::from(target_rect.y), &open_curve);
        let w = MotionTrack::settled_at(f64::from(target_rect.w), &open_curve);
        let h = MotionTrack::settled_at(f64::from(target_rect.h), &open_curve);
        let mut scale = MotionTrack::settled_at(1.0, &open_curve);

        match style {
            WindowOpenStyle::Popin(pct) => {
                let start_scale = pct.unwrap_or(cfg.open_scale_from).clamp(0.2, 1.0);
                scale = MotionTrack::from_to(start_scale, 1.0, now_ms, &open_curve, None);
            }
            WindowOpenStyle::Slide(edge_opt) => {
                let edge = edge_opt.unwrap_or_else(|| nearest_viewport_edge(target_rect, viewport));
                match edge {
                    SlideEdge::Left => {
                        x = MotionTrack::from_to(
                            -f64::from(target_rect.w),
                            f64::from(target_rect.x),
                            now_ms,
                            &open_curve,
                            None,
                        );
                    }
                    SlideEdge::Right => {
                        x = MotionTrack::from_to(
                            f64::from(viewport.width),
                            f64::from(target_rect.x),
                            now_ms,
                            &open_curve,
                            None,
                        );
                    }
                    SlideEdge::Top => {
                        y = MotionTrack::from_to(
                            -f64::from(target_rect.h),
                            f64::from(target_rect.y),
                            now_ms,
                            &open_curve,
                            None,
                        );
                    }
                    SlideEdge::Bottom => {
                        y = MotionTrack::from_to(
                            f64::from(viewport.height),
                            f64::from(target_rect.y),
                            now_ms,
                            &open_curve,
                            None,
                        );
                    }
                }
            }
            WindowOpenStyle::Gnomed => {
                scale = MotionTrack::from_to(0.55, 1.0, now_ms, &open_curve, None);
            }
            WindowOpenStyle::Fade => {}
            WindowOpenStyle::None => {}
        }

        self.entries.insert(
            win_id,
            WindowSpringEntry {
                x,
                y,
                w,
                h,
                scale,
                opacity: MotionTrack::from_to(0.0, 1.0, now_ms, &fade_curve, None),
                focus_factor: MotionTrack::settled_at(1.0, &border_curve),
                instant_override: false,
                closing: false,
            },
        );
    }

    /// Smoothly retargets `win_id`'s `(x, y, width, height)` tracks when layout geometry changes!
    pub fn retarget_geometry(
        &mut self,
        win_id: WindowId,
        target_rect: LogicalRect,
        now_ms: u64,
        cfg: &AnimationConfig,
        instant_bypass: bool,
    ) {
        let Some(entry) = self.entries.get_mut(&win_id) else {
            self.on_window_opened(
                win_id,
                target_rect,
                LogicalViewport::new(1920, 1080, 1.0),
                now_ms,
                cfg,
                None,
                true,
            );
            return;
        };
        if !cfg.enabled || instant_bypass || entry.instant_override {
            entry.x.snap_to(f64::from(target_rect.x));
            entry.y.snap_to(f64::from(target_rect.y));
            entry.w.snap_to(f64::from(target_rect.w));
            entry.h.snap_to(f64::from(target_rect.h));
            return;
        }

        let move_curve = cfg.channel_curve(&cfg.windows_move, None);
        entry
            .x
            .retarget(f64::from(target_rect.x), now_ms, &move_curve, None);
        entry
            .y
            .retarget(f64::from(target_rect.y), now_ms, &move_curve, None);
        entry
            .w
            .retarget(f64::from(target_rect.w), now_ms, &move_curve, None);
        entry
            .h
            .retarget(f64::from(target_rect.h), now_ms, &move_curve, None);
    }

    /// Retargets `win_id`'s animated opacity multiplier via `cfg.fade`.
    pub fn retarget_opacity(
        &mut self,
        win_id: WindowId,
        target_opacity: f64,
        now_ms: u64,
        cfg: &AnimationConfig,
        instant_bypass: bool,
    ) {
        let clamped = target_opacity.clamp(0.0, 1.0);
        let Some(entry) = self.entries.get_mut(&win_id) else {
            return;
        };
        if !cfg.enabled || instant_bypass || entry.instant_override {
            entry.opacity.snap_to(clamped);
            return;
        }
        let fade_curve = cfg.channel_curve(&cfg.fade, None);
        entry.opacity.retarget(clamped, now_ms, &fade_curve, None);
    }

    /// Retargets `win_id`'s border focus transition factor (`0.0` inactive <-> `1.0` active) via `cfg.border`.
    pub fn retarget_focus(
        &mut self,
        win_id: WindowId,
        is_focused: bool,
        now_ms: u64,
        cfg: &AnimationConfig,
        instant_bypass: bool,
    ) {
        let target = if is_focused { 1.0 } else { 0.0 };
        let Some(entry) = self.entries.get_mut(&win_id) else {
            return;
        };
        if (entry.focus_factor.target() - target).abs() <= 1e-4 {
            return;
        }
        if !cfg.enabled || instant_bypass || entry.instant_override {
            entry.focus_factor.snap_to(target);
            return;
        }
        let border_curve = cfg.channel_curve(&cfg.border, None);
        entry
            .focus_factor
            .retarget(target, now_ms, &border_curve, None);
    }

    /// Starts a close shrink/fade-out spring (`1.0 -> close_scale_to`, `opacity -> 0.0`), or
    /// removes immediately if animations are disabled.
    pub fn remove_immediately(&mut self, win_id: WindowId) {
        self.entries.remove(&win_id);
    }

    /// Evaluates the interpolated visual rectangle, opacity multiplier, and border focus factor at `now_ms`.
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
                focus_factor: 1.0,
                is_animating: false,
            };
        };

        let rx = entry.x.sample(now_ms);
        let ry = entry.y.sample(now_ms);
        let rw = entry.w.sample(now_ms).max(32.0);
        let rh = entry.h.sample(now_ms).max(32.0);
        let scale = entry.scale.sample(now_ms).clamp(0.2, 1.5);
        let alpha = entry.opacity.sample(now_ms).clamp(0.0, 1.0) as f32;
        let focus = entry.focus_factor.sample(now_ms).clamp(0.0, 1.0) as f32;

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
            && entry.opacity.is_settled()
            && entry.focus_factor.is_settled());

        let _ = entry.closing;
        SampledWindowVisual {
            rect: LogicalRect::new(sx, sy, scaled_w, scaled_h),
            opacity_multiplier: alpha,
            focus_factor: focus,
            is_animating,
        }
    }

    fn step_entry_animating(entry: &mut WindowSpringEntry, now_ms: u64) -> bool {
        let _ = entry.x.sample(now_ms);
        let _ = entry.y.sample(now_ms);
        let _ = entry.w.sample(now_ms);
        let _ = entry.h.sample(now_ms);
        let _ = entry.scale.sample(now_ms);
        let _ = entry.opacity.sample(now_ms);
        let _ = entry.focus_factor.sample(now_ms);
        !(entry.x.is_settled()
            && entry.y.is_settled()
            && entry.w.is_settled()
            && entry.h.is_settled()
            && entry.scale.is_settled()
            && entry.opacity.is_settled()
            && entry.focus_factor.is_settled())
    }

    /// Returns `true` if any window still has an unsettled animation in flight (requiring next VBlank redraw).
    pub fn has_active_animations(&mut self, now_ms: u64) -> bool {
        let mut active = false;
        for entry in self.entries.values_mut() {
            if Self::step_entry_animating(entry, now_ms) {
                active = true;
            }
        }
        active
    }

    /// Returns `true` if any window in `win_ids` currently has an unsettled geometry/opacity animation.
    pub fn has_active_for_windows<'a>(
        &mut self,
        now_ms: u64,
        win_ids: impl IntoIterator<Item = &'a WindowId>,
    ) -> bool {
        let mut active = false;
        for win_id in win_ids {
            if let Some(entry) = self.entries.get_mut(win_id)
                && Self::step_entry_animating(entry, now_ms)
            {
                active = true;
            }
        }
        active
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampledWorkspaceTransform {
    pub offset_x: i32,
    pub offset_y: i32,
    pub opacity_multiplier: f32,
    pub is_animating: bool,
}

impl Default for SampledWorkspaceTransform {
    fn default() -> Self {
        Self {
            offset_x: 0,
            offset_y: 0,
            opacity_multiplier: 1.0,
            is_animating: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceTransitionEntry {
    offset_x: MotionTrack,
    offset_y: MotionTrack,
    opacity: MotionTrack,
}

/// Manages per-output workspace transition animations (`slide`, `slidevert`, `fade`, `none`)
/// on headed workspace switches while keeping `Headless`/screencast frames untouched.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceTransitionTable {
    entries: BTreeMap<String, WorkspaceTransitionEntry>,
}

impl WorkspaceTransitionTable {
    /// Starts a workspace transition on `output_name` (`direction`: `+1` forward, `-1` backward).
    pub fn trigger_switch(
        &mut self,
        output_name: &str,
        direction: i32,
        viewport: LogicalViewport,
        now_ms: u64,
        cfg: &AnimationConfig,
        instant_bypass: bool,
    ) {
        let curve = cfg.channel_curve(&cfg.workspaces, None);
        if !cfg.enabled || instant_bypass || matches!(curve, CurveSpec::Instant) {
            self.entries.insert(
                output_name.to_owned(),
                WorkspaceTransitionEntry {
                    offset_x: MotionTrack::settled_at(0.0, &curve),
                    offset_y: MotionTrack::settled_at(0.0, &curve),
                    opacity: MotionTrack::settled_at(1.0, &curve),
                },
            );
            return;
        }

        let style = cfg
            .workspaces
            .style
            .as_deref()
            .unwrap_or("slide")
            .trim()
            .to_ascii_lowercase();
        let sign = if direction < 0 { -1.0 } else { 1.0 };

        let entry = match style.as_str() {
            "none" | "off" => WorkspaceTransitionEntry {
                offset_x: MotionTrack::settled_at(0.0, &curve),
                offset_y: MotionTrack::settled_at(0.0, &curve),
                opacity: MotionTrack::settled_at(1.0, &curve),
            },
            "fade" => WorkspaceTransitionEntry {
                offset_x: MotionTrack::settled_at(0.0, &curve),
                offset_y: MotionTrack::settled_at(0.0, &curve),
                opacity: MotionTrack::from_to(0.0, 1.0, now_ms, &curve, None),
            },
            "slidevert" | "slide_vert" | "vertical" => WorkspaceTransitionEntry {
                offset_x: MotionTrack::settled_at(0.0, &curve),
                offset_y: MotionTrack::from_to(
                    sign * f64::from(viewport.height.max(1)),
                    0.0,
                    now_ms,
                    &curve,
                    None,
                ),
                opacity: MotionTrack::from_to(0.35, 1.0, now_ms, &curve, None),
            },
            _ => WorkspaceTransitionEntry {
                offset_x: MotionTrack::from_to(
                    sign * f64::from(viewport.width.max(1)),
                    0.0,
                    now_ms,
                    &curve,
                    None,
                ),
                offset_y: MotionTrack::settled_at(0.0, &curve),
                opacity: MotionTrack::from_to(0.35, 1.0, now_ms, &curve, None),
            },
        };
        self.entries.insert(output_name.to_owned(), entry);
    }

    pub fn sample(&mut self, output_name: &str, now_ms: u64) -> SampledWorkspaceTransform {
        let Some(entry) = self.entries.get_mut(output_name) else {
            return SampledWorkspaceTransform::default();
        };
        let ox = entry.offset_x.sample(now_ms).round() as i32;
        let oy = entry.offset_y.sample(now_ms).round() as i32;
        let alpha = entry.opacity.sample(now_ms).clamp(0.0, 1.0) as f32;
        let is_animating = !(entry.offset_x.is_settled()
            && entry.offset_y.is_settled()
            && entry.opacity.is_settled());
        SampledWorkspaceTransform {
            offset_x: ox,
            offset_y: oy,
            opacity_multiplier: alpha,
            is_animating,
        }
    }

    pub fn has_active_animations(&mut self, now_ms: u64) -> bool {
        let mut active = false;
        for entry in self.entries.values_mut() {
            let _ = entry.offset_x.sample(now_ms);
            let _ = entry.offset_y.sample(now_ms);
            let _ = entry.opacity.sample(now_ms);
            if !(entry.offset_x.is_settled()
                && entry.offset_y.is_settled()
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
        let vp = LogicalViewport::new(1920, 1080, 1.0);
        let expanded = blur.expand_damage_rect(LogicalRect::new(100, 100, 200, 150), vp);
        assert_eq!(expanded, LogicalRect::new(52, 52, 296, 246));
    }

    #[test]
    fn parse_hex_color_handles_all_supported_formats_and_fallback() {
        let fb = [0.1, 0.2, 0.3, 0.4];

        let c_hex6 = parse_hex_color("#58a6ff", fb);
        assert!((c_hex6[0] - 88.0 / 255.0).abs() < 1e-4);
        assert!((c_hex6[1] - 166.0 / 255.0).abs() < 1e-4);
        assert!((c_hex6[2] - 1.0).abs() < 1e-4);
        assert!((c_hex6[3] - 1.0).abs() < 1e-4);

        let c_hex8 = parse_hex_color("#00000073", fb);
        assert!((c_hex8[0] - 0.0).abs() < 1e-4);
        assert!((c_hex8[3] - 0x73 as f32 / 255.0).abs() < 1e-4);

        let c_0x6 = parse_hex_color("0x30363d", fb);
        assert!((c_0x6[0] - 0x30 as f32 / 255.0).abs() < 1e-4);
        assert!((c_0x6[3] - 1.0).abs() < 1e-4);

        let c_0x8 = parse_hex_color("0x80ff0000", fb);
        assert!((c_0x8[0] - 1.0).abs() < 1e-4);
        assert!((c_0x8[1] - 0.0).abs() < 1e-4);
        assert!((c_0x8[3] - 0x80 as f32 / 255.0).abs() < 1e-4);

        let c_rgba = parse_hex_color("rgba(88, 166, 255, 0.5)", fb);
        assert!((c_rgba[0] - 88.0 / 255.0).abs() < 1e-4);
        assert!((c_rgba[2] - 1.0).abs() < 1e-4);
        assert!((c_rgba[3] - 0.5).abs() < 1e-4);

        assert_eq!(parse_hex_color("not-a-color", fb), fb);
        assert_eq!(parse_hex_color("#12345", fb), fb);
    }

    #[test]
    fn decoration_config_defaults_match_hyprland_baseline() {
        let border = BorderConfig::default();
        assert!(border.enabled);
        assert_eq!(border.width, 2.0);
        assert_eq!(border.active_color, "#58a6ff");
        assert_eq!(border.inactive_color, "#30363d");
        assert_eq!(border.agent_leased_color, "#d2a8ff");
        assert_eq!(border.human_preempt_color, "#f0883e");

        let ring = FocusRingConfig::default();
        assert!(!ring.enabled);
        assert_eq!(ring.width, 2.0);

        let shadow = ShadowConfig::default();
        assert!(shadow.enabled);
        assert_eq!(shadow.range, 16);
        assert_eq!(shadow.render_power, 3);
        assert!(!shadow.sharp);
        assert_eq!(shadow.color, "#00000073");
        assert_eq!(shadow.offset_y, 4.0);
        assert_eq!(shadow.scale, 1.0);

        let dim = DimConfig::default();
        assert!(!dim.dim_inactive);
        assert!((dim.dim_strength - 0.15).abs() < 1e-6);
        assert!((dim.brightness - 1.0).abs() < 1e-6);
        assert!((dim.contrast - 1.0).abs() < 1e-6);
    }

    #[test]
    fn hooke_spring_substepping_and_retarget_preserve_momentum() {
        let params = HookeSpringParams::from_physical(1.0, 20.0, 320.0, 0.0005);
        let mut track = HookeSpringTrack::from_to(0.0, 100.0, 0, params);

        track.step_dt(0.016);
        assert!(track.position() > 0.0);
        assert!(track.velocity() > 0.0);

        let mut track_ms = HookeSpringTrack::from_to(0.0, 500.0, 0, params);
        let pos_32 = track_ms.sample(32);
        let (_, vel_32) = track_ms.sample_pos_vel(32);
        assert!(pos_32 > 0.0 && pos_32 < 500.0);
        assert!(vel_32 > 100.0);

        // Retarget mid-flight: position & velocity stay continuous without reset!
        track_ms.retarget(250.0, 32, params);
        assert!((track_ms.position() - pos_32).abs() < 1e-9);
        assert!((track_ms.velocity() - vel_32).abs() < 1e-9);

        // Settles cleanly at new target within ~1.2s
        assert!((track_ms.sample(1500) - 250.0).abs() < 1e-4);
        assert!(track_ms.is_settled());
    }

    #[test]
    fn hooke_from_ratio_matches_critical_damping_formula() {
        let p = HookeSpringParams::from_ratio(1.0, 400.0, 1.0, 0.0005);
        assert!((p.damping - 40.0).abs() < 1e-9);

        let mut tr = HookeSpringTrack::from_to(0.0, 100.0, 0, p);
        tr.launch_from_velocity(10.0, 200.0, 350.0, 0, p);
        assert!(!tr.is_settled());
        assert!((tr.velocity() - 350.0).abs() < 1e-9);
    }

    #[test]
    fn cubic_bezier_solver_supports_overshoot_and_chain_rule_velocity() {
        let overshot = CubicBezier::new(0.05, 0.9, 0.1, 1.1);
        assert!((overshot.sample(0.0) - 0.0).abs() < 1e-9);
        assert!((overshot.sample(1.0) - 1.0).abs() < 1e-9);
        let mid = overshot.sample(0.5);
        assert!(mid > 1.0, "expected overshoot > 1.0, got {mid}");
        assert!(overshot.velocity_norm(0.1) > 0.0);

        let linear = CubicBezier::new(0.0, 0.0, 1.0, 1.0);
        for step in 1..10 {
            let t = f64::from(step) / 10.0;
            assert!((linear.sample(t) - t).abs() < 1e-6);
            assert!((linear.velocity_norm(t) - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn named_presets_and_curve_spec_deserialization_cover_lua_shapes() {
        assert!(resolve_named_curve("linear").is_some());
        assert!(resolve_named_curve("ease_out").is_some());
        assert!(resolve_named_curve("default").is_some());
        assert!(resolve_named_curve("md3_decel").is_some());
        assert!(resolve_named_curve("overshot").is_some());
        assert!(resolve_named_curve("fluent").is_some());
        assert!(resolve_named_curve("snappy_spring").is_some());
        assert!(resolve_named_curve("bouncy_spring").is_some());
        assert!(resolve_named_curve("gentle_spring").is_some());

        use serde::Deserialize;
        let bez = CurveSpec::deserialize(serde::de::value::MapDeserializer::<
            _,
            serde::de::value::Error,
        >::new(
            [("type", "bezier"), ("x1", "0.05")].into_iter()
        ));
        assert!(bez.is_err());

        let preset = CurveSpec::deserialize(serde::de::value::StrDeserializer::<
            serde::de::value::Error,
        >::new("overshot"))
        .unwrap();
        assert!(matches!(preset, CurveSpec::Bezier { .. }));

        let inst = CurveSpec::deserialize(serde::de::value::StrDeserializer::<
            serde::de::value::Error,
        >::new("instant"))
        .unwrap();
        assert_eq!(inst, CurveSpec::Instant);
    }

    #[test]
    fn window_open_style_and_workspace_transitions_work_as_configured() {
        assert_eq!(
            WindowOpenStyle::parse("popin 85%"),
            Some(WindowOpenStyle::Popin(Some(0.85)))
        );
        assert_eq!(
            WindowOpenStyle::parse("slide"),
            Some(WindowOpenStyle::Slide(None))
        );
        assert_eq!(
            WindowOpenStyle::parse("slide left"),
            Some(WindowOpenStyle::Slide(Some(SlideEdge::Left)))
        );
        assert_eq!(
            WindowOpenStyle::parse("gnomed"),
            Some(WindowOpenStyle::Gnomed)
        );
        assert_eq!(WindowOpenStyle::parse("fade"), Some(WindowOpenStyle::Fade));

        let mut cfg = AnimationConfig::default();
        cfg.windows_open.style = Some("slide left".to_owned());
        cfg.workspaces.style = Some("slidevert".to_owned());
        cfg.border.curve = Some("overshot".to_owned());

        let vp = LogicalViewport::new(1920, 1080, 1.0);
        let mut win_tbl = WindowAnimationTable::default();
        let wid = WindowId::new(42, 0);
        win_tbl.on_window_opened(
            wid,
            LogicalRect::new(100, 100, 800, 600),
            vp,
            0,
            &cfg,
            None,
            false,
        );
        let s0 = win_tbl.sample_visual(wid, LogicalRect::new(100, 100, 800, 600), 16);
        assert!(s0.is_animating);
        assert!(s0.rect.x < 100);

        win_tbl.retarget_focus(wid, false, 16, &cfg, false);
        let s_mid = win_tbl.sample_visual(wid, LogicalRect::new(100, 100, 800, 600), 140);
        assert!(s_mid.focus_factor < 1.0);

        let mut ws_tbl = WorkspaceTransitionTable::default();
        ws_tbl.trigger_switch("eDP-1", 1, vp, 0, &cfg, false);
        let w_mid = ws_tbl.sample("eDP-1", 32);
        assert!(w_mid.is_animating);
        assert!(w_mid.offset_y > 0);
        let w_end = ws_tbl.sample("eDP-1", 1200);
        assert_eq!(w_end.offset_y, 0);
        assert!(!w_end.is_animating);
    }
}
