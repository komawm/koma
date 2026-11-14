use crate::geometry::LogicalPoint;

pub trait TrajectorySolver {
    /// Evaluates pointer position at normalized time `t` in `[0.0, 1.0]`.
    fn sample(&self, from: LogicalPoint, to: LogicalPoint, t: f64) -> LogicalPoint;
}

/// Minimum-jerk human reaching trajectory (10*tau^3 - 15*tau^4 + 6*tau^5) with perpendicular arc offset.
#[derive(Debug, Clone, Copy)]
pub struct MinimumJerkSolver {
    pub arc_curvature: f64,
}

impl TrajectorySolver for MinimumJerkSolver {
    fn sample(&self, from: LogicalPoint, to: LogicalPoint, t: f64) -> LogicalPoint {
        let tau = t.clamp(0.0, 1.0);
        let s = tau * tau * tau * (10.0 + tau * (-15.0 + 6.0 * tau));
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        // Parabolic lateral arc peaking at tau = 0.5 and vanishing at endpoints.
        let bell = 4.0 * tau * (1.0 - tau) * self.arc_curvature;
        LogicalPoint {
            x: from.x + dx * s - dy * bell,
            y: from.y + dy * s + dx * bell,
        }
    }
}

/// Critically damped second-order spring trajectory solver.
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
