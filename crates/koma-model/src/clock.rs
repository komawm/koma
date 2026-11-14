use serde::{Deserialize, Serialize};

/// Four-tier adaptive workspace frame clock (`ARCHITECTURE.md` §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameClockTier {
    /// Tier 1: Locked to hardware DRM/KMS VSync (`60-240 Hz`).
    HeadedVsync,
    /// Tier 2: `60 Hz` `wl_surface.frame` ticks for `1.0 s` after input/damage; `0 Hz` idle GPU (`Render-on-Read`).
    HeadlessBurst,
    /// Tier 3: `5-10 Hz` downsampled `384x216` preview while `koma-overview` subscribes.
    PreviewThumbnail,
    /// Tier 4a: `1 Hz` heartbeat while settled.
    IdleHeartbeat,
    /// Tier 4b: `0 Hz` halted after `idle_freeze_ms`; all offscreen FBOs released.
    FrozenZeroHz,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FrameClockConfig {
    pub burst_hz: u32,
    pub burst_window_ms: u64,
    pub preview_hz: u32,
    pub idle_freeze_ms: u64,
}

impl Default for FrameClockConfig {
    fn default() -> Self {
        Self {
            burst_hz: 60,
            burst_window_ms: 1_000,
            preview_hz: 8,
            idle_freeze_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTickDecision {
    pub tier: FrameClockTier,
    pub send_client_frame_callbacks: bool,
    pub composite_thumbnail_fbo: bool,
    pub release_pooled_fbo: bool,
}

#[derive(Debug, Clone)]
pub struct AdaptiveFrameClock {
    config: FrameClockConfig,
    is_headed: bool,
    preview_subscribers: u32,
    last_activity_ms: u64,
    last_frame_callback_ms: u64,
}

impl AdaptiveFrameClock {
    #[must_use]
    pub const fn new(is_headed: bool, config: FrameClockConfig, now_ms: u64) -> Self {
        Self {
            config,
            is_headed,
            preview_subscribers: 0,
            last_activity_ms: now_ms,
            last_frame_callback_ms: 0,
        }
    }

    pub fn record_activity(&mut self, now_ms: u64) {
        self.last_activity_ms = now_ms;
    }

    pub fn set_headed(&mut self, headed: bool, now_ms: u64) {
        self.is_headed = headed;
        self.last_activity_ms = now_ms;
    }

    pub fn set_preview_subscribers(&mut self, count: u32) {
        self.preview_subscribers = count;
    }

    #[must_use]
    pub const fn current_tier(&self, now_ms: u64) -> FrameClockTier {
        if self.is_headed {
            return FrameClockTier::HeadedVsync;
        }
        let elapsed = now_ms.saturating_sub(self.last_activity_ms);
        if elapsed < self.config.burst_window_ms {
            FrameClockTier::HeadlessBurst
        } else if self.preview_subscribers > 0 {
            FrameClockTier::PreviewThumbnail
        } else if elapsed < self.config.idle_freeze_ms {
            FrameClockTier::IdleHeartbeat
        } else {
            FrameClockTier::FrozenZeroHz
        }
    }

    pub fn poll_tick(&mut self, now_ms: u64) -> FrameTickDecision {
        let tier = self.current_tier(now_ms);
        match tier {
            FrameClockTier::HeadedVsync => {
                self.last_frame_callback_ms = now_ms;
                FrameTickDecision {
                    tier,
                    send_client_frame_callbacks: true,
                    composite_thumbnail_fbo: false,
                    release_pooled_fbo: false,
                }
            }
            FrameClockTier::HeadlessBurst => {
                let interval_ms = (1_000 / self.config.burst_hz.max(1)) as u64;
                let due = now_ms.saturating_sub(self.last_frame_callback_ms) >= interval_ms;
                if due {
                    self.last_frame_callback_ms = now_ms;
                }
                FrameTickDecision {
                    tier,
                    send_client_frame_callbacks: due,
                    // Render-on-Read: 0 Hz background GPU shader execution during headless burst!
                    composite_thumbnail_fbo: false,
                    release_pooled_fbo: false,
                }
            }
            FrameClockTier::PreviewThumbnail => {
                let interval_ms = (1_000 / self.config.preview_hz.max(1)) as u64;
                let due = now_ms.saturating_sub(self.last_frame_callback_ms) >= interval_ms;
                if due {
                    self.last_frame_callback_ms = now_ms;
                }
                FrameTickDecision {
                    tier,
                    send_client_frame_callbacks: due,
                    composite_thumbnail_fbo: due,
                    release_pooled_fbo: false,
                }
            }
            FrameClockTier::IdleHeartbeat => {
                let due = now_ms.saturating_sub(self.last_frame_callback_ms) >= 1_000;
                if due {
                    self.last_frame_callback_ms = now_ms;
                }
                FrameTickDecision {
                    tier,
                    send_client_frame_callbacks: due,
                    composite_thumbnail_fbo: false,
                    release_pooled_fbo: false,
                }
            }
            FrameClockTier::FrozenZeroHz => FrameTickDecision {
                tier,
                send_client_frame_callbacks: false,
                composite_thumbnail_fbo: false,
                release_pooled_fbo: true,
            },
        }
    }
}
