use std::{
    collections::VecDeque,
    f64::consts::{PI, TAU},
};

use serde::{Deserialize, Serialize};

use crate::{
    animation::parse_hex_color,
    geometry::{LogicalPoint, LogicalRect},
    kinematics::{
        BezierPointerSolver, MAX_POINTER_SPRING_MS, MinimumJerkSolver, PointerTrajectorySpec,
    },
};

const DIST_EPS: f64 = 1e-4;
const TANGENT_EPS: f64 = 1e-9;
const SUBDIVISIONS_PER_SEGMENT: usize = 16;
const CHOOSER_SAMPLES: usize = 24;
const VERLET_DT_MAX: f64 = 1.0 / 240.0;
const REST_CURSOR_ANGLE_RAD: f64 = -1.92;
const DEFAULT_ACCENT_HEX: &str = "#d2a8ff";
const DEFAULT_ACCENT_RGBA: [f32; 4] = [0.823_529_4, 0.658_823_55, 1.0, 1.0];

pub const CURSOR_SETTLE_FADE_MS: u64 = 280;
pub const CURSOR_CLICK_PULSE_MS: u64 = 160;
pub const CURSOR_CLICK_RIPPLE_MS: u64 = 360;
pub const CURSOR_TRAIL_LIFETIME_MS: u64 = 220;
pub const CURSOR_TRAIL_MAX_SAMPLES: usize = 32;
pub const CURSOR_TRAIL_RENDER_POINTS: usize = 8;
pub const CURSOR_MAX_RIPPLES: usize = 2;

/// 5-point Gauss-Legendre quadrature nodes on `[-1, 1]`.
const GL5_NODES: [f64; 5] = [
    0.0,
    -0.538_469_310_105_683_1,
    0.538_469_310_105_683_1,
    -0.906_179_845_938_664,
    0.906_179_845_938_664,
];

/// 5-point Gauss-Legendre quadrature weights on `[-1, 1]`.
const GL5_WEIGHTS: [f64; 5] = [
    0.568_888_888_888_888_9,
    0.478_628_670_499_366_5,
    0.478_628_670_499_366_5,
    0.236_926_885_056_189_1,
    0.236_926_885_056_189_1,
];

#[inline]
const fn pt(x: f64, y: f64) -> LogicalPoint {
    LogicalPoint { x, y }
}

#[inline]
fn add_pt(a: LogicalPoint, b: LogicalPoint) -> LogicalPoint {
    pt(a.x + b.x, a.y + b.y)
}

#[inline]
fn sub_pt(a: LogicalPoint, b: LogicalPoint) -> LogicalPoint {
    pt(a.x - b.x, a.y - b.y)
}

#[inline]
fn scale_pt(a: LogicalPoint, s: f64) -> LogicalPoint {
    pt(a.x * s, a.y * s)
}

#[inline]
fn dist_pt(a: LogicalPoint, b: LogicalPoint) -> f64 {
    (b.x - a.x).hypot(b.y - a.y)
}

#[inline]
fn is_finite_pt(p: LogicalPoint) -> bool {
    p.x.is_finite() && p.y.is_finite()
}

#[inline]
fn normalize_or_zero(v: LogicalPoint) -> LogicalPoint {
    let len = v.x.hypot(v.y);
    if len > TANGENT_EPS {
        pt(v.x / len, v.y / len)
    } else {
        pt(0.0, 0.0)
    }
}

/// Wraps an angle in radians onto `(-PI, PI]`.
///
/// Complexity: $O(1)$ time and $O(1)$ space.
#[inline]
#[must_use]
pub fn wrap_pi(angle_rad: f64) -> f64 {
    if !angle_rad.is_finite() {
        return 0.0;
    }
    let wrapped = (angle_rad + PI).rem_euclid(TAU) - PI;
    if wrapped <= -PI {
        wrapped + TAU
    } else {
        wrapped
    }
}

/// Deduplicates consecutive non-finite or nearly identical waypoints within `DIST_EPS`.
///
/// Complexity: $O(N)$ time and $O(N)$ space for $N$ input waypoints.
fn deduplicate_waypoints(waypoints: &[LogicalPoint]) -> Vec<LogicalPoint> {
    let mut out = Vec::with_capacity(waypoints.len());
    for &p in waypoints {
        if !is_finite_pt(p) {
            continue;
        }
        if let Some(&last) = out.last()
            && dist_pt(last, p) <= DIST_EPS
        {
            continue;
        }
        out.push(p);
    }
    out
}

/// Spatial curve family used when constructing multi-waypoint or heading-continuous pointer paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSplineKind {
    #[default]
    Auto,
    CatmullRom,
    Bezier,
}

/// Centripetal ($\alpha = 0.5$) Barry-Goldman Catmull-Rom spline over deduplicated $N \ge 2$ waypoints
/// with extrapolated phantom endpoints.
///
/// Guarantees no self-intersections or cusps on non-uniform chord lengths and produces finite analytic
/// tangents even across degenerate or sharp-turn segments.
///
/// Complexity: $O(N)$ build time and $O(N)$ space; $O(1)$ time and $O(1)$ space per segment evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct CentripetalCatmullRom {
    /// Length `n + 2`: `[phantom_start, w_0, ..., w_{n-1}, phantom_end]`.
    ctrl: Vec<LogicalPoint>,
}

impl CentripetalCatmullRom {
    /// Builds a centripetal Catmull-Rom spline over `waypoints`. Returns `None` if fewer than 2 distinct
    /// finite points remain after deduplication.
    ///
    /// Complexity: $O(N)$ time and $O(N)$ space.
    #[must_use]
    pub fn new(waypoints: &[LogicalPoint]) -> Option<Self> {
        let dedup = deduplicate_waypoints(waypoints);
        if dedup.len() < 2 {
            return None;
        }
        let n = dedup.len();
        let phantom_start = sub_pt(scale_pt(dedup[0], 2.0), dedup[1]);
        let phantom_end = sub_pt(scale_pt(dedup[n - 1], 2.0), dedup[n - 2]);
        let mut ctrl = Vec::with_capacity(n + 2);
        ctrl.push(phantom_start);
        ctrl.extend(dedup);
        ctrl.push(phantom_end);
        Some(Self { ctrl })
    }

    /// Number of spline segments ($N - 1$).
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.ctrl.len().saturating_sub(3)
    }

    /// Returns the original interior waypoints slice `w_0..w_{N-1}`.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub fn waypoints(&self) -> &[LogicalPoint] {
        &self.ctrl[1..self.ctrl.len() - 1]
    }

    /// Evaluates `(position, derivative_wrt_u)` on segment `seg` at local parameter `u` in `[0, 1]`
    /// via the Barry-Goldman centripetal algorithm ($\alpha = 0.5$).
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[must_use]
    pub fn eval_segment(&self, seg: usize, u: f64) -> (LogicalPoint, LogicalPoint) {
        let seg_count = self.segment_count();
        let idx = seg.min(seg_count.saturating_sub(1));
        let u_clamped = if u.is_finite() {
            u.clamp(0.0, 1.0)
        } else {
            0.0
        };

        let p0 = self.ctrl[idx];
        let p1 = self.ctrl[idx + 1];
        let p2 = self.ctrl[idx + 2];
        let p3 = self.ctrl[idx + 3];

        // Centripetal knot spacing: dt = |P_{i+1} - P_i|^{0.5}, guarded away from zero on hairpins.
        let dt_floor = DIST_EPS.sqrt();
        let dt01 = dist_pt(p0, p1).sqrt().max(dt_floor);
        let dt12 = dist_pt(p1, p2).sqrt().max(dt_floor);
        let dt23 = dist_pt(p2, p3).sqrt().max(dt_floor);

        let t0 = 0.0;
        let t1 = dt01;
        let t2 = t1 + dt12;
        let t3 = t2 + dt23;
        let t = t1 + u_clamped * dt12;

        let dt02 = (t2 - t0).max(dt_floor);
        let dt13 = (t3 - t1).max(dt_floor);

        let a1 = add_pt(scale_pt(p0, (t1 - t) / dt01), scale_pt(p1, (t - t0) / dt01));
        let a2 = add_pt(scale_pt(p1, (t2 - t) / dt12), scale_pt(p2, (t - t1) / dt12));
        let a3 = add_pt(scale_pt(p2, (t3 - t) / dt23), scale_pt(p3, (t - t2) / dt23));

        let da1 = scale_pt(sub_pt(p1, p0), 1.0 / dt01);
        let da2 = scale_pt(sub_pt(p2, p1), 1.0 / dt12);
        let da3 = scale_pt(sub_pt(p3, p2), 1.0 / dt23);

        let b1 = add_pt(scale_pt(a1, (t2 - t) / dt02), scale_pt(a2, (t - t0) / dt02));
        let b2 = add_pt(scale_pt(a2, (t3 - t) / dt13), scale_pt(a3, (t - t1) / dt13));

        let db1 = add_pt(
            scale_pt(sub_pt(a2, a1), 1.0 / dt02),
            add_pt(
                scale_pt(da1, (t2 - t) / dt02),
                scale_pt(da2, (t - t0) / dt02),
            ),
        );
        let db2 = add_pt(
            scale_pt(sub_pt(a3, a2), 1.0 / dt13),
            add_pt(
                scale_pt(da2, (t3 - t) / dt13),
                scale_pt(da3, (t - t1) / dt13),
            ),
        );

        let pos = add_pt(scale_pt(b1, (t2 - t) / dt12), scale_pt(b2, (t - t1) / dt12));
        let dc_dt = add_pt(
            scale_pt(sub_pt(b2, b1), 1.0 / dt12),
            add_pt(
                scale_pt(db1, (t2 - t) / dt12),
                scale_pt(db2, (t - t1) / dt12),
            ),
        );
        let dc_du = scale_pt(dc_dt, dt12);

        (pos, dc_du)
    }

    /// Returns the chord vector `P2 - P1` for segment `seg`.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub fn segment_chord(&self, seg: usize) -> LogicalPoint {
        let idx = seg.min(self.segment_count().saturating_sub(1));
        sub_pt(self.ctrl[idx + 2], self.ctrl[idx + 1])
    }
}

/// Planar cubic Bezier curve segment $B(u)$ with control points `(p0, c1, c2, p3)`.
///
/// Complexity: $O(1)$ time and $O(1)$ space for evaluation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CubicBezierSegment2D {
    pub p0: LogicalPoint,
    pub c1: LogicalPoint,
    pub c2: LogicalPoint,
    pub p3: LogicalPoint,
}

impl CubicBezierSegment2D {
    /// Creates a cubic Bezier segment from four control points.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub const fn new(
        p0: LogicalPoint,
        c1: LogicalPoint,
        c2: LogicalPoint,
        p3: LogicalPoint,
    ) -> Self {
        Self { p0, c1, c2, p3 }
    }

    /// Evaluates `(position, derivative_wrt_u)` at local parameter `u` in `[0, 1]`.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub fn eval(&self, u: f64) -> (LogicalPoint, LogicalPoint) {
        let u = if u.is_finite() {
            u.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let om = 1.0 - u;
        let om2 = om * om;
        let u2 = u * u;

        let b0 = om2 * om;
        let b1 = 3.0 * om2 * u;
        let b2 = 3.0 * om * u2;
        let b3 = u2 * u;

        let pos = pt(
            b0 * self.p0.x + b1 * self.c1.x + b2 * self.c2.x + b3 * self.p3.x,
            b0 * self.p0.y + b1 * self.c1.y + b2 * self.c2.y + b3 * self.p3.y,
        );

        let d0 = 3.0 * om2;
        let d1 = 6.0 * om * u;
        let d2 = 3.0 * u2;
        let deriv = pt(
            d0 * (self.c1.x - self.p0.x)
                + d1 * (self.c2.x - self.c1.x)
                + d2 * (self.p3.x - self.c2.x),
            d0 * (self.c1.y - self.p0.y)
                + d1 * (self.c2.y - self.c1.y)
                + d2 * (self.p3.y - self.c2.y),
        );

        (pos, deriv)
    }

    /// Returns the chord vector `p3 - p0`.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub fn chord(&self) -> LogicalPoint {
        sub_pt(self.p3, self.p0)
    }
}

/// Evaluates minimum-cost cubic Bezier path across `direct`, `turn` (left/right), `brake`, and
/// `orbit` (CW/CCW) curve families (up to 7 candidates), sampling $K = 24$ points per candidate.
///
/// Balances excess arc length ratio, discrete bending energy $\sum \Delta\theta_k^2$, peak turn
/// angle $\max |\Delta\theta_k|$, initial heading mismatch, and viewport out-of-bounds penalty.
///
/// Complexity: $O(1)$ time and $O(1)$ space (fixed 7 candidates $\times$ 24 samples).
#[derive(Debug, Clone, Copy, Default)]
pub struct HeadingArcChooser;

impl HeadingArcChooser {
    /// Selects the lowest-cost `CubicBezierSegment2D` from `from` to `to`.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[must_use]
    pub fn choose(
        from: LogicalPoint,
        to: LogicalPoint,
        initial_heading_rad: Option<f64>,
        bounds: Option<LogicalRect>,
    ) -> CubicBezierSegment2D {
        let chord = sub_pt(to, from);
        let dist = chord.x.hypot(chord.y);
        if !dist.is_finite() || dist <= DIST_EPS {
            return CubicBezierSegment2D::new(from, from, to, to);
        }

        let dir = pt(chord.x / dist, chord.y / dist);
        let perp = pt(-dir.y, dir.x);
        let chord_angle = dir.y.atan2(dir.x);
        let h0 = initial_heading_rad
            .filter(|a| a.is_finite())
            .unwrap_or(chord_angle);
        let u0 = pt(h0.cos(), h0.sin());

        // Candidate family 1: direct chord.
        let c_direct = CubicBezierSegment2D::new(
            from,
            add_pt(from, scale_pt(dir, 0.34 * dist)),
            sub_pt(to, scale_pt(dir, 0.34 * dist)),
            to,
        );

        // Candidate family 2: gentle heading-blended left/right turns.
        let make_turn = |side: f64| {
            let start_dir =
                normalize_or_zero(add_pt(scale_pt(u0, 0.85), scale_pt(perp, side * 0.15)));
            let end_in_dir = normalize_or_zero(add_pt(dir, scale_pt(perp, -side * 0.24)));
            CubicBezierSegment2D::new(
                from,
                add_pt(from, scale_pt(start_dir, 0.38 * dist)),
                sub_pt(to, scale_pt(end_in_dir, 0.36 * dist)),
                to,
            )
        };

        // Candidate family 3: deceleration / brake arcs for sharp heading reversals.
        let make_brake = |side: f64| {
            let end_in_dir =
                normalize_or_zero(add_pt(scale_pt(dir, 0.90), scale_pt(perp, -side * 0.14)));
            CubicBezierSegment2D::new(
                from,
                add_pt(from, scale_pt(u0, 0.22 * dist)),
                sub_pt(to, scale_pt(end_in_dir, 0.28 * dist)),
                to,
            )
        };

        // Candidate family 4: wide CW/CCW orbit arcs for smooth ~180 deg U-turns and edge avoidance.
        let make_orbit = |side: f64| {
            let start_dir =
                normalize_or_zero(add_pt(scale_pt(u0, 0.45), scale_pt(perp, side * 0.55)));
            let end_in_dir =
                normalize_or_zero(add_pt(scale_pt(dir, 0.55), scale_pt(perp, -side * 0.48)));
            CubicBezierSegment2D::new(
                from,
                add_pt(from, scale_pt(start_dir, 0.52 * dist)),
                sub_pt(to, scale_pt(end_in_dir, 0.50 * dist)),
                to,
            )
        };

        let candidates = [
            c_direct,
            make_turn(1.0),
            make_turn(-1.0),
            make_brake(1.0),
            make_brake(-1.0),
            make_orbit(1.0),
            make_orbit(-1.0),
        ];

        let mut best = c_direct;
        let mut best_cost = f64::INFINITY;
        for cand in candidates {
            let cost = score_candidate_bezier(&cand, dist, initial_heading_rad, bounds);
            if cost < best_cost {
                best_cost = cost;
                best = cand;
            }
        }
        best
    }
}

/// Evaluates the cost of a candidate cubic Bezier over $K = 24$ uniformly sampled parameter points.
///
/// Complexity: $O(1)$ time and $O(1)$ space ($K = 24$).
fn score_candidate_bezier(
    seg: &CubicBezierSegment2D,
    chord_dist: f64,
    initial_heading_rad: Option<f64>,
    bounds: Option<LogicalRect>,
) -> f64 {
    let mut pts = [pt(0.0, 0.0); CHOOSER_SAMPLES];
    for (k, slot) in pts.iter_mut().enumerate() {
        let u = (k as f64) / ((CHOOSER_SAMPLES - 1) as f64);
        *slot = seg.eval(u).0;
    }

    let mut arc_len = 0.0;
    let mut prev_angle: Option<f64> = None;
    let mut first_angle: Option<f64> = None;
    let mut bend_energy = 0.0;
    let mut peak_turn = 0.0_f64;

    for k in 0..(CHOOSER_SAMPLES - 1) {
        let delta = sub_pt(pts[k + 1], pts[k]);
        let step_len = delta.x.hypot(delta.y);
        arc_len += step_len;
        if step_len > 1e-6 {
            let ang = delta.y.atan2(delta.x);
            if first_angle.is_none() {
                first_angle = Some(ang);
            }
            if let Some(prev) = prev_angle {
                let dtheta = wrap_pi(ang - prev);
                bend_energy += dtheta * dtheta;
                peak_turn = peak_turn.max(dtheta.abs());
            }
            prev_angle = Some(ang);
        }
    }

    let excess_ratio = ((arc_len / chord_dist.max(DIST_EPS)) - 1.0).max(0.0);

    let heading_mismatch = if let Some(h0) = initial_heading_rad.filter(|a| a.is_finite()) {
        let (_, deriv0) = seg.eval(0.0);
        let depart = if deriv0.x.hypot(deriv0.y) > 1e-6 {
            deriv0.y.atan2(deriv0.x)
        } else {
            first_angle.unwrap_or(h0)
        };
        wrap_pi(depart - h0).abs()
    } else {
        0.0
    };

    let oob_penalty = if let Some(b) = bounds {
        let min_x = f64::from(b.x);
        let min_y = f64::from(b.y);
        let max_x = min_x + f64::from(b.w);
        let max_y = min_y + f64::from(b.h);
        let mut oob_sum = 0.0;
        let mut oob_hits = 0usize;
        for p in pts {
            let dx_out = if p.x < min_x {
                min_x - p.x
            } else if p.x > max_x {
                p.x - max_x
            } else {
                0.0
            };
            let dy_out = if p.y < min_y {
                min_y - p.y
            } else if p.y > max_y {
                p.y - max_y
            } else {
                0.0
            };
            let d = dx_out.hypot(dy_out);
            if d > 0.0 {
                oob_hits += 1;
                oob_sum += d / chord_dist.max(1.0);
            }
        }
        let oob_frac = (oob_hits as f64) / (CHOOSER_SAMPLES as f64);
        25.0 * oob_sum + 8.0 * oob_frac
    } else {
        0.0
    };

    1.25 * excess_ratio
        + 0.65 * bend_energy
        + 1.40 * peak_turn
        + 1.85 * heading_mismatch
        + oob_penalty
}

#[derive(Debug, Clone, PartialEq)]
enum SplineGeometry {
    Point(LogicalPoint, f64),
    CatmullRom(CentripetalCatmullRom),
    Bezier(Vec<CubicBezierSegment2D>),
}

/// Adaptive arc-length parameterized multi-segment spline backed by 5-point Gauss-Legendre
/// quadrature over $M = 16$ uniform parameter subdivisions per segment.
///
/// Build Complexity: $O(N \cdot M)$ time and $O(N \cdot M)$ space where $N$ is waypoint count and $M = 16$.
/// Sample Complexity: $O(\log(N \cdot M))$ time and $O(1)$ space (zero heap allocations per tick).
#[derive(Debug, Clone, PartialEq)]
pub struct ArcLengthSpline {
    geometry: SplineGeometry,
    /// Monotonic cumulative LUT `(cum_arc_len, segment_index, local_u)`.
    lut: Vec<(f64, u32, f64)>,
    total_length: f64,
}

impl ArcLengthSpline {
    /// Builds an `ArcLengthSpline` from waypoints according to `spline_kind`, optional
    /// `initial_heading_rad`, and optional `viewport_bounds`.
    ///
    /// Complexity: $O(N \cdot M)$ time and $O(N \cdot M)$ space ($M = 16$).
    #[must_use]
    pub fn from_waypoints(
        waypoints: &[LogicalPoint],
        spline_kind: PathSplineKind,
        initial_heading_rad: Option<f64>,
        viewport_bounds: Option<LogicalRect>,
    ) -> Self {
        let dedup = deduplicate_waypoints(waypoints);
        if dedup.is_empty() {
            return Self::degenerate(pt(0.0, 0.0), initial_heading_rad.unwrap_or(0.0));
        }
        if dedup.len() == 1 {
            return Self::degenerate(dedup[0], initial_heading_rad.unwrap_or(0.0));
        }

        let use_bezier = match spline_kind {
            PathSplineKind::Bezier => true,
            PathSplineKind::CatmullRom => false,
            PathSplineKind::Auto => dedup.len() == 2 && initial_heading_rad.is_some(),
        };

        if use_bezier {
            let mut segs = Vec::with_capacity(dedup.len() - 1);
            let mut cur_heading = initial_heading_rad.filter(|a| a.is_finite());
            for i in 0..(dedup.len() - 1) {
                let p0 = dedup[i];
                let p3 = dedup[i + 1];
                let seg = HeadingArcChooser::choose(p0, p3, cur_heading, viewport_bounds);
                let (_, d_end) = seg.eval(1.0);
                cur_heading = if d_end.x.hypot(d_end.y) > 1e-6 {
                    Some(d_end.y.atan2(d_end.x))
                } else {
                    let ch = seg.chord();
                    (ch.x.hypot(ch.y) > 1e-6).then(|| ch.y.atan2(ch.x))
                };
                segs.push(seg);
            }
            return Self::from_bezier_segments(segs);
        }

        match CentripetalCatmullRom::new(&dedup) {
            Some(cr) => Self::from_catmull_rom(cr),
            None => Self::degenerate(dedup[0], initial_heading_rad.unwrap_or(0.0)),
        }
    }

    /// Wraps a `CentripetalCatmullRom` into an arc-length LUT spline.
    ///
    /// Complexity: $O(N \cdot M)$ time and $O(N \cdot M)$ space ($M = 16$).
    #[must_use]
    pub fn from_catmull_rom(cr: CentripetalCatmullRom) -> Self {
        let seg_count = cr.segment_count();
        Self::build_lut(SplineGeometry::CatmullRom(cr), seg_count)
    }

    /// Wraps one or more `CubicBezierSegment2D` spans into an arc-length LUT spline.
    ///
    /// Complexity: $O(N \cdot M)$ time and $O(N \cdot M)$ space ($M = 16$).
    #[must_use]
    pub fn from_bezier_segments(segs: Vec<CubicBezierSegment2D>) -> Self {
        if segs.is_empty() {
            return Self::degenerate(pt(0.0, 0.0), 0.0);
        }
        let seg_count = segs.len();
        Self::build_lut(SplineGeometry::Bezier(segs), seg_count)
    }

    fn degenerate(p: LogicalPoint, heading_rad: f64) -> Self {
        Self {
            geometry: SplineGeometry::Point(
                p,
                if heading_rad.is_finite() {
                    wrap_pi(heading_rad)
                } else {
                    0.0
                },
            ),
            lut: vec![(0.0, 0, 0.0)],
            total_length: 0.0,
        }
    }

    fn build_lut(geometry: SplineGeometry, seg_count: usize) -> Self {
        let mut lut = Vec::with_capacity(1 + seg_count * SUBDIVISIONS_PER_SEGMENT);
        lut.push((0.0, 0, 0.0));
        let mut cum_len = 0.0;

        for seg_idx in 0..seg_count {
            for sub in 0..SUBDIVISIONS_PER_SEGMENT {
                let u0 = (sub as f64) / (SUBDIVISIONS_PER_SEGMENT as f64);
                let u1 = ((sub + 1) as f64) / (SUBDIVISIONS_PER_SEGMENT as f64);
                let half = 0.5 * (u1 - u0);
                let mid = 0.5 * (u0 + u1);

                let mut seg_arc = 0.0;
                for i in 0..5 {
                    let u_sample = mid + half * GL5_NODES[i];
                    let (_, deriv) = eval_geometry_raw(&geometry, seg_idx, u_sample);
                    let speed = deriv.x.hypot(deriv.y);
                    if speed.is_finite() {
                        seg_arc += GL5_WEIGHTS[i] * speed;
                    }
                }
                seg_arc *= half;
                cum_len += seg_arc.max(0.0);
                lut.push((cum_len, seg_idx as u32, u1));
            }
        }

        Self {
            geometry,
            lut,
            total_length: cum_len,
        }
    }

    /// Total arc length along the spline in logical pixels.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub const fn total_length(&self) -> f64 {
        self.total_length
    }

    /// Number of geometric segments.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub fn segment_count(&self) -> usize {
        match &self.geometry {
            SplineGeometry::Point(_, _) => 0,
            SplineGeometry::CatmullRom(cr) => cr.segment_count(),
            SplineGeometry::Bezier(segs) => segs.len(),
        }
    }

    /// Start point of the spline.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub fn start_point(&self) -> LogicalPoint {
        eval_geometry_raw(&self.geometry, 0, 0.0).0
    }

    /// Terminal target point of the spline.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub fn end_point(&self) -> LogicalPoint {
        let last = self.segment_count().saturating_sub(1);
        eval_geometry_raw(&self.geometry, last, 1.0).0
    }

    /// Samples `(point, heading_rad)` at uniform arc-length fraction `frac` in `[0.0, 1.0]`
    /// using binary search (`partition_point`) over the cumulative arc-length LUT.
    ///
    /// Complexity: $O(\log(N \cdot M))$ time and $O(1)$ space (zero heap allocations).
    #[must_use]
    pub fn sample_at_fraction(&self, frac: f64) -> (LogicalPoint, f64) {
        if let SplineGeometry::Point(p, heading) = &self.geometry {
            return (*p, *heading);
        }
        let seg_count = self.segment_count();
        if seg_count == 0 || self.total_length <= TANGENT_EPS || self.lut.len() < 2 {
            let (pos, _) = eval_geometry_raw(&self.geometry, 0, 0.0);
            return (pos, self.chord_heading(0));
        }

        let f = if frac.is_finite() {
            frac.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if f <= 0.0 {
            return self.eval_with_heading(0, 0.0);
        }
        if f >= 1.0 {
            return self.eval_with_heading(seg_count - 1, 1.0);
        }

        let target_s = f * self.total_length;
        let hi = self
            .lut
            .partition_point(|&(s, _, _)| s < target_s)
            .clamp(1, self.lut.len() - 1);
        let lo = hi - 1;

        let (s_lo, seg_lo, u_lo) = self.lut[lo];
        let (s_hi, seg_hi, u_hi) = self.lut[hi];
        let seg = seg_hi as usize;
        let u_start = if seg_lo == seg_hi { u_lo } else { 0.0 };
        let ds = s_hi - s_lo;
        let t = if ds > 1e-12 {
            ((target_s - s_lo) / ds).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let u = (u_start + t * (u_hi - u_start)).clamp(0.0, 1.0);
        self.eval_with_heading(seg, u)
    }

    fn eval_with_heading(&self, seg: usize, u: f64) -> (LogicalPoint, f64) {
        let (pos, deriv) = eval_geometry_raw(&self.geometry, seg, u);
        if deriv.x.hypot(deriv.y) > 1e-6 && deriv.x.is_finite() && deriv.y.is_finite() {
            return (pos, deriv.y.atan2(deriv.x));
        }
        // Fallback near zero-speed cusps or zero-length handles: nudge parameter or use segment chord.
        for delta_u in [1e-3, -1e-3, 1e-2, -1e-2] {
            let probe = (u + delta_u).clamp(0.0, 1.0);
            let (_, probe_deriv) = eval_geometry_raw(&self.geometry, seg, probe);
            if probe_deriv.x.hypot(probe_deriv.y) > 1e-6 {
                return (pos, probe_deriv.y.atan2(probe_deriv.x));
            }
        }
        (pos, self.chord_heading(seg))
    }

    fn chord_heading(&self, seg: usize) -> f64 {
        let ch = match &self.geometry {
            SplineGeometry::Point(_, h) => return *h,
            SplineGeometry::CatmullRom(cr) => cr.segment_chord(seg),
            SplineGeometry::Bezier(segs) => segs
                .get(seg.min(segs.len().saturating_sub(1)))
                .map_or(pt(0.0, 0.0), CubicBezierSegment2D::chord),
        };
        if ch.x.hypot(ch.y) > TANGENT_EPS {
            ch.y.atan2(ch.x)
        } else {
            0.0
        }
    }
}

#[inline]
fn eval_geometry_raw(
    geometry: &SplineGeometry,
    seg: usize,
    u: f64,
) -> (LogicalPoint, LogicalPoint) {
    match geometry {
        SplineGeometry::Point(p, h) => (*p, pt(h.cos(), h.sin())),
        SplineGeometry::CatmullRom(cr) => cr.eval_segment(seg, u),
        SplineGeometry::Bezier(segs) => {
            if segs.is_empty() {
                (pt(0.0, 0.0), pt(1.0, 0.0))
            } else {
                segs[seg.min(segs.len() - 1)].eval(u)
            }
        }
    }
}

/// Substepped (`dt_max = 1/240` s) Velocity Verlet 1D harmonic oscillator driven by `(response, damping_ratio)`.
///
/// Advances position `x` toward `target` with second-order Velocity Verlet integration.
///
/// Complexity: $O(1)$ per clamped frame tick (capped substep loop) and $O(1)$ space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VelocityVerletSpring1D {
    pub x: f64,
    pub v: f64,
    pub target: f64,
    pub response: f64,
    pub damping_ratio: f64,
    pub pos_epsilon: f64,
    pub vel_epsilon: f64,
    last_ms: u64,
    settled: bool,
}

impl VelocityVerletSpring1D {
    /// Creates a spring already settled at `value`.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[must_use]
    pub fn settled_at(value: f64, response: f64, damping_ratio: f64, now_ms: u64) -> Self {
        let v = if value.is_finite() { value } else { 0.0 };
        Self {
            x: v,
            v: 0.0,
            target: v,
            response: sanitize_response(response),
            damping_ratio: sanitize_damping(damping_ratio),
            pos_epsilon: 5e-4,
            vel_epsilon: 5e-3,
            last_ms: now_ms,
            settled: true,
        }
    }

    /// Launches a spring from `(from, initial_vel)` toward `to` at `now_ms`.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[must_use]
    pub fn launch(
        from: f64,
        to: f64,
        initial_vel: f64,
        response: f64,
        damping_ratio: f64,
        now_ms: u64,
    ) -> Self {
        let x0 = if from.is_finite() { from } else { 0.0 };
        let tgt = if to.is_finite() { to } else { x0 };
        let v0 = if initial_vel.is_finite() {
            initial_vel
        } else {
            0.0
        };
        let pos_epsilon = 5e-4;
        let vel_epsilon = 5e-3;
        let settled = (tgt - x0).abs() <= pos_epsilon && v0.abs() <= vel_epsilon;
        Self {
            x: if settled { tgt } else { x0 },
            v: if settled { 0.0 } else { v0 },
            target: tgt,
            response: sanitize_response(response),
            damping_ratio: sanitize_damping(damping_ratio),
            pos_epsilon,
            vel_epsilon,
            last_ms: now_ms,
            settled,
        }
    }

    /// Sets custom convergence thresholds `(pos_epsilon, vel_epsilon)` and refreshes settled status.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[must_use]
    pub fn with_tolerances(mut self, pos_epsilon: f64, vel_epsilon: f64) -> Self {
        self.pos_epsilon = pos_epsilon.max(1e-6);
        self.vel_epsilon = vel_epsilon.max(1e-5);
        self.settled = self.is_within_rest();
        if self.settled {
            self.x = self.target;
            self.v = 0.0;
        }
        self
    }

    /// Retargets to `new_target` while keeping instantaneous `(x, v)` momentum.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    pub fn retarget(&mut self, new_target: f64, now_ms: u64, response: f64, damping_ratio: f64) {
        self.advance_to(now_ms);
        let tgt = if new_target.is_finite() {
            new_target
        } else {
            self.target
        };
        self.target = tgt;
        self.response = sanitize_response(response);
        self.damping_ratio = sanitize_damping(damping_ratio);
        if self.is_within_rest() {
            self.x = self.target;
            self.v = 0.0;
            self.settled = true;
        } else {
            self.settled = false;
        }
    }

    /// Advances by `dt_secs` using `dt_max = 1/240` s Velocity Verlet substeps.
    ///
    /// Complexity: $O(1)$ per clamped 100ms chunk (bounded substeps) and $O(1)$ space.
    pub fn step_dt(&mut self, dt_secs: f64) {
        if self.settled || !dt_secs.is_finite() || dt_secs <= 0.0 {
            return;
        }
        let total_dt = dt_secs.clamp(0.0, 0.1);
        if total_dt <= 0.0 {
            return;
        }
        let steps = ((total_dt / VERLET_DT_MAX).ceil() as usize).clamp(1, 24);
        let h = total_dt / (steps as f64);
        let omega = TAU / self.response;
        let k = omega * omega;
        let c = 2.0 * self.damping_ratio * omega;
        let denom = 1.0 + 0.5 * c * h;

        for _ in 0..steps {
            let a0 = -k * (self.x - self.target) - c * self.v;
            let x_next = self.x + self.v * h + 0.5 * a0 * h * h;
            let v_half = self.v + 0.5 * a0 * h;
            let a_next = (-k * (x_next - self.target) - c * v_half) / denom;
            self.x = x_next;
            self.v = v_half + 0.5 * a_next * h;
        }

        if !self.x.is_finite() || !self.v.is_finite() || self.is_within_rest() {
            self.x = self.target;
            self.v = 0.0;
            self.settled = true;
        }
    }

    /// Advances the spring up to `now_ms` and returns `(x, v)`.
    ///
    /// Complexity: $O(1)$ per bounded simulation span and $O(1)$ space.
    pub fn sample_pos_vel(&mut self, now_ms: u64) -> (f64, f64) {
        self.advance_to(now_ms);
        (self.x, self.v)
    }

    /// Snap immediately to `value` and mark settled.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    pub fn snap_to(&mut self, value: f64, now_ms: u64) {
        let v = if value.is_finite() { value } else { 0.0 };
        self.x = v;
        self.v = 0.0;
        self.target = v;
        self.last_ms = self.last_ms.max(now_ms);
        self.settled = true;
    }

    #[inline]
    #[must_use]
    pub const fn is_settled(&self) -> bool {
        self.settled
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

    #[inline]
    fn is_within_rest(&self) -> bool {
        (self.x - self.target).abs() <= self.pos_epsilon && self.v.abs() <= self.vel_epsilon
    }
}

#[inline]
fn sanitize_response(response: f64) -> f64 {
    if response.is_finite() {
        response.clamp(0.01, 5.0)
    } else {
        0.18
    }
}

#[inline]
fn sanitize_damping(damping_ratio: f64) -> f64 {
    if damping_ratio.is_finite() {
        damping_ratio.clamp(0.1, 4.0)
    } else {
        0.92
    }
}

#[derive(Debug, Clone)]
enum TemporalDriver {
    Instant,
    MinimumJerk {
        start_ms: u64,
        duration_ms: u64,
        _solver: MinimumJerkSolver,
    },
    Bezier {
        start_ms: u64,
        duration_ms: u64,
        solver: BezierPointerSolver,
    },
    Spring {
        spring: VelocityVerletSpring1D,
    },
}

/// Unified multi-waypoint pointer trajectory track pairing `ArcLengthSpline` spatial geometry
/// with `MinimumJerk`, `Bezier`, `VelocityVerletSpring1D`, or `Instant` temporal progression.
///
/// Build Complexity: $O(N \cdot M)$ time and space ($M = 16$).
/// Step Complexity: $O(\log(N \cdot M))$ time and $O(1)$ space per tick.
#[derive(Debug, Clone)]
pub struct PointerPathTrack {
    spline: ArcLengthSpline,
    driver: TemporalDriver,
    target: LogicalPoint,
    max_finish_at_ms: u64,
    last_pos: LogicalPoint,
    last_vel: LogicalPoint,
    last_heading_rad: Option<f64>,
    settled: bool,
}

impl PointerPathTrack {
    /// Launches a trajectory through `waypoints` with the requested `spline`, initial kinematics,
    /// optional viewport containment `bounds`, and temporal `spec`.
    ///
    /// Complexity: $O(N \cdot M)$ build time and $O(N \cdot M)$ space ($M = 16$).
    #[must_use]
    pub fn launch_waypoints(
        waypoints: &[LogicalPoint],
        spline_kind: PathSplineKind,
        initial_vel: LogicalPoint,
        initial_heading_rad: Option<f64>,
        viewport_bounds: Option<LogicalRect>,
        now_ms: u64,
        spec: PointerTrajectorySpec,
    ) -> Self {
        let dedup = deduplicate_waypoints(waypoints);
        if dedup.is_empty() {
            return Self::instant_at(pt(0.0, 0.0), now_ms, initial_heading_rad);
        }
        if dedup.len() == 1 {
            return Self::instant_at(dedup[0], now_ms, initial_heading_rad);
        }

        let target = dedup[dedup.len() - 1];
        let start = dedup[0];
        let arc_spline = if dedup.len() == 2
            && initial_heading_rad.is_none()
            && spline_kind == PathSplineKind::Auto
            && let PointerTrajectorySpec::MinimumJerk { arc_curvature, .. }
            | PointerTrajectorySpec::Bezier { arc_curvature, .. }
            | PointerTrajectorySpec::HookeSpring { arc_curvature, .. } = spec
            && arc_curvature.is_finite()
            && arc_curvature.abs() > 1e-6
        {
            // Preserve smooth perpendicular arc curvature for two-point trajectories.
            let chord = sub_pt(target, start);
            let perp = pt(-chord.y, chord.x);
            let bow = scale_pt(perp, arc_curvature * 1.333_333_333_333_333_3);
            let c1 = add_pt(add_pt(start, scale_pt(chord, 1.0 / 3.0)), bow);
            let c2 = add_pt(add_pt(start, scale_pt(chord, 2.0 / 3.0)), bow);
            ArcLengthSpline::from_bezier_segments(vec![CubicBezierSegment2D::new(
                start, c1, c2, target,
            )])
        } else {
            ArcLengthSpline::from_waypoints(
                &dedup,
                spline_kind,
                initial_heading_rad,
                viewport_bounds,
            )
        };

        let total_len = arc_spline.total_length();
        let (_, start_heading) = arc_spline.sample_at_fraction(0.0);
        if total_len <= DIST_EPS {
            return Self::instant_at(target, now_ms, Some(start_heading));
        }

        match spec {
            PointerTrajectorySpec::Instant => Self::instant_at(target, now_ms, Some(start_heading)),
            PointerTrajectorySpec::MinimumJerk {
                duration_ms,
                arc_curvature,
            } => {
                let dur = duration_ms.max(1);
                Self {
                    spline: arc_spline,
                    driver: TemporalDriver::MinimumJerk {
                        start_ms: now_ms,
                        duration_ms: dur,
                        _solver: MinimumJerkSolver { arc_curvature },
                    },
                    target,
                    max_finish_at_ms: now_ms.saturating_add(dur),
                    last_pos: start,
                    last_vel: pt(0.0, 0.0),
                    last_heading_rad: Some(start_heading),
                    settled: false,
                }
            }
            PointerTrajectorySpec::Bezier {
                bezier,
                duration_ms,
                arc_curvature,
            } => {
                let dur = duration_ms.max(1);
                Self {
                    spline: arc_spline,
                    driver: TemporalDriver::Bezier {
                        start_ms: now_ms,
                        duration_ms: dur,
                        solver: BezierPointerSolver {
                            bezier,
                            arc_curvature,
                        },
                    },
                    target,
                    max_finish_at_ms: now_ms.saturating_add(dur),
                    last_pos: start,
                    last_vel: pt(0.0, 0.0),
                    last_heading_rad: Some(start_heading),
                    settled: false,
                }
            }
            PointerTrajectorySpec::HookeSpring {
                params,
                duration_cap_ms,
                ..
            } => {
                let omega = (params.stiffness / params.mass).sqrt().max(1e-3);
                let response = (TAU / omega).clamp(0.02, 5.0);
                let damping_ratio = (params.damping
                    / (2.0 * (params.mass * params.stiffness).sqrt()))
                .clamp(0.1, 4.0);
                let start_dir = pt(start_heading.cos(), start_heading.sin());
                let proj_speed = if is_finite_pt(initial_vel) {
                    initial_vel.x * start_dir.x + initial_vel.y * start_dir.y
                } else {
                    0.0
                };
                let s_vel0 = (proj_speed / total_len.max(1.0)).clamp(-6.0, 6.0);
                let spring = VelocityVerletSpring1D::launch(
                    0.0,
                    1.0,
                    s_vel0,
                    response,
                    damping_ratio,
                    now_ms,
                )
                .with_tolerances(
                    (0.5 / total_len).clamp(1e-4, 5e-3),
                    (5.0 / total_len).clamp(1e-3, 5e-2),
                );
                let cap = duration_cap_ms
                    .unwrap_or(MAX_POINTER_SPRING_MS)
                    .clamp(1, MAX_POINTER_SPRING_MS);
                Self {
                    spline: arc_spline,
                    driver: TemporalDriver::Spring { spring },
                    target,
                    max_finish_at_ms: now_ms.saturating_add(cap),
                    last_pos: start,
                    last_vel: if is_finite_pt(initial_vel) {
                        initial_vel
                    } else {
                        pt(0.0, 0.0)
                    },
                    last_heading_rad: Some(start_heading),
                    settled: false,
                }
            }
        }
    }

    /// Convenience two-point trajectory constructor supporting all `PointerTrajectorySpec` modes.
    ///
    /// Complexity: $O(M)$ build time and $O(M)$ space ($M = 16$).
    #[must_use]
    pub fn launch_two_point(
        from: LogicalPoint,
        to: LogicalPoint,
        initial_vel: LogicalPoint,
        initial_heading_rad: Option<f64>,
        viewport_bounds: Option<LogicalRect>,
        now_ms: u64,
        spec: PointerTrajectorySpec,
    ) -> Self {
        Self::launch_waypoints(
            &[from, to],
            PathSplineKind::Auto,
            initial_vel,
            initial_heading_rad,
            viewport_bounds,
            now_ms,
            spec,
        )
    }

    fn instant_at(target: LogicalPoint, now_ms: u64, heading_rad: Option<f64>) -> Self {
        let h = heading_rad.filter(|a| a.is_finite()).unwrap_or(0.0);
        let pos = if is_finite_pt(target) {
            target
        } else {
            pt(0.0, 0.0)
        };
        Self {
            spline: ArcLengthSpline::degenerate(pos, h),
            driver: TemporalDriver::Instant,
            target: pos,
            max_finish_at_ms: now_ms,
            last_pos: pos,
            last_vel: pt(0.0, 0.0),
            last_heading_rad: heading_rad.filter(|a| a.is_finite()),
            settled: true,
        }
    }

    /// Advances the trajectory to `now_ms`, updating cached velocity & heading, and returns pointer position.
    ///
    /// Complexity: $O(\log(N \cdot M))$ time and $O(1)$ space per tick.
    pub fn step(&mut self, now_ms: u64) -> LogicalPoint {
        if self.settled {
            return self.target;
        }

        if now_ms >= self.max_finish_at_ms {
            self.settled = true;
            self.last_pos = self.target;
            self.last_vel = pt(0.0, 0.0);
            if let TemporalDriver::Spring { spring } = &mut self.driver {
                spring.snap_to(1.0, now_ms);
            }
            return self.target;
        }

        let total_len = self.spline.total_length();
        let (frac, ds_dt, finished_now) = match &mut self.driver {
            TemporalDriver::Instant => (1.0, 0.0, true),
            TemporalDriver::MinimumJerk {
                start_ms,
                duration_ms,
                ..
            } => {
                let dur_ms = (*duration_ms).max(1);
                let elapsed = now_ms.saturating_sub(*start_ms);
                let tau = (elapsed as f64 / dur_ms as f64).clamp(0.0, 1.0);
                let om = 1.0 - tau;
                let s = tau * tau * tau * (10.0 + tau * (-15.0 + 6.0 * tau));
                let ds_dtau = 30.0 * tau * tau * om * om;
                let dur_s = dur_ms as f64 / 1000.0;
                (s.clamp(0.0, 1.0), ds_dtau / dur_s, elapsed >= dur_ms)
            }
            TemporalDriver::Bezier {
                start_ms,
                duration_ms,
                solver,
            } => {
                let dur_ms = (*duration_ms).max(1);
                let elapsed = now_ms.saturating_sub(*start_ms);
                let tau = (elapsed as f64 / dur_ms as f64).clamp(0.0, 1.0);
                let s = solver.bezier.sample(tau);
                let ds_dtau = solver.bezier.velocity_norm(tau);
                let dur_s = dur_ms as f64 / 1000.0;
                (s.clamp(0.0, 1.0), ds_dtau / dur_s, elapsed >= dur_ms)
            }
            TemporalDriver::Spring { spring } => {
                let (x, v) = spring.sample_pos_vel(now_ms);
                (x.clamp(0.0, 1.0), v, spring.is_settled())
            }
        };

        let (pos, heading) = self.spline.sample_at_fraction(frac);
        self.last_heading_rad = Some(heading);
        if finished_now {
            self.settled = true;
            self.last_pos = self.target;
            self.last_vel = pt(0.0, 0.0);
            return self.target;
        }

        let speed_px_s = total_len * ds_dt;
        self.last_pos = pos;
        self.last_vel = pt(heading.cos() * speed_px_s, heading.sin() * speed_px_s);
        pos
    }

    /// Retargets to a new waypoint sequence starting from the current instantaneous position,
    /// velocity, and heading.
    ///
    /// Complexity: $O(N \cdot M)$ build time and $O(N \cdot M)$ space ($M = 16$).
    pub fn retarget_waypoints(
        &mut self,
        waypoints: &[LogicalPoint],
        spline_kind: PathSplineKind,
        viewport_bounds: Option<LogicalRect>,
        now_ms: u64,
        spec: PointerTrajectorySpec,
    ) {
        let cur_pos = self.step(now_ms);
        let cur_vel = self.last_vel;
        let cur_heading = self.last_heading_rad;
        let mut combined = Vec::with_capacity(waypoints.len() + 1);
        combined.push(cur_pos);
        combined.extend_from_slice(waypoints);
        *self = Self::launch_waypoints(
            &combined,
            spline_kind,
            cur_vel,
            cur_heading,
            viewport_bounds,
            now_ms,
            spec,
        );
    }

    /// Last sampled physical velocity `(vx, vy)` in logical pixels per second.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub const fn current_velocity(&self) -> LogicalPoint {
        self.last_vel
    }

    /// Last sampled movement heading in radians, if defined.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub const fn current_heading_rad(&self) -> Option<f64> {
        self.last_heading_rad
    }

    /// Whether the trajectory has arrived and settled at `target_point()`.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub const fn is_settled(&self) -> bool {
        self.settled
    }

    /// Safety deadline in milliseconds by which the trajectory finishes.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub const fn max_finish_at_ms(&self) -> u64 {
        self.max_finish_at_ms
    }

    /// Final destination waypoint of this trajectory.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub const fn target_point(&self) -> LogicalPoint {
        self.target
    }

    /// Total arc length of the underlying spline in logical pixels.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[inline]
    #[must_use]
    pub const fn total_length(&self) -> f64 {
        self.spline.total_length()
    }
}

/// Lua-configurable presentation options for the per-workspace AI Agent cursor indicator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentCursorConfig {
    pub enabled: bool,
    pub size: f32,
    pub accent_color: String,
    pub trail_enabled: bool,
    pub fog_enabled: bool,
    pub ripple_enabled: bool,
    pub idle_timeout_ms: u64,
    pub idle_wobble: bool,
}

impl Default for AgentCursorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            size: 24.0,
            accent_color: DEFAULT_ACCENT_HEX.to_owned(),
            trail_enabled: true,
            fog_enabled: true,
            ripple_enabled: true,
            idle_timeout_ms: 12_000,
            idle_wobble: true,
        }
    }
}

impl AgentCursorConfig {
    /// Parses `accent_color` hex string into normalized linear-ish `[r, g, b, a]` components.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[must_use]
    pub fn accent_rgba(&self) -> [f32; 4] {
        parse_hex_color(&self.accent_color, DEFAULT_ACCENT_RGBA)
    }
}

/// Render-ready snapshot of the virtual AI cursor, aura fog, ripples, and tapered ribbon trail.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCursorFrameSnapshot {
    pub tip_pos: LogicalPoint,
    pub body_offset: LogicalPoint,
    pub rotation_rad: f32,
    pub body_scale: f32,
    pub opacity: f32,
    pub size: f32,
    pub accent_rgba: [f32; 4],
    pub fog_offset: LogicalPoint,
    pub fog_scale: f32,
    pub fog_opacity: f32,
    pub ripples: Vec<(LogicalPoint, f32, f32)>,
    pub trail_points: Vec<(LogicalPoint, f32, f32)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TrailSample {
    pos: LogicalPoint,
    time_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CursorClickRipple {
    center: LogicalPoint,
    start_ms: u64,
}

/// Dual-layer spring visual state machine for the AI agent cursor (tip spring + heading angle
/// spring + trailing fog/body displacement + sinusoidal click pulse/ripples + ring-buffer trail).
#[derive(Debug, Clone)]
pub struct AgentCursorVisualDynamics {
    active: bool,
    activated_ms: u64,
    last_activity_ms: u64,
    settle_fade_start_ms: Option<u64>,
    last_step_ms: u64,
    last_target_vel: LogicalPoint,
    last_heading_rad: Option<f64>,
    tip_x: VelocityVerletSpring1D,
    tip_y: VelocityVerletSpring1D,
    angle_spring: VelocityVerletSpring1D,
    body_x: VelocityVerletSpring1D,
    body_y: VelocityVerletSpring1D,
    fog_x: VelocityVerletSpring1D,
    fog_y: VelocityVerletSpring1D,
    click_pulse_start_ms: Option<u64>,
    ripples: VecDeque<CursorClickRipple>,
    trail: VecDeque<TrailSample>,
}

impl Default for AgentCursorVisualDynamics {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentCursorVisualDynamics {
    const TIP_RESPONSE: f64 = 0.085;
    const TIP_DAMPING: f64 = 0.82;
    const ANGLE_RESPONSE: f64 = 0.14;
    const ANGLE_DAMPING: f64 = 0.84;
    const BODY_RESPONSE: f64 = 0.11;
    const BODY_DAMPING: f64 = 0.80;
    const FOG_RESPONSE: f64 = 0.18;
    const FOG_DAMPING: f64 = 0.85;

    /// Constructs a hidden idle visual dynamics tracker.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: false,
            activated_ms: 0,
            last_activity_ms: 0,
            settle_fade_start_ms: None,
            last_step_ms: 0,
            last_target_vel: pt(0.0, 0.0),
            last_heading_rad: None,
            tip_x: VelocityVerletSpring1D::settled_at(
                0.0,
                Self::TIP_RESPONSE,
                Self::TIP_DAMPING,
                0,
            ),
            tip_y: VelocityVerletSpring1D::settled_at(
                0.0,
                Self::TIP_RESPONSE,
                Self::TIP_DAMPING,
                0,
            ),
            angle_spring: VelocityVerletSpring1D::settled_at(
                REST_CURSOR_ANGLE_RAD,
                Self::ANGLE_RESPONSE,
                Self::ANGLE_DAMPING,
                0,
            ),
            body_x: VelocityVerletSpring1D::settled_at(
                0.0,
                Self::BODY_RESPONSE,
                Self::BODY_DAMPING,
                0,
            ),
            body_y: VelocityVerletSpring1D::settled_at(
                0.0,
                Self::BODY_RESPONSE,
                Self::BODY_DAMPING,
                0,
            ),
            fog_x: VelocityVerletSpring1D::settled_at(
                0.0,
                Self::FOG_RESPONSE,
                Self::FOG_DAMPING,
                0,
            ),
            fog_y: VelocityVerletSpring1D::settled_at(
                0.0,
                Self::FOG_RESPONSE,
                Self::FOG_DAMPING,
                0,
            ),
            click_pulse_start_ms: None,
            ripples: VecDeque::with_capacity(CURSOR_MAX_RIPPLES),
            trail: VecDeque::with_capacity(CURSOR_TRAIL_MAX_SAMPLES),
        }
    }

    /// Updates cursor target position and instantaneous motion hint, activating visibility if hidden.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    pub fn set_target(
        &mut self,
        pos: LogicalPoint,
        vel_hint: LogicalPoint,
        heading_hint: Option<f64>,
        now_ms: u64,
    ) {
        if !is_finite_pt(pos) {
            return;
        }
        let was_active = self.active;
        self.active = true;
        self.last_activity_ms = now_ms;
        self.settle_fade_start_ms = None;
        self.last_target_vel = if is_finite_pt(vel_hint) {
            vel_hint
        } else {
            pt(0.0, 0.0)
        };
        if let Some(h) = heading_hint.filter(|a| a.is_finite()) {
            self.last_heading_rad = Some(wrap_pi(h));
        }

        if !was_active {
            self.activated_ms = now_ms;
            self.last_step_ms = now_ms;
            self.tip_x.snap_to(pos.x, now_ms);
            self.tip_y.snap_to(pos.y, now_ms);
            self.tip_x.pos_epsilon = 0.05;
            self.tip_x.vel_epsilon = 0.5;
            self.tip_y.pos_epsilon = 0.05;
            self.tip_y.vel_epsilon = 0.5;
            self.body_x.snap_to(0.0, now_ms);
            self.body_y.snap_to(0.0, now_ms);
            self.fog_x.snap_to(0.0, now_ms);
            self.fog_y.snap_to(0.0, now_ms);
            let start_ang = self.last_heading_rad.unwrap_or(REST_CURSOR_ANGLE_RAD);
            self.angle_spring.snap_to(start_ang, now_ms);
            self.trail.clear();
            self.trail.push_back(TrailSample {
                pos,
                time_ms: now_ms,
            });
        } else {
            self.tip_x
                .retarget(pos.x, now_ms, Self::TIP_RESPONSE, Self::TIP_DAMPING);
            self.tip_y
                .retarget(pos.y, now_ms, Self::TIP_RESPONSE, Self::TIP_DAMPING);
        }
    }

    /// Snaps the resting visual tip without activating trail streaks (e.g., when tracking an anchored
    /// window scroll while visible).
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    pub fn track_anchored_position(&mut self, pos: LogicalPoint, now_ms: u64) {
        if !self.active || !is_finite_pt(pos) {
            return;
        }
        self.tip_x
            .retarget(pos.x, now_ms, Self::TIP_RESPONSE, Self::TIP_DAMPING);
        self.tip_y
            .retarget(pos.y, now_ms, Self::TIP_RESPONSE, Self::TIP_DAMPING);
    }

    /// Triggers click pulse (`160 ms`) and expanding click ripple (`360 ms`).
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    pub fn trigger_click(&mut self, pos: LogicalPoint, now_ms: u64, cfg: &AgentCursorConfig) {
        if !cfg.enabled || !is_finite_pt(pos) {
            return;
        }
        self.set_target(pos, pt(0.0, 0.0), None, now_ms);
        self.click_pulse_start_ms = Some(now_ms);
        if cfg.ripple_enabled {
            while self.ripples.len() >= CURSOR_MAX_RIPPLES {
                self.ripples.pop_front();
            }
            self.ripples.push_back(CursorClickRipple {
                center: pos,
                start_ms: now_ms,
            });
        }
    }

    /// Transitions the agent cursor into settle fade-out (`280 ms`), or hides immediately.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    pub fn begin_settle(&mut self, now_ms: u64, immediate: bool) {
        if immediate {
            self.active = false;
            self.settle_fade_start_ms = None;
            self.click_pulse_start_ms = None;
            self.ripples.clear();
            self.trail.clear();
            return;
        }
        if self.active && self.settle_fade_start_ms.is_none() {
            self.settle_fade_start_ms = Some(now_ms);
        }
    }

    /// Handles `Mod+G` human takeover preemption by smoothly settling the visual indicator.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    pub fn on_preempted(&mut self, now_ms: u64) {
        self.last_target_vel = pt(0.0, 0.0);
        self.begin_settle(now_ms, false);
    }

    /// Returns whether the compositor frame loop needs another tick for visual cursor effects.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space (no allocations).
    #[must_use]
    pub fn needs_animation_tick(&self, now_ms: u64, cfg: &AgentCursorConfig) -> bool {
        if !cfg.enabled || !self.active {
            return false;
        }
        let fade_start = self.effective_fade_start_ms(cfg);
        let fade_end = fade_start.saturating_add(CURSOR_SETTLE_FADE_MS);
        if now_ms >= fade_end {
            return false;
        }
        if now_ms >= fade_start {
            return true;
        }
        if !self.tip_x.is_settled()
            || !self.tip_y.is_settled()
            || !self.angle_spring.is_settled()
            || !self.body_x.is_settled()
            || !self.body_y.is_settled()
            || !self.fog_x.is_settled()
            || !self.fog_y.is_settled()
        {
            return true;
        }
        if let Some(p_start) = self.click_pulse_start_ms
            && now_ms < p_start.saturating_add(CURSOR_CLICK_PULSE_MS)
        {
            return true;
        }
        if self
            .ripples
            .iter()
            .any(|r| now_ms < r.start_ms.saturating_add(CURSOR_CLICK_RIPPLE_MS))
        {
            return true;
        }
        if self
            .trail
            .back()
            .is_some_and(|s| now_ms < s.time_ms.saturating_add(CURSOR_TRAIL_LIFETIME_MS))
        {
            return true;
        }
        cfg.idle_wobble
    }

    /// Current tip position if active.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space.
    #[must_use]
    pub const fn current_tip_pos(&self) -> Option<LogicalPoint> {
        if self.active {
            Some(pt(self.tip_x.x, self.tip_y.x))
        } else {
            None
        }
    }

    /// Advances all visual springs and constructs the render-ready `AgentCursorFrameSnapshot`
    /// if the cursor is currently visible at `now_ms`.
    ///
    /// Complexity: $O(1)$ time and $O(1)$ space (bounded capacities: 32 trail samples -> 8 output
    /// points, 2 active ripples).
    pub fn step_frame(
        &mut self,
        now_ms: u64,
        cfg: &AgentCursorConfig,
    ) -> Option<AgentCursorFrameSnapshot> {
        if !cfg.enabled || !self.active {
            return None;
        }

        let fade_start = self.effective_fade_start_ms(cfg);
        let opacity = if now_ms >= fade_start {
            let fade_elapsed = now_ms.saturating_sub(fade_start);
            if fade_elapsed >= CURSOR_SETTLE_FADE_MS {
                self.begin_settle(now_ms, true);
                return None;
            }
            let p = fade_elapsed as f32 / CURSOR_SETTLE_FADE_MS as f32;
            (1.0 - p).clamp(0.0, 1.0)
        } else {
            1.0
        };

        let (tip_x, tip_vx) = self.tip_x.sample_pos_vel(now_ms);
        let (tip_y, tip_vy) = self.tip_y.sample_pos_vel(now_ms);
        let tip_pos = pt(tip_x, tip_y);
        let eff_vx = if self.tip_x.is_settled() {
            self.last_target_vel.x
        } else {
            tip_vx
        };
        let eff_vy = if self.tip_y.is_settled() {
            self.last_target_vel.y
        } else {
            tip_vy
        };
        let speed = eff_vx.hypot(eff_vy);
        let is_moving = speed > 18.0;
        let moving_recently = now_ms.saturating_sub(self.last_activity_ms) < 220;

        // Body & fog displacement lag slightly opposite instantaneous motion and relax to (0, 0).
        let clamp_vec = |vx: f64, vy: f64, gain: f64, max_len: f64| -> LogicalPoint {
            let ox = -vx * gain;
            let oy = -vy * gain;
            let len = ox.hypot(oy);
            if len > max_len && len > TANGENT_EPS {
                pt(ox * (max_len / len), oy * (max_len / len))
            } else {
                pt(ox, oy)
            }
        };
        let body_tgt = clamp_vec(eff_vx, eff_vy, 0.008, 6.0);
        let fog_tgt = clamp_vec(eff_vx, eff_vy, 0.018, 14.0);
        self.body_x
            .retarget(body_tgt.x, now_ms, Self::BODY_RESPONSE, Self::BODY_DAMPING);
        self.body_y
            .retarget(body_tgt.y, now_ms, Self::BODY_RESPONSE, Self::BODY_DAMPING);
        self.fog_x
            .retarget(fog_tgt.x, now_ms, Self::FOG_RESPONSE, Self::FOG_DAMPING);
        self.fog_y
            .retarget(fog_tgt.y, now_ms, Self::FOG_RESPONSE, Self::FOG_DAMPING);
        let (bx, _) = self.body_x.sample_pos_vel(now_ms);
        let (by, _) = self.body_y.sample_pos_vel(now_ms);
        let (fx, _) = self.fog_x.sample_pos_vel(now_ms);
        let (fy, _) = self.fog_y.sample_pos_vel(now_ms);

        // Angle spring using wrap_pi shortest-arc error; rests with gentle anchored wobble.
        let wobble_offset = if cfg.idle_wobble && !is_moving {
            let phase = (now_ms.saturating_sub(self.activated_ms) as f64) * 0.0025;
            (phase * 0.8).sin() * 0.09
        } else {
            0.0
        };
        let base_desired_angle = if is_moving {
            eff_vy.atan2(eff_vx)
        } else if moving_recently {
            self.last_heading_rad.unwrap_or(REST_CURSOR_ANGLE_RAD)
        } else {
            REST_CURSOR_ANGLE_RAD + wobble_offset
        };
        let shortest_delta = wrap_pi(base_desired_angle - wrap_pi(self.angle_spring.x));
        let unwrapped_target = self.angle_spring.x + shortest_delta;
        self.angle_spring.retarget(
            unwrapped_target,
            now_ms,
            Self::ANGLE_RESPONSE,
            Self::ANGLE_DAMPING,
        );
        let (ang_now, _) = self.angle_spring.sample_pos_vel(now_ms);
        self.angle_spring.x = wrap_pi(ang_now);
        self.angle_spring.target = wrap_pi(self.angle_spring.target);
        let rotation_rad = self.angle_spring.x as f32;

        // Sinusoidal click pulse (160 ms: scale = 1.0 - 0.16 * (PI * p).sin()).
        let mut click_boost = 0.0_f32;
        let body_scale = if let Some(p_start) = self.click_pulse_start_ms {
            let elapsed = now_ms.saturating_sub(p_start);
            if elapsed < CURSOR_CLICK_PULSE_MS {
                let p = elapsed as f64 / CURSOR_CLICK_PULSE_MS as f64;
                let s = (PI * p).sin() as f32;
                click_boost = s;
                1.0 - 0.16 * s
            } else {
                self.click_pulse_start_ms = None;
                1.0
            }
        } else {
            1.0
        };

        // Prune & evaluate expanding click ripples (360 ms, capped at 2 active).
        while let Some(front) = self.ripples.front() {
            if now_ms.saturating_sub(front.start_ms) >= CURSOR_CLICK_RIPPLE_MS {
                self.ripples.pop_front();
            } else {
                break;
            }
        }
        let cursor_size = cfg.size.clamp(12.0, 96.0);
        let ripples = if cfg.ripple_enabled {
            self.ripples
                .iter()
                .filter_map(|r| {
                    let elapsed = now_ms.saturating_sub(r.start_ms);
                    if elapsed >= CURSOR_CLICK_RIPPLE_MS {
                        return None;
                    }
                    let p = (elapsed as f32 / CURSOR_CLICK_RIPPLE_MS as f32).clamp(0.0, 1.0);
                    let ease_out = 1.0 - (1.0 - p) * (1.0 - p);
                    let radius = 4.0 + ease_out * (cursor_size * 1.75);
                    let alpha = (1.0 - p) * (1.0 - p) * 0.75 * opacity;
                    Some((r.center, radius, alpha))
                })
                .collect()
        } else {
            Vec::new()
        };

        // Update 32-entry trail ring buffer (220 ms lifetime) and resample to <= 8 points.
        while let Some(front) = self.trail.front() {
            if now_ms.saturating_sub(front.time_ms) > CURSOR_TRAIL_LIFETIME_MS {
                self.trail.pop_front();
            } else {
                break;
            }
        }
        if cfg.trail_enabled {
            let moved_enough = self
                .trail
                .back()
                .is_none_or(|prev| dist_pt(prev.pos, tip_pos) > 1.5);
            if moved_enough {
                while self.trail.len() >= CURSOR_TRAIL_MAX_SAMPLES {
                    self.trail.pop_front();
                }
                self.trail.push_back(TrailSample {
                    pos: tip_pos,
                    time_ms: now_ms,
                });
            }
        } else {
            self.trail.clear();
        }
        let trail_points = if cfg.trail_enabled {
            resample_trail_points(&self.trail, now_ms, cursor_size, opacity)
        } else {
            Vec::new()
        };

        let speed_norm = (speed / 1200.0).clamp(0.0, 1.0) as f32;
        let fog_scale = if cfg.fog_enabled {
            1.0 + 0.35 * speed_norm + 0.18 * click_boost
        } else {
            1.0
        };
        let fog_opacity = if cfg.fog_enabled {
            (0.28 + 0.32 * speed_norm + 0.15 * click_boost) * opacity
        } else {
            0.0
        };

        self.last_step_ms = now_ms;
        Some(AgentCursorFrameSnapshot {
            tip_pos,
            body_offset: pt(bx, by),
            rotation_rad,
            body_scale,
            opacity,
            size: cursor_size,
            accent_rgba: cfg.accent_rgba(),
            fog_offset: pt(fx, fy),
            fog_scale,
            fog_opacity,
            ripples,
            trail_points,
        })
    }

    #[inline]
    fn effective_fade_start_ms(&self, cfg: &AgentCursorConfig) -> u64 {
        let idle_fade = self.last_activity_ms.saturating_add(cfg.idle_timeout_ms);
        match self.settle_fade_start_ms {
            Some(explicit) => explicit.min(idle_fade),
            None => idle_fade,
        }
    }
}

/// Resamples a ring buffer of up to 32 trail samples into `<= 8` control points with tapered width/alpha.
///
/// Complexity: $O(1)$ time and $O(1)$ space (input capped at 32, output capped at 8).
fn resample_trail_points(
    trail: &VecDeque<TrailSample>,
    now_ms: u64,
    cursor_size: f32,
    overall_opacity: f32,
) -> Vec<(LogicalPoint, f32, f32)> {
    let n = trail.len();
    if n < 2 {
        return Vec::new();
    }
    let out_count = n.min(CURSOR_TRAIL_RENDER_POINTS);
    let max_half_w = (cursor_size * 0.22).max(1.5);
    let mut out = Vec::with_capacity(out_count);

    for i in 0..out_count {
        let t = (i as f64) / ((out_count - 1) as f64);
        let f_idx = t * ((n - 1) as f64);
        let i0 = (f_idx.floor() as usize).min(n - 1);
        let i1 = (i0 + 1).min(n - 1);
        let alpha_idx = f_idx - (i0 as f64);
        let s0 = trail[i0];
        let s1 = trail[i1];

        let pos = pt(
            s0.pos.x + (s1.pos.x - s0.pos.x) * alpha_idx,
            s0.pos.y + (s1.pos.y - s0.pos.y) * alpha_idx,
        );
        let time_ms = s0.time_ms as f64 + (s1.time_ms as f64 - s0.time_ms as f64) * alpha_idx;
        let age = (now_ms as f64 - time_ms).clamp(0.0, CURSOR_TRAIL_LIFETIME_MS as f64);
        let age_fade = (1.0 - (age / CURSOR_TRAIL_LIFETIME_MS as f64)) as f32;
        let t_f = t as f32;
        let alpha = (overall_opacity * age_fade * t_f.powf(1.25) * 0.72).clamp(0.0, 1.0);
        let half_width = max_half_w * (0.15 + 0.85 * t_f);
        out.push((pos, alpha, half_width));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::CubicBezier;

    #[test]
    fn centripetal_catmull_rom_interpolates_waypoints_and_survives_duplicates_and_cusps() {
        let w0 = pt(10.0, 20.0);
        let w1 = pt(120.0, 40.0);
        let w2 = pt(260.0, 210.0);
        let w3 = pt(400.0, 90.0);

        let cr = CentripetalCatmullRom::new(&[w0, w0, w1, w2, w2, w3])
            .expect("spline with 4 unique waypoints should construct");
        assert_eq!(cr.segment_count(), 3);
        assert_eq!(cr.waypoints(), &[w0, w1, w2, w3]);

        // Exact interpolation at segment start/end.
        let (p_start, d_start) = cr.eval_segment(0, 0.0);
        assert!((p_start.x - w0.x).abs() < 1e-6 && (p_start.y - w0.y).abs() < 1e-6);
        assert!(d_start.x.is_finite() && d_start.y.is_finite());

        let (p_mid, _) = cr.eval_segment(1, 0.0);
        assert!((p_mid.x - w1.x).abs() < 1e-6 && (p_mid.y - w1.y).abs() < 1e-6);

        let (p_end, d_end) = cr.eval_segment(2, 1.0);
        assert!((p_end.x - w3.x).abs() < 1e-6 && (p_end.y - w3.y).abs() < 1e-6);
        assert!(d_end.x.is_finite() && d_end.y.is_finite());

        // Exact 180-degree foldback hairpin must yield finite positions and finite derivatives.
        let hairpin = CentripetalCatmullRom::new(&[pt(0.0, 0.0), pt(100.0, 0.0), pt(0.0, 0.0)])
            .expect("hairpin with 3 points and 2 segments should construct");
        for seg in 0..hairpin.segment_count() {
            for k in 0..=10 {
                let u = k as f64 / 10.0;
                let (p, d) = hairpin.eval_segment(seg, u);
                assert!(is_finite_pt(p) && is_finite_pt(d));
            }
        }

        // Fewer than 2 distinct points returns None.
        assert!(CentripetalCatmullRom::new(&[w0, w0]).is_none());
    }

    #[test]
    fn arc_length_spline_gauss_legendre_uniformity_is_under_two_percent() {
        let cr = CentripetalCatmullRom::new(&[
            pt(0.0, 0.0),
            pt(180.0, 40.0),
            pt(260.0, 260.0),
            pt(520.0, 180.0),
        ])
        .unwrap();
        let spline = ArcLengthSpline::from_catmull_rom(cr);
        assert!(spline.total_length() > 500.0);

        let samples = 40usize;
        let mut pts = Vec::with_capacity(samples + 1);
        for i in 0..=samples {
            let (p, h) = spline.sample_at_fraction(i as f64 / samples as f64);
            assert!(is_finite_pt(p) && h.is_finite());
            pts.push(p);
        }
        let step_lens: Vec<f64> = pts.windows(2).map(|w| dist_pt(w[0], w[1])).collect();
        let expected_step = spline.total_length() / samples as f64;
        for step in step_lens {
            let rel_err = (step - expected_step).abs() / expected_step;
            assert!(
                rel_err < 0.02,
                "arc-length step uniformity error {rel_err:.4} exceeded 2% (step={step:.3}, expected={expected_step:.3})"
            );
        }
    }

    #[test]
    fn heading_arc_chooser_handles_aligned_reversal_and_viewport_bounds() {
        // Without initial heading, chooses straight direct path.
        let direct = HeadingArcChooser::choose(pt(100.0, 100.0), pt(400.0, 100.0), None, None);
        let (mid_pos, _) = direct.eval(0.5);
        assert!((mid_pos.y - 100.0).abs() < 1e-4);

        // Moving downward (+y), target is right (+x) right near top/bottom wall:
        // initial heading points toward top wall (-PI/2), target goes right-down along wall.
        // Viewport bounds start at y=0, so swinging upward into y < 0 must be penalized in favor of staying inside bounds!
        let bounds = LogicalRect::new(0, 0, 800, 600);
        let chosen = HeadingArcChooser::choose(
            pt(100.0, 20.0),
            pt(400.0, 20.0),
            Some(-0.2), // slightly aiming toward top edge y=0
            Some(bounds),
        );
        for k in 0..24 {
            let u = k as f64 / 23.0;
            let (p, _) = chosen.eval(u);
            assert!(
                p.y >= -2.0,
                "chosen arc should stay clear of top viewport boundary, got y={}",
                p.y
            );
        }
    }

    #[test]
    fn pointer_path_track_supports_multi_waypoint_spring_and_retarget() {
        let mut track = PointerPathTrack::launch_waypoints(
            &[pt(50.0, 50.0), pt(250.0, 120.0), pt(450.0, 80.0)],
            PathSplineKind::CatmullRom,
            pt(0.0, 0.0),
            None,
            None,
            1_000,
            PointerTrajectorySpec::MinimumJerk {
                duration_ms: 200,
                arc_curvature: 0.0,
            },
        );
        assert!(!track.is_settled());
        let p_mid = track.step(1_100);
        assert!(p_mid.x > 50.0 && p_mid.x < 450.0);
        assert!(track.current_heading_rad().is_some());
        assert!(track.current_velocity().x.hypot(track.current_velocity().y) > 10.0);

        // Retarget mid-flight preserves continuity from p_mid.
        track.retarget_waypoints(
            &[pt(600.0, 300.0)],
            PathSplineKind::Auto,
            None,
            1_100,
            PointerTrajectorySpec::Bezier {
                bezier: CubicBezier::new(0.22, 1.0, 0.36, 1.0),
                duration_ms: 150,
                arc_curvature: 0.0,
            },
        );
        let p_retarget_start = track.step(1_100);
        assert!(dist_pt(p_retarget_start, p_mid) < 1e-4);
        let p_end = track.step(1_260);
        assert!(track.is_settled());
        assert!(dist_pt(p_end, pt(600.0, 300.0)) < 1e-4);
    }

    #[test]
    fn pointer_path_track_verlet_spring_advances_and_settles() {
        let mut track = PointerPathTrack::launch_two_point(
            pt(0.0, 0.0),
            pt(300.0, 200.0),
            pt(150.0, 0.0),
            Some(0.0),
            Some(LogicalRect::new(0, 0, 1920, 1080)),
            0,
            PointerTrajectorySpec::HookeSpring {
                params: crate::animation::HookeSpringParams::from_ratio(0.95, 500.0, 1.0, 0.0005),
                duration_cap_ms: Some(1_000),
                arc_curvature: 0.0,
            },
        );
        let p1 = track.step(80);
        assert!(p1.x > 0.0 && p1.x <= 300.0);
        let p_final = track.step(1_050);
        assert!(track.is_settled());
        assert_eq!(p_final, pt(300.0, 200.0));
    }

    #[test]
    fn agent_cursor_visual_dynamics_click_pulse_ripple_trail_and_settle_fade() {
        let cfg = AgentCursorConfig::default();
        let mut dyn_state = AgentCursorVisualDynamics::new();
        assert!(!dyn_state.needs_animation_tick(0, &cfg));
        assert!(dyn_state.step_frame(0, &cfg).is_none());

        // Move across multiple steps to populate ring buffer (cap 32, resampled <= 8).
        for step in 0..45 {
            let t = step * 8;
            let pos = pt(
                100.0 + (step as f64) * 10.0,
                200.0 + ((step as f64) * 0.15).sin() * 25.0,
            );
            dyn_state.set_target(pos, pt(400.0, 40.0), Some(0.1), t);
            let snap = dyn_state
                .step_frame(t, &cfg)
                .expect("active cursor should yield snapshot");
            assert!(snap.trail_points.len() <= CURSOR_TRAIL_RENDER_POINTS);
            assert_eq!(snap.opacity, 1.0);
        }

        // Trigger 3 rapid clicks: only <= 2 ripples active simultaneously + 160ms pulse dips scale.
        dyn_state.trigger_click(pt(550.0, 210.0), 400, &cfg);
        dyn_state.trigger_click(pt(555.0, 212.0), 420, &cfg);
        dyn_state.trigger_click(pt(560.0, 215.0), 440, &cfg);
        let click_snap = dyn_state
            .step_frame(480, &cfg)
            .expect("visible at click pulse peak");
        assert!(click_snap.ripples.len() <= CURSOR_MAX_RIPPLES);
        assert!(
            (f64::from(click_snap.body_scale) - (1.0 - 0.16 * (PI * 40.0 / 160.0).sin())).abs()
                < 1e-4
        );

        // Begin smooth settle fade-out (280 ms).
        dyn_state.begin_settle(600, false);
        let mid_fade = dyn_state
            .step_frame(740, &cfg)
            .expect("halfway through 280ms fade");
        assert!((mid_fade.opacity - 0.5).abs() < 0.02);
        assert!(dyn_state.needs_animation_tick(740, &cfg));

        assert!(dyn_state.step_frame(880, &cfg).is_none());
        assert!(!dyn_state.needs_animation_tick(880, &cfg));
    }
}
