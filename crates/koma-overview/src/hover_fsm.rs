//! Deterministic `150 ms` status-bar hover dwell & pill-scrubbing state machine (`ARCHITECTURE.md` §3.3).
//!
//! Solves Waybar's GTK3 dynamic image tooltip limitation (#1999):
//! - Quick cursor transit (`< 150 ms`) across workspace pills triggers **zero** popup surface mapping
//!   and **zero** `koma-admin.sock` `capture.frame` / GPU wakeups.
//! - Once `150 ms` elapses, maps the `400x252` (`384x216` thumbnail + header) `zwlr_layer_surface_v1`
//!   card directly beneath the hovered pill and subscribes the workspace to Tier 3 (`8 Hz`).
//! - Sliding directly onto an adjacent pill while the popup is already open scrubs **instantly** (`0 ms`)
//!   without tearing down and recreating the Wayland layer surface!

use koma_proto::WorkspaceId;

pub const HOVER_DWELL_MS: u64 = 150;
pub const PREVIEW_REFRESH_MS: u64 = 125; // 8 Hz Tier 3 `PreviewThumbnail` cadence
pub const THUMBNAIL_WIDTH: u32 = 384;
pub const THUMBNAIL_HEIGHT: u32 = 216;
pub const POPUP_PADDING_PX: u32 = 8;
pub const POPUP_HEADER_HEIGHT: u32 = 28;
pub const POPUP_CARD_WIDTH: u32 = THUMBNAIL_WIDTH + POPUP_PADDING_PX * 2; // 400 px
pub const POPUP_CARD_HEIGHT: u32 = THUMBNAIL_HEIGHT + POPUP_HEADER_HEIGHT + POPUP_PADDING_PX; // 252 px
pub const BAR_POPUP_GAP_PX: i32 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverPhase {
    Idle,
    Dwelling {
        ws_id: WorkspaceId,
        anchor_x: i32,
        bar_height: i32,
        entered_at_ms: u64,
    },
    ActivePopup {
        ws_id: WorkspaceId,
        anchor_x: i32,
        bar_height: i32,
        last_refresh_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverEffect {
    /// Schedule a one-shot timer on `calloop` to check dwell completion at `deadline_ms`.
    ArmDwellTimer { deadline_ms: u64 },
    /// Subscribe `ws_id` to Tier 3 preview (`ws.set_preview_subscribe`), map/reposition popup, and capture frame.
    ShowOrMovePopup {
        unsubscribe_prev: Option<WorkspaceId>,
        subscribe_new: WorkspaceId,
        margin_left: i32,
        margin_top: i32,
    },
    /// Fetch a fresh `8 Hz` `384x216` frame for the currently visible popup workspace.
    RefreshPopupFrame { ws_id: WorkspaceId },
    /// Unmap popup `zwlr_layer_surface_v1` and drop Tier 3 preview subscription (`0%` idle GPU).
    HidePopup { unsubscribe_ws: WorkspaceId },
}

#[derive(Debug, Clone)]
pub struct HoverDwellMachine {
    phase: HoverPhase,
}

impl Default for HoverDwellMachine {
    fn default() -> Self {
        Self {
            phase: HoverPhase::Idle,
        }
    }
}

impl HoverDwellMachine {
    #[must_use]
    pub const fn phase(&self) -> &HoverPhase {
        &self.phase
    }

    #[must_use]
    pub fn active_workspace(&self) -> Option<&WorkspaceId> {
        match &self.phase {
            HoverPhase::ActivePopup { ws_id, .. } => Some(ws_id),
            _ => None,
        }
    }

    /// Invoked when the pointer enters a status-bar workspace button (`Waybar` / `Ironbar` / edge tracker).
    pub fn on_hover_enter(
        &mut self,
        ws_id: WorkspaceId,
        anchor_x: i32,
        bar_height: i32,
        now_ms: u64,
    ) -> Option<HoverEffect> {
        let clamped_x = anchor_x.max(4);
        let top_margin = bar_height.max(0) + BAR_POPUP_GAP_PX;

        match &self.phase {
            HoverPhase::Idle => {
                let deadline_ms = now_ms.saturating_add(HOVER_DWELL_MS);
                self.phase = HoverPhase::Dwelling {
                    ws_id,
                    anchor_x: clamped_x,
                    bar_height,
                    entered_at_ms: now_ms,
                };
                Some(HoverEffect::ArmDwellTimer { deadline_ms })
            }
            HoverPhase::Dwelling {
                ws_id: cur_ws,
                anchor_x: cur_x,
                ..
            } => {
                if *cur_ws == ws_id && *cur_x == clamped_x {
                    return None;
                }
                let deadline_ms = now_ms.saturating_add(HOVER_DWELL_MS);
                self.phase = HoverPhase::Dwelling {
                    ws_id,
                    anchor_x: clamped_x,
                    bar_height,
                    entered_at_ms: now_ms,
                };
                Some(HoverEffect::ArmDwellTimer { deadline_ms })
            }
            HoverPhase::ActivePopup {
                ws_id: prev_ws,
                anchor_x: prev_x,
                ..
            } => {
                if *prev_ws == ws_id && *prev_x == clamped_x {
                    return None;
                }
                let unsubscribe_prev = (*prev_ws != ws_id).then(|| prev_ws.clone());
                let subscribe_new = ws_id.clone();
                self.phase = HoverPhase::ActivePopup {
                    ws_id,
                    anchor_x: clamped_x,
                    bar_height,
                    last_refresh_ms: now_ms,
                };
                Some(HoverEffect::ShowOrMovePopup {
                    unsubscribe_prev,
                    subscribe_new,
                    margin_left: clamped_x,
                    margin_top: top_margin,
                })
            }
        }
    }

    /// Invoked when the pointer leaves the status-bar workspace module.
    pub fn on_hover_leave(&mut self) -> Option<HoverEffect> {
        match std::mem::replace(&mut self.phase, HoverPhase::Idle) {
            HoverPhase::Idle | HoverPhase::Dwelling { .. } => None,
            HoverPhase::ActivePopup { ws_id, .. } => Some(HoverEffect::HidePopup {
                unsubscribe_ws: ws_id,
            }),
        }
    }

    /// Polls monotonic timers (`150 ms` dwell threshold & `125 ms` `8 Hz` refresh cadence).
    pub fn poll_tick(&mut self, now_ms: u64) -> Option<HoverEffect> {
        match &mut self.phase {
            HoverPhase::Idle => None,
            HoverPhase::Dwelling {
                ws_id,
                anchor_x,
                bar_height,
                entered_at_ms,
            } => {
                if now_ms.saturating_sub(*entered_at_ms) >= HOVER_DWELL_MS {
                    let target_ws = ws_id.clone();
                    let margin_left = *anchor_x;
                    let margin_top = (*bar_height).max(0) + BAR_POPUP_GAP_PX;
                    let bh = *bar_height;
                    self.phase = HoverPhase::ActivePopup {
                        ws_id: target_ws.clone(),
                        anchor_x: margin_left,
                        bar_height: bh,
                        last_refresh_ms: now_ms,
                    };
                    Some(HoverEffect::ShowOrMovePopup {
                        unsubscribe_prev: None,
                        subscribe_new: target_ws,
                        margin_left,
                        margin_top,
                    })
                } else {
                    None
                }
            }
            HoverPhase::ActivePopup {
                ws_id,
                last_refresh_ms,
                ..
            } => {
                if now_ms.saturating_sub(*last_refresh_ms) >= PREVIEW_REFRESH_MS {
                    *last_refresh_ms = now_ms;
                    Some(HoverEffect::RefreshPopupFrame {
                        ws_id: ws_id.clone(),
                    })
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_transient_hover_under_150ms_and_scrubs_instantly_when_open() {
        let mut fsm = HoverDwellMachine::default();

        // Enter ws_01 at t = 100 ms, leave at t = 210 ms (110 ms < 150 ms) -> zero popup!
        let fx = fsm.on_hover_enter(WorkspaceId::from("ws_01"), 120, 32, 100);
        assert_eq!(fx, Some(HoverEffect::ArmDwellTimer { deadline_ms: 250 }));
        assert_eq!(fsm.poll_tick(210), None);
        assert_eq!(fsm.on_hover_leave(), None);

        // Re-enter ws_01 at t = 300 ms and hold until t = 450 ms -> popup maps!
        let _ = fsm.on_hover_enter(WorkspaceId::from("ws_01"), 120, 32, 300);
        let open_fx = fsm.poll_tick(450).expect("150ms dwell elapsed");
        assert_eq!(
            open_fx,
            HoverEffect::ShowOrMovePopup {
                unsubscribe_prev: None,
                subscribe_new: WorkspaceId::from("ws_01"),
                margin_left: 120,
                margin_top: 38,
            }
        );

        // Slide immediately onto ws_02 at t = 470 ms -> 0 ms instant scrub!
        let scrub_fx = fsm
            .on_hover_enter(WorkspaceId::from("ws_02"), 200, 32, 470)
            .expect("instant scrub while popup open");
        assert_eq!(
            scrub_fx,
            HoverEffect::ShowOrMovePopup {
                unsubscribe_prev: Some(WorkspaceId::from("ws_01")),
                subscribe_new: WorkspaceId::from("ws_02"),
                margin_left: 200,
                margin_top: 38,
            }
        );

        // Leave bar -> unsubscribes ws_02 cleanly
        assert_eq!(
            fsm.on_hover_leave(),
            Some(HoverEffect::HidePopup {
                unsubscribe_ws: WorkspaceId::from("ws_02")
            })
        );
    }
}
