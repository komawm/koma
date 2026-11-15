use crate::{
    animation::{CubicBezier, HookeSpringParams, HookeSpringTrack},
    geometry::LogicalPoint,
};

/// Upper safety bound (in ms) for damped Hooke pointer springs without an explicit duration cap.
pub const MAX_POINTER_SPRING_MS: u64 = 2_500;

const ZERO_PT: LogicalPoint = LogicalPoint { x: 0.0, y: 0.0 };

#[inline]
const fn pt(x: f64, y: f64) -> LogicalPoint {
    LogicalPoint { x, y }
}

pub trait TrajectorySolver {
    /// Evaluates pointer position at normalized time `t` in `[0.0, 1.0]`.
    fn sample(&self, from: LogicalPoint, to: LogicalPoint, t: f64) -> LogicalPoint;
}

/// Evaluates position and physical velocity `(pos, vel_px_per_sec)` along a parametric curve.
#[inline]
fn sample_parametric_with_arc(
    from: LogicalPoint,
    to: LogicalPoint,
    s: f64,
    ds_dtau: f64,
    tau: f64,
    arc_curvature: f64,
    duration_secs: f64,
) -> (LogicalPoint, LogicalPoint) {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let bell = 4.0 * tau * (1.0 - tau) * arc_curvature;
    let pos = LogicalPoint {
        x: from.x + dx * s - dy * bell,
        y: from.y + dy * s + dx * bell,
    };
    let inv_t = if duration_secs > 1e-6 {
        1.0 / duration_secs
    } else {
        0.0
    };
    let dbell_dtau = 4.0 * (1.0 - 2.0 * tau) * arc_curvature;
    let vel = LogicalPoint {
        x: (dx * ds_dtau - dy * dbell_dtau) * inv_t,
        y: (dy * ds_dtau + dx * dbell_dtau) * inv_t,
    };
    (pos, vel)
}

/// Minimum-jerk human reaching trajectory (`10*tau^3 - 15*tau^4 + 6*tau^5`) with perpendicular arc offset.
#[derive(Debug, Clone, Copy)]
pub struct MinimumJerkSolver {
    pub arc_curvature: f64,
}

impl MinimumJerkSolver {
    /// Evaluates position and physical velocity `(pos, vel)` at normalized `t` over `duration_ms`.
    #[must_use]
    pub fn sample_pos_vel(
        &self,
        from: LogicalPoint,
        to: LogicalPoint,
        t: f64,
        duration_ms: u64,
    ) -> (LogicalPoint, LogicalPoint) {
        let tau = t.clamp(0.0, 1.0);
        let s = tau * tau * tau * (10.0 + tau * (-15.0 + 6.0 * tau));
        let one_minus_tau = 1.0 - tau;
        let ds_dtau = 30.0 * tau * tau * one_minus_tau * one_minus_tau;
        let duration_secs = (duration_ms.max(1) as f64) / 1000.0;
        sample_parametric_with_arc(from, to, s, ds_dtau, tau, self.arc_curvature, duration_secs)
    }
}

impl TrajectorySolver for MinimumJerkSolver {
    fn sample(&self, from: LogicalPoint, to: LogicalPoint, t: f64) -> LogicalPoint {
        self.sample_pos_vel(from, to, t, 1_000).0
    }
}

/// Normalized critically damped second-order spring trajectory solver (legacy fixed-`omega` helper).
#[derive(Debug, Clone, Copy)]
pub struct SpringSolver {
    pub omega: f64,
}

impl Default for SpringSolver {
    fn default() -> Self {
        Self { omega: 12.0 }
    }
}

impl TrajectorySolver for SpringSolver {
    fn sample(&self, from: LogicalPoint, to: LogicalPoint, t: f64) -> LogicalPoint {
        let tau = t.clamp(0.0, 1.0);
        let exp_term = (-self.omega * tau).exp();
        let s = (1.0 - (1.0 + self.omega * tau) * exp_term).clamp(0.0, 1.0);
        LogicalPoint {
            x: from.x + (to.x - from.x) * s,
            y: from.y + (to.y - from.y) * s,
        }
    }
}

/// Time-bounded pointer trajectory driven by a `CubicBezier` curve (preserving overshoot values).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BezierPointerSolver {
    pub bezier: CubicBezier,
    pub arc_curvature: f64,
}

impl BezierPointerSolver {
    /// Evaluates position and physical velocity `(pos, vel)` at normalized `t` over `duration_ms`.
    #[must_use]
    pub fn sample_pos_vel(
        &self,
        from: LogicalPoint,
        to: LogicalPoint,
        t: f64,
        duration_ms: u64,
    ) -> (LogicalPoint, LogicalPoint) {
        let tau = t.clamp(0.0, 1.0);
        let s = self.bezier.sample(tau);
        let ds_dtau = self.bezier.velocity_norm(tau);
        let duration_secs = (duration_ms.max(1) as f64) / 1000.0;
        sample_parametric_with_arc(from, to, s, ds_dtau, tau, self.arc_curvature, duration_secs)
    }
}

impl TrajectorySolver for BezierPointerSolver {
    fn sample(&self, from: LogicalPoint, to: LogicalPoint, t: f64) -> LogicalPoint {
        self.sample_pos_vel(from, to, t, 1_000).0
    }
}

#[must_use]
fn arc_velocity_impulse(
    from: LogicalPoint,
    to: LogicalPoint,
    arc_curvature: f64,
    params: HookeSpringParams,
) -> LogicalPoint {
    if !arc_curvature.is_finite() || arc_curvature.abs() <= 1e-6 {
        return ZERO_PT;
    }
    let omega0 = (params.stiffness / params.mass).sqrt();
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    pt(-dy * arc_curvature * omega0, dx * arc_curvature * omega0)
}

/// Coupled 2D damped Hooke's law semi-implicit Euler pointer spring:
///
/// `m * p'' + c * p' + k * (p - p_target) = 0` integrated per-axis with 1/240s sub-stepping
/// ($O(1)$ time, $O(1)$ space per tick) and 2D `(vx, vy)` momentum preservation across retargets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerSpring2D {
    pub x: HookeSpringTrack,
    pub y: HookeSpringTrack,
}

impl PointerSpring2D {
    /// Launches a 2D Hooke pointer spring from `(from, initial_velocity)` toward `to` at `now_ms`.
    #[must_use]
    pub fn launch(
        from: LogicalPoint,
        to: LogicalPoint,
        initial_velocity: LogicalPoint,
        arc_curvature: f64,
        now_ms: u64,
        params: HookeSpringParams,
    ) -> Self {
        let impulse = arc_velocity_impulse(from, to, arc_curvature, params);
        let mut x = HookeSpringTrack::settled_at(from.x, params);
        let mut y = HookeSpringTrack::settled_at(from.y, params);
        x.launch_from_velocity(from.x, to.x, initial_velocity.x + impulse.x, now_ms, params);
        y.launch_from_velocity(from.y, to.y, initial_velocity.y + impulse.y, now_ms, params);
        Self { x, y }
    }

    /// Evaluates current `(x, y)` position at `now_ms`.
    pub fn sample(&mut self, now_ms: u64) -> LogicalPoint {
        pt(self.x.sample(now_ms), self.y.sample(now_ms))
    }

    /// Evaluates current `(position, velocity)` at `now_ms`.
    pub fn sample_pos_vel(&mut self, now_ms: u64) -> (LogicalPoint, LogicalPoint) {
        let (px, vx) = self.x.sample_pos_vel(now_ms);
        let (py, vy) = self.y.sample_pos_vel(now_ms);
        (pt(px, py), pt(vx, vy))
    }

    /// Retargets to `new_to` at `now_ms` while preserving live `(vx, vy)` momentum.
    pub fn retarget(
        &mut self,
        new_to: LogicalPoint,
        arc_curvature: f64,
        now_ms: u64,
        params: HookeSpringParams,
    ) {
        let (cur_pos, cur_vel) = self.sample_pos_vel(now_ms);
        let impulse = arc_velocity_impulse(cur_pos, new_to, arc_curvature, params);
        self.x
            .launch_from_velocity(cur_pos.x, new_to.x, cur_vel.x + impulse.x, now_ms, params);
        self.y
            .launch_from_velocity(cur_pos.y, new_to.y, cur_vel.y + impulse.y, now_ms, params);
    }

    #[must_use]
    pub const fn is_settled(&self) -> bool {
        self.x.is_settled() && self.y.is_settled()
    }

    #[must_use]
    pub const fn target(&self) -> LogicalPoint {
        pt(self.x.target(), self.y.target())
    }

    pub fn snap_to(&mut self, pt: LogicalPoint) {
        self.x.snap_to(pt.x);
        self.y.snap_to(pt.y);
    }
}

/// Resolved trajectory specification for macro/IPC pointer motion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerTrajectorySpec {
    Instant,
    MinimumJerk {
        duration_ms: u64,
        arc_curvature: f64,
    },
    Bezier {
        bezier: CubicBezier,
        duration_ms: u64,
        arc_curvature: f64,
    },
    HookeSpring {
        params: HookeSpringParams,
        duration_cap_ms: Option<u64>,
        arc_curvature: f64,
    },
}

impl PointerTrajectorySpec {
    /// Estimates coroutine park timeout in milliseconds for `TrampolineScheduler`.
    #[must_use]
    pub fn estimated_park_ms(&self, span_px: f64) -> u64 {
        match *self {
            Self::Instant => 0,
            Self::MinimumJerk { duration_ms, .. } | Self::Bezier { duration_ms, .. } => duration_ms,
            Self::HookeSpring {
                params,
                duration_cap_ms,
                ..
            } => {
                let est = params.estimated_settle_ms(span_px);
                duration_cap_ms.map_or(est, |cap| est.min(cap).max(1))
            }
        }
    }
}

#[derive(Debug, Clone)]
enum PointerTrackKind {
    Instant {
        to: LogicalPoint,
    },
    MinimumJerk {
        from: LogicalPoint,
        to: LogicalPoint,
        start_ms: u64,
        duration_ms: u64,
        solver: MinimumJerkSolver,
    },
    Bezier {
        from: LogicalPoint,
        to: LogicalPoint,
        start_ms: u64,
        duration_ms: u64,
        solver: BezierPointerSolver,
    },
    HookeSpring {
        spring: PointerSpring2D,
    },
}

/// Stateful 2D pointer trajectory track supporting `minimum_jerk`, `bezier`, and 2D damped Hooke springs
/// with continuous `(px, py)` and `(vx, vy)` hand-off on mid-flight retargeting ($O(1)$ per step).
#[derive(Debug, Clone)]
pub struct PointerTrajectoryTrack {
    kind: PointerTrackKind,
    to: LogicalPoint,
    max_finish_at_ms: u64,
    settled: bool,
}

impl PointerTrajectoryTrack {
    /// Launches a new trajectory from `(from, initial_velocity)` toward `to` at `now_ms`.
    #[must_use]
    pub fn launch(
        from: LogicalPoint,
        to: LogicalPoint,
        initial_velocity: LogicalPoint,
        now_ms: u64,
        spec: PointerTrajectorySpec,
    ) -> Self {
        match spec {
            PointerTrajectorySpec::Instant => Self {
                kind: PointerTrackKind::Instant { to },
                to,
                max_finish_at_ms: now_ms,
                settled: true,
            },
            PointerTrajectorySpec::MinimumJerk {
                duration_ms,
                arc_curvature,
            } => {
                if duration_ms == 0 {
                    return Self::launch(to, to, ZERO_PT, now_ms, PointerTrajectorySpec::Instant);
                }
                Self {
                    kind: PointerTrackKind::MinimumJerk {
                        from,
                        to,
                        start_ms: now_ms,
                        duration_ms,
                        solver: MinimumJerkSolver { arc_curvature },
                    },
                    to,
                    max_finish_at_ms: now_ms.saturating_add(duration_ms),
                    settled: false,
                }
            }
            PointerTrajectorySpec::Bezier {
                bezier,
                duration_ms,
                arc_curvature,
            } => {
                let dur = duration_ms.clamp(1, 5_000);
                Self {
                    kind: PointerTrackKind::Bezier {
                        from,
                        to,
                        start_ms: now_ms,
                        duration_ms: dur,
                        solver: BezierPointerSolver {
                            bezier,
                            arc_curvature,
                        },
                    },
                    to,
                    max_finish_at_ms: now_ms.saturating_add(dur),
                    settled: false,
                }
            }
            PointerTrajectorySpec::HookeSpring {
                params,
                duration_cap_ms,
                arc_curvature,
            } => {
                let cap_ms = duration_cap_ms
                    .unwrap_or(MAX_POINTER_SPRING_MS)
                    .clamp(1, MAX_POINTER_SPRING_MS);
                let spring = PointerSpring2D::launch(
                    from,
                    to,
                    initial_velocity,
                    arc_curvature,
                    now_ms,
                    params,
                );
                let settled = spring.is_settled();
                Self {
                    kind: PointerTrackKind::HookeSpring { spring },
                    to,
                    max_finish_at_ms: now_ms.saturating_add(cap_ms),
                    settled,
                }
            }
        }
    }

    /// Samples instantaneous `(position, velocity)` at `now_ms`.
    pub fn sample_pos_vel(&mut self, now_ms: u64) -> (LogicalPoint, LogicalPoint) {
        if self.settled || now_ms >= self.max_finish_at_ms {
            self.settled = true;
            if let PointerTrackKind::HookeSpring { spring } = &mut self.kind {
                spring.snap_to(self.to);
            }
            return (self.to, ZERO_PT);
        }

        match &mut self.kind {
            PointerTrackKind::Instant { to } => {
                self.settled = true;
                (*to, ZERO_PT)
            }
            PointerTrackKind::MinimumJerk {
                from,
                to,
                start_ms,
                duration_ms,
                solver,
            } => {
                let elapsed = now_ms.saturating_sub(*start_ms);
                let t = (elapsed as f64) / ((*duration_ms).max(1) as f64);
                let (pos, vel) = solver.sample_pos_vel(*from, *to, t, *duration_ms);
                if elapsed >= *duration_ms {
                    self.settled = true;
                    return (*to, ZERO_PT);
                }
                (pos, vel)
            }
            PointerTrackKind::Bezier {
                from,
                to,
                start_ms,
                duration_ms,
                solver,
            } => {
                let elapsed = now_ms.saturating_sub(*start_ms);
                let t = (elapsed as f64) / ((*duration_ms).max(1) as f64);
                let (pos, vel) = solver.sample_pos_vel(*from, *to, t, *duration_ms);
                if elapsed >= *duration_ms {
                    self.settled = true;
                    return (*to, ZERO_PT);
                }
                (pos, vel)
            }
            PointerTrackKind::HookeSpring { spring } => {
                let (pos, vel) = spring.sample_pos_vel(now_ms);
                if spring.is_settled() {
                    self.settled = true;
                    spring.snap_to(self.to);
                    return (self.to, ZERO_PT);
                }
                (pos, vel)
            }
        }
    }

    /// Advances the trajectory to `now_ms` and returns the interpolated logical pointer position.
    pub fn step(&mut self, now_ms: u64) -> LogicalPoint {
        self.sample_pos_vel(now_ms).0
    }

    /// Smoothly retargets the pointer toward `new_to` at `now_ms`, preserving `(vx, vy)` momentum.
    pub fn retarget(&mut self, new_to: LogicalPoint, now_ms: u64, spec: PointerTrajectorySpec) {
        let (cur_pos, cur_vel) = self.sample_pos_vel(now_ms);
        if let (
            PointerTrackKind::HookeSpring { spring },
            PointerTrajectorySpec::HookeSpring {
                params,
                duration_cap_ms,
                arc_curvature,
            },
        ) = (&mut self.kind, spec)
        {
            let cap_ms = duration_cap_ms
                .unwrap_or(MAX_POINTER_SPRING_MS)
                .clamp(1, MAX_POINTER_SPRING_MS);
            spring.retarget(new_to, arc_curvature, now_ms, params);
            self.to = new_to;
            self.max_finish_at_ms = now_ms.saturating_add(cap_ms);
            self.settled = spring.is_settled();
            return;
        }
        *self = Self::launch(cur_pos, new_to, cur_vel, now_ms, spec);
    }

    #[must_use]
    pub const fn is_settled(&self) -> bool {
        self.settled
    }

    #[must_use]
    pub const fn target(&self) -> LogicalPoint {
        self.to
    }

    #[must_use]
    pub const fn max_finish_at_ms(&self) -> u64 {
        self.max_finish_at_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_spring_2d_underdamped_overshoots_and_settles_with_hooke_euler() {
        let params = HookeSpringParams::from_physical(1.0, 10.0, 300.0, 0.01);
        let from = pt(0.0, 0.0);
        let to = pt(200.0, 100.0);
        let mut spring = PointerSpring2D::launch(from, to, ZERO_PT, 0.0, 0, params);

        let mut max_x = 0.0_f64;
        for t in (8_u64..=2_000).step_by(8) {
            let p = spring.sample(t);
            max_x = max_x.max(p.x);
            if spring.is_settled() {
                break;
            }
        }
        assert!(
            max_x > 205.0,
            "underdamped 2D Hooke spring should overshoot 200 px (peak={max_x:.2})"
        );
        assert!(
            spring.is_settled(),
            "2D Hooke spring must converge within 2s"
        );
        let final_pt = spring.sample(2_000);
        assert!((final_pt.x - 200.0).abs() < 1e-6);
        assert!((final_pt.y - 100.0).abs() < 1e-6);
    }

    #[test]
    fn pointer_trajectory_track_preserves_2d_velocity_on_retarget() {
        let params = HookeSpringParams::from_physical(1.0, 22.0, 320.0, 0.01);
        let spec = PointerTrajectorySpec::HookeSpring {
            params,
            duration_cap_ms: None,
            arc_curvature: 0.0,
        };
        let mut track =
            PointerTrajectoryTrack::launch(pt(0.0, 0.0), pt(300.0, 0.0), ZERO_PT, 0, spec);

        let mut prev_pos = ZERO_PT;
        for t in [16_u64, 32, 48] {
            prev_pos = track.step(t);
        }
        let (_, vel_at_retarget) = track.sample_pos_vel(48);
        assert!(
            vel_at_retarget.x > 100.0,
            "pointer must have positive +x velocity before retarget: vx={}",
            vel_at_retarget.x
        );

        // Retarget behind prev_pos.x: 2D inertia carries x further right before turning around.
        let new_target = pt((prev_pos.x - 20.0).max(0.0), 200.0);
        track.retarget(new_target, 48, spec);
        let (next_pos, _) = track.sample_pos_vel(56);
        assert!(
            next_pos.x > prev_pos.x,
            "2D momentum must carry x smoothly past prev_pos.x ({:.2} -> {:.2}) before curving to ({:.1}, {:.1})",
            prev_pos.x,
            next_pos.x,
            new_target.x,
            new_target.y
        );
        assert!(
            next_pos.y > 0.0,
            "y must immediately accelerate toward 200 (y={:.2})",
            next_pos.y
        );

        for t in (64_u64..=2_500).step_by(8) {
            let _ = track.step(t);
            if track.is_settled() {
                break;
            }
        }
        assert!(track.is_settled());
        let settled = track.step(2_500);
        assert!((settled.x - new_target.x).abs() < 1e-6);
        assert!((settled.y - new_target.y).abs() < 1e-6);
    }

    #[test]
    fn bezier_pointer_solver_preserves_overshoot_and_arc_curvature() {
        let solver = BezierPointerSolver {
            bezier: CubicBezier::new(0.05, 0.9, 0.1, 1.1),
            arc_curvature: 0.0,
        };
        let from = pt(10.0, 20.0);
        let to = pt(210.0, 20.0);
        assert_eq!(solver.sample(from, to, 0.0), from);
        assert_eq!(solver.sample(from, to, 1.0), to);
        let peak = (1..=20)
            .map(|i| solver.sample(from, to, (i as f64) / 20.0).x)
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(
            peak > to.x,
            "overshoot cubic bezier should exceed destination x (peak={peak:.2})"
        );
    }
}
