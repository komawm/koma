use koma_model::{Anchor9, LogicalPoint, LogicalRect};

#[derive(Debug, Clone)]
pub enum RecordedStep {
    Click { pt: LogicalPoint, button: String },
    KeyChord(String),
    TextBurst(String),
    WaitSettle { quiet_ms: u64, timeout_ms: u64 },
}

/// Snaps an absolute window click to the nearest of the 9 window edges/corners (`LUA_API_SPEC.md` §5.4).
#[must_use]
pub fn snap_to_nearest_anchor(pt: LogicalPoint, win: LogicalRect) -> (Anchor9, i32, i32) {
    let candidates = [
        Anchor9::TopLeft,
        Anchor9::Top,
        Anchor9::TopRight,
        Anchor9::Left,
        Anchor9::Center,
        Anchor9::Right,
        Anchor9::BottomLeft,
        Anchor9::Bottom,
        Anchor9::BottomRight,
    ];
    let mut best = Anchor9::TopLeft;
    let mut best_dist_sq = f64::INFINITY;
    for anchor in candidates {
        let origin = anchor.origin_in(win);
        let dx = pt.x - origin.x;
        let dy = pt.y - origin.y;
        let d2 = dx * dx + dy * dy;
        if d2 < best_dist_sq {
            best_dist_sq = d2;
            best = anchor;
        }
    }
    let origin = best.origin_in(win);
    (
        best,
        (pt.x - origin.x).round() as i32,
        (pt.y - origin.y).round() as i32,
    )
}

/// Emits formatted, human-editable Lua 5.4 macro source from recorded `seat0` steps.
#[must_use]
pub fn compile_recorded_macro(
    macro_id: &str,
    app_id: &str,
    win_rect: LogicalRect,
    steps: &[RecordedStep],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "koma.macro.define({{
    id = {macro_id:?},
    app_id = \"^{app_id}$\",
    run = function(ctx, _args)
"
    ));
    for step in steps {
        match step {
            RecordedStep::Click { pt, button } => {
                let (anchor, dx, dy) = snap_to_nearest_anchor(*pt, win_rect);
                let anchor_name =
                    serde_json::to_string(&anchor).unwrap_or_else(|_| "\"top-left\"".into());
                out.push_str(&format!(
                    "        ctx:click({{ anchor = {anchor_name}, x = {dx}, y = {dy} }}, {{ button = {button:?} }})
"
                ));
            }
            RecordedStep::KeyChord(chord) => {
                out.push_str(&format!(
                    "        ctx:key({chord:?})
"
                ));
            }
            RecordedStep::TextBurst(txt) => {
                out.push_str(&format!(
                    "        ctx:text({txt:?}, {{ strategy = \"auto\" }})
"
                ));
            }
            RecordedStep::WaitSettle {
                quiet_ms,
                timeout_ms,
            } => {
                out.push_str(&format!(
                    "        ctx:wait_settle({{ quiet_ms = {quiet_ms}, timeout_ms = {timeout_ms} }})
"
                ));
            }
        }
    }
    out.push_str(
        "    end,
})
",
    );
    out
}
