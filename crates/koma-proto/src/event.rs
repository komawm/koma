use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::ids::{Epoch, SessionId, WindowId, WorkspaceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventTopic {
    #[serde(rename = "workspace.lifecycle")]
    WorkspaceLifecycle,
    #[serde(rename = "workspace.lease")]
    WorkspaceLease,
    #[serde(rename = "window.topology")]
    WindowTopology,
    #[serde(rename = "window.damage_settled")]
    WindowDamageSettled,
    #[serde(rename = "security.alerts")]
    SecurityAlerts,
    #[serde(rename = "stream.control")]
    StreamControl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum WorkspaceEvent {
    #[serde(rename = "workspace.created")]
    Created { name: String },
    #[serde(rename = "workspace.epoch_changed")]
    EpochChanged {
        new_epoch: Epoch,
        holder: Option<SessionId>,
    },
    #[serde(rename = "workspace.preempted")]
    Preempted {
        reason: String,
        flushed_keys: Vec<String>,
        cancelled_lua_tx: bool,
    },
    #[serde(rename = "workspace.resumed")]
    Resumed { new_epoch: Epoch },
    #[serde(rename = "window.mapped")]
    WindowMapped {
        win_id: WindowId,
        app_id: String,
        title: String,
    },
    #[serde(rename = "window.settled")]
    WindowSettled { quiet_ms: u64 },
    #[serde(rename = "security.verification_needed")]
    VerificationNeeded { reason: String, win_id: WindowId },
    #[serde(rename = "stream.lagged")]
    StreamLagged { dropped_count: u64, latest_seq: u64 },
}

impl WorkspaceEvent {
    #[must_use]
    pub const fn is_critical_lease_barrier(&self) -> bool {
        matches!(
            self,
            Self::EpochChanged { .. } | Self::Preempted { .. } | Self::Resumed { .. }
        )
    }

    #[must_use]
    pub const fn is_coalesceable_settle(&self) -> bool {
        matches!(self, Self::WindowSettled { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEventEnvelope {
    pub seq: u64,
    pub timestamp_ns: u64,
    pub topic: EventTopic,
    pub ws_id: WorkspaceId,
    pub epoch: Epoch,
    pub event: WorkspaceEvent,
}

/// Non-blocking 256-slot event ring buffer implementing `IPC_SPEC.md` §6.3 backpressure.
#[derive(Debug)]
pub struct EventRingBuffer {
    capacity: usize,
    next_seq: u64,
    dropped_count: u64,
    queue: VecDeque<IpcEventEnvelope>,
}

impl Default for EventRingBuffer {
    fn default() -> Self {
        Self::new(256)
    }
}

impl EventRingBuffer {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(4);
        Self {
            capacity: cap,
            next_seq: 1,
            dropped_count: 0,
            queue: VecDeque::with_capacity(cap),
        }
    }

    pub fn push(
        &mut self,
        timestamp_ns: u64,
        topic: EventTopic,
        ws_id: WorkspaceId,
        epoch: Epoch,
        event: WorkspaceEvent,
    ) {
        let seq = self.next_seq;
        self.next_seq += 1;

        // Rule 1: Coalesce consecutive or pending `window.settled` frames on the same workspace.
        if event.is_coalesceable_settle() {
            if let Some(existing) = self
                .queue
                .iter_mut()
                .find(|e| e.ws_id == ws_id && e.event.is_coalesceable_settle())
            {
                existing.seq = seq;
                existing.timestamp_ns = timestamp_ns;
                existing.epoch = epoch;
                existing.event = event;
                return;
            }
        }

        // Rule 2 & 3: Evict oldest non-critical event when full; never drop critical lease barriers.
        if self.queue.len() >= self.capacity {
            if let Some(idx) = self
                .queue
                .iter()
                .position(|e| !e.event.is_critical_lease_barrier())
            {
                self.queue.remove(idx);
                self.dropped_count += 1;
            } else {
                self.queue.pop_front();
                self.dropped_count += 1;
            }
        }

        self.queue.push_back(IpcEventEnvelope {
            seq,
            timestamp_ns,
            topic,
            ws_id,
            epoch,
            event,
        });
    }

    pub fn pop_next(&mut self) -> Option<IpcEventEnvelope> {
        if self.dropped_count > 0 {
            if let Some(front) = self.queue.front() {
                let lagged = IpcEventEnvelope {
                    seq: front.seq,
                    timestamp_ns: front.timestamp_ns,
                    topic: EventTopic::StreamControl,
                    ws_id: front.ws_id.clone(),
                    epoch: front.epoch,
                    event: WorkspaceEvent::StreamLagged {
                        dropped_count: self.dropped_count,
                        latest_seq: self.next_seq.saturating_sub(1),
                    },
                };
                self.dropped_count = 0;
                return Some(lagged);
            }
        }
        self.queue.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_preempt_events_under_backpressure() {
        let mut ring = EventRingBuffer::new(4);
        let ws = WorkspaceId::from("ws_01");
        ring.push(
            10,
            EventTopic::WorkspaceLease,
            ws.clone(),
            Epoch(2),
            WorkspaceEvent::Preempted {
                reason: "mod_g".into(),
                flushed_keys: vec!["Shift_L".into()],
                cancelled_lua_tx: true,
            },
        );
        for i in 0..8 {
            ring.push(
                20 + i,
                EventTopic::WindowTopology,
                ws.clone(),
                Epoch(2),
                WorkspaceEvent::WindowMapped {
                    win_id: WindowId::new(i, 1),
                    app_id: "foot".into(),
                    title: "shell".into(),
                },
            );
        }
        let first = ring.pop_next().expect("lagged marker");
        assert!(matches!(first.event, WorkspaceEvent::StreamLagged { .. }));
        let second = ring.pop_next().expect("preserved preempt barrier");
        assert!(matches!(second.event, WorkspaceEvent::Preempted { .. }));
    }
}
